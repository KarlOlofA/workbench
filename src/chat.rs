use dioxus::prelude::*;
use serde_json::Value;

use crate::claude::{PermissionDecision, PermissionRequest, tool_summary};
use crate::markdown;
use crate::state::{AppState, ChatItem, Cmd, RunMode};

#[component]
pub fn ChatPane() -> Element {
    let st = use_context::<AppState>();
    let items = st.items.read();

    rsx! {
        div { class: "chat",
            div { class: "chat-log",
                div { class: "chat-log-inner",
                    if items.is_empty() {
                        div { class: "empty",
                            "Ask Claude something about "
                            code { "{st.cwd.read().display()}" }
                        }
                    }
                    for (idx, item) in items.iter().enumerate() {
                        ChatEntry { key: "{idx}", item: item.clone() }
                    }
                    if *st.busy.read() {
                        if st.permissions.read().is_empty() {
                            div { class: "thinking", "Claude is working…" }
                        } else {
                            div { class: "thinking", "Waiting for your permission…" }
                        }
                    }
                    QueuedMessages {}
                }
            }
            if let Some(req) = st.permissions.read().first().cloned() {
                PermissionPrompt { key: "{req.request_id}", req, more: st.permissions.read().len() - 1 }
            }
            ChatInput {}
        }
    }
}

#[derive(Clone, PartialEq)]
enum Choice {
    Allow,
    AllowAlways(Vec<Value>),
    Deny,
    DenyWithNote,
    Stop,
}

fn permission_choices(req: &PermissionRequest) -> Vec<(Choice, String)> {
    let (rules, label) = req.always_rules();
    vec![
        (Choice::Allow, "Yes".to_string()),
        (Choice::AllowAlways(rules), format!("Yes, and don't ask again for {label}")),
        (Choice::Deny, "No".to_string()),
        (Choice::DenyWithNote, "No, and tell Claude what to do instead".to_string()),
        (Choice::Stop, "No, and stop Claude".to_string()),
    ]
}

pub fn focus(e: MountedEvent) {
    let el = e.data();
    spawn(async move {
        let _ = el.set_focus(true).await;
    });
}

#[component]
fn PermissionPrompt(req: PermissionRequest, more: usize) -> Element {
    let st = use_context::<AppState>();
    let claude = use_coroutine_handle::<Cmd>();
    let mut cursor = use_signal(|| 0usize);
    let mut note = use_signal(|| None::<String>);
    let choices = permission_choices(&req);
    let n = choices.len();
    let summary = tool_summary(&req.tool, &req.input, &st.cwd.read());
    let detail = serde_json::to_string_pretty(&req.input).unwrap_or_default();

    let answer = {
        let req = req.clone();
        move |decision: PermissionDecision| {
            claude.send(Cmd::Permission { request_id: req.request_id.clone(), decision });
        }
    };
    let choose = {
        let req = req.clone();
        let answer = answer.clone();
        move |choice: Choice| match choice {
            Choice::Allow => answer(PermissionDecision::Allow { input: req.input.clone(), permissions: Vec::new() }),
            Choice::AllowAlways(permissions) => answer(PermissionDecision::Allow { input: req.input.clone(), permissions }),
            Choice::Deny => answer(PermissionDecision::Deny { message: "The user declined this tool call.".into(), interrupt: false }),
            Choice::DenyWithNote => note.set(Some(String::new())),
            Choice::Stop => answer(PermissionDecision::Deny { message: "The user declined this tool call and stopped the task.".into(), interrupt: true }),
        }
    };
    let key_choices = choices.clone();
    let mut key_choose = choose.clone();
    let submit_note = answer.clone();
    let note_text = note.read().clone();

    rsx! {
        div {
            class: "permission",
            tabindex: "0",
            onmounted: focus,
            onkeydown: move |e: KeyboardEvent| {
                if note.peek().is_some() {
                    return;
                }
                let c = *cursor.peek();
                let handled = match e.key() {
                    Key::ArrowDown => {
                        cursor.set((c + 1) % n);
                        true
                    }
                    Key::ArrowUp => {
                        cursor.set((c + n - 1) % n);
                        true
                    }
                    Key::Enter => {
                        key_choose(key_choices[c].0.clone());
                        true
                    }
                    Key::Escape => {
                        key_choose(Choice::Deny);
                        true
                    }
                    Key::Character(s) => match s.as_str() {
                        "j" => {
                            cursor.set((c + 1) % n);
                            true
                        }
                        "k" => {
                            cursor.set((c + n - 1) % n);
                            true
                        }
                        " " => {
                            key_choose(key_choices[c].0.clone());
                            true
                        }
                        digit => match digit.parse::<usize>() {
                            Ok(i) if (1..=n).contains(&i) => {
                                cursor.set(i - 1);
                                key_choose(key_choices[i - 1].0.clone());
                                true
                            }
                            _ => false,
                        },
                    },
                    _ => false,
                };
                if handled {
                    e.prevent_default();
                    e.stop_propagation();
                }
            },
            div { class: "permission-head",
                span { class: "permission-tag", "Permission" }
                span { "Claude wants to use " }
                b { "{req.tool}" }
                if more > 0 {
                    span { class: "muted small-text", " · {more} more waiting" }
                }
            }
            if !summary.is_empty() {
                code { class: "permission-summary", "{summary}" }
            }
            if let Some(reason) = &req.reason {
                div { class: "muted small-text", "{reason}" }
            }
            if let Some(path) = &req.blocked_path {
                div { class: "muted small-text", "Outside the allowed directories: {path}" }
            }
            details { class: "permission-detail",
                summary { "Details" }
                pre { "{detail}" }
            }
            match note_text {
                Some(text) => rsx! {
                    div { class: "permission-note",
                        input {
                            value: "{text}",
                            placeholder: "What should Claude do instead? (Enter to send, Esc to go back)",
                            onmounted: focus,
                            oninput: move |e| note.set(Some(e.value())),
                            onkeydown: move |e: KeyboardEvent| {
                                e.stop_propagation();
                                match e.key() {
                                    Key::Enter => {
                                        e.prevent_default();
                                        let text = note.peek().clone().unwrap_or_default();
                                        let message = if text.trim().is_empty() {
                                            "The user declined this tool call.".to_string()
                                        } else {
                                            format!("The user declined this tool call and said: {}", text.trim())
                                        };
                                        submit_note(PermissionDecision::Deny { message, interrupt: false });
                                    }
                                    Key::Escape => {
                                        e.prevent_default();
                                        note.set(None);
                                    }
                                    _ => {}
                                }
                            },
                        }
                    }
                },
                None => rsx! {
                    ul { class: "permission-choices",
                        for (i, (choice, label)) in choices.into_iter().enumerate() {
                            li {
                                key: "{i}",
                                class: if i == *cursor.read() { "active" } else { "" },
                                onmouseenter: move |_| cursor.set(i),
                                onclick: {
                                    let mut choose = choose.clone();
                                    move |_| choose(choice.clone())
                                },
                                span { class: "permission-key", "{i + 1}" }
                                span { "{label}" }
                            }
                        }
                    }
                    div { class: "muted small-text", "↑/↓ or j/k to move · Enter to choose · 1-{n} to pick directly · Esc to deny" }
                },
            }
        }
    }
}

#[component]
fn ChatEntry(item: ChatItem) -> Element {
    match item {
        ChatItem::User(msg) => {
            let context = msg.context.join(", ");
            rsx! {
                div { class: "msg user",
                    div { class: "bubble",
                        if !context.is_empty() {
                            div { class: "msg-context", title: "Sent with these graph nodes as context", "◇ {context}" }
                        }
                        "{msg.text}"
                    }
                }
            }
        }
        ChatItem::Prompt { label, text } => rsx! {
            div { class: "msg user",
                details { class: "bubble prompt",
                    summary { span { class: "prompt-tag", "Workbench" } "{label}" }
                    pre { "{text}" }
                }
            }
        },
        ChatItem::Assistant { text, streaming } => {
            let html = markdown::to_html(&text);
            rsx! {
                div { class: if streaming { "msg assistant streaming" } else { "msg assistant" },
                    div { class: "md", dangerous_inner_html: "{html}" }
                }
            }
        }
        ChatItem::Tool { name, summary, input, result, .. } => {
            let status = match &result {
                None => "pending",
                Some((_, true)) => "error",
                Some((_, false)) => "ok",
            };
            rsx! {
                details { class: "tool {status}",
                    summary {
                        span { class: "tool-dot" }
                        span { class: "tool-name", "{name}" }
                        span { class: "tool-summary", "{summary}" }
                    }
                    pre { class: "tool-input", "{input}" }
                    if let Some((out, _)) = result {
                        pre { class: "tool-output", "{truncate(&out, 6000)}" }
                    }
                }
            }
        }
        ChatItem::Info(text) => rsx! { div { class: "msg info", "{text}" } },
        ChatItem::Error(text) => rsx! { div { class: "msg error", "{text}" } },
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… ({} more bytes)", &s[..end], s.len() - end)
}

#[component]
fn QueuedMessages() -> Element {
    let mut st = use_context::<AppState>();
    let queued: Vec<(String, String, usize)> = st.queued.read().iter().map(|m| (m.text.clone(), m.context.join(", "), m.context.len())).collect();

    rsx! {
        for (idx, (text, context, n_context)) in queued.into_iter().enumerate() {
            div { key: "q{idx}", class: "msg user queued",
                div { class: "bubble",
                    span { class: "queued-tag", "Queued" }
                    if n_context > 0 {
                        span { class: "queued-tag", title: "{context}", "◇ {n_context}" }
                    }
                    "{text}"
                    button {
                        class: "small queued-x",
                        title: "Remove from queue",
                        onclick: move |_| {
                            let mut q = st.queued.write();
                            if idx < q.len() {
                                q.remove(idx);
                            }
                        },
                        "✕"
                    }
                }
            }
        }
    }
}

#[component]
fn RunModeToggle() -> Element {
    let mut st = use_context::<AppState>();
    let run_mode = *st.run_mode.read();
    let locked = st.job.read().is_some();

    rsx! {
        div { class: "mode-toggle", title: "{run_mode.hint()}",
            for m in RunMode::ALL {
                button {
                    class: if m == run_mode { "mode-btn active" } else { "mode-btn" },
                    disabled: locked,
                    title: "{m.hint()}",
                    onclick: move |_| st.run_mode.set(m),
                    "{m.label()}"
                }
            }
        }
    }
}

#[component]
fn ChatInput() -> Element {
    let mut st = use_context::<AppState>();
    let claude = use_coroutine_handle::<Cmd>();
    let busy = *st.busy.read();

    let mut send = move |now: bool| {
        let text = st.chat_input.peek().trim().to_string();
        if text.is_empty() {
            return;
        }
        st.chat_input.set(String::new());
        let msg = st.message(text);
        claude.send(if now { Cmd::SendNow(msg) } else { Cmd::Send(msg) });
    };
    let context = st.selected_titles();
    let context_tip = context.join("\n");
    let context_label = match context.len() {
        0 => String::new(),
        1 => context[0].clone(),
        n => format!("{n} nodes"),
    };

    let placeholder = if busy {
        "Claude is working…  (Enter to queue, Ctrl+Enter to send now, Ctrl+C to stop)"
    } else {
        "Message Claude…  (Enter to send, Shift+Enter for newline)"
    };

    rsx! {
        div { class: "chat-toolbar",
            RunModeToggle {}
            div { class: "spacer" }
            if context.is_empty() {
                span { class: "muted small-text", title: "Select nodes in the graph, or ghost nodes from other branches, to send them as context", "Context: project" }
            } else {
                span { class: "context-chip", title: "Messages include these graph nodes as context:\n{context_tip}",
                    "Context: ◇ {context_label}"
                    span {
                        class: "chip-x",
                        title: "Clear the selection and use the normal context",
                        onclick: move |_| st.clear_context(),
                        "✕"
                    }
                }
            }
        }
        div { class: "chat-input",
            textarea {
                placeholder: "{placeholder}",
                value: "{st.chat_input}",
                oninput: move |e| st.chat_input.set(e.value()),
                onkeydown: move |e: KeyboardEvent| {
                    let m = e.modifiers();
                    if e.key() == Key::Enter && !m.shift() {
                        e.prevent_default();
                        send(m.ctrl());
                    } else if m.ctrl() && !m.meta() && e.code() == Code::KeyC && *st.busy.peek() {
                        e.prevent_default();
                        claude.send(Cmd::Interrupt);
                    }
                },
            }
        }
    }
}
