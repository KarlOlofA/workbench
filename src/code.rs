use dioxus::prelude::*;

use crate::git::{self, FileDiff, LineKind};
use crate::github_view::GitHubView;
use crate::highlight;
use crate::editor::EditorView;
use crate::state::{AppState, Tab};

#[component]
pub fn CodePane() -> Element {
    let mut st = use_context::<AppState>();
    let tab = *st.tab.read();
    let dirty = st.open_file.read().as_ref().is_some_and(|f| f.dirty());

    let tab_btn = move |t: Tab, label: &'static str| {
        rsx! {
            button {
                class: if tab == t { "tab active" } else { "tab" },
                onclick: move |_| st.tab.set(t),
                "{label}"
            }
        }
    };

    rsx! {
        div { class: "code",
            div { class: "tabs",
                {tab_btn(Tab::Files, "Files")}
                {tab_btn(Tab::Editor, if dirty { "Editor ●" } else { "Editor" })}
                {tab_btn(Tab::Diff, "Changes")}
                {tab_btn(Tab::GitHub, "GitHub")}
            }
            div { class: "tab-body",
                match tab {
                    Tab::Files => rsx! { FilesView {} },
                    Tab::Diff => rsx! { DiffView {} },
                    Tab::GitHub => rsx! { GitHubView {} },
                    Tab::Editor => rsx! {},
                }
                EditorView { visible: tab == Tab::Editor }
            }
        }
    }
}

#[component]
fn FilesView() -> Element {
    let mut st = use_context::<AppState>();
    let mut filter = use_signal(String::new);

    let data = use_resource(move || async move {
        let _ = st.repo_version.read();
        let cwd = st.cwd.read().clone();
        tokio::task::spawn_blocking(move || {
            let status = git::status(&cwd);
            let files = git::ls_files(&cwd);
            (status, files)
        })
        .await
        .ok()
    });

    let Some(Some((status, files))) = data.read().clone() else {
        return rsx! { div { class: "muted pad", "Loading…" } };
    };

    let needle = filter.read().to_lowercase();
    let visible: Vec<String> = files
        .unwrap_or_default()
        .into_iter()
        .filter(|f| needle.is_empty() || f.to_lowercase().contains(&needle))
        .take(400)
        .collect();

    rsx! {
        div { class: "files",
            h3 { "Changed" }
            match status {
                Err(e) => rsx! { div { class: "muted", "Not a git repository: {e}" } },
                Ok(entries) if entries.is_empty() => rsx! { div { class: "muted", "Working tree clean" } },
                Ok(entries) => rsx! {
                    ul { class: "file-list",
                        for entry in entries {
                            li {
                                key: "{entry.path}",
                                onclick: {
                                    let p = entry.path.clone();
                                    move |_| st.open(&p)
                                },
                                span { class: "status status-{status_class(&entry.code)}", "{entry.code.trim()}" }
                                span { "{entry.path}" }
                            }
                        }
                    }
                },
            }
            h3 { "All files" }
            input {
                class: "filter",
                placeholder: "Filter files…",
                value: "{filter}",
                oninput: move |e| filter.set(e.value()),
            }
            ul { class: "file-list",
                for f in visible {
                    li {
                        key: "{f}",
                        onclick: {
                            let p = f.clone();
                            move |_| st.open(&p)
                        },
                        "{f}"
                    }
                }
            }
        }
    }
}

fn status_class(code: &str) -> &'static str {
    match code.trim() {
        "??" | "A" | "AM" => "added",
        "D" | "AD" => "deleted",
        _ => "modified",
    }
}

#[component]
fn DiffView() -> Element {
    let st = use_context::<AppState>();
    let mut base = use_signal(|| "HEAD".to_string());
    let mut applied_base = use_signal(|| "HEAD".to_string());

    let data = use_resource(move || async move {
        let _ = st.repo_version.read();
        let cwd = st.cwd.read().clone();
        let base = applied_base.read().clone();
        tokio::task::spawn_blocking(move || (git::current_branch(&cwd), git::diff(&cwd, &base)))
            .await
            .ok()
    });

    let (branch, result) = match data.read().clone() {
        Some(Some(v)) => v,
        _ => return rsx! { div { class: "muted pad", "Loading…" } },
    };

    let branch = branch.unwrap_or_default();
    let header = rsx! {
        div { class: "diff-bar",
            span { class: "muted", "Branch " }
            code { "{branch}" }
            span { class: "muted", " compared to " }
            input {
                class: "base",
                value: "{base}",
                placeholder: "HEAD, main, origin/main…",
                oninput: move |e| base.set(e.value()),
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Enter {
                        applied_base.set(base.peek().clone());
                    }
                },
            }
            button { onclick: move |_| applied_base.set(base.peek().clone()), "Compare" }
        }
    };

    let files = match result {
        Err(e) => return rsx! { {header} div { class: "error pad", "{e}" } },
        Ok(f) => f,
    };

    let (added, removed) = files.iter().fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
    let file_count = files.len();

    rsx! {
        div { class: "diff",
            {header}
            div { class: "diff-summary",
                "{file_count} files changed "
                span { class: "add", "+{added}" }
                " "
                span { class: "del", "−{removed}" }
            }
            if files.is_empty() {
                div { class: "muted pad", "No changes." }
            }
            for f in files {
                FileDiffView { key: "{f.path}", file: f }
            }
        }
    }
}

#[component]
pub fn FileDiffView(file: FileDiff, #[props(default = true)] local: bool) -> Element {
    let mut st = use_context::<AppState>();
    let path = file.path.clone();

    rsx! {
        details { class: "file-diff", open: true,
            summary {
                span { class: "path", "{file.path}" }
                span { class: "add", " +{file.added}" }
                span { class: "del", " −{file.removed}" }
                div { class: "spacer" }
                if local {
                    button {
                        onclick: {
                            let p = path.clone();
                            move |e: MouseEvent| {
                                e.prevent_default();
                                e.stop_propagation();
                                st.open(&p);
                            }
                        },
                        "Open"
                    }
                }
            }
            if file.binary {
                div { class: "muted pad", "Binary file" }
            }
            for (hi, hunk) in file.hunks.iter().enumerate() {
                div { key: "{hi}", class: "hunk",
                    div { class: "hunk-header",
                        code { "{hunk.header}" }
                        button {
                            onclick: {
                                let patch = hunk.to_patch();
                                let p = path.clone();
                                move |_| st.append_to_input(&format!("In `{p}`:\n```diff\n{patch}\n```\n"))
                            },
                            "Ask Workbench"
                        }
                    }
                    table { class: "hunk-lines",
                        tbody {
                            for (li, line) in hunk.lines.iter().enumerate() {
                                tr {
                                    key: "{li}",
                                    class: match line.kind {
                                        LineKind::Added => "added",
                                        LineKind::Removed => "removed",
                                        LineKind::Context => "context",
                                        LineKind::Meta => "meta",
                                    },
                                    td { class: "ln", {line.old_no.map(|n| n.to_string()).unwrap_or_default()} }
                                    td { class: "ln", {line.new_no.map(|n| n.to_string()).unwrap_or_default()} }
                                    td { class: "sign",
                                        match line.kind {
                                            LineKind::Added => "+",
                                            LineKind::Removed => "−",
                                            _ => " ",
                                        }
                                    }
                                    td {
                                        class: "src",
                                        dangerous_inner_html: highlight::highlight_line_html(&path, &line.text),
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
