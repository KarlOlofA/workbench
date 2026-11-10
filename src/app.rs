use std::path::PathBuf;

use dioxus::prelude::*;

use crate::chat;
use crate::claude::PermissionMode;
use crate::git;
use crate::history::{self, SessionInfo};
use crate::screens::{Screen, ScreenControls, ScreenState, Screens};
use crate::session;
use crate::state::{AppState, Cmd, LeftTab, Tab};
use crate::update_view::UpdateMenu;

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

pub fn open_folder(mut st: AppState, claude: Coroutine<Cmd>, dir: PathBuf) {
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
    claude.send(Cmd::Restart { keep_session: false });
    st.bump_repo();
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
    let claude = use_coroutine(move |rx: UnboundedReceiver<Cmd>| session::run(st, rx));

    use_effect(move || {
        let cwd = st.cwd.read().clone();
        st.load_workbench();
        if let Some(id) = history::last_session(&cwd) {
            claude.send(Cmd::Resume { id });
        }
    });

    use_effect(move || {
        let _ = st.repo_version.read();
        st.sync_workbench_from_disk();
        st.load_ghosts();
    });

    rsx! {
        div { class: "app",
            Header {}
            Screens {}
        }
    }
}

#[component]
fn Header() -> Element {
    let mut st = use_context::<AppState>();
    let claude = use_coroutine_handle::<Cmd>();
    let cwd = st.cwd.read().display().to_string();
    let project = st.current_project();
    let is_repo = git::is_repo(&st.cwd.read());
    let model = st.model.read().clone();
    let mode = *st.permission_mode.read();
    let cost = *st.last_cost.read();

    let pick_folder = move |_| {
        spawn(async move {
            let start = st.cwd.peek().clone();
            if let Some(handle) = rfd::AsyncFileDialog::new().set_directory(start).pick_folder().await {
                open_folder(st, claude, handle.path().to_path_buf());
            }
        });
    };

    rsx! {
        div { class: "header",
            ScreenControls {}
            button { onclick: pick_folder, "Open folder…" }
            if let Some((pid, pname)) = project.map(|p| (p.id, p.name)) {
                button {
                    class: "project-chip",
                    title: "Show all repositories in {pname}",
                    onclick: move |_| {
                        st.left_tab.set(LeftTab::Graph);
                        st.set_screen(Screen::Graph, ScreenState::Open);
                        st.show_project(Some(pid.clone()));
                    },
                    "◈ {pname}"
                }
            }
            span { class: "cwd", title: "{cwd}", "{cwd}" }
            if is_repo {
                BranchMenu {}
            } else {
                span { class: "badge warn", "not a git repo" }
            }
            div { class: "spacer" }
            UpdateMenu {}
            if !model.is_empty() { span { class: "badge", "{model}" } }
            if let Some(c) = cost { span { class: "badge", "${c:.4}" } }
            select {
                value: "{mode.as_arg()}",
                onchange: move |e| {
                    st.permission_mode.set(PermissionMode::from_arg(&e.value()));
                    claude.send(Cmd::Restart { keep_session: true });
                },
                for m in PermissionMode::ALL {
                    option { value: "{m.as_arg()}", selected: m == mode, "{m.label()}" }
                }
            }
            if mode == PermissionMode::FullAuto {
                span {
                    class: "badge warn",
                    title: "Claude runs commands and edits files without asking. It still asks before commands that delete files (rm, rmdir, unlink, shred, git rm, find -delete).",
                    "Full auto"
                }
            }
            HistoryMenu {}
            button {
                onclick: move |_| {
                    let cwd = st.cwd.peek().clone();
                    if let Err(e) = history::forget(&cwd) {
                        st.workbench_error.set(Some(e));
                    }
                    claude.send(Cmd::Restart { keep_session: false });
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
    let _ = st.repo_version.read();
    let current = git::current_branch(&st.cwd.read()).unwrap_or_else(|| "?".into());
    let busy = *st.busy.read();
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
        if *st.busy.peek() || *working.peek() {
            return;
        }
        working.set(true);
        error.set(None);
        let cwd = st.cwd.peek().clone();
        spawn(async move {
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
                            "Stop Claude before switching branches"
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
    let claude = use_coroutine_handle::<Cmd>();
    let mut open = use_signal(|| false);
    let mut sessions = use_signal(|| None::<Vec<SessionInfo>>);
    let mut cursor = use_signal(|| 0usize);
    let current = st.session_id.read().clone();
    let busy = *st.busy.read();
    let is_open = *open.read();

    let mut toggle = move || {
        if *open.peek() {
            open.set(false);
            return;
        }
        open.set(true);
        sessions.set(None);
        let cwd = st.cwd.peek().clone();
        let current = st.session_id.peek().clone();
        spawn(async move {
            let list = tokio::task::spawn_blocking(move || history::list(&cwd)).await.unwrap_or_default();
            let at = current.and_then(|c| list.iter().position(|s| s.id == c)).unwrap_or(0);
            cursor.set(at);
            sessions.set(Some(list));
        });
    };
    let mut pick = move |id: String| {
        if *st.busy.peek() {
            return;
        }
        open.set(false);
        if st.session_id.peek().as_deref() != Some(id.as_str()) {
            claude.send(Cmd::Resume { id });
        }
    };
    let list = sessions.read().clone();

    rsx! {
        div { class: "history",
            button {
                class: if is_open { "active" } else { "" },
                title: "Earlier Claude sessions in this folder",
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
                            "Stop Claude before switching sessions"
                        } else {
                            "↑/↓ or j/k to move · Enter to resume · Esc to close"
                        }
                    }
                }
            }
        }
    }
}
