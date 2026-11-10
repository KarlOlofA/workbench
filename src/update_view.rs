use std::path::PathBuf;

use dioxus::prelude::*;

use crate::state::AppState;
use crate::update::{self, Available};

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Idle,
    Building(String),
    Ready(PathBuf),
    Failed(String),
}

const SHOWN_COMMITS: usize = 6;

/// Header button that appears when GitHub's `main` has commits this build lacks.
/// Checks once on startup and opens itself to offer the update.
#[component]
pub fn UpdateMenu() -> Element {
    let st = use_context::<AppState>();
    let mut available = use_signal(|| None::<Available>);
    let mut open = use_signal(|| false);
    let mut phase = use_signal(|| Phase::Idle);

    use_hook(move || {
        spawn(async move {
            match tokio::task::spawn_blocking(update::check).await {
                Ok(Ok(Some(a))) => {
                    available.set(Some(a));
                    open.set(true);
                }
                Ok(Err(e)) => eprintln!("update check failed: {e}"),
                _ => {}
            }
        });
    });

    let Some(info) = available.read().clone() else {
        return rsx! {};
    };
    let current = phase.read().clone();
    let busy = *st.busy.read();
    let is_open = *open.read();
    let n = info.behind_by;
    let plural = if n == 1 { "" } else { "s" };
    let more = n.saturating_sub(info.commits.len().min(SHOWN_COMMITS) as u64);
    let label = match &current {
        Phase::Building(_) => "Updating…",
        Phase::Ready(_) => "Restart to update",
        Phase::Failed(_) => "Update failed",
        Phase::Idle => "Update available",
    };

    let start = move |_| {
        if matches!(*phase.peek(), Phase::Building(_)) {
            return;
        }
        phase.set(Phase::Building(String::new()));
        spawn(async move {
            let r = update::build_latest(move |line| phase.set(Phase::Building(line))).await;
            phase.set(match r {
                Ok(dir) => {
                    open.set(true);
                    Phase::Ready(dir)
                }
                Err(e) => {
                    open.set(true);
                    Phase::Failed(e)
                }
            });
        });
    };

    let restart = move |_| {
        if *st.busy.peek() {
            return;
        }
        let Phase::Ready(dir) = phase.peek().clone() else { return };
        let cwd = st.cwd.peek().clone();
        match update::install_and_relaunch(&dir, &cwd) {
            Ok(()) => std::process::exit(0),
            Err(e) => phase.set(Phase::Failed(e)),
        }
    };

    let log = update::log_path().display().to_string();

    rsx! {
        div { class: "history update-menu",
            button {
                class: if is_open { "active update-button" } else { "update-button" },
                title: "A newer Workbench build is on GitHub {update::BRANCH}",
                onclick: move |_| {
                    let o = *open.peek();
                    open.set(!o);
                },
                "⬆ {label}"
            }
            if is_open {
                div { class: "history-backdrop", onclick: move |_| open.set(false) }
                div { class: "history-pop update-pop",
                    div { class: "update-body",
                        div { class: "update-title", "Workbench update available" }
                        div { class: "muted small-text",
                            "{update::BRANCH} on {update::REPO} has {n} new commit{plural} since this build ({update::short(update::BUILD_COMMIT)})."
                        }
                        ul { class: "update-commits",
                            for c in info.commits.iter().take(SHOWN_COMMITS) {
                                li { key: "{c.sha}",
                                    code { "{update::short(&c.sha)}" }
                                    span { "{c.message}" }
                                }
                            }
                            if more > 0 {
                                li { class: "muted", "…and {more} more" }
                            }
                        }
                        match &current {
                            Phase::Idle => rsx! {
                                div { class: "muted small-text",
                                    "Downloads {update::BRANCH} and builds it in the background. You can keep working while it builds."
                                }
                                div { class: "update-actions",
                                    button { class: "primary", onclick: start, "Update" }
                                    button { onclick: move |_| open.set(false), "Later" }
                                }
                            },
                            Phase::Building(line) => rsx! {
                                div { class: "update-progress", "Building… this can take a few minutes." }
                                div { class: "update-line muted small-text", title: "{line}", "{line}" }
                            },
                            Phase::Ready(_) => rsx! {
                                div { class: "update-progress", "Build finished. Restart to install it." }
                                div { class: "update-actions",
                                    button {
                                        class: "primary",
                                        disabled: busy,
                                        title: if busy { "Stop Claude before restarting" } else { "Install the new build and reopen this folder" },
                                        onclick: restart,
                                        "Restart now"
                                    }
                                    button { onclick: move |_| open.set(false), "Later" }
                                }
                                if busy {
                                    div { class: "muted small-text", "Stop Claude before restarting." }
                                }
                            },
                            Phase::Failed(e) => rsx! {
                                div { class: "branch-error", "{e}" }
                                div { class: "muted small-text", "Full log: {log}" }
                                div { class: "update-actions",
                                    button { onclick: start, "Retry" }
                                    button { onclick: move |_| open.set(false), "Close" }
                                }
                            },
                        }
                    }
                }
            }
        }
    }
}
