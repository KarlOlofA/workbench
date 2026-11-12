use dioxus::prelude::*;

use crate::agent::{self, Agent, TurnOptions};
use crate::api_chat;
use crate::claude::{ChatMode, ClaudeEvent, ClaudeProcess, EXIT_PLAN_TOOL, PermissionDecision, SpawnOptions, tool_file_path, tool_summary, tool_touches_files};
use crate::history;
use crate::projects::same_dir;
use crate::mcp;
use crate::providers::Provider;
use crate::prompts;
use crate::state::{AppState, Chat, ChatItem, ChatRequest, Cmd, PlanApproval};

struct Live {
    agent: Agent,
    rules_version: u64,
    /// The mode the agent runs in; a different chat mode restarts it on the next message.
    mode: ChatMode,
}

/// The session loop of one agent: receives its commands, owns its agent process and turns the
/// agent's events into its chat items.
pub async fn run(mut st: AppState, mut chat: Chat, mut rx: tokio::sync::mpsc::UnboundedReceiver<Cmd>) {
    let st = &mut st;
    let chat = &mut chat;
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<(u64, ClaudeEvent)>();
    let mut live: Option<Live> = None;
    let mut generation = 0u64;

    loop {
        if !*chat.busy.peek() && !chat.queued.peek().is_empty() {
            let msg = chat.queued.write().remove(0);
            if st.owns_turn(chat) {
                st.resume_job();
            }
            let text = msg.prompt.clone();
            deliver(st, chat, &mut live, &mut generation, &ev_tx, ChatItem::User(msg), text).await;
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
                        st.save_transcript(&chat.agent_id.cloned(), Some(id.clone()));
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
                            st.save_transcript(&chat.agent_id.cloned(), None);
                            chat.items.write().clear();
                            chat.last_cost.set(None);
                        }
                        continue;
                    }
                };
                deliver(st, chat, &mut live, &mut generation, &ev_tx, shown, text).await;
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
                        let name = st.provider_settings.peek().provider.short_name();
                        chat.items.write().push(ChatItem::Error(format!("{name} exited (code {code:?})")));
                        chat.busy.set(false);
                        finish(st, chat, false);
                    }
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
    shown: ChatItem,
    text: String,
) {
    let rules_version = *st.rules_version.peek();
    let provider = st.provider_settings.peek().provider;
    if provider != Provider::ClaudeCode {
        start_turn(st, chat, live, generation, ev_tx, shown, text, provider).await;
        return;
    }
    let mode = *chat.chat_mode.peek();
    if live.as_ref().is_some_and(|l| l.rules_version != rules_version || l.mode != mode)
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
            mcp_config: mcp::claude_config(&st.mcp_servers.peek()),
            model: st.provider_settings.peek().model(Provider::ClaudeCode),
        };
        match ClaudeProcess::spawn(&opts, *generation, ev_tx.clone()) {
            Ok(proc) => *live = Some(Live { agent: Agent::Workbench(proc), rules_version, mode }),
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

/// The Workbench context every provider gets: graph upkeep, rules, related repos and the project.
/// Agents can also start other agents in their chat session (see `prompts::spawn_requests`).
fn system_prompt(st: &AppState, chat: &Chat) -> Option<String> {
    let base = st.with_refs(|refs| prompts::system_append(Some(&*st.graph.peek()), &st.rules.peek(), &st.related_repos.peek(), Some(refs)));
    let dir = chat.dir.cloned();
    let project = st.project_for(&dir);
    let with_project = prompts::with_project(base, project.as_ref(), &dir).unwrap_or_default();
    Some(format!("{with_project}{}", prompts::spawn_agent_instructions()))
}

/// Whether the agent works in the open folder, whose graph and files the app shows.
fn here(st: &AppState, chat: &Chat) -> bool {
    same_dir(&chat.dir.peek(), &st.cwd.peek())
}

/// Starts tracking the graph edits of this agent's turn when it works in the open folder.
fn begin_turn(st: &mut AppState, chat: &Chat) {
    if here(st, chat) {
        st.begin_chat_turn(&chat.agent_id.cloned());
    }
}

/// Ends the tracked graph turn or job, when it is this agent's.
fn finish(st: &mut AppState, chat: &Chat, success: bool) {
    if st.owns_turn(chat) {
        st.finish_job(success);
    }
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
) {
    // Answering in the chat instead of picking a mode replaces the plan.
    chat.plan_approval.set(None);
    if let Some(l) = live.take() {
        l.agent.kill().await;
    }
    *generation += 1;
    let settings = st.provider_settings.peek().clone();
    let mode = *chat.chat_mode.peek();
    let mut instructions = system_prompt(st, chat).unwrap_or_default();
    if mode == ChatMode::Plan {
        instructions.push_str(prompts::plan_mode_instructions());
    }
    let opts = TurnOptions {
        cwd: chat.dir.cloned(),
        model: settings.model(provider),
        resume: chat.session_id.peek().clone(),
        mode,
        system_prompt: instructions,
        mcp_servers: st.mcp_servers.peek().clone(),
        prompt: text,
        history: api_chat::history(&chat.items.peek()),
    };
    chat.model.set(if opts.model.is_empty() { provider.short_name().to_string() } else { opts.model.clone() });
    match agent::start_turn(provider, &settings, opts, *generation, ev_tx.clone()) {
        Ok(agent) => *live = Some(Live { agent, rules_version: *st.rules_version.peek(), mode }),
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
            if st.provider_settings.peek().provider == Provider::ClaudeCode {
                st.save_transcript(&chat.agent_id.cloned(), Some(session_id.clone()));
            }
            chat.session_id.set(Some(session_id));
            chat.model.set(model);
        }
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
                st.bump_repo();
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
                chat.items
                    .write()
                    .push(ChatItem::Error(text.unwrap_or_else(|| "request failed".into())));
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
            let spawns = match items.iter_mut().rev().find_map(|i| match i {
                ChatItem::Assistant { text, .. } => Some(text),
                _ => None,
            }) {
                Some(text) => {
                    let (shown, spawns) = prompts::spawn_requests(text);
                    *text = shown;
                    spawns
                }
                None => Vec::new(),
            };
            drop(items);
            for (name, task) in spawns {
                chat.items.write().push(ChatItem::Info(format!("Started agent “{name}” in this session")));
                st.request_chat(ChatRequest::NewAgent { session: chat.session.cloned(), name: Some(name), task: Some(task), activate: false });
            }
            if questions {
                st.await_answers();
            } else {
                finish(st, chat, !is_error);
            }
            // Providers without a plan mode of their own: a plan-mode reply that doesn't ask
            // questions is the plan, so ask how to run it.
            let plan_from_reply = st.provider_settings.peek().provider != Provider::ClaudeCode && *chat.chat_mode.peek() == ChatMode::Plan;
            if plan_from_reply && !is_error && !asked {
                let plan = chat.items.peek().iter().rev().find_map(|i| match i {
                    ChatItem::Assistant { text, .. } => Some(text.clone()),
                    _ => None,
                });
                if let Some(plan) = plan.filter(|p| !p.trim().is_empty()) {
                    chat.plan_approval.set(Some(PlanApproval { plan, request: None }));
                }
            }
            if here(st, chat) {
                st.bump_repo();
                st.reload_open_file_if_clean();
            }
        }
        ClaudeEvent::Permission(req) if req.tool == EXIT_PLAN_TOOL => {
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
        ClaudeEvent::ControlRequest { .. } | ClaudeEvent::Exited => {}
    }
}
