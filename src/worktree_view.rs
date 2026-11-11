//! Worktrees and agent activity in the projects view and the projects sidebar.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use dioxus::prelude::*;

use crate::activity::AgentProc;
use crate::app::{open_folder, refresh_worktrees};
use crate::git::{self, Worktree};
use crate::logos::LogoIcon;
use crate::projects::same_dir;
use crate::state::{AppState, Cmd};

/// The worktrees of a project repository, without the checkout the project points at
/// (that one is the repository itself).
pub fn other_worktrees(st: &AppState, repo_path: &str) -> Vec<Worktree> {
    let repo = Path::new(repo_path);
    st.worktrees.read().get(repo_path).map(|l| l.iter().filter(|w| !same_dir(&w.path, repo)).cloned().collect()).unwrap_or_default()
}

/// Every agent in a repository and its worktrees.
pub fn agents_in_repo(activity: &HashMap<PathBuf, Vec<AgentProc>>, repo_path: &str, worktrees: &[Worktree]) -> Vec<AgentProc> {
    let mut dirs = vec![PathBuf::from(repo_path)];
    dirs.extend(worktrees.iter().map(|w| w.path.clone()));
    dirs.iter().flat_map(|d| activity.get(d).cloned().unwrap_or_default()).collect()
}

/// A small marker for a list row: `●` when an agent works there, `○` when agents are idle.
#[component]
pub fn AgentDot(agents: Vec<AgentProc>) -> Element {
    if agents.is_empty() {
        return rsx! {};
    }
    let working = agents.iter().filter(|a| a.working).count();
    let tip = agents.iter().map(describe).collect::<Vec<_>>().join("\n");
    rsx! {
        span {
            class: if working > 0 { "agent-dot working" } else { "agent-dot" },
            title: "{tip}",
            if working > 0 { "●" } else { "○" }
            if agents.len() > 1 { " {agents.len()}" }
        }
    }
}

fn describe(a: &AgentProc) -> String {
    let state = if a.working { "working" } else { "idle" };
    if a.in_app { format!("{} (Workbench) · {state}", a.kind.label()) } else { format!("{} · {state} · pid {}", a.kind.label(), a.pid) }
}

/// One chip per agent, saying what it is and whether it's working.
#[component]
pub fn AgentChips(agents: Vec<AgentProc>) -> Element {
    rsx! {
        div { class: "chips",
            for a in agents {
                span {
                    key: "{a.pid}-{a.in_app}",
                    class: if a.working { "chip agent working" } else { "chip agent" },
                    title: if a.in_app { "Workbench's own session".to_string() } else { format!("Running outside Workbench, pid {}. Working means it or its commands are using CPU.", a.pid) },
                    LogoIcon { logo: a.kind.logo() }
                    "{a.kind.label()}"
                    if a.in_app { " (Workbench)" }
                    span { class: "muted", if a.working { " · working" } else { " · idle" } }
                }
            }
        }
    }
}

/// The worktrees of one project repository, with their agents, and a form to add one.
#[component]
pub fn WorktreeList(repo_path: String) -> Element {
    let st = use_context::<AppState>();
    let claude = use_coroutine_handle::<Cmd>();
    let mut branch = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);
    let mut working = use_signal(|| false);
    let list = st.worktrees.read().get(&repo_path).cloned();
    let activity = st.agent_activity();
    let cwd = st.cwd.read().clone();
    let repo_dir = PathBuf::from(&repo_path);
    let main_dir = list.as_ref().and_then(|l| l.iter().find(|w| w.main).map(|w| w.path.clone())).unwrap_or_else(|| repo_dir.clone());
    let draft = branch.read().clone();
    let target = draft.as_deref().filter(|b| !b.trim().is_empty()).map(|b| git::default_worktree_path(&main_dir, b));

    let create_repo = repo_dir.clone();
    let mut create = move || {
        let Some(b) = branch.peek().clone() else { return };
        let path = git::default_worktree_path(&main_dir, &b);
        let repo = create_repo.clone();
        working.set(true);
        error.set(None);
        spawn(async move {
            let r = tokio::task::spawn_blocking(move || git::add_worktree(&repo, &path, &b)).await.unwrap_or_else(|e| Err(e.to_string()));
            working.set(false);
            match r {
                Ok(()) => {
                    branch.set(None);
                    refresh_worktrees(st).await;
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };

    rsx! {
        div { class: "worktrees",
            match list {
                None => rsx! { div { class: "muted small-text", "Looking for worktrees…" } },
                Some(l) if l.is_empty() => rsx! { div { class: "muted small-text", "Not a git repository." } },
                Some(l) => rsx! {
                    for w in l {
                        {
                            let here = same_dir(&w.path, &cwd);
                            let agents = activity.get(&w.path).cloned().unwrap_or_default();
                            let dir = w.path.clone();
                            let path_text = w.path.display().to_string();
                            let class = if here { "worktree current" } else { "worktree" };
                            rsx! {
                                div { key: "{path_text}", class: "{class}",
                                    div { class: "worktree-head",
                                        span { class: "worktree-branch", "⑂ {w.label()}" }
                                        if w.main { span { class: "chip", title: "The repository's main checkout", "main" } }
                                        if w.locked { span { class: "chip", "locked" } }
                                        if w.prunable { span { class: "chip warn", title: "The folder is gone; `git worktree prune` removes the entry", "missing" } }
                                        match w.changes {
                                            Some(0) => rsx! { span { class: "muted small-text", "clean" } },
                                            Some(n) => rsx! { span { class: "status-modified small-text", "{n} changed" } },
                                            None => rsx! {},
                                        }
                                        div { class: "spacer" }
                                        if here {
                                            span { class: "chip", "open" }
                                        } else {
                                            button {
                                                class: "small",
                                                disabled: w.prunable,
                                                title: "Open this worktree's graph",
                                                onclick: move |_| open_folder(st, claude, dir.clone()),
                                                "Open"
                                            }
                                        }
                                    }
                                    code { class: "worktree-path", title: "{path_text}", "{path_text}" }
                                    if !agents.is_empty() {
                                        AgentChips { agents }
                                    }
                                }
                            }
                        }
                    }
                },
            }
            match draft {
                None => rsx! {
                    button { class: "small worktree-new", onclick: move |_| branch.set(Some(String::new())), "+ New worktree" }
                },
                Some(b) => rsx! {
                    div { class: "worktree-form",
                        input {
                            placeholder: "Branch (new or existing), Enter to create",
                            value: "{b}",
                            onmounted: crate::chat::focus,
                            oninput: move |e| branch.set(Some(e.value())),
                            onkeydown: {
                                let mut create = create.clone();
                                move |e: KeyboardEvent| match e.key() {
                                Key::Enter => create(),
                                Key::Escape => branch.set(None),
                                _ => {}
                                }
                            },
                        }
                        if let Some(t) = target {
                            div { class: "muted small-text", "Creates " code { "{t.display()}" } }
                        }
                        div { class: "inspector-actions",
                            button { class: "primary small", disabled: working() || b.trim().is_empty(), onclick: move |_| create(), if working() { "Creating…" } else { "Create" } }
                            button { class: "small", onclick: move |_| branch.set(None), "Cancel" }
                        }
                    }
                },
            }
            if let Some(e) = error.read().clone() {
                div { class: "branch-error", "{e}" }
            }
        }
    }
}
