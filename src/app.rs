use std::path::PathBuf;

use dioxus::prelude::*;

use crate::activity;
use crate::chat;
use crate::claude::ChatMode;
use crate::git;
use crate::history::{self, SessionInfo};
use crate::state::{AppState, ChatRequest, Cmd, Tab, WorkbenchView};
use crate::update_view::UpdateMenu;
use crate::workbench::{Body, ChatDock};

pub fn initial_cwd() -> PathBuf {
    if let Some(arg) = std::env::args().nth(1) {
        let p = PathBuf::from(arg);
        if p.is_dir() {
            return p.canonicalize().unwrap_or(p);
        }
    }
    // A launcher-started app often runs from the filesystem root (`/`, or `C:\` on
    // Windows); the home folder is a better start than that.
    match std::env::current_dir() {
        Ok(p) if p.parent().is_some() => p,
        cwd => crate::platform::home_dir().or(cwd.ok()).unwrap_or_else(|| PathBuf::from("/")),
    }
}

/// Opens a folder's graph and files. The chat switches to a session in that folder (see `App`).
pub fn open_folder(mut st: AppState, dir: PathBuf) {
    st.project_view.set(None);
    if !dir.is_dir() {
        st.workbench_error.set(Some(format!("{} is not a folder", dir.display())));
        return;
    }
    if *st.cwd.peek() == dir {
        return;
    }
    st.cwd.set(dir);
    st.open_file.set(None);
    st.tab.set(Tab::Files);
    st.bump_repo();
}

/// Reads the worktrees of every project repository again.
pub async fn refresh_worktrees(mut st: AppState) {
    let paths: Vec<String> = st.projects.peek().iter().flat_map(|p| p.repos.iter().map(|r| r.path.clone())).collect();
    let found = tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .filter(|p| std::path::Path::new(p).is_dir())
            .filter_map(|p| git::worktrees(std::path::Path::new(&p)).ok().map(|w| (p, w)))
            .collect::<std::collections::HashMap<_, _>>()
    })
    .await
    .unwrap_or_default();
    if *st.worktrees.peek() != found {
        st.worktrees.set(found);
    }
}

/// How often running agents are checked. Worktrees are read every `WORKTREE_EVERY` checks.
const AGENT_POLL: std::time::Duration = std::time::Duration::from_secs(2);
const WORKTREE_EVERY: u64 = 3;

/// Keeps `AppState::agents` and `AppState::worktrees` up to date for as long as the app runs.
fn watch_activity(mut st: AppState) {
    spawn(async move {
        let mut scanner = activity::Scanner::default();
        for tick in 0u64.. {
            let (back, agents) = tokio::task::spawn_blocking(move || {
                let agents = scanner.scan();
                (scanner, agents)
            })
            .await
            .unwrap_or_else(|_| (activity::Scanner::default(), Vec::new()));
            scanner = back;
            if *st.agents.peek() != agents {
                st.agents.set(agents);
            }
            if tick % WORKTREE_EVERY == 0 {
                refresh_worktrees(st).await;
            }
            tokio::time::sleep(AGENT_POLL).await;
        }
    });
}

/// How often the ledger watcher looks at `.workbench/ledger.ndjson`.
const LEDGER_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// The ledger file's size and modification time, or `None` when there is no ledger.
fn ledger_signature(cwd: &std::path::Path) -> Result<Option<(u64, std::time::SystemTime)>, String> {
    match std::fs::metadata(cwd.join(crate::ledger::LEDGER_FILE)) {
        Ok(m) => Ok(Some((m.len(), m.modified().map_err(|e| e.to_string())?))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", crate::ledger::LEDGER_FILE)),
    }
}

/// Adopts what others append to the open folder's ledger while the app runs (PRO-246): a
/// CI agent's work order, a teammate's pull, another Workbench window. It polls the file's
/// size and modification time, which also works on network mounts where file events don't
/// arrive, and records each look in `AppState::ledger_watch`, so the UI can tell when it stops.
fn watch_ledger(mut st: AppState) {
    spawn(async move {
        let mut seen: Option<(PathBuf, Option<(u64, std::time::SystemTime)>)> = None;
        loop {
            let cwd = st.cwd.peek().clone();
            let look = cwd.clone();
            let signature = tokio::task::spawn_blocking(move || ledger_signature(&look)).await.unwrap_or_else(|e| Err(e.to_string()));
            match signature {
                Ok(sig) => {
                    // A newly opened folder is loaded by `load_workbench`; only later changes count.
                    let changed = seen.as_ref().is_some_and(|(dir, old)| *dir == cwd && *old != sig);
                    let busy = st.job.peek().is_some() || st.turn_start.peek().is_some();
                    if changed && !busy {
                        st.adopt_external_ledger();
                    }
                    // During an agent turn, look again once it has ended.
                    if !(changed && busy) {
                        seen = Some((cwd, sig));
                    }
                    st.ledger_watch.set(crate::state::LedgerWatch { checked: Some(std::time::Instant::now()), error: None });
                }
                Err(e) => {
                    let checked = st.ledger_watch.peek().checked;
                    st.ledger_watch.set(crate::state::LedgerWatch { checked, error: Some(e) });
                }
            }
            tokio::time::sleep(LEDGER_POLL).await;
        }
    });
}

#[component]
pub fn App() -> Element {
    let mut st = use_context_provider(|| AppState::new(initial_cwd()));
    use_hook(move || {
        if let Some(file) = std::env::args().nth(2) {
            let path = std::path::PathBuf::from(&file);
            let rel = if path.is_absolute() { st.rel_path(&path) } else { file };
            st.open(&rel);
        }
    });
    use_hook(move || st.start_chats());
    use_effect(move || create_requested_chats(st));
    use_hook(move || watch_activity(st));
    use_hook(move || watch_ledger(st));
    crate::chat_window::use_chat_window(st);
    // Pick up new or removed repositories, and worktrees made by git commands, straight away.
    use_effect(move || {
        let _ = st.projects.read();
        let _ = st.repo_version.read();
        spawn(refresh_worktrees(st));
    });

    use_effect(move || {
        let _ = st.cwd.read();
        st.load_workbench();
    });

    // Opening a folder or a project view shows a chat that belongs there.
    use_effect(move || {
        let _ = st.cwd.read();
        let _ = st.project_view.read();
        st.show_chat_for_view();
    });

    use_effect(move || {
        let _ = st.repo_version.read();
        st.sync_workbench_from_disk();
        st.load_ghosts();
    });

    rsx! {
        div { class: "app",
            Header {}
            Body {}
            // Floats over the whole window; it can be dragged anywhere and resized from its edges.
            ChatDock {}
        }
    }
}

/// Creates the sessions and agents asked for in `AppState::chat_requests`. Runs in the main
/// window, so their signals and session loops live as long as the app.
fn create_requested_chats(mut st: AppState) {
    if st.chat_requests.read().is_empty() {
        return;
    }
    let requests = std::mem::take(&mut *st.chat_requests.write());
    for request in requests {
        match request {
            ChatRequest::NewSession { dir } => {
                let in_open_folder = crate::projects::same_dir(&dir, &st.cwd.peek());
                let chat = st.create_session(dir);
                if in_open_folder {
                    st.active_chat.set(Some(chat));
                }
            }
            ChatRequest::NewProjectSession { project } => {
                if let Some(chat) = st.create_project_session(&project) {
                    st.active_chat.set(Some(chat));
                }
            }
            ChatRequest::NewAgent { session, name, task, activate, provider, model, full_auto } => {
                let Some(mut chat) = st.create_agent(&session, name, provider, model) else { continue };
                if full_auto {
                    chat.chat_mode.set(ChatMode::FullAuto);
                }
                if let Some(text) = task {
                    chat.send(Cmd::Prompt { label: "Task from the session".into(), text });
                }
                if activate {
                    st.active_chat.set(Some(chat));
                }
            }
            ChatRequest::AgentInRepo { dir, name, task, provider, model, full_auto } => {
                let mut chat = st.create_named_session(dir, name, provider, model);
                if full_auto {
                    chat.chat_mode.set(ChatMode::FullAuto);
                }
                chat.send(Cmd::Prompt { label: "Task from another agent".into(), text: task });
            }
            ChatRequest::Discussion { agent } => {
                st.start_discussion(&agent);
            }
        }
    }
}

#[component]
fn Header() -> Element {
    let mut st = use_context::<AppState>();
    let is_repo = *use_memo(move || git::is_repo(&st.cwd.read())).read();
    let chat = *st.active_chat.read();
    let model = chat.map(|c| c.model.read().clone()).unwrap_or_default();
    let full_auto = chat.is_some_and(|c| *c.chat_mode.read() == ChatMode::FullAuto);
    let cost = chat.and_then(|c| *c.last_cost.read());
    let view = *st.view.read();
    let dirty = st.open_file.read().as_ref().is_some_and(|f| f.dirty());
    // The graph and the code share the main screen; these tabs switch between them.
    let tab = move |v: WorkbenchView, label: &'static str| {
        rsx! {
            button {
                class: if view == v { "view-tab active" } else { "view-tab" },
                onclick: move |_| st.view.set(v),
                "{label}"
            }
        }
    };

    rsx! {
        div { class: "header",
            div { class: "view-tabs",
                {tab(WorkbenchView::Graph, "Graph")}
                {tab(WorkbenchView::Code, if dirty { "Code ●" } else { "Code" })}
            }
            div {
                class: "spacer",
                // No OS title bar on Linux, so let the header's empty space move the window.
                onmousedown: move |_| {
                    if cfg!(target_os = "linux") {
                        dioxus::desktop::window().drag();
                    }
                },
                ondoubleclick: move |_| {
                    if cfg!(target_os = "linux") {
                        dioxus::desktop::window().toggle_maximized();
                    }
                },
            }
            UpdateMenu {}
            if is_repo {
                BranchMenu {}
            } else {
                span { class: "badge warn", "not a git repo" }
            }
            if !model.is_empty() { span { class: "badge", "{model}" } }
            if let Some(c) = cost { span { class: "badge", "${c:.4}" } }
            if full_auto {
                span {
                    class: "badge warn",
                    title: "{ChatMode::FullAuto.hint()}. Change the mode under the chat.",
                    "Full auto"
                }
            }
            // History resumes an earlier conversation in the chat shown, so it needs one.
            if chat.is_some() { HistoryMenu {} }
            button {
                title: "Start a new chat session in this folder",
                onclick: move |_| {
                    let dir = st.cwd.peek().clone();
                    st.request_chat(ChatRequest::NewSession { dir });
                },
                "New session"
            }
        }
    }
}

#[derive(Clone, PartialEq)]
enum BranchPick {
    Switch(git::SwitchTarget),
    Create(String),
}

#[component]
fn BranchMenu() -> Element {
    let mut st = use_context::<AppState>();
    let mut open = use_signal(|| false);
    let mut targets = use_signal(|| None::<Result<Vec<git::SwitchTarget>, String>>);
    let mut filter = use_signal(String::new);
    let mut cursor = use_signal(|| 0usize);
    let mut working = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let current = use_memo(move || {
        let _ = st.repo_version.read();
        git::current_branch(&st.cwd.read()).unwrap_or_else(|| "?".into())
    })
    .read()
    .clone();
    let busy = st.chat_busy();
    let is_open = *open.read();
    let is_working = *working.read();

    let mut toggle = move || {
        if *open.peek() {
            open.set(false);
            return;
        }
        open.set(true);
        targets.set(None);
        filter.set(String::new());
        cursor.set(0);
        error.set(None);
        let cwd = st.cwd.peek().clone();
        spawn(async move {
            let r = tokio::task::spawn_blocking(move || git::switch_targets(&cwd)).await.unwrap_or_else(|e| Err(e.to_string()));
            targets.set(Some(r));
        });
    };

    let picks = {
        let q = filter.read().trim().to_string();
        let ql = q.to_lowercase();
        let mut v: Vec<BranchPick> = match targets.read().as_ref() {
            Some(Ok(list)) => list.iter().filter(|t| t.name.to_lowercase().contains(&ql)).cloned().map(BranchPick::Switch).collect(),
            _ => Vec::new(),
        };
        let exists = q == current || v.iter().any(|p| matches!(p, BranchPick::Switch(t) if t.name == q));
        if !q.is_empty() && !exists && !q.contains(char::is_whitespace) {
            v.push(BranchPick::Create(q));
        }
        v
    };

    let mut pick = move |p: BranchPick| {
        if st.chat_busy() || *working.peek() {
            return;
        }
        working.set(true);
        error.set(None);
        let cwd = st.cwd.peek().clone();
        // A new branch starts a new chat session.
        let created = matches!(p, BranchPick::Create(_));
        spawn(async move {
            let session_dir = cwd.clone();
            let r = tokio::task::spawn_blocking(move || match &p {
                BranchPick::Switch(t) => git::switch_branch(&cwd, t),
                BranchPick::Create(name) => git::create_branch(&cwd, name),
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
            working.set(false);
            match r {
                Ok(()) => {
                    open.set(false);
                    st.bump_repo();
                    st.reload_open_file_if_clean();
                    if created {
                        st.request_chat(ChatRequest::NewSession { dir: session_dir });
                    }
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };

    let list = targets.read().clone();
    let picks_for_keys = picks.clone();
    let at = (*cursor.read()).min(picks.len().saturating_sub(1));

    rsx! {
        div { class: "history branch-menu",
            button {
                class: if is_open { "active branch-button" } else { "branch-button" },
                title: "Current branch. Click to switch branch",
                onclick: move |_| toggle(),
                "⎇ {current}"
            }
            if is_open {
                div { class: "history-backdrop", onclick: move |_| open.set(false) }
                div { class: "history-pop branch-pop",
                    input {
                        class: "branch-filter",
                        placeholder: "Filter, or type a new branch name…",
                        value: "{filter}",
                        onmounted: chat::focus,
                        oninput: move |e| {
                            filter.set(e.value());
                            cursor.set(0);
                        },
                        onkeydown: move |e: KeyboardEvent| {
                            let n = picks_for_keys.len();
                            let c = (*cursor.peek()).min(n.saturating_sub(1));
                            let step = |d: isize| if n == 0 { 0 } else { (c as isize + d).rem_euclid(n as isize) as usize };
                            let handled = match e.key() {
                                Key::ArrowDown => {
                                    cursor.set(step(1));
                                    true
                                }
                                Key::ArrowUp => {
                                    cursor.set(step(-1));
                                    true
                                }
                                Key::Enter => {
                                    if let Some(p) = picks_for_keys.get(c).cloned() {
                                        pick(p);
                                    }
                                    true
                                }
                                Key::Escape => {
                                    open.set(false);
                                    true
                                }
                                _ => false,
                            };
                            if handled {
                                e.prevent_default();
                                e.stop_propagation();
                            }
                        },
                    }
                    match list {
                        None => rsx! { div { class: "muted pad", "Loading branches…" } },
                        Some(Err(e)) => rsx! { div { class: "branch-error", "{e}" } },
                        Some(Ok(_)) if picks.is_empty() => rsx! { div { class: "muted pad", "No other branches." } },
                        Some(Ok(_)) => rsx! {
                            ul { class: "history-list",
                                for (i, p) in picks.into_iter().enumerate() {
                                    li {
                                        key: "{i}",
                                        class: if i == at { "active" } else { "" },
                                        onmouseenter: move |_| cursor.set(i),
                                        onclick: {
                                            let p = p.clone();
                                            move |_| pick(p.clone())
                                        },
                                        match &p {
                                            BranchPick::Switch(t) => rsx! {
                                                span { class: "history-title", "{t.name}" }
                                                if let Some(r) = &t.remote {
                                                    span { class: "history-when", title: "Creates a local branch tracking {r}", "{r}" }
                                                }
                                            },
                                            BranchPick::Create(name) => rsx! {
                                                span { class: "history-title", "+ Create branch “{name}” from {current}" }
                                            },
                                        }
                                    }
                                }
                            }
                        },
                    }
                    if let Some(e) = error.read().clone() {
                        div { class: "branch-error", "{e}" }
                    }
                    div { class: "history-hint muted small-text",
                        if busy {
                            "Stop Workbench before switching branches"
                        } else if is_working {
                            "Switching…"
                        } else {
                            "↑/↓ to move · Enter to switch · Esc to close"
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn HistoryMenu() -> Element {
    let st = use_context::<AppState>();
    let chat = st.chat();
    let mut open = use_signal(|| false);
    let mut sessions = use_signal(|| None::<Vec<SessionInfo>>);
    let mut cursor = use_signal(|| 0usize);
    let current = chat.session_id.read().clone();
    let busy = *chat.busy.read();
    let is_open = *open.read();

    let mut toggle = move || {
        if *open.peek() {
            open.set(false);
            return;
        }
        open.set(true);
        sessions.set(None);
        let cwd = st.cwd.peek().clone();
        let current = chat.session_id.peek().clone();
        spawn(async move {
            let list = tokio::task::spawn_blocking(move || history::list(&cwd)).await.unwrap_or_default();
            let at = current.and_then(|c| list.iter().position(|s| s.id == c)).unwrap_or(0);
            cursor.set(at);
            sessions.set(Some(list));
        });
    };
    let mut pick = move |id: String| {
        if *chat.busy.peek() {
            return;
        }
        open.set(false);
        if chat.session_id.peek().as_deref() != Some(id.as_str()) {
            chat.send(Cmd::Resume { id });
        }
    };
    let list = sessions.read().clone();

    rsx! {
        div { class: "history",
            button {
                class: if is_open { "active" } else { "" },
                title: "Earlier Workbench sessions in this folder",
                onclick: move |_| toggle(),
                "History"
            }
            if is_open {
                div { class: "history-backdrop", onclick: move |_| open.set(false) }
                div {
                    class: "history-pop",
                    tabindex: "0",
                    onmounted: chat::focus,
                    onkeydown: move |e: KeyboardEvent| {
                        let n = sessions.peek().as_ref().map(Vec::len).unwrap_or(0);
                        let c = *cursor.peek();
                        let step = |d: isize| if n == 0 { 0 } else { (c as isize + d).rem_euclid(n as isize) as usize };
                        let key = e.key();
                        let handled = match &key {
                            Key::ArrowDown => {
                                cursor.set(step(1));
                                true
                            }
                            Key::ArrowUp => {
                                cursor.set(step(-1));
                                true
                            }
                            Key::Character(s) if s == "j" => {
                                cursor.set(step(1));
                                true
                            }
                            Key::Character(s) if s == "k" => {
                                cursor.set(step(-1));
                                true
                            }
                            Key::Enter => {
                                let id = sessions.peek().as_ref().and_then(|l| l.get(c)).map(|s| s.id.clone());
                                if let Some(id) = id {
                                    pick(id);
                                }
                                true
                            }
                            Key::Escape => {
                                open.set(false);
                                true
                            }
                            _ => false,
                        };
                        if handled {
                            e.prevent_default();
                            e.stop_propagation();
                        }
                    },
                    match list {
                        None => rsx! { div { class: "muted pad", "Loading sessions…" } },
                        Some(l) if l.is_empty() => rsx! { div { class: "muted pad", "No earlier sessions in this folder." } },
                        Some(l) => rsx! {
                            ul { class: "history-list",
                                for (i, s) in l.into_iter().enumerate() {
                                    li {
                                        key: "{s.id}",
                                        class: if i == *cursor.read() { "active" } else { "" },
                                        title: "{s.id}",
                                        onmouseenter: move |_| cursor.set(i),
                                        onclick: {
                                            let id = s.id.clone();
                                            move |_| pick(id.clone())
                                        },
                                        span { class: "history-title", "{s.title}" }
                                        if current.as_deref() == Some(s.id.as_str()) {
                                            span { class: "chip", "current" }
                                        }
                                        span { class: "history-when", "{history::ago(s.modified)}" }
                                    }
                                }
                            }
                        },
                    }
                    div { class: "history-hint muted small-text",
                        if busy {
                            "Stop Workbench before switching sessions"
                        } else {
                            "↑/↓ or j/k to move · Enter to resume · Esc to close"
                        }
                    }
                }
            }
        }
    }
}
