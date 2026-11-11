use dioxus::prelude::*;
use futures_util::StreamExt;

use crate::agent::{self, Agent, TurnOptions};
use crate::api_chat;
use crate::claude::{ChatMode, ClaudeEvent, ClaudeProcess, EXIT_PLAN_TOOL, PermissionDecision, SpawnOptions, tool_file_path, tool_summary, tool_touches_files};
use crate::history;
use crate::mcp;
use crate::providers::Provider;
use crate::prompts;
use crate::state::{AppState, ChatItem, Cmd, PlanApproval};

struct Live {
    agent: Agent,
    rules_version: u64,
    /// The mode the agent runs in; a different chat mode restarts it on the next message.
    mode: ChatMode,
}

pub async fn run(mut st: AppState, mut rx: UnboundedReceiver<Cmd>) {
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<(u64, ClaudeEvent)>();
    let mut live: Option<Live> = None;
    let mut generation = 0u64;

    loop {
        if !*st.busy.peek() && !st.queued.peek().is_empty() {
            let msg = st.queued.write().remove(0);
            st.resume_job();
            let text = msg.prompt.clone();
            deliver(&mut st, &mut live, &mut generation, &ev_tx, ChatItem::User(msg), text).await;
            continue;
        }
        tokio::select! {
            cmd = rx.next() => {
                let Some(cmd) = cmd else { break };
                let (shown, text) = match cmd {
                    Cmd::Send(msg) if *st.busy.peek() || !st.queued.peek().is_empty() => {
                        st.queued.write().push(msg);
                        continue;
                    }
                    Cmd::Send(msg) | Cmd::SendNow(msg) => {
                        st.resume_job();
                        let text = msg.prompt.clone();
                        (ChatItem::User(msg), text)
                    }
                    Cmd::Prompt { label, text } => (ChatItem::Prompt { label, text: text.clone() }, text),
                    Cmd::Permission { request_id, decision } => {
                        let req = {
                            let mut pending = st.permissions.write();
                            pending.iter().position(|p| p.request_id == request_id).map(|i| pending.remove(i))
                        };
                        let Some(req) = req else { continue };
                        if let Some(l) = live.as_mut()
                            && let Err(e) = l.agent.respond_permission(&request_id, &decision).await
                        {
                            st.items.write().push(ChatItem::Error(format!("could not answer the permission request: {e:#}")));
                            continue;
                        }
                        let cwd = st.cwd.peek().clone();
                        let summary = tool_summary(&req.tool, &req.input, &cwd);
                        let verdict = match &decision {
                            PermissionDecision::Allow { permissions, .. } if !permissions.is_empty() => "Always allowed",
                            PermissionDecision::Allow { .. } => "Allowed",
                            PermissionDecision::Deny { interrupt: true, .. } => "Denied and stopped",
                            PermissionDecision::Deny { .. } => "Denied",
                        };
                        st.items.write().push(ChatItem::Info(format!("{verdict} {}: {summary}", req.tool)));
                        continue;
                    }
                    Cmd::ApprovePlan { mode } => {
                        let Some(approval) = st.plan_approval.take() else { continue };
                        if let Some(mode) = mode {
                            st.chat_mode.set(mode);
                        }
                        match (approval.request, mode) {
                            // Workbench: answer its ExitPlanMode request and switch the running session's mode.
                            (Some(req), Some(mode)) => {
                                let set_mode = serde_json::json!({ "type": "setMode", "mode": mode.cli_arg(), "destination": "session" });
                                let decision = PermissionDecision::Allow { input: req.input.clone(), permissions: vec![set_mode] };
                                answer_plan(&mut st, &mut live, &req.request_id, &decision).await;
                                if let Some(l) = live.as_mut() {
                                    l.mode = mode;
                                }
                                st.items.write().push(ChatItem::Info(format!("Plan approved · running in {}", mode.label())));
                                continue;
                            }
                            (Some(req), None) => {
                                let decision = PermissionDecision::Deny {
                                    message: "The user wants to keep planning. Ask them what to change in the plan.".into(),
                                    interrupt: false,
                                };
                                answer_plan(&mut st, &mut live, &req.request_id, &decision).await;
                                st.items.write().push(ChatItem::Info("Kept planning".into()));
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
                        deny_pending(&mut st, &mut live).await;
                        if let Some(l) = live.as_mut()
                            && l.agent.interrupt().await.is_err()
                        {
                            if let Some(l) = live.take() {
                                l.agent.kill().await;
                            }
                            generation += 1;
                            st.busy.set(false);
                            st.finish_job(false);
                        }
                        continue;
                    }
                    Cmd::Resume { id } => {
                        if let Some(l) = live.take() {
                            l.agent.kill().await;
                        }
                        generation += 1;
                        st.permissions.write().clear();
                        st.plan_approval.set(None);
                        st.queued.write().clear();
                        st.busy.set(false);
                        st.finish_job(false);
                        let cwd = st.cwd.peek().clone();
                        let load_id = id.clone();
                        let items = tokio::task::spawn_blocking(move || history::load(&cwd, &load_id)).await.unwrap_or_default();
                        st.session_id.set(Some(id));
                        st.last_cost.set(None);
                        st.items.set(items);
                        continue;
                    }
                    Cmd::Restart { keep_session } => {
                        if let Some(l) = live.take() {
                            l.agent.kill().await;
                        }
                        generation += 1;
                        st.permissions.write().clear();
                        st.plan_approval.set(None);
                        st.busy.set(false);
                        st.queued.write().clear();
                        st.finish_job(false);
                        if !keep_session {
                            st.session_id.set(None);
                            st.items.write().clear();
                            st.last_cost.set(None);
                        }
                        continue;
                    }
                };
                deliver(&mut st, &mut live, &mut generation, &ev_tx, shown, text).await;
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
                    st.permissions.write().clear();
                    // A plan from a one-turn provider outlives its process; Workbench's plan request doesn't.
                    if st.plan_approval.peek().as_ref().is_some_and(|a| a.request.is_some()) {
                        st.plan_approval.set(None);
                    }
                    // One-turn providers exit after every finished turn; only an exit mid-turn is a failure.
                    if *st.busy.peek() {
                        let name = st.provider_settings.peek().provider.short_name();
                        st.items.write().push(ChatItem::Error(format!("{name} exited (code {code:?})")));
                        st.busy.set(false);
                        st.finish_job(false);
                    }
                    continue;
                }
                if let ClaudeEvent::Permission(req) = &ev
                    && st.chat_mode.peek().auto_allows(req)
                    && let Some(l) = live.as_mut()
                {
                    let decision = PermissionDecision::Allow { input: req.input.clone(), permissions: Vec::new() };
                    match l.agent.respond_permission(&req.request_id, &decision).await {
                        Ok(()) => {
                            let cwd = st.cwd.peek().clone();
                            let summary = tool_summary(&req.tool, &req.input, &cwd);
                            st.items.write().push(ChatItem::Info(format!("Auto-allowed {}: {summary}", req.tool)));
                            continue;
                        }
                        Err(e) => st.items.write().push(ChatItem::Error(format!("could not auto-allow the permission request: {e:#}"))),
                    }
                }
                handle_event(&mut st, ev);
            }
        }
    }
}

async fn deliver(
    st: &mut AppState,
    live: &mut Option<Live>,
    generation: &mut u64,
    ev_tx: &tokio::sync::mpsc::UnboundedSender<(u64, ClaudeEvent)>,
    shown: ChatItem,
    text: String,
) {
    let rules_version = *st.rules_version.peek();
    let provider = st.provider_settings.peek().provider;
    if provider != Provider::ClaudeCode {
        start_turn(st, live, generation, ev_tx, shown, text, provider).await;
        return;
    }
    let mode = *st.chat_mode.peek();
    if live.as_ref().is_some_and(|l| l.rules_version != rules_version || l.mode != mode)
        && let Some(l) = live.take()
    {
        l.agent.kill().await;
        *generation += 1;
        st.permissions.write().clear();
        st.plan_approval.set(None);
    }
    if live.is_none() {
        *generation += 1;
        let opts = SpawnOptions {
            cwd: st.cwd.peek().clone(),
            resume: st.session_id.peek().clone(),
            mode,
            append_system_prompt: system_prompt(st),
            mcp_config: mcp::claude_config(&st.mcp_servers.peek()),
            model: st.provider_settings.peek().model(Provider::ClaudeCode),
        };
        match ClaudeProcess::spawn(&opts, *generation, ev_tx.clone()) {
            Ok(proc) => *live = Some(Live { agent: Agent::Workbench(proc), rules_version, mode }),
            Err(e) => {
                st.items.write().push(ChatItem::Error(format!("{e:#}")));
                st.finish_job(false);
                return;
            }
        }
    }
    st.begin_chat_turn();
    st.items.write().push(shown);
    st.busy.set(true);
    if let Some(l) = live.as_mut()
        && let Err(e) = l.agent.send_user(&text).await
    {
        st.items.write().push(ChatItem::Error(format!("send failed: {e:#}")));
        st.busy.set(false);
        st.finish_job(false);
        *live = None;
    }
}

/// The Workbench context every provider gets: graph upkeep, rules, related repos and the project.
fn system_prompt(st: &AppState) -> Option<String> {
    let base = st.with_refs(|refs| prompts::system_append(Some(&*st.graph.peek()), &st.rules.peek(), &st.related_repos.peek(), Some(refs)));
    let cwd = st.cwd.peek().clone();
    let project = st.project_for(&cwd);
    prompts::with_project(base, project.as_ref(), &cwd)
}

/// Every provider except Workbench runs one turn per message: a fresh CLI
/// process that resumes the previous session, or one API request.
async fn start_turn(
    st: &mut AppState,
    live: &mut Option<Live>,
    generation: &mut u64,
    ev_tx: &tokio::sync::mpsc::UnboundedSender<(u64, ClaudeEvent)>,
    shown: ChatItem,
    text: String,
    provider: Provider,
) {
    // Answering in the chat instead of picking a mode replaces the plan.
    st.plan_approval.set(None);
    if let Some(l) = live.take() {
        l.agent.kill().await;
    }
    *generation += 1;
    let settings = st.provider_settings.peek().clone();
    let mode = *st.chat_mode.peek();
    let mut instructions = system_prompt(st).unwrap_or_default();
    if mode == ChatMode::Plan {
        instructions.push_str(prompts::plan_mode_instructions());
    }
    let opts = TurnOptions {
        cwd: st.cwd.peek().clone(),
        model: settings.model(provider),
        resume: st.session_id.peek().clone(),
        mode,
        system_prompt: instructions,
        mcp_servers: st.mcp_servers.peek().clone(),
        prompt: text,
        history: api_chat::history(&st.items.peek()),
    };
    st.model.set(if opts.model.is_empty() { provider.short_name().to_string() } else { opts.model.clone() });
    match agent::start_turn(provider, &settings, opts, *generation, ev_tx.clone()) {
        Ok(agent) => *live = Some(Live { agent, rules_version: *st.rules_version.peek(), mode }),
        Err(e) => {
            st.items.write().push(shown);
            st.items.write().push(ChatItem::Error(format!("{e:#}")));
            st.finish_job(false);
            return;
        }
    }
    st.begin_chat_turn();
    st.items.write().push(shown);
    st.busy.set(true);
}

/// Answers Workbench's ExitPlanMode request.
async fn answer_plan(st: &mut AppState, live: &mut Option<Live>, request_id: &str, decision: &PermissionDecision) {
    if let Some(l) = live.as_mut()
        && let Err(e) = l.agent.respond_permission(request_id, decision).await
    {
        st.items.write().push(ChatItem::Error(format!("could not answer the plan request: {e:#}")));
    }
}

async fn deny_pending(st: &mut AppState, live: &mut Option<Live>) {
    let mut pending: Vec<_> = st.permissions.write().drain(..).collect();
    pending.extend(st.plan_approval.take().and_then(|a| a.request));
    let Some(l) = live.as_mut() else { return };
    let decision = PermissionDecision::Deny { message: "The user stopped the task.".into(), interrupt: true };
    for p in pending {
        let _ = l.agent.respond_permission(&p.request_id, &decision).await;
    }
}

fn handle_event(st: &mut AppState, ev: ClaudeEvent) {
    if matches!(ev, ClaudeEvent::TextDelta(_) | ClaudeEvent::ToolUse { .. } | ClaudeEvent::ToolResult { .. })
        && !*st.busy.peek()
    {
        st.busy.set(true);
    }
    match ev {
        ClaudeEvent::Init { session_id, model, mcp_servers } => {
            st.mcp_status.set(mcp_servers);
            // The session history menu reads Workbench transcripts, so only Workbench sessions are remembered.
            let cwd = st.cwd.peek().clone();
            if st.provider_settings.peek().provider == Provider::ClaudeCode
                && let Err(e) = history::remember(&cwd, &session_id)
            {
                st.items.write().push(ChatItem::Error(format!("could not remember the session: {e}")));
            }
            st.session_id.set(Some(session_id));
            st.model.set(model);
        }
        ClaudeEvent::TextDelta(delta) => {
            let mut items = st.items.write();
            match items.last_mut() {
                Some(ChatItem::Assistant { text, streaming: true }) => text.push_str(&delta),
                _ => items.push(ChatItem::Assistant { text: delta, streaming: true }),
            }
        }
        ClaudeEvent::AssistantText(full) => {
            let mut items = st.items.write();
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
            let cwd = st.cwd.peek().clone();
            let summary = tool_summary(&name, &input, &cwd);
            let pretty = serde_json::to_string_pretty(&input).unwrap_or_default();
            let mut items = st.items.write();
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
                let mut items = st.items.write();
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
            if let Some(path) = touched {
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
            st.busy.set(false);
            st.last_cost.set(cost_usd);
            let asked = text.as_deref().is_some_and(prompts::asks_questions);
            let questions = !is_error && asked && st.job.peek().is_some_and(|j| j.mode == ChatMode::Plan);
            if is_error {
                st.items
                    .write()
                    .push(ChatItem::Error(text.unwrap_or_else(|| "request failed".into())));
            }
            let mut items = st.items.write();
            for item in items.iter_mut() {
                if let ChatItem::Assistant { text, streaming } = item {
                    *streaming = false;
                    if prompts::asks_questions(text) {
                        *text = prompts::strip_marker(text);
                    }
                }
            }
            drop(items);
            if questions {
                st.await_answers();
            } else {
                st.finish_job(!is_error);
            }
            // Providers without a plan mode of their own: a plan-mode reply that doesn't ask
            // questions is the plan, so ask how to run it.
            let plan_from_reply = st.provider_settings.peek().provider != Provider::ClaudeCode && *st.chat_mode.peek() == ChatMode::Plan;
            if plan_from_reply && !is_error && !asked {
                let plan = st.items.peek().iter().rev().find_map(|i| match i {
                    ChatItem::Assistant { text, .. } => Some(text.clone()),
                    _ => None,
                });
                if let Some(plan) = plan.filter(|p| !p.trim().is_empty()) {
                    st.plan_approval.set(Some(PlanApproval { plan, request: None }));
                }
            }
            st.bump_repo();
            st.reload_open_file_if_clean();
        }
        ClaudeEvent::Permission(req) if req.tool == EXIT_PLAN_TOOL => {
            let plan = req.input.get("plan").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
            st.plan_approval.set(Some(PlanApproval { plan, request: Some(req) }));
        }
        ClaudeEvent::Permission(req) => st.permissions.write().push(req),
        ClaudeEvent::ControlCancel { request_id } => {
            st.permissions.write().retain(|p| p.request_id != request_id);
            if st.plan_approval.peek().as_ref().and_then(|a| a.request.as_ref()).is_some_and(|r| r.request_id == request_id) {
                st.plan_approval.set(None);
            }
        }
        ClaudeEvent::Stderr(line) => st.items.write().push(ChatItem::Info(line)),
        ClaudeEvent::ControlRequest { .. } | ClaudeEvent::Exited => {}
    }
}
