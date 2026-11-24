use dioxus::prelude::*;
use serde_json::Value;

use crate::claude::{ChatMode, PermissionDecision, PermissionRequest, tool_summary};
use crate::dropdown::{self, Dropdown};
use crate::projects::Project;
use crate::worktree_view::other_worktrees;
use crate::icons::{Icon, IconSvg};
use crate::logos::{Logo, LogoIcon};
use crate::markdown;
use crate::prompts;
use crate::state::{AppState, Chat, ChatItem, ChatRequest, ChatView, Cmd, PlanApproval};

/// Clicks on file references in replies (`a.file-ref`, see `markdown::to_html_with_files`)
/// open the file in the editor at the line they name. One listener per window; a new one
/// replaces the last, whose bridge is gone with its component.
const FILE_REF_JS: &str = r#"
if (window.__wbFileRef) document.removeEventListener("click", window.__wbFileRef);
window.__wbFileRef = (e) => {
  const a = e.target.closest && e.target.closest("a.file-ref");
  if (!a) return;
  e.preventDefault();
  if (!a.classList.contains("missing")) dioxus.send([a.dataset.path, Number(a.dataset.line) || 0, Number(a.dataset.col) || 0]);
};
document.addEventListener("click", window.__wbFileRef);
await new Promise(() => {});
"#;

fn use_file_ref_clicks(mut st: AppState) {
    use_hook(move || {
        let mut eval = document::eval(FILE_REF_JS);
        spawn(async move {
            while let Ok((path, line, col)) = eval.recv::<(String, u32, u32)>().await {
                st.open_at(&path, line, col);
            }
        });
    });
}

#[component]
pub fn ChatPane() -> Element {
    let mut st = use_context::<AppState>();
    use_file_ref_clicks(st);
    let chat = st.shown_chat();
    let items = chat.items.read();
    let discussion = chat.is_discussion();
    // In the discussion chat: what the execution chat waits for, so the user goes back to it.
    let execution_waits = discussion && st.chat().waiting();

    rsx! {
        div { class: if discussion { "chat discussion" } else { "chat" },
            div { class: "chat-log",
                div { class: "chat-log-inner",
                    if items.is_empty() && discussion {
                        div { class: "empty",
                            "Discuss the work without running anything. "
                            "This chat reads the execution chat and the code, but never changes them."
                        }
                    } else if items.is_empty() {
                        div { class: "empty",
                            "Craft something with "
                            span { class: "accent", "Workbench" }
                            " about "
                            code { "{chat.dir.read().display()}" }
                        }
                    }
                    for row in rows(&items, *chat.busy.read(), &chat.dir.cloned()) {
                        match row {
                            Row::One(idx, item) => rsx! { ChatEntry { key: "{idx}", item } },
                            Row::Tools(idx, tools) => rsx! { ToolGroup { key: "g{idx}", tools } },
                            Row::Changes(idx, paths) => rsx! { ChangedFiles { key: "c{idx}", dir: chat.dir.cloned(), paths } },
                        }
                    }
                    if chat.plan_approval.read().is_some() {
                        div { class: "thinking", "Waiting for you to approve the plan…" }
                    } else if *chat.busy.read() {
                        if chat.permissions.read().is_empty() {
                            div { class: "thinking", "Workbench is working…" }
                        } else {
                            div { class: "thinking", "Waiting for your permission…" }
                        }
                    }
                    QueuedMessages {}
                    if execution_waits {
                        div { class: "thinking discussion-hint",
                            "The agent is waiting for your answer in the execution chat · "
                            button { class: "small", onclick: move |_| st.set_chat_view(ChatView::Execution), "Go to Execute" }
                        }
                    }
                }
            }
            if let Some(approval) = chat.plan_approval.read().clone() {
                PlanPrompt { approval }
            }
            if let Some(req) = chat.permissions.read().first().cloned() {
                PermissionPrompt { key: "{req.request_id}", req, more: chat.permissions.read().len() - 1 }
            }
            ChatInput {}
        }
    }
}

/// A line in the chat log: one item, a run of back-to-back tool calls shown as a group, or the
/// files a finished turn changed. Rows are keyed by the index of their first item, so a growing
/// group keeps its state.
enum Row {
    One(usize, ChatItem),
    Tools(usize, Vec<ChatItem>),
    Changes(usize, Vec<String>),
}

/// A turn starts at a message to the agent. When it ends (at the next message, or when the agent
/// is no longer busy), the files it changed get a `Row::Changes` after it.
fn rows(items: &[ChatItem], busy: bool, dir: &std::path::Path) -> Vec<Row> {
    let mut out: Vec<Row> = Vec::new();
    let mut turn_start = 0;
    for (idx, item) in items.iter().enumerate() {
        if matches!(item, ChatItem::User(_) | ChatItem::Prompt { .. }) {
            let paths = changed_files(&items[turn_start..idx], dir);
            if !paths.is_empty() {
                out.push(Row::Changes(turn_start, paths));
            }
            turn_start = idx;
        }
        if matches!(item, ChatItem::Tool { .. }) {
            if let Some(Row::Tools(_, tools)) = out.last_mut() {
                tools.push(item.clone());
            } else {
                out.push(Row::Tools(idx, vec![item.clone()]));
            }
        } else {
            out.push(Row::One(idx, item.clone()));
        }
    }
    let paths = changed_files(&items[turn_start..], dir);
    if !busy && !paths.is_empty() {
        out.push(Row::Changes(turn_start, paths));
    }
    out
}

/// The files that successful edit tool calls in `items` wrote, relative to `dir`, in the order
/// they were first changed. Every provider reports its edits as Edit or Write (see `codex.rs`
/// and `vibe.rs`); Codex lists several files under `changes`. Files outside `dir` are left out.
fn changed_files(items: &[ChatItem], dir: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let ChatItem::Tool { name, input, result: Some((_, false)), .. } = item else { continue };
        if !matches!(name.as_str(), "Edit" | "Write" | "MultiEdit" | "NotebookEdit") {
            continue;
        }
        let input: Value = serde_json::from_str(input).unwrap_or_default();
        let mut paths: Vec<&str> = ["file_path", "notebook_path", "path"]
            .iter()
            .filter_map(|k| input.get(k).and_then(Value::as_str))
            .take(1)
            .collect();
        if let Some(changes) = input.get("changes").and_then(Value::as_array) {
            paths = changes.iter().filter_map(|c| c.get("path").and_then(Value::as_str)).collect();
        }
        for p in paths {
            let p = std::path::Path::new(p);
            let rel = if p.is_absolute() { p.strip_prefix(dir).ok() } else { Some(p) };
            let Some(rel) = rel.map(|r| r.display().to_string()) else { continue };
            if !rel.is_empty() && !out.contains(&rel) {
                out.push(rel);
            }
        }
    }
    out
}

/// The files a turn changed, folded under one button. Opened, it lists each file with its
/// uncommitted diff against HEAD (from git, so it can include changes made before this turn);
/// each file unfolds to show its hunks.
#[component]
fn ChangedFiles(dir: std::path::PathBuf, paths: Vec<String>) -> Element {
    let st = use_context::<AppState>();
    let mut open = use_signal(|| false);
    let is_open = *open.read();
    let n = paths.len();
    let local = *st.cwd.read() == dir;

    rsx! {
        div { class: "changed-files",
            button {
                class: "tool-group-toggle",
                title: if is_open { "Hide the changed files" } else { "Show the changed files and their diffs" },
                onclick: move |_| open.set(!is_open),
                span { class: if is_open { "tool-mark open" } else { "tool-mark" }, ">" }
                span { if n == 1 { "1 changed file" } else { "{n} changed files" } }
                span { class: "muted", "· changes in working tree" }
            }
            if is_open {
                for path in paths {
                    ChangedFile { key: "{path}", dir: dir.clone(), path, local }
                }
            }
        }
    }
}

/// One changed file's diff, read from git when it is shown, and again whenever the repo changes.
#[component]
fn ChangedFile(dir: std::path::PathBuf, path: String, local: bool) -> Element {
    let st = use_context::<AppState>();
    let label = path.clone();
    let diff = use_resource(use_reactive!(|(dir, path)| async move {
        let _ = st.repo_version.read();
        crate::git::diff_file(&dir, &path)
    }));

    match &*diff.read() {
        None => rsx! { div { class: "muted pad", "{label}…" } },
        Some(Err(e)) => rsx! { div { class: "error pad", "{label}: {e}" } },
        Some(Ok(None)) => rsx! { div { class: "muted pad", "{label}: no uncommitted changes" } },
        Some(Ok(Some(file))) => rsx! { crate::code::FileDiffView { file: file.clone(), local, open: false } },
    }
}

/// Back-to-back tool calls. Collapsed, only the latest call shows, under a button that lists
/// them all; a single call shows on its own.
#[component]
fn ToolGroup(tools: Vec<ChatItem>) -> Element {
    let mut open = use_signal(|| false);
    let n = tools.len();
    if n == 1 {
        return rsx! { ChatEntry { item: tools[0].clone() } };
    }
    let failed = tools.iter().filter(|t| matches!(t, ChatItem::Tool { result: Some((_, true)), .. })).count();
    let is_open = *open.read();
    let shown: Vec<ChatItem> = if is_open { tools } else { tools.into_iter().skip(n - 1).collect() };
    let shown_from = if is_open { 0 } else { n - 1 };

    rsx! {
        div { class: "tool-group",
            button {
                class: "tool-group-toggle",
                title: if is_open { "Show only the latest tool call" } else { "Show all {n} tool calls" },
                onclick: move |_| open.set(!is_open),
                span { class: if is_open { "tool-mark open" } else { "tool-mark" }, ">" }
                span { "{n} tool calls" }
                if failed > 0 {
                    span { class: "tool-group-failed", "· {failed} failed" }
                }
                if !is_open {
                    span { class: "muted", "· latest below" }
                }
            }
            for (i, item) in shown.into_iter().enumerate() {
                ChatEntry { key: "{shown_from + i}", item }
            }
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
        (Choice::DenyWithNote, "No, and tell Workbench what to do instead".to_string()),
        (Choice::Stop, "No, and stop Workbench".to_string()),
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
    let chat = st.shown_chat();
    let mut cursor = use_signal(|| 0usize);
    let mut note = use_signal(|| None::<String>);
    let choices = permission_choices(&req);
    let n = choices.len();
    let summary = tool_summary(&req.tool, &req.input, &st.cwd.read());
    let detail = serde_json::to_string_pretty(&req.input).unwrap_or_default();

    let answer = {
        let req = req.clone();
        move |decision: PermissionDecision| {
            chat.send(Cmd::Permission { request_id: req.request_id.clone(), decision });
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
                span { "Workbench wants to use " }
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
                            placeholder: "What should Workbench do instead? (Enter to send, Esc to go back)",
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
    let st = use_context::<AppState>();
    match item {
        ChatItem::User(msg) => {
            let context = msg.context.join(", ");
            let text = markdown::text_with_files(&msg.text, &st.cwd.peek());
            rsx! {
                div { class: "msg user",
                    div { class: "bubble",
                        if !context.is_empty() {
                            div { class: "msg-context", title: "Sent with these graph nodes as context", "◇ {context}" }
                        }
                        span { dangerous_inner_html: "{text}" }
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
            let html = markdown::to_html_with_files(&text, &st.cwd.peek());
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
                        span { class: "tool-mark", ">" }
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
        ChatItem::Focus(matches) => rsx! { FocusMatches { matches } },
    }
}

/// The nodes a reply named, as buttons that show each one on the graph.
#[component]
fn FocusMatches(matches: Vec<prompts::FocusRequest>) -> Element {
    let mut st = use_context::<AppState>();
    let g = st.graph.read().clone();
    rsx! {
        div { class: "msg info focus-matches",
            span { class: "muted small-text", if matches.len() == 1 { "Node in this reply:" } else { "Nodes in this reply:" } }
            for (i, m) in matches.into_iter().enumerate() {
                {
                    let title = g.net(m.module.as_deref()).and_then(|n| n.node(&m.node)).map(|n| n.title.clone());
                    rsx! {
                        button {
                            key: "{i}",
                            class: "small",
                            disabled: title.is_none(),
                            title: if title.is_none() { "This node isn't in the graph" } else { "Show it on the graph" },
                            onclick: move |_| st.focus_node(m.module.clone(), m.node.clone()),
                            "{title.clone().unwrap_or_else(|| m.node.clone())} · {m.confidence}"
                        }
                    }
                }
            }
        }
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
    let st = use_context::<AppState>();
    let mut chat = st.shown_chat();
    let queued: Vec<(String, String, usize)> = chat.queued.read().iter().map(|m| (m.text.clone(), m.context.join(", "), m.context.len())).collect();

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
                            let mut q = chat.queued.write();
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

/// The chat's mode, offered in the variants the current provider can run. A change reaches
/// the agent with the next message; Workbench's own permission answers (Auto, Full auto)
/// follow it at once.
#[component]
fn ModeToggle() -> Element {
    let st = use_context::<AppState>();
    let mut chat = st.shown_chat();
    let provider = st.agent_provider(&chat).0;
    let current = *chat.chat_mode.read();

    rsx! {
        div { class: "mode-toggle", title: "{provider.mode_hint(current)}",
            for m in provider.modes().iter().copied() {
                button {
                    class: match (m == current, m) {
                        (true, ChatMode::FullAuto) => "mode-btn active danger",
                        (true, _) => "mode-btn active",
                        _ => "mode-btn",
                    },
                    title: "{provider.mode_hint(m)}",
                    onclick: move |_| chat.chat_mode.set(m),
                    "{provider.mode_label(m)}"
                }
            }
        }
    }
}

/// A finished plan: shows it and asks which mode to run it in, or to keep planning.
/// Keys 1–3 pick a mode, Esc keeps planning.
#[component]
fn PlanPrompt(approval: PlanApproval) -> Element {
    let st = use_context::<AppState>();
    let chat = st.shown_chat();
    let provider = st.agent_provider(&chat).0;
    // Only the run modes the current provider supports (API providers just answer).
    let run_modes: Vec<ChatMode> = ChatMode::RUN_PLAN.into_iter().filter(|m| provider.modes().contains(m)).collect();
    let key_modes = run_modes.clone();
    let html = markdown::to_html(&approval.plan);
    let answer = move |mode: Option<ChatMode>| chat.send(Cmd::ApprovePlan { mode });

    rsx! {
        div {
            class: "permission plan-approval",
            tabindex: "0",
            onmounted: focus,
            onkeydown: move |e: KeyboardEvent| {
                let pick = match e.key() {
                    Key::Escape => Some(None),
                    Key::Character(d) => d.parse::<usize>().ok().and_then(|i| key_modes.get(i.wrapping_sub(1))).map(|m| Some(*m)),
                    _ => None,
                };
                if let Some(mode) = pick {
                    e.prevent_default();
                    answer(mode);
                }
            },
            div { class: "permission-head",
                span { class: "permission-tag", "Plan" }
                span { "The plan is ready. How do you want to run it?" }
            }
            if !approval.plan.trim().is_empty() {
                div { class: "plan-body md", dangerous_inner_html: "{html}" }
            }
            div { class: "plan-choices",
                for (i, m) in run_modes.into_iter().enumerate() {
                    button {
                        key: "{i}",
                        class: if i == 0 { "primary" } else { "" },
                        title: "{provider.mode_hint(m)}",
                        onclick: move |_| answer(Some(m)),
                        span { class: "permission-key", "{i + 1}" }
                        "{provider.mode_label(m)}"
                    }
                }
                div { class: "spacer" }
                button { title: "Tell the agent what to change in the plan", onclick: move |_| answer(None), "Keep planning" }
            }
        }
    }
}

#[component]
fn ChatInput() -> Element {
    let mut st = use_context::<AppState>();
    let mut chat = st.shown_chat();
    let busy = *chat.busy.read();

    let mut send = move |now: bool| {
        let text = chat.input.peek().trim().to_string();
        if text.is_empty() {
            return;
        }
        chat.input.set(String::new());
        let msg = st.message(text);
        chat.send(if now { Cmd::SendNow(msg) } else { Cmd::Send(msg) });
    };
    let context = st.selected_titles();
    let context_tip = context.join("\n");
    let context_label = match context.len() {
        0 => String::new(),
        1 => context[0].clone(),
        n => format!("{n} nodes"),
    };

    let discussion = chat.is_discussion();
    let placeholder = match (discussion, busy) {
        (true, true) => "Thinking…  (Enter to queue, Ctrl+C to stop)",
        (true, false) => "Ask about what the agent is doing…  (Enter to send, Shift+Enter for newline)",
        (false, true) => "Workbench is working…  (Enter to queue, Ctrl+Enter to send now, Ctrl+C to stop)",
        (false, false) => "Message Workbench…  (Enter to send, Shift+Enter for newline)",
    };

    let empty = chat.input.read().trim().is_empty();

    rsx! {
        div { class: "chat-input",
            div { class: "composer",
                textarea {
                    placeholder: "{placeholder}",
                    value: "{chat.input}",
                    oninput: move |e| chat.input.set(e.value()),
                    onkeydown: move |e: KeyboardEvent| {
                        let m = e.modifiers();
                        if e.key() == Key::Enter && !m.shift() {
                            e.prevent_default();
                            send(m.ctrl());
                        } else if m.ctrl() && !m.meta() && e.code() == Code::KeyC && *chat.busy.peek() {
                            e.prevent_default();
                            chat.send(Cmd::Interrupt);
                        }
                    },
                }
                div { class: "chat-toolbar",
                    if discussion {
                        span {
                            class: "context-chip discussion-chip",
                            title: "This chat only reads and answers. Each message carries what happened in the execution chat since your last one.",
                            "Discussion · read only"
                        }
                    } else {
                        ModeToggle {}
                    }
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
                    div { class: "spacer" }
                    if busy && empty {
                        button { class: "send-btn stop", title: "Stop the agent (Ctrl+C)", onclick: move |_| chat.send(Cmd::Interrupt), span { class: "stop-square" } }
                    } else {
                        button {
                            class: "send-btn",
                            disabled: empty,
                            title: if busy { "Queue this message (Ctrl+Enter sends it now)" } else { "Send (Enter)" },
                            onclick: move |_| send(false),
                            IconSvg { icon: Icon::ArrowUp }
                        }
                    }
                }
            }
        }
    }
}

/// Starts a new chat session in `dir` (the open folder when not given), opens that folder and
/// shows the session in the chat. For places where a folder has no session.
#[component]
pub fn NewSessionButton(dir: Option<std::path::PathBuf>) -> Element {
    let mut st = use_context::<AppState>();
    rsx! {
        button {
            class: "small",
            title: "Start a new chat session in this folder",
            onclick: move |e: MouseEvent| {
                e.stop_propagation();
                let dir = dir.clone().unwrap_or_else(|| st.cwd.peek().clone());
                crate::app::open_folder(st, dir.clone());
                st.request_chat(ChatRequest::NewSession { dir });
            },
            "New session"
        }
    }
}

/// What the chat shows when it has no session to show: in a project view a New project
/// session button and the project's sessions to pick from, elsewhere a New session button.
#[component]
pub fn NoChat() -> Element {
    let mut st = use_context::<AppState>();
    let Some(project) = st.viewed_project() else {
        return rsx! {
            span { "No chat session in this folder" }
            NewSessionButton {}
        };
    };
    let project_id = project.id.clone();
    let has_folder = project.folder().is_some();
    rsx! {
        span { "No project session yet" }
        button {
            class: "small",
            disabled: !has_folder,
            title: if has_folder { "Start a chat session for the whole project" } else { "Add a repository to the project first" },
            onclick: move |_| st.request_chat(ChatRequest::NewProjectSession { project: project_id.clone() }),
            "New project session"
        }
        SessionPicker { project }
    }
}

const NEW_PROJECT_SESSION: &str = "+new-project-session";

/// In a project view: picks the chat session to show from the project's own sessions and the
/// sessions in its repositories and their worktrees, or starts a new project-level session.
/// Showing a repository's session keeps the project view open.
#[component]
pub fn SessionPicker(project: Project) -> Element {
    let mut st = use_context::<AppState>();
    let current = (*st.active_chat.read()).map(|c| c.session.cloned()).unwrap_or_default();
    let mut choices: Vec<dropdown::Choice> = st.project_sessions(&project.id).into_iter().map(|s| dropdown::Choice::new(s.id, format!("◈ {}", s.title))).collect();
    for repo in &project.repos {
        let mut dirs = vec![repo.dir()];
        dirs.extend(other_worktrees(&st, &repo.path).into_iter().map(|w| w.path));
        for session in dirs.iter().flat_map(|d| st.sessions_in(d)) {
            choices.push(dropdown::Choice::new(session.id, format!("{} · {}", repo.label(), session.title)));
        }
    }
    choices.push(dropdown::Choice::new(NEW_PROJECT_SESSION, "+ New project session"));
    let project_id = project.id.clone();
    rsx! {
        Dropdown {
            title: "The chat session to show: the project's own, or one in its repositories",
            value: current,
            placeholder: "Sessions in {project.name}",
            choices,
            onchange: move |id: String| {
                if id == NEW_PROJECT_SESSION {
                    st.request_chat(ChatRequest::NewProjectSession { project: project_id.clone() });
                } else if let Some(chat) = st.session_chats(&id).first().copied() {
                    st.active_chat.set(Some(chat));
                }
            },
        }
    }
}

/// The agents of the active chat session as tabs, for the chat's top bar. Clicking a tab shows
/// that agent's chat; `+` adds an agent to the session. In a project view the session's title
/// is a `SessionPicker`.
#[component]
pub fn ChatTabs() -> Element {
    let mut st = use_context::<AppState>();
    let session_id = st.chat().session.cloned();
    let Some(session) = st.session(&session_id) else { return rsx! {} };
    let chats = st.session_chats(&session_id);

    rsx! {
        div {
            class: "chat-tabs",
            // The bar around the tabs drags the chat; the tabs themselves don't.
            onmousedown: move |e| e.stop_propagation(),
            match st.viewed_project() {
                Some(project) => rsx! { SessionPicker { project } },
                None => rsx! { span { class: "chat-tabs-session muted small-text", title: "{session.dir.display()}", "{session.title}" } },
            }
            for agent in session.agents {
                if let Some(chat) = chats.iter().copied().find(|c| c.is(&agent.id)) {
                    ChatTab { key: "{agent.id}", chat, name: agent.name }
                }
            }
            button {
                class: "small chat-tab-add",
                title: "Add an agent to this session",
                onclick: move |_| st.request_chat(ChatRequest::NewAgent { session: session_id.clone(), name: None, task: None, activate: true, provider: None, model: None, full_auto: false }),
                "+"
            }
        }
    }
}

#[component]
fn ChatTab(chat: Chat, name: String) -> Element {
    let mut st = use_context::<AppState>();
    let active = st.chat() == chat;
    let busy = *chat.busy.read();
    let waiting = chat.waiting();
    let provider = st.agent_provider(&chat).0;

    rsx! {
        button {
            class: if active { "chat-tab active" } else { "chat-tab" },
            title: if waiting { "{provider.label()} · waiting for your answer" } else if busy { "{provider.label()} · working" } else { "{provider.label()} · waiting for a prompt" },
            onclick: move |_| st.active_chat.set(Some(chat)),
            span {
                class: "chat-tab-close",
                title: if busy { "Close this agent, stopping its work" } else { "Close this agent" },
                onclick: move |e: MouseEvent| {
                    e.stop_propagation();
                    st.close_agent(chat);
                },
                "✕"
            }
            LogoIcon { logo: Logo::of(provider) }
            "{name}"
            if waiting {
                span { class: "chat-tab-flag", "!" }
            } else if busy {
                span { class: "agent-dot working", "●" }
            }
        }
    }
}

/// Switches the chat between the active agent's execution chat, which runs the work, and its
/// read-only discussion chat (PRO-248). Each side shows a dot while it works; Execute shows !
/// while the agent waits for an answer.
#[component]
pub fn ChatViewToggle() -> Element {
    let mut st = use_context::<AppState>();
    let chat = st.chat();
    let view = *st.chat_view.read();
    let discussion = st.discussion(&chat);
    let exec_busy = *chat.busy.read();
    let exec_waiting = chat.waiting();
    let discussion_busy = discussion.is_some_and(|d| *d.busy.read());
    let class = |v: ChatView| if v == view { "mode-btn active" } else { "mode-btn" };
    // Switching to another agent's tab in the Discuss view starts that agent's discussion chat.
    // Doesn't write `chat_view`, which it reads, so it can't trigger itself.
    use_effect(move || {
        let active = *st.active_chat.read();
        if *st.chat_view.read() == ChatView::Discussion
            && let Some(chat) = active
        {
            let agent = chat.agent_id.cloned();
            if !st.discussions.peek().contains_key(&agent) {
                st.request_chat(ChatRequest::Discussion { agent });
            }
        }
    });

    rsx! {
        div {
            class: "mode-toggle chat-view-toggle",
            onmousedown: move |e| e.stop_propagation(),
            button {
                class: class(ChatView::Execution),
                title: "The agent's chat: it runs the work",
                onclick: move |_| st.set_chat_view(ChatView::Execution),
                "Execute"
                if exec_waiting && view != ChatView::Execution {
                    span { class: "chat-tab-flag", "!" }
                } else if exec_busy && view != ChatView::Execution {
                    span { class: "agent-dot working", "●" }
                }
            }
            button {
                class: class(ChatView::Discussion),
                title: "A separate read-only chat to ask about and discuss what the agent is doing, without running anything",
                onclick: move |_| st.set_chat_view(ChatView::Discussion),
                "Discuss"
                if discussion_busy && view != ChatView::Discussion {
                    span { class: "agent-dot working", "●" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Message;
    use std::path::Path;

    fn tool(name: &str, input: &str, failed: bool) -> ChatItem {
        let result = Some((String::new(), failed));
        ChatItem::Tool { id: String::new(), name: name.into(), summary: String::new(), input: input.into(), result }
    }

    fn user() -> ChatItem {
        ChatItem::User(Message { text: "go".into(), context: vec![], prompt: String::new() })
    }

    #[test]
    fn changed_files_lists_successful_edits_once_relative_to_the_folder() {
        let items = vec![
            tool("Edit", r#"{"file_path":"/repo/src/a.rs"}"#, false),
            tool("Write", r#"{"file_path":"/repo/src/b.rs"}"#, false),
            tool("Edit", r#"{"file_path":"/repo/src/a.rs"}"#, false),
            tool("Edit", r#"{"file_path":"/repo/src/failed.rs"}"#, true),
            tool("Read", r#"{"file_path":"/repo/src/read.rs"}"#, false),
            tool("Edit", r#"{"file_path":"/elsewhere/c.rs"}"#, false),
            tool("Edit", r#"{"file_path":"src/codex.rs","changes":[{"path":"src/d.rs"},{"path":"src/e.rs"}]}"#, false),
        ];
        assert_eq!(changed_files(&items, Path::new("/repo")), vec!["src/a.rs", "src/b.rs", "src/d.rs", "src/e.rs"]);
    }

    #[test]
    fn rows_show_changed_files_after_each_finished_turn() {
        let edit = tool("Edit", r#"{"file_path":"/repo/a.rs"}"#, false);
        let items = vec![user(), edit.clone(), user(), edit];
        let kinds = |busy| {
            rows(&items, busy, Path::new("/repo"))
                .iter()
                .map(|r| match r {
                    Row::One(..) => "one",
                    Row::Tools(..) => "tools",
                    Row::Changes(..) => "changes",
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(kinds(false), vec!["one", "tools", "changes", "one", "tools", "changes"]);
        assert_eq!(kinds(true), vec!["one", "tools", "changes", "one", "tools"]);
    }
}
