use dioxus::prelude::*;

use crate::agent::{self, AgentAdapter, TurnOptions};
use crate::api_chat;
use crate::claude::{ChatMode, ClaudeEvent, ClaudeProcess, DISCUSSION_DISALLOWED_TOOLS, EXIT_PLAN_TOOL, PermissionDecision, SpawnOptions, tool_file_path, tool_summary, tool_touches_files};
use crate::history;
use crate::projects::same_dir;
use crate::{mcp, mcp_oauth, scope};
use crate::providers::{self, Provider};
use crate::prompts;
use crate::state::{AppState, Chat, ChatItem, ChatRequest, Cmd, PlanApproval};

struct Live {
    agent: Box<dyn AgentAdapter>,
    rules_version: u64,
    /// The mode the agent runs in; a different chat mode restarts it on the next message.
    mode: ChatMode,
    /// The MCP config the Workbench process was started with; a new one (such as a refreshed sign-in token) restarts it.
    mcp_config: Option<String>,
    /// The model the agent runs; a job routed to another model restarts it.
    model: String,
}

/// The session loop of one agent: receives its commands, owns its agent process and turns the
/// agent's events into its chat items.
pub async fn run(mut st: AppState, mut chat: Chat, mut rx: tokio::sync::mpsc::UnboundedReceiver<Cmd>) {
    let st = &mut st;
    let chat = &mut chat;
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<(u64, ClaudeEvent)>();
    let mut live: Option<Live> = None;
    let mut generation = 0u64;
    // How many of the execution chat's items a discussion chat has been shown (PRO-248).
    let mut digested = 0usize;

    loop {
        if !*chat.busy.peek() && !chat.queued.peek().is_empty() {
            let msg = chat.queued.write().remove(0);
            if st.owns_turn(chat) {
                st.resume_job();
            }
            let text = msg.prompt.clone();
            deliver(st, chat, &mut live, &mut generation, &ev_tx, &mut digested, ChatItem::User(msg), text).await;
            continue;
        }
        tokio::select! {
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break };
                let (shown, text) = match cmd {
                    Cmd::Send(msg) if *chat.busy.peek() || !chat.queued.peek().is_empty() => {
                        chat.queued.write().push(msg);
                        continue;
                    }
                    Cmd::Send(msg) | Cmd::SendNow(msg) => {
                        if st.owns_turn(chat) {
                            st.resume_job();
                        }
                        let text = msg.prompt.clone();
                        (ChatItem::User(msg), text)
                    }
                    Cmd::Prompt { label, text } => (ChatItem::Prompt { label, text: text.clone() }, text),
                    Cmd::Permission { request_id, decision } => {
                        let req = {
                            let mut pending = chat.permissions.write();
                            pending.iter().position(|p| p.request_id == request_id).map(|i| pending.remove(i))
                        };
                        let Some(req) = req else { continue };
                        if let Some(l) = live.as_mut()
                            && let Err(e) = l.agent.respond_permission(&request_id, &decision).await
                        {
                            chat.items.write().push(ChatItem::Error(format!("could not answer the permission request: {e:#}")));
                            continue;
                        }
                        let cwd = chat.dir.cloned();
                        let summary = tool_summary(&req.tool, &req.input, &cwd);
                        let verdict = match &decision {
                            PermissionDecision::Allow { permissions, .. } if !permissions.is_empty() => "Always allowed",
                            PermissionDecision::Allow { .. } => "Allowed",
                            PermissionDecision::Deny { interrupt: true, .. } => "Denied and stopped",
                            PermissionDecision::Deny { .. } => "Denied",
                        };
                        chat.items.write().push(ChatItem::Info(format!("{verdict} {}: {summary}", req.tool)));
                        continue;
                    }
                    Cmd::ApprovePlan { mode } => {
                        let Some(approval) = chat.plan_approval.take() else { continue };
                        if let Some(mode) = mode {
                            chat.chat_mode.set(mode);
                        }
                        match (approval.request, mode) {
                            // Workbench: answer its ExitPlanMode request and switch the running session's mode.
                            (Some(req), Some(mode)) => {
                                let set_mode = serde_json::json!({ "type": "setMode", "mode": mode.cli_arg(), "destination": "session" });
                                let decision = PermissionDecision::Allow { input: req.input.clone(), permissions: vec![set_mode] };
                                answer_plan(chat, &mut live, &req.request_id, &decision).await;
                                if let Some(l) = live.as_mut() {
                                    l.mode = mode;
                                }
                                chat.items.write().push(ChatItem::Info(format!("Plan approved · running in {}", mode.label())));
                                continue;
                            }
                            (Some(req), None) => {
                                let decision = PermissionDecision::Deny {
                                    message: "The user wants to keep planning. Ask them what to change in the plan.".into(),
                                    interrupt: false,
                                };
                                answer_plan(chat, &mut live, &req.request_id, &decision).await;
                                chat.items.write().push(ChatItem::Info("Kept planning".into()));
                                continue;
                            }
                            // Other providers: ask for the plan to be implemented in a new turn, in the picked mode.
                            (None, Some(mode)) => {
                                let text = "The plan is approved. Implement it now.".to_string();
                                (ChatItem::Prompt { label: format!("Run the plan · {}", mode.label()), text: text.clone() }, text)
                            }
                            (None, None) => continue,
                        }
                    }
                    Cmd::Interrupt => {
                        deny_pending(chat, &mut live).await;
                        if let Some(l) = live.as_mut()
                            && l.agent.interrupt().await.is_err()
                        {
                            if let Some(l) = live.take() {
                                l.agent.kill().await;
                            }
                            generation += 1;
                            chat.busy.set(false);
                            finish(st, chat, false);
                        }
                        continue;
                    }
                    Cmd::Resume { id } => {
                        if let Some(l) = live.take() {
                            l.agent.kill().await;
                        }
                        generation += 1;
                        chat.permissions.write().clear();
                        chat.plan_approval.set(None);
                        chat.queued.write().clear();
                        chat.busy.set(false);
                        finish(st, chat, false);
                        let cwd = chat.dir.cloned();
                        let load_id = id.clone();
                        let items = tokio::task::spawn_blocking(move || history::load(&cwd, &load_id)).await.unwrap_or_default();
                        st.save_transcript(chat, Some(id.clone()));
                        chat.session_id.set(Some(id));
                        chat.last_cost.set(None);
                        chat.items.set(items);
                        continue;
                    }
                    Cmd::Close => {
                        deny_pending(chat, &mut live).await;
                        if let Some(l) = live.take() {
                            l.agent.kill().await;
                        }
                        finish(st, chat, false);
                        return;
                    }
                    Cmd::Restart { keep_session } => {
                        if let Some(l) = live.take() {
                            l.agent.kill().await;
                        }
                        generation += 1;
                        chat.permissions.write().clear();
                        chat.plan_approval.set(None);
                        chat.busy.set(false);
                        chat.queued.write().clear();
                        finish(st, chat, false);
                        if !keep_session {
                            chat.session_id.set(None);
                            st.save_transcript(chat, None);
                            chat.items.write().clear();
                            chat.last_cost.set(None);
                        }
                        continue;
                    }
                };
                deliver(st, chat, &mut live, &mut generation, &ev_tx, &mut digested, shown, text).await;
            }
            Some((event_gen, ev)) = ev_rx.recv() => {
                if event_gen != generation {
                    continue;
                }
                if let ClaudeEvent::ControlRequest { request_id, subtype } = &ev {
                    if let Some(l) = live.as_mut() {
                        let _ = l.agent.respond_error(request_id, &format!("Workbench does not handle `{subtype}` requests")).await;
                    }
                    continue;
                }
                if let ClaudeEvent::Exited = ev {
                    let code = live.as_mut().and_then(|l| l.agent.exit_code());
                    live = None;
                    chat.permissions.write().clear();
                    // A plan from a one-turn provider outlives its process; Workbench's plan request doesn't.
                    if chat.plan_approval.peek().as_ref().is_some_and(|a| a.request.is_some()) {
                        chat.plan_approval.set(None);
                    }
                    // One-turn providers exit after every finished turn; only an exit mid-turn is a failure.
                    if *chat.busy.peek() {
                        let name = st.agent_provider(chat).0.short_name();
                        chat.items.write().push(ChatItem::Error(format!("{name} exited (code {code:?})")));
                        chat.busy.set(false);
                        finish(st, chat, false);
                    }
                    continue;
                }
                // A discussion chat is read only: it never gets permission to act.
                if let ClaudeEvent::Permission(req) = &ev
                    && chat.is_discussion()
                    && let Some(l) = live.as_mut()
                {
                    let decision = PermissionDecision::Deny { message: DISCUSSION_DENIAL.into(), interrupt: false };
                    if let Err(e) = l.agent.respond_permission(&req.request_id, &decision).await {
                        chat.items.write().push(ChatItem::Error(format!("could not deny the permission request: {e:#}")));
                    }
                    let cwd = chat.dir.cloned();
                    chat.items.write().push(ChatItem::Info(format!("Denied {} (discussion chats are read only): {}", req.tool, tool_summary(&req.tool, &req.input, &cwd))));
                    continue;
                }
                if let ClaudeEvent::Permission(req) = &ev
                    && chat.chat_mode.peek().auto_allows(req)
                    && let Some(l) = live.as_mut()
                {
                    let decision = PermissionDecision::Allow { input: req.input.clone(), permissions: Vec::new() };
                    match l.agent.respond_permission(&req.request_id, &decision).await {
                        Ok(()) => {
                            let cwd = chat.dir.cloned();
                            let summary = tool_summary(&req.tool, &req.input, &cwd);
                            chat.items.write().push(ChatItem::Info(format!("Auto-allowed {}: {summary}", req.tool)));
                            continue;
                        }
                        Err(e) => chat.items.write().push(ChatItem::Error(format!("could not auto-allow the permission request: {e:#}"))),
                    }
                }
                handle_event(st, chat, ev);
            }
        }
    }
}

async fn deliver(
    st: &mut AppState,
    chat: &mut Chat,
    live: &mut Option<Live>,
    generation: &mut u64,
    ev_tx: &tokio::sync::mpsc::UnboundedSender<(u64, ClaudeEvent)>,
    digested: &mut usize,
    mut shown: ChatItem,
    mut text: String,
) {
    if chat.is_discussion() {
        text = discussion_prompt(st, chat, digested, &text);
        if let ChatItem::User(msg) = &mut shown {
            msg.prompt = text.clone();
        }
    }
    let rules_version = *st.rules_version.peek();
    let (provider, model) = st.agent_provider(chat);
    if provider != Provider::ClaudeCode {
        start_turn(st, chat, live, generation, ev_tx, shown, text, provider, model).await;
        return;
    }
    let mode = *chat.chat_mode.peek();
    let servers = st.mcp_servers.peek().clone();
    let servers = crate::figma::with_token(servers, &crate::figma::saved_token());
    let mcp_config = mcp::claude_config(&mcp_oauth::with_tokens(&servers).await);
    if live.as_ref().is_some_and(|l| l.rules_version != rules_version || l.mode != mode || l.mcp_config != mcp_config || l.model != model)
        && let Some(l) = live.take()
    {
        l.agent.kill().await;
        *generation += 1;
        chat.permissions.write().clear();
        chat.plan_approval.set(None);
    }
    if live.is_none() {
        *generation += 1;
        let opts = SpawnOptions {
            cwd: chat.dir.cloned(),
            resume: chat.session_id.peek().clone(),
            mode,
            append_system_prompt: system_prompt(st, chat),
            mcp_config: mcp_config.clone(),
            model: model.clone(),
            disallowed_tools: if chat.is_discussion() { DISCUSSION_DISALLOWED_TOOLS.to_vec() } else { Vec::new() },
        };
        match ClaudeProcess::spawn(&opts, *generation, ev_tx.clone()) {
            Ok(proc) => *live = Some(Live { agent: Box::new(proc), rules_version, mode, mcp_config, model }),
            Err(e) => {
                chat.items.write().push(ChatItem::Error(format!("{e:#}")));
                finish(st, chat, false);
                return;
            }
        }
    }
    begin_turn(st, chat);
    chat.items.write().push(shown);
    chat.busy.set(true);
    if let Some(l) = live.as_mut()
        && let Err(e) = l.agent.send_user(&text).await
    {
        chat.items.write().push(ChatItem::Error(format!("send failed: {e:#}")));
        chat.busy.set(false);
        finish(st, chat, false);
        *live = None;
    }
}

/// The Workbench context every provider gets: graph upkeep, rules, related repos, the bound Figma project and the project.
/// Agents can also start other agents in their chat session (see `prompts::spawn_requests`).
/// A project-level agent gets the project's repositories instead, since it works above them.
fn system_prompt(st: &AppState, chat: &Chat) -> Option<String> {
    let dir = chat.dir.cloned();
    if chat.is_discussion() {
        let base = st.with_refs(|refs| prompts::system_append(Some(&*st.graph.peek()), &st.rules.peek(), &st.related_repos.peek(), Some(refs)));
        let base = prompts::with_project(base, st.chat_project(chat).as_ref(), &dir).unwrap_or_default();
        let agent = st.execution_of(chat).and_then(|c| agent_name(st, &c)).unwrap_or_else(|| "the agent".into());
        return Some(format!("{base}{}{}", prompts::focus_instructions(), prompts::discussion_instructions(&agent)));
    }
    let project = st.chat_project(chat);
    let context = match (st.project_session_of(chat), project.as_ref()) {
        (Some(_), Some(p)) => prompts::project_agent_prompt(p, &dir),
        _ => {
            let base = st.with_refs(|refs| prompts::system_append(Some(&*st.graph.peek()), &st.rules.peek(), &st.related_repos.peek(), Some(refs)));
            let base = prompts::with_project(base, project.as_ref(), &dir).unwrap_or_default();
            let figma = st.figma.peek().as_ref().map(crate::figma::prompt_section).unwrap_or_default();
            format!("{base}{figma}{}{}", prompts::focus_instructions(), prompts::query_instructions(&crate::query::command_line(&dir)))
        }
    };
    Some(format!("{context}{}", prompts::spawn_agent_instructions(project.as_ref(), &st.provider_settings.peek())))
}

/// Starts the agents a reply asks for: in the same session, or in a new session in a
/// repository of the agent's project, on the provider and model the block names, if any.
fn start_spawns(st: &mut AppState, mut chat: Chat, spawns: Vec<prompts::SpawnRequest>) {
    let project = st.chat_project(&chat);
    let parent_full_auto = *chat.chat_mode.peek() == ChatMode::FullAuto;
    for spawn in spawns {
        let provider = match spawn.provider.as_deref().map(|id| (id, Provider::find(id))) {
            None => None,
            Some((_, Some(p))) => Some(p),
            Some((id, None)) => {
                let ids: Vec<&str> = Provider::ALL.iter().map(|p| p.id()).collect();
                chat.items.write().push(ChatItem::Error(format!(
                    "Could not start agent “{}”: “{id}” is not a provider; use one of {}",
                    spawn.name,
                    ids.join(", ")
                )));
                continue;
            }
        };
        // A model without a provider is for the provider in the settings.
        let runs_on = match (provider, &spawn.model) {
            (Some(p), Some(m)) => format!(" on {} · {m}", p.label()),
            (Some(p), None) => format!(" on {}", p.label()),
            (None, Some(m)) => format!(" on {m}"),
            (None, None) => String::new(),
        };
        // Full auto carries over to the new agent when its provider can run it (API providers only answer).
        let child_provider = provider.unwrap_or(st.provider_settings.peek().provider);
        let full_auto = parent_full_auto && child_provider.modes().contains(&ChatMode::FullAuto);
        let in_mode = if full_auto { " in Full auto" } else { "" };
        let Some(repo_name) = spawn.repo else {
            chat.items.write().push(ChatItem::Info(format!("Started agent “{}”{runs_on}{in_mode} in this session", spawn.name)));
            st.request_chat(ChatRequest::NewAgent {
                session: chat.session.cloned(),
                name: Some(spawn.name),
                task: Some(spawn.task),
                activate: false,
                provider,
                model: spawn.model,
                full_auto,
            });
            continue;
        };
        match project.as_ref().and_then(|p| p.find_repo(&repo_name)) {
            Some(repo) => {
                chat.items.write().push(ChatItem::Info(format!("Started agent “{}”{runs_on}{in_mode} in a new session in {}", spawn.name, repo.label())));
                st.request_chat(ChatRequest::AgentInRepo { dir: repo.dir(), name: spawn.name, task: spawn.task, provider, model: spawn.model, full_auto });
            }
            None => chat.items.write().push(ChatItem::Error(format!(
                "Could not start agent “{}”: “{repo_name}” is not a repository of this agent's project",
                spawn.name
            ))),
        }
    }
}

/// Whether the agent works in the open folder, whose graph and files the app shows.
fn here(st: &AppState, chat: &Chat) -> bool {
    same_dir(&chat.dir.peek(), &st.cwd.peek())
}

/// Starts tracking the graph edits of this agent's turn when it works in the open folder.
fn begin_turn(st: &mut AppState, chat: &Chat) {
    if here(st, chat) && !chat.is_discussion() {
        st.begin_chat_turn(&chat.agent_id.cloned());
    }
}

/// Ends the tracked graph turn or job, when it is this agent's.
/// After a successful turn, names the files it changed that no node owns (PRO-233).
fn finish(st: &mut AppState, chat: &Chat, success: bool) {
    if !st.owns_turn(chat) {
        return;
    }
    let mut items = chat.items;
    if let Some(warning) = st.finish_job(success) {
        items.write().push(ChatItem::Error(warning));
    }
    let unowned = st.unowned_changes();
    let notice = scope::report_unowned(&unowned, file_searches(&items.peek()));
    if let Some(notice) = notice.filter(|_| success) {
        items.write().push(ChatItem::Info(notice));
    }
}

/// When the agent's provider ran out of tokens and a fallback is set (PRO-236), moves the agent
/// to the fallback and sends it the conversation so far, so the work goes on. The fallback can't
/// resume the old provider's session, so it starts a new one from the visible chat.
fn fall_back_if_out_of_tokens(st: &mut AppState, chat: &mut Chat, error: &str) {
    let Some(fallback) = st.provider_settings.peek().fallback.clone() else { return };
    let (current, _) = st.agent_provider(chat);
    if current == fallback.provider || !providers::out_of_tokens(error) {
        return;
    }
    chat.provider.set(Some(fallback.provider));
    chat.pinned_model.set(Some(fallback.model.trim().to_string()).filter(|m| !m.is_empty()));
    chat.session_id.set(None);
    if !fallback.provider.modes().contains(&chat.chat_mode.peek()) {
        chat.chat_mode.set(ChatMode::Plan);
    }
    let handover = handover(&chat.items.peek());
    chat.items.write().push(ChatItem::Info(format!(
        "{} ran out of tokens. Continuing on {} with the conversation so far.",
        current.label(),
        fallback.provider.label()
    )));
    chat.send(Cmd::Restart { keep_session: true });
    chat.send(Cmd::Prompt { label: format!("Conversation handed over from {}", current.label()), text: handover });
}

/// How many recent messages the fallback provider gets.
const HANDOVER_MESSAGES: usize = 20;

/// The recent user messages and replies, as a prompt for the provider that takes over.
fn handover(items: &[ChatItem]) -> String {
    let messages: Vec<String> = items
        .iter()
        .filter_map(|i| match i {
            ChatItem::User(m) => Some(format!("User: {}", m.text)),
            ChatItem::Assistant { text, .. } => Some(format!("Assistant: {text}")),
            _ => None,
        })
        .collect();
    let recent = &messages[messages.len().saturating_sub(HANDOVER_MESSAGES)..];
    format!(
        "You are taking over this conversation from another provider, which ran out of tokens. \
         Here are its latest messages. Continue with the user's last request; check the files for work already done.\n\n{}",
        recent.join("\n\n")
    )
}

/// What a discussion chat's agent is told when it asks for permission to act.
const DISCUSSION_DENIAL: &str = "This is a read-only discussion chat: you may read and search, but not change anything. \
     Explain what you would do instead, so the user can tell the execution agent.";

/// How many of the execution chat's latest items a discussion message carries at most.
const DIGEST_ITEMS: usize = 40;
/// Longest text of one item in a digest; longer replies and results are cut.
const DIGEST_ITEM_CHARS: usize = 1500;

/// The name of the agent a chat runs, as its session's tab shows it.
fn agent_name(st: &AppState, chat: &Chat) -> Option<String> {
    let id = chat.agent_id.cloned();
    st.sessions.peek().iter().flat_map(|s| s.agents.iter()).find(|a| a.id == id).map(|a| a.name.clone())
}

/// The prompt a discussion chat sends for the user's `text`: what happened in the execution
/// chat since the last discussion message, its current state, then the user's question.
/// `digested` counts the execution items already sent, so each message only carries new ones.
fn discussion_prompt(st: &AppState, chat: &Chat, digested: &mut usize, text: &str) -> String {
    let Some(exec) = st.execution_of(chat) else { return text.to_string() };
    let items = exec.items.peek();
    // The execution chat was cleared or reloaded: start over.
    if *digested > items.len() {
        *digested = 0;
    }
    let digest = transcript_digest(&items[*digested..], DIGEST_ITEMS);
    *digested = items.len();
    let state = execution_state(&exec);
    discussion_message(&digest, &state, text)
}

/// The execution chat's state, for a discussion message.
fn execution_state(exec: &Chat) -> String {
    if let Some(approval) = exec.plan_approval.peek().as_ref() {
        return format!("Waiting for the user to approve this plan:\n{}", cut(&approval.plan, DIGEST_ITEM_CHARS * 2));
    }
    if let Some(req) = exec.permissions.peek().first() {
        let summary = tool_summary(&req.tool, &req.input, &exec.dir.peek());
        return format!("Waiting for the user's permission to use {}: {summary}", req.tool);
    }
    let mode = exec.chat_mode.peek().label();
    match *exec.busy.peek() {
        true => format!("Working (mode: {mode})"),
        false => format!("Idle, waiting for the user's next message (mode: {mode})"),
    }
}

fn discussion_message(digest: &str, state: &str, text: &str) -> String {
    let digest = if digest.is_empty() { "(nothing new since your last message)" } else { digest };
    format!(
        "<execution_chat>\nNew in the execution chat since my last message:\n\n{digest}\n\nIts state now: {state}\n</execution_chat>\n\n{text}"
    )
}

/// The latest `limit` items of a chat as plain text: messages, replies, tool calls with
/// whether they failed, notes and errors. Long texts are cut.
fn transcript_digest(items: &[ChatItem], limit: usize) -> String {
    let lines: Vec<String> = items
        .iter()
        .filter_map(|i| match i {
            ChatItem::User(m) => Some(format!("User: {}", cut(&m.text, DIGEST_ITEM_CHARS))),
            ChatItem::Prompt { label, text } => Some(format!("Prompt ({label}): {}", cut(text, DIGEST_ITEM_CHARS))),
            ChatItem::Assistant { text, .. } if !text.trim().is_empty() => Some(format!("Agent: {}", cut(text, DIGEST_ITEM_CHARS))),
            ChatItem::Tool { name, summary, result, .. } => {
                let outcome = match result {
                    None => "running",
                    Some((_, true)) => "failed",
                    Some((_, false)) => "ok",
                };
                Some(format!("Tool {name} ({outcome}): {}", cut(summary, 200)))
            }
            ChatItem::Info(text) => Some(format!("Note: {}", cut(text, 300))),
            ChatItem::Error(text) => Some(format!("Error: {}", cut(text, 600))),
            _ => None,
        })
        .collect();
    let skipped = lines.len().saturating_sub(limit);
    let mut out = lines[skipped..].join("\n\n");
    if skipped > 0 {
        out = format!("({skipped} earlier items left out)\n\n{out}");
    }
    out
}

/// `text` cut to at most `max` characters, marked when cut.
fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

/// The file-system searches (Grep, Glob) the agent ran since the user's last message.
fn file_searches(items: &[ChatItem]) -> usize {
    items
        .iter()
        .rev()
        .take_while(|i| !matches!(i, ChatItem::User(_)))
        .filter(|i| matches!(i, ChatItem::Tool { name, .. } if name == "Grep" || name == "Glob"))
        .count()
}

/// Every provider except Workbench runs one turn per message: a fresh CLI
/// process that resumes the previous session, or one API request.
async fn start_turn(
    st: &mut AppState,
    chat: &mut Chat,
    live: &mut Option<Live>,
    generation: &mut u64,
    ev_tx: &tokio::sync::mpsc::UnboundedSender<(u64, ClaudeEvent)>,
    shown: ChatItem,
    text: String,
    provider: Provider,
    model: String,
) {
    // Answering in the chat instead of picking a mode replaces the plan.
    chat.plan_approval.set(None);
    if let Some(l) = live.take() {
        l.agent.kill().await;
    }
    *generation += 1;
    let settings = st.provider_settings.peek().clone();
    let mode = *chat.chat_mode.peek();
    let servers = st.mcp_servers.peek().clone();
    let servers = crate::figma::with_token(servers, &crate::figma::saved_token());
    let mcp_servers = mcp_oauth::with_tokens(&servers).await;
    let mut instructions = system_prompt(st, chat).unwrap_or_default();
    if mode == ChatMode::Plan && !chat.is_discussion() {
        instructions.push_str(prompts::plan_mode_instructions());
    }
    let opts = TurnOptions {
        cwd: chat.dir.cloned(),
        model,
        resume: chat.session_id.peek().clone(),
        mode,
        extra_dirs: st.other_project_dirs(&chat.dir.cloned()),
        system_prompt: instructions,
        mcp_servers,
        prompt: text,
        history: api_chat::history(&chat.items.peek()),
    };
    chat.model.set(if opts.model.is_empty() { provider.short_name().to_string() } else { opts.model.clone() });
    match agent::start_turn(provider, &settings, opts, *generation, ev_tx.clone()) {
        Ok(agent) => *live = Some(Live { agent, rules_version: *st.rules_version.peek(), mode, mcp_config: None, model: String::new() }),
        Err(e) => {
            chat.items.write().push(shown);
            chat.items.write().push(ChatItem::Error(format!("{e:#}")));
            finish(st, chat, false);
            return;
        }
    }
    begin_turn(st, chat);
    chat.items.write().push(shown);
    chat.busy.set(true);
}

/// Answers Workbench's ExitPlanMode request.
async fn answer_plan(chat: &mut Chat, live: &mut Option<Live>, request_id: &str, decision: &PermissionDecision) {
    if let Some(l) = live.as_mut()
        && let Err(e) = l.agent.respond_permission(request_id, decision).await
    {
        chat.items.write().push(ChatItem::Error(format!("could not answer the plan request: {e:#}")));
    }
}

async fn deny_pending(chat: &mut Chat, live: &mut Option<Live>) {
    let mut pending: Vec<_> = chat.permissions.write().drain(..).collect();
    pending.extend(chat.plan_approval.take().and_then(|a| a.request));
    let Some(l) = live.as_mut() else { return };
    let decision = PermissionDecision::Deny { message: "The user stopped the task.".into(), interrupt: true };
    for p in pending {
        let _ = l.agent.respond_permission(&p.request_id, &decision).await;
    }
}

fn handle_event(st: &mut AppState, chat: &mut Chat, ev: ClaudeEvent) {
    if matches!(ev, ClaudeEvent::TextDelta(_) | ClaudeEvent::ToolUse { .. } | ClaudeEvent::ToolResult { .. })
        && !*chat.busy.peek()
    {
        chat.busy.set(true);
    }
    match ev {
        ClaudeEvent::Init { session_id, model, mcp_servers } => {
            st.mcp_status.set(mcp_servers);
            // Reopening the app loads Workbench transcripts, so only Workbench sessions are remembered.
            if st.agent_provider(chat).0 == Provider::ClaudeCode {
                st.save_transcript(chat, Some(session_id.clone()));
            }
            chat.session_id.set(Some(session_id));
            chat.model.set(model);
        }
        ClaudeEvent::McpStatus(statuses) => st.mcp_status.set(statuses),
        ClaudeEvent::TextDelta(delta) => {
            let mut items = chat.items.write();
            match items.last_mut() {
                Some(ChatItem::Assistant { text, streaming: true }) => text.push_str(&delta),
                _ => items.push(ChatItem::Assistant { text: delta, streaming: true }),
            }
        }
        ClaudeEvent::AssistantText(full) => {
            let mut items = chat.items.write();
            let streaming = items
                .iter()
                .rposition(|i| matches!(i, ChatItem::Assistant { streaming: true, .. }));
            match streaming {
                Some(idx) => items[idx] = ChatItem::Assistant { text: full, streaming: false },
                None if !full.trim().is_empty() => items.push(ChatItem::Assistant { text: full, streaming: false }),
                None => {}
            }
        }
        ClaudeEvent::ToolUse { id, name, input } => {
            let cwd = chat.dir.cloned();
            let summary = tool_summary(&name, &input, &cwd);
            let pretty = serde_json::to_string_pretty(&input).unwrap_or_default();
            let mut items = chat.items.write();
            for item in items.iter_mut() {
                if let ChatItem::Assistant { streaming, .. } = item {
                    *streaming = false;
                }
            }
            items.push(ChatItem::Tool { id, name, summary, input: pretty, result: None });
        }
        ClaudeEvent::ToolResult { id, content, is_error } => {
            let mut touched = None;
            {
                let mut items = chat.items.write();
                if let Some(ChatItem::Tool { name, input, result, .. }) = items
                    .iter_mut()
                    .rev()
                    .find(|i| matches!(i, ChatItem::Tool { id: tid, .. } if *tid == id))
                {
                    *result = Some((content, is_error));
                    if tool_touches_files(name) && !is_error {
                        let parsed: serde_json::Value = serde_json::from_str(input).unwrap_or_default();
                        touched = Some(tool_file_path(&parsed));
                    }
                }
            }
            if let Some(path) = touched
                && here(st, chat)
            {
                if let Some(p) = &path
                    && st.run_scope.peek().is_some()
                {
                    let rel = st.rel_path(p);
                    if !st.live_edits.peek().contains(&rel) {
                        st.live_edits.write().push(rel);
                    }
                }
                st.bump_repo_soon();
                st.reload_open_file_if_clean();
                if let Some(p) = path
                    && st.open_file.peek().is_none()
                    && !st.rel_path(&p).starts_with(".workbench/")
                {
                    let rel = st.rel_path(&p);
                    st.open(&rel);
                }
            }
        }
        ClaudeEvent::Done { cost_usd, is_error, text } => {
            chat.busy.set(false);
            chat.last_cost.set(cost_usd);
            let asked = text.as_deref().is_some_and(prompts::asks_questions);
            let questions = !is_error && asked && st.owns_turn(chat) && st.job.peek().is_some_and(|j| j.mode == ChatMode::Plan);
            if is_error {
                let error = text.clone().unwrap_or_else(|| "request failed".into());
                chat.items.write().push(ChatItem::Error(error.clone()));
                fall_back_if_out_of_tokens(st, chat, &error);
            }
            let mut items = chat.items.write();
            for item in items.iter_mut() {
                if let ChatItem::Assistant { text, streaming } = item {
                    *streaming = false;
                    if prompts::asks_questions(text) {
                        *text = prompts::strip_marker(text);
                    }
                }
            }
            let (spawns, focus) = match items.iter_mut().rev().find_map(|i| match i {
                ChatItem::Assistant { text, .. } => Some(text),
                _ => None,
            }) {
                Some(text) => {
                    let (shown, spawns) = prompts::spawn_requests(text);
                    let (shown, focus) = prompts::focus_requests(&shown);
                    *text = shown;
                    (spawns, focus)
                }
                None => (Vec::new(), Vec::new()),
            };
            if !focus.is_empty() {
                items.push(ChatItem::Focus(focus.clone()));
            }
            drop(items);
            // A discussion chat starts nothing and its replies are never plans to run.
            let discussion = chat.is_discussion();
            if !discussion {
                start_spawns(st, *chat, spawns);
            }
            // One match is shown right away; with several the user picks one in the chat.
            if let [only] = focus.as_slice()
                && here(st, chat)
            {
                st.focus_node(only.module.clone(), only.node.clone());
            }
            if questions {
                st.await_answers();
            } else {
                finish(st, chat, !is_error);
            }
            // Providers without a plan mode of their own: a plan-mode reply that doesn't ask
            // questions is the plan, so ask how to run it.
            let plan_from_reply = !discussion && st.agent_provider(chat).0 != Provider::ClaudeCode && *chat.chat_mode.peek() == ChatMode::Plan;
            if plan_from_reply && !is_error && !asked {
                let plan = chat.items.peek().iter().rev().find_map(|i| match i {
                    ChatItem::Assistant { text, .. } => Some(text.clone()),
                    _ => None,
                });
                if let Some(plan) = plan.filter(|p| !p.trim().is_empty()) {
                    chat.plan_approval.set(Some(PlanApproval { plan, request: None }));
                }
            }
            if here(st, chat) && !discussion {
                st.bump_repo();
                st.reload_open_file_if_clean();
            }
        }
        ClaudeEvent::Permission(req) if req.tool == EXIT_PLAN_TOOL && !chat.is_discussion() => {
            let plan = req.input.get("plan").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
            chat.plan_approval.set(Some(PlanApproval { plan, request: Some(req) }));
        }
        ClaudeEvent::Permission(req) => chat.permissions.write().push(req),
        ClaudeEvent::ControlCancel { request_id } => {
            chat.permissions.write().retain(|p| p.request_id != request_id);
            if chat.plan_approval.peek().as_ref().and_then(|a| a.request.as_ref()).is_some_and(|r| r.request_id == request_id) {
                chat.plan_approval.set(None);
            }
        }
        ClaudeEvent::Stderr(line) => chat.items.write().push(ChatItem::Info(line)),
        ClaudeEvent::Unsupported(what) => chat
            .items
            .write()
            .push(ChatItem::Error(format!("Claude Code output Workbench doesn't recognise: {what}. Some agent output may be missing."))),
        ClaudeEvent::ControlRequest { .. } | ClaudeEvent::Exited => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Message;

    fn user(text: &str) -> ChatItem {
        ChatItem::User(Message { text: text.into(), context: Vec::new(), prompt: format!("ctx {text}") })
    }

    #[test]
    fn digest_shows_messages_replies_and_tool_outcomes() {
        let items = vec![
            user("add a login page"),
            ChatItem::Assistant { text: "On it.".into(), streaming: false },
            ChatItem::Tool { id: "1".into(), name: "Edit".into(), summary: "src/login.rs".into(), input: String::new(), result: Some((String::new(), false)) },
            ChatItem::Tool { id: "2".into(), name: "Bash".into(), summary: "cargo test".into(), input: String::new(), result: Some((String::new(), true)) },
            ChatItem::Tool { id: "3".into(), name: "Read".into(), summary: "src/app.rs".into(), input: String::new(), result: None },
            ChatItem::Error("boom".into()),
        ];
        let digest = transcript_digest(&items, 40);
        assert!(digest.contains("User: add a login page"), "{digest}");
        assert!(!digest.contains("ctx add"), "the user's words, not the prompt sent: {digest}");
        assert!(digest.contains("Agent: On it."));
        assert!(digest.contains("Tool Edit (ok): src/login.rs"));
        assert!(digest.contains("Tool Bash (failed): cargo test"));
        assert!(digest.contains("Tool Read (running): src/app.rs"));
        assert!(digest.contains("Error: boom"));
    }

    #[test]
    fn digest_keeps_the_latest_items_and_says_how_many_it_left_out() {
        let items: Vec<ChatItem> = (0..5).map(|i| user(&format!("m{i}"))).collect();
        let digest = transcript_digest(&items, 2);
        assert!(digest.starts_with("(3 earlier items left out)"), "{digest}");
        assert!(digest.contains("m3") && digest.contains("m4") && !digest.contains("m2"));
    }

    #[test]
    fn long_texts_are_cut_on_char_boundaries() {
        assert_eq!(cut("åäö", 2), "åä…");
        assert_eq!(cut("abc", 3), "abc");
    }

    #[test]
    fn discussion_message_wraps_the_question() {
        let msg = discussion_message("", "Working (mode: Auto)", "what is it doing?");
        assert!(msg.contains("nothing new since your last message"));
        assert!(msg.contains("Its state now: Working (mode: Auto)"));
        assert!(msg.ends_with("</execution_chat>\n\nwhat is it doing?"));
    }
}
