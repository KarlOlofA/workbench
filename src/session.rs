use dioxus::prelude::*;
use futures_util::StreamExt;

use crate::claude::{ClaudeEvent, ClaudeProcess, PermissionDecision, SpawnOptions, tool_file_path, tool_summary, tool_touches_files};
use crate::history;
use crate::projects;
use crate::prompts;
use crate::state::{AppState, ChatItem, Cmd, RunMode};

struct Live {
    proc: ClaudeProcess,
    rules_version: u64,
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
                            && let Err(e) = l.proc.respond_permission(&request_id, &decision).await
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
                    Cmd::Interrupt => {
                        deny_pending(&mut st, &mut live).await;
                        if let Some(l) = live.as_mut()
                            && l.proc.interrupt().await.is_err()
                        {
                            if let Some(l) = live.take() {
                                l.proc.kill().await;
                            }
                            generation += 1;
                            st.busy.set(false);
                            st.finish_job(false);
                        }
                        continue;
                    }
                    Cmd::Resume { id } => {
                        if let Some(l) = live.take() {
                            l.proc.kill().await;
                        }
                        generation += 1;
                        st.permissions.write().clear();
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
                            l.proc.kill().await;
                        }
                        generation += 1;
                        st.permissions.write().clear();
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
                        let _ = l.proc.respond_error(request_id, &format!("Workbench does not handle `{subtype}` requests")).await;
                    }
                    continue;
                }
                if let ClaudeEvent::Exited = ev {
                    let code = live.as_mut().and_then(|l| l.proc.exit_code());
                    live = None;
                    st.permissions.write().clear();
                    if *st.busy.peek() {
                        st.items.write().push(ChatItem::Error(format!("claude exited (code {code:?})")));
                    }
                    st.busy.set(false);
                    st.finish_job(false);
                    continue;
                }
                if let ClaudeEvent::Permission(req) = &ev
                    && st.permission_mode.peek().auto_allows(req)
                    && let Some(l) = live.as_mut()
                {
                    let decision = PermissionDecision::Allow { input: req.input.clone(), permissions: Vec::new() };
                    match l.proc.respond_permission(&req.request_id, &decision).await {
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
    if live.as_ref().is_some_and(|l| l.rules_version != rules_version)
        && let Some(l) = live.take()
    {
        l.proc.kill().await;
        *generation += 1;
        st.permissions.write().clear();
    }
    if live.is_none() {
        *generation += 1;
        let opts = SpawnOptions {
            cwd: st.cwd.peek().clone(),
            resume: st.session_id.peek().clone(),
            permission_mode: *st.permission_mode.peek(),
            append_system_prompt: {
                let base = st.with_refs(|refs| {
                    prompts::system_append(Some(&*st.graph.peek()), &st.rules.peek(), &st.related_repos.peek(), Some(refs))
                });
                let cwd = st.cwd.peek().clone();
                let project = projects::containing(&st.projects.peek(), &cwd).cloned();
                prompts::with_project(base, project.as_ref(), &cwd)
            },
        };
        match ClaudeProcess::spawn(&opts, *generation, ev_tx.clone()) {
            Ok(proc) => *live = Some(Live { proc, rules_version }),
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
        && let Err(e) = l.proc.send_user(&text).await
    {
        st.items.write().push(ChatItem::Error(format!("send failed: {e:#}")));
        st.busy.set(false);
        st.finish_job(false);
        *live = None;
    }
}

async fn deny_pending(st: &mut AppState, live: &mut Option<Live>) {
    let pending: Vec<_> = st.permissions.write().drain(..).collect();
    let Some(l) = live.as_mut() else { return };
    let decision = PermissionDecision::Deny { message: "The user stopped the task.".into(), interrupt: true };
    for p in pending {
        let _ = l.proc.respond_permission(&p.request_id, &decision).await;
    }
}

fn handle_event(st: &mut AppState, ev: ClaudeEvent) {
    if matches!(ev, ClaudeEvent::TextDelta(_) | ClaudeEvent::ToolUse { .. } | ClaudeEvent::ToolResult { .. })
        && !*st.busy.peek()
    {
        st.busy.set(true);
    }
    match ev {
        ClaudeEvent::Init { session_id, model } => {
            let cwd = st.cwd.peek().clone();
            if let Err(e) = history::remember(&cwd, &session_id) {
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
            let questions = !is_error
                && text.as_deref().is_some_and(prompts::asks_questions)
                && st.job.peek().is_some_and(|j| j.mode == RunMode::Plan);
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
            st.bump_repo();
            st.reload_open_file_if_clean();
        }
        ClaudeEvent::Permission(req) => st.permissions.write().push(req),
        ClaudeEvent::ControlCancel { request_id } => st.permissions.write().retain(|p| p.request_id != request_id),
        ClaudeEvent::Stderr(line) => st.items.write().push(ChatItem::Info(line)),
        ClaudeEvent::ControlRequest { .. } | ClaudeEvent::Exited => {}
    }
}
