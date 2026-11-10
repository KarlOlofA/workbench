use std::path::Path;

use dioxus::prelude::*;

use crate::code::FileDiffView;
use crate::git;
use crate::github::{self, MergeMethod, NewPr, PrState, ReviewKind};
use crate::markdown;
use crate::prompts;
use crate::state::{AppState, Cmd};

const MAX_FILE_CHARS: usize = 200_000;
const MAX_CHAT_CHARS: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GhTab {
    Prs,
    NewPr,
    Related,
}

#[derive(Debug, Clone, PartialEq)]
enum Status {
    Idle,
    Busy(String),
    Done(String),
    Failed(String),
}

#[derive(Clone, Copy)]
struct GhCtx {
    tab: Signal<GhTab>,
    repo: Signal<Option<String>>,
    open_pr: Signal<Option<u64>>,
    reload: Signal<u64>,
}

fn run_action(
    mut status: Signal<Status>,
    mut ctx: GhCtx,
    cwd: std::path::PathBuf,
    label: &str,
    f: impl FnOnce(&Path) -> Result<String, String> + Send + 'static,
) {
    if matches!(*status.peek(), Status::Busy(_)) {
        return;
    }
    status.set(Status::Busy(label.to_string()));
    spawn(async move {
        let result = tokio::task::spawn_blocking(move || f(&cwd)).await.unwrap_or_else(|e| Err(e.to_string()));
        match result {
            Ok(msg) => {
                status.set(Status::Done(msg));
                *ctx.reload.write() += 1;
            }
            Err(e) => status.set(Status::Failed(e)),
        }
    });
}

fn status_line(status: &Status) -> Element {
    match status {
        Status::Idle => rsx! {},
        Status::Busy(label) => rsx! { div { class: "gh-status muted", "{label}…" } },
        Status::Done(msg) => rsx! { div { class: "gh-status ok", "{msg}" } },
        Status::Failed(e) => rsx! { div { class: "gh-status error", "{e}" } },
    }
}

fn open_url(url: &str) {
    if url.starts_with("https://") {
        crate::platform::open_url(url);
    }
}

fn truncate(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

fn state_class(state: &str, draft: bool) -> &'static str {
    if draft {
        return "draft";
    }
    match state.to_ascii_uppercase().as_str() {
        "OPEN" => "open",
        "MERGED" => "merged",
        _ => "closed",
    }
}

fn label_of(v: &Option<String>) -> String {
    v.as_deref().filter(|s| !s.is_empty()).map(|s| s.replace('_', " ").to_lowercase()).unwrap_or_default()
}

#[component]
pub fn GitHubView() -> Element {
    let st = use_context::<AppState>();
    let mut ctx = use_context_provider(|| GhCtx {
        tab: Signal::new(GhTab::Prs),
        repo: Signal::new(None),
        open_pr: Signal::new(None),
        reload: Signal::new(0),
    });
    let current = use_resource(move || async move {
        let cwd = st.cwd.read().clone();
        tokio::task::spawn_blocking(move || github::current_repo(&cwd)).await.unwrap_or_else(|e| Err(e.to_string()))
    });

    let tab = *ctx.tab.read();
    let repo = ctx.repo.read().clone();
    let related = st.related_repos.read().clone();
    let current_name = match current.read().clone() {
        Some(Ok(r)) => format!("This repo ({})", r.name_with_owner),
        Some(Err(_)) => "This repo (not on GitHub?)".into(),
        None => "This repo".into(),
    };
    let tab_btn = move |t: GhTab, label: &'static str| {
        rsx! {
            button {
                class: if tab == t { "subtab active" } else { "subtab" },
                onclick: move |_| ctx.tab.set(t),
                "{label}"
            }
        }
    };

    rsx! {
        div { class: "gh",
            div { class: "diff-bar gh-bar",
                select {
                    title: "Repository to read pull requests from",
                    onchange: move |e: FormEvent| {
                        let v = e.value();
                        ctx.repo.set(if v.is_empty() { None } else { Some(v) });
                        ctx.open_pr.set(None);
                    },
                    option { value: "", selected: repo.is_none(), "{current_name}" }
                    for r in related {
                        option { key: "{r}", value: "{r}", selected: repo.as_deref() == Some(r.as_str()), "{r}" }
                    }
                }
                div { class: "subtabs gh-tabs",
                    {tab_btn(GhTab::Prs, "Pull requests")}
                    {tab_btn(GhTab::NewPr, "New PR")}
                    {tab_btn(GhTab::Related, "Related repos")}
                }
                div { class: "spacer" }
                button { class: "small", title: "Reload from GitHub", onclick: move |_| *ctx.reload.write() += 1, "↻" }
            }
            match tab {
                GhTab::Prs => rsx! { PrsTab {} },
                GhTab::NewPr => rsx! { NewPrTab {} },
                GhTab::Related => rsx! { RelatedTab {} },
            }
        }
    }
}

#[component]
fn PrsTab() -> Element {
    let ctx = use_context::<GhCtx>();
    match *ctx.open_pr.read() {
        Some(n) => rsx! { PrDetailView { key: "{n}", number: n } },
        None => rsx! { PrList {} },
    }
}

#[component]
fn PrList() -> Element {
    let st = use_context::<AppState>();
    let mut ctx = use_context::<GhCtx>();
    let mut filter = use_signal(|| PrState::Open);
    let prs = use_resource(move || async move {
        let _ = ctx.reload.read();
        let cwd = st.cwd.read().clone();
        let repo = ctx.repo.read().clone();
        let state = *filter.read();
        tokio::task::spawn_blocking(move || github::list_prs(&cwd, repo.as_deref(), state))
            .await
            .unwrap_or_else(|e| Err(e.to_string()))
    });
    let current = *filter.read();

    let body = match prs.read().clone() {
        None => rsx! { div { class: "muted pad", "Loading pull requests…" } },
        Some(Err(e)) => rsx! {
            div { class: "error pad", "{e}" }
            div { class: "muted pad small-text", "Workbench talks to GitHub through the `gh` CLI. Install it and run `gh auth login` in a terminal." }
        },
        Some(Ok(list)) if list.is_empty() => rsx! { div { class: "muted pad", "No {current.arg()} pull requests." } },
        Some(Ok(list)) => rsx! {
            ul { class: "gh-list",
                for pr in list {
                    {
                        let n = pr.number;
                        let cls = state_class(&pr.state, pr.is_draft);
                        let review = label_of(&pr.review_decision);
                        rsx! {
                            li { key: "{n}", onclick: move |_| ctx.open_pr.set(Some(n)),
                                div { class: "gh-row-head",
                                    span { class: "gh-state {cls}", if pr.is_draft { "draft" } else { "{pr.state.to_lowercase()}" } }
                                    span { class: "gh-num", "#{n}" }
                                    span { class: "gh-title", "{pr.title}" }
                                }
                                div { class: "muted small-text",
                                    "{pr.author.login} · {pr.head_ref_name} → {pr.base_ref_name}"
                                    if !review.is_empty() { " · {review}" }
                                }
                            }
                        }
                    }
                }
            }
        },
    };

    rsx! {
        div { class: "gh-body",
            div { class: "inspector-actions",
                select {
                    onchange: move |e: FormEvent| filter.set(PrState::from_arg(&e.value())),
                    for s in PrState::ALL {
                        option { value: "{s.arg()}", selected: s == current, "{s.arg()}" }
                    }
                }
            }
            {body}
        }
    }
}

#[component]
fn PrDetailView(number: u64) -> Element {
    let mut st = use_context::<AppState>();
    let mut ctx = use_context::<GhCtx>();
    let claude = use_coroutine_handle::<Cmd>();
    let mut show_diff = use_signal(|| false);
    let mut draft = use_signal(String::new);
    let mut method = use_signal(|| MergeMethod::Squash);
    let mut delete_branch = use_signal(|| true);
    let mut confirm_merge = use_signal(|| false);
    let status = use_signal(|| Status::Idle);

    let detail = use_resource(move || async move {
        let _ = ctx.reload.read();
        let cwd = st.cwd.read().clone();
        let repo = ctx.repo.read().clone();
        tokio::task::spawn_blocking(move || github::view_pr(&cwd, repo.as_deref(), number))
            .await
            .unwrap_or_else(|e| Err(e.to_string()))
    });
    let diff = use_resource(move || async move {
        if !*show_diff.read() {
            return None;
        }
        let cwd = st.cwd.read().clone();
        let repo = ctx.repo.read().clone();
        Some(
            tokio::task::spawn_blocking(move || github::pr_diff(&cwd, repo.as_deref(), number))
                .await
                .unwrap_or_else(|e| Err(e.to_string())),
        )
    });

    let back = rsx! {
        button { class: "small", onclick: move |_| ctx.open_pr.set(None), "← All pull requests" }
    };
    let pr = match detail.read().clone() {
        None => return rsx! { div { class: "gh-body", {back} div { class: "muted pad", "Loading #{number}…" } } },
        Some(Err(e)) => return rsx! { div { class: "gh-body", {back} div { class: "error pad", "{e}" } } },
        Some(Ok(pr)) => pr,
    };

    let busy = matches!(*status.read(), Status::Busy(_));
    let cls = state_class(&pr.state, pr.is_draft);
    let review = label_of(&pr.review_decision);
    let checks = pr.checks();
    let body_html = markdown::to_html(if pr.body.trim().is_empty() { "_No description._" } else { &pr.body });
    let url = pr.url.clone();
    let title = pr.title.clone();
    let open = pr.is_open();
    let empty_draft = draft.read().trim().is_empty();
    let mergeable = pr.mergeable.to_lowercase();
    let merge_state = pr.merge_state_status.replace('_', " ").to_lowercase();
    let cur_method = *method.read();
    let reviews: Vec<(usize, github::Review, String)> = pr
        .reviews
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, r)| {
            let verdict = r.state.replace('_', " ").to_lowercase();
            (i, r, verdict)
        })
        .collect();

    let review_action = move |kind: ReviewKind| {
        let body = draft.peek().clone();
        let repo = ctx.repo.peek().clone();
        let cwd = st.cwd.peek().clone();
        let label = match kind {
            ReviewKind::Comment => "Posting comment",
            ReviewKind::Approve => "Approving",
            ReviewKind::RequestChanges => "Requesting changes",
        };
        let mut draft = draft;
        run_action(status, ctx, cwd, label, move |cwd| {
            match kind {
                ReviewKind::Comment => github::comment(cwd, repo.as_deref(), number, &body)?,
                _ => github::review(cwd, repo.as_deref(), number, kind, &body)?,
            }
            Ok("Posted to GitHub.".into())
        });
        draft.set(String::new());
    };

    let merge = move |_| {
        if !*confirm_merge.peek() {
            confirm_merge.set(true);
            return;
        }
        confirm_merge.set(false);
        let repo = ctx.repo.peek().clone();
        let cwd = st.cwd.peek().clone();
        let m = *method.peek();
        let del = *delete_branch.peek();
        run_action(status, ctx, cwd, "Merging", move |cwd| {
            let out = github::merge(cwd, repo.as_deref(), number, m, del)?;
            Ok(if out.is_empty() { format!("Merged #{number}.") } else { out })
        });
        if ctx.repo.peek().is_none() {
            st.bump_repo();
        }
    };

    let ask_claude = move |_| {
        let repo = ctx.repo.peek().clone();
        let text = prompts::pr_review(repo.as_deref(), number, &title);
        claude.send(Cmd::Prompt { label: format!("Review PR #{number}"), text });
    };

    let diff_view = match diff.read().clone() {
        None | Some(None) => rsx! {},
        Some(Some(Err(e))) => rsx! { div { class: "error pad", "{e}" } },
        Some(Some(Ok(files))) => rsx! {
            for f in files {
                FileDiffView { key: "{f.path}", file: f, local: false }
            }
        },
    };

    rsx! {
        div { class: "gh-body",
            div { class: "inspector-actions",
                {back}
                div { class: "spacer" }
                button { class: "small", onclick: move |_| open_url(&url), "Open on GitHub" }
            }
            div { class: "gh-pr-head",
                span { class: "gh-state {cls}", if pr.is_draft { "draft" } else { "{pr.state.to_lowercase()}" } }
                h3 { "#{pr.number} {pr.title}" }
            }
            div { class: "muted small-text",
                "{pr.author.login} wants to merge {pr.head_ref_name} into {pr.base_ref_name} · "
                span { class: "add", "+{pr.additions}" }
                " "
                span { class: "del", "−{pr.deletions}" }
                " in {pr.changed_files} files"
                if !review.is_empty() { " · {review}" }
            }
            if !checks.is_empty() {
                div { class: "chips",
                    for (i, c) in checks.iter().enumerate() {
                        span { key: "{i}", class: "chip check-{c.outcome.to_lowercase()}", title: "{c.outcome}", "{c.name}" }
                    }
                }
            }
            div { class: "md gh-md", dangerous_inner_html: "{body_html}" }

            if !pr.reviews.is_empty() || !pr.comments.is_empty() {
                h4 { class: "gh-section", "Conversation" }
            }
            for (i, r, verdict) in reviews {
                div { key: "r{i}", class: "gh-comment",
                    div { class: "muted small-text",
                        b { "{r.author.login}" }
                        " · {verdict}"
                    }
                    if !r.body.trim().is_empty() {
                        div { class: "md", dangerous_inner_html: markdown::to_html(&r.body) }
                    }
                }
            }
            for (i, c) in pr.comments.iter().enumerate() {
                div { key: "c{i}", class: "gh-comment",
                    div { class: "muted small-text",
                        b { "{c.author.login}" }
                        " · {c.created_at}"
                    }
                    div { class: "md", dangerous_inner_html: markdown::to_html(&c.body) }
                }
            }

            h4 { class: "gh-section", "Review" }
            div { class: "inspector-actions",
                button {
                    disabled: *st.busy.read(),
                    title: "Let Claude read the PR and its diff and report back in the chat. Nothing is posted.",
                    onclick: ask_claude,
                    "Ask Claude to review"
                }
                button {
                    class: if *show_diff.read() { "toggle on" } else { "toggle" },
                    onclick: move |_| {
                        let v = !*show_diff.peek();
                        show_diff.set(v);
                    },
                    if *show_diff.read() { "Hide diff" } else { "Show diff" }
                }
            }
            if open {
                textarea {
                    class: "note",
                    placeholder: "Write a comment or review (markdown)…",
                    value: "{draft}",
                    oninput: move |e| draft.set(e.value()),
                }
                div { class: "inspector-actions",
                    button { disabled: busy || empty_draft, onclick: move |_| review_action(ReviewKind::Comment), "Comment" }
                    button { disabled: busy, onclick: move |_| review_action(ReviewKind::Approve), "Approve" }
                    button { disabled: busy || empty_draft, onclick: move |_| review_action(ReviewKind::RequestChanges), "Request changes" }
                }
                h4 { class: "gh-section", "Merge" }
                div { class: "muted small-text", "Mergeable: {mergeable} · state: {merge_state}" }
                div { class: "inspector-actions",
                    select {
                        disabled: busy,
                        onchange: move |e: FormEvent| {
                            method.set(MergeMethod::from_key(&e.value()));
                            confirm_merge.set(false);
                        },
                        for m in MergeMethod::ALL {
                            option { value: "{m.key()}", selected: m == cur_method, "{m.key()}" }
                        }
                    }
                    label { class: "check",
                        input {
                            r#type: "checkbox",
                            checked: *delete_branch.read(),
                            onchange: move |e: FormEvent| delete_branch.set(e.checked()),
                        }
                        "Delete branch"
                    }
                    div { class: "spacer" }
                    if *confirm_merge.read() {
                        button { class: "small", onclick: move |_| confirm_merge.set(false), "Cancel" }
                    }
                    button {
                        class: if *confirm_merge.read() { "danger" } else { "primary" },
                        disabled: busy || pr.is_draft,
                        title: if pr.is_draft { "Draft pull requests can't be merged" } else { "Merge on GitHub" },
                        onclick: merge,
                        if *confirm_merge.read() { "Confirm {cur_method.key()} merge of #{number}" } else { "Merge…" }
                    }
                }
            }
            {status_line(&status.read())}
            {diff_view}
        }
    }
}

#[component]
fn NewPrTab() -> Element {
    let mut st = use_context::<AppState>();
    let ctx = use_context::<GhCtx>();
    let claude = use_coroutine_handle::<Cmd>();
    let mut title = use_signal(String::new);
    let mut body = use_signal(String::new);
    let mut base = use_signal(String::new);
    let mut draft = use_signal(|| false);
    let mut push = use_signal(|| true);
    let status = use_signal(|| Status::Idle);

    let info = use_resource(move || async move {
        let _ = st.repo_version.read();
        let cwd = st.cwd.read().clone();
        tokio::task::spawn_blocking(move || (git::current_branch(&cwd), github::current_repo(&cwd)))
            .await
            .ok()
    });

    if ctx.repo.read().is_some() {
        return rsx! {
            div { class: "muted pad", "Pull requests are opened from this project's repository. Switch the repository selector back to “This repo”." }
        };
    }
    let Some(Some((branch, repo))) = info.read().clone() else {
        return rsx! { div { class: "muted pad", "Loading…" } };
    };
    let branch = branch.unwrap_or_default();
    let default_base = repo.as_ref().map(|r| r.default_branch()).unwrap_or_else(|_| "main".into());
    let repo_error = repo.err();
    let busy = matches!(*status.read(), Status::Busy(_));
    let on_base = !branch.is_empty() && (branch == default_base || branch == "HEAD");
    let empty_title = title.read().trim().is_empty();
    let base_shown = if base.read().is_empty() { default_base.clone() } else { base.read().clone() };

    let create_base = base_shown.clone();
    let create = move |_| {
        let pr = NewPr {
            title: title.peek().clone(),
            body: body.peek().clone(),
            base: create_base.clone(),
            head: String::new(),
            draft: *draft.peek(),
        };
        let push_first = *push.peek();
        let cwd = st.cwd.peek().clone();
        run_action(status, ctx, cwd, "Creating pull request", move |cwd| {
            if push_first {
                git::push_current_branch(cwd)?;
            }
            let url = github::create_pr(cwd, &pr)?;
            Ok(format!("Opened {url}"))
        });
        st.bump_repo();
    };

    let draft_base = base_shown.clone();
    let draft_branch = branch.clone();
    let ask_claude = move |_| {
        let text = format!(
            "Write a pull request title and description for the current branch `{draft_branch}` against `{draft_base}`. \
             Read `git log {draft_base}..HEAD` and `git diff {draft_base}...HEAD`, and use the architecture graph in `.workbench/graph.json` to explain which parts of the system change. \
             Reply with the title on the first line, then the description in markdown (summary, notable changes, how to test). Do not change any files and do not run gh."
        );
        claude.send(Cmd::Prompt { label: "Draft a PR description".into(), text });
    };

    rsx! {
        div { class: "gh-body inspector",
            if let Some(e) = repo_error {
                div { class: "error small-text", "{e}" }
            }
            div { class: "muted small-text", "From " code { "{branch}" } " into " code { "{base_shown}" } }
            if on_base {
                div { class: "hint-box", "You are on the base branch. Create and commit to a feature branch before opening a pull request." }
            }
            label { "Base branch" }
            input {
                value: "{base}",
                placeholder: "{default_base}",
                oninput: move |e| base.set(e.value()),
            }
            label { "Title" }
            input { value: "{title}", oninput: move |e| title.set(e.value()) }
            label { "Description" }
            textarea {
                class: "desc",
                placeholder: "What changed and why (markdown)",
                value: "{body}",
                oninput: move |e| body.set(e.value()),
            }
            div { class: "inspector-actions",
                label { class: "check",
                    input { r#type: "checkbox", checked: *push.read(), onchange: move |e: FormEvent| push.set(e.checked()) }
                    "Push branch first"
                }
                label { class: "check",
                    input { r#type: "checkbox", checked: *draft.read(), onchange: move |e: FormEvent| draft.set(e.checked()) }
                    "Draft"
                }
                div { class: "spacer" }
                button {
                    disabled: *st.busy.read() || branch.is_empty(),
                    title: "Claude writes a title and description from the branch's commits; copy them from the chat",
                    onclick: ask_claude,
                    "Draft with Claude"
                }
                button { class: "primary", disabled: busy || empty_title || on_base, onclick: create, "Create pull request" }
            }
            {status_line(&status.read())}
        }
    }
}

#[component]
fn RelatedTab() -> Element {
    let mut st = use_context::<AppState>();
    let mut ctx = use_context::<GhCtx>();
    let mut input = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut browsing = use_signal(|| None::<String>);
    let related = st.related_repos.read().clone();
    let current = browsing.read().clone();

    let mut add = move || {
        let parsed = github::normalize_repo(&input.peek());
        match parsed {
            Some(r) => {
                if !st.related_repos.peek().contains(&r) {
                    let name = r.clone();
                    st.update_related(|list| list.push(name));
                }
                input.set(String::new());
                error.set(None);
                browsing.set(Some(r));
            }
            None => error.set(Some("Use owner/repo or a github.com URL".into())),
        }
    };

    rsx! {
        div { class: "gh-body",
            div { class: "muted small-text",
                "Related repositories give Claude context from other projects. They are saved in .workbench/github.json and listed in Claude's system prompt, so it can read them with gh."
            }
            div { class: "inspector-actions",
                input {
                    class: "gh-repo-input",
                    placeholder: "owner/repo or https://github.com/owner/repo",
                    value: "{input}",
                    oninput: move |e| input.set(e.value()),
                    onkeydown: move |e: KeyboardEvent| {
                        if e.key() == Key::Enter {
                            add();
                        }
                    },
                }
                button { onclick: move |_| add(), "Add" }
            }
            if let Some(e) = error.read().clone() {
                div { class: "error small-text", "{e}" }
            }
            if related.is_empty() {
                div { class: "muted pad", "No related repositories yet." }
            }
            ul { class: "gh-list",
                for r in related {
                    {
                        let is_open = current.as_deref() == Some(r.as_str());
                        let r_browse = r.clone();
                        let r_prs = r.clone();
                        let r_del = r.clone();
                        rsx! {
                            li { key: "{r}", class: if is_open { "active" } else { "" },
                                div { class: "gh-row-head",
                                    span { class: "gh-title", "{r}" }
                                    div { class: "spacer" }
                                    button {
                                        class: "small",
                                        onclick: move |e: MouseEvent| {
                                            e.stop_propagation();
                                            browsing.set(if is_open { None } else { Some(r_browse.clone()) });
                                        },
                                        if is_open { "Close" } else { "Browse" }
                                    }
                                    button {
                                        class: "small",
                                        onclick: move |e: MouseEvent| {
                                            e.stop_propagation();
                                            ctx.repo.set(Some(r_prs.clone()));
                                            ctx.open_pr.set(None);
                                            ctx.tab.set(GhTab::Prs);
                                        },
                                        "PRs"
                                    }
                                    button {
                                        class: "small",
                                        title: "Remove from related repositories",
                                        onclick: move |e: MouseEvent| {
                                            e.stop_propagation();
                                            let r = r_del.clone();
                                            if browsing.peek().as_deref() == Some(r.as_str()) {
                                                browsing.set(None);
                                            }
                                            if ctx.repo.peek().as_deref() == Some(r.as_str()) {
                                                ctx.repo.set(None);
                                            }
                                            st.update_related(|list| list.retain(|x| *x != r));
                                        },
                                        "✕"
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if let Some(r) = current {
                RepoBrowser { key: "{r}", repo: r }
            }
        }
    }
}

#[component]
fn RepoBrowser(repo: String) -> Element {
    let mut st = use_context::<AppState>();
    let ctx = use_context::<GhCtx>();
    let mut path = use_signal(String::new);
    let mut file = use_signal(|| None::<String>);
    let repo_info = repo.clone();
    let repo_list = repo.clone();
    let repo_file = repo.clone();

    let info = use_resource(move || {
        let repo = repo_info.clone();
        async move {
            let cwd = st.cwd.read().clone();
            tokio::task::spawn_blocking(move || github::view_repo(&cwd, &repo)).await.unwrap_or_else(|e| Err(e.to_string()))
        }
    });
    let listing = use_resource(move || {
        let repo = repo_list.clone();
        async move {
            let _ = ctx.reload.read();
            let cwd = st.cwd.read().clone();
            let p = path.read().clone();
            tokio::task::spawn_blocking(move || github::list_contents(&cwd, &repo, &p))
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
        }
    });
    let content = use_resource(move || {
        let repo = repo_file.clone();
        async move {
            let f = file.read().clone()?;
            let cwd = st.cwd.read().clone();
            Some(
                tokio::task::spawn_blocking(move || github::read_file(&cwd, &repo, &f))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string())),
            )
        }
    });

    let cur_path = path.read().clone();
    let segments: Vec<(String, String)> = {
        let mut acc = String::new();
        cur_path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(s);
                (s.to_string(), acc.clone())
            })
            .collect()
    };
    let description = match info.read().clone() {
        Some(Ok(r)) => r.description.unwrap_or_default(),
        Some(Err(e)) => e,
        None => String::new(),
    };
    let open_file = file.read().clone();

    let file_view = match (open_file.clone(), content.read().clone()) {
        (Some(f), Some(Some(Ok(text)))) => {
            let (shown, cut) = truncate(&text, MAX_FILE_CHARS);
            let (for_chat, chat_cut) = truncate(&text, MAX_CHAT_CHARS);
            let note = if chat_cut { " (truncated)" } else { "" };
            let chat_text = format!("From `{repo}` — `{f}`{note}:\n```\n{for_chat}\n```\n");
            rsx! {
                div { class: "gh-file-bar",
                    code { "{f}" }
                    div { class: "spacer" }
                    button { class: "small", onclick: move |_| st.append_to_input(&chat_text), "Add to chat" }
                    button { class: "small", onclick: move |_| file.set(None), "✕" }
                }
                if cut {
                    div { class: "muted small-text", "Showing the first {MAX_FILE_CHARS} characters." }
                }
                pre { class: "gh-file", "{shown}" }
            }
        }
        (Some(_), Some(Some(Err(e)))) => rsx! { div { class: "error pad", "{e}" } },
        (Some(_), _) => rsx! { div { class: "muted pad", "Loading file…" } },
        (None, _) => rsx! {},
    };

    let entries = match listing.read().clone() {
        None => rsx! { div { class: "muted pad", "Loading…" } },
        Some(Err(e)) => rsx! { div { class: "error pad", "{e}" } },
        Some(Ok(list)) => rsx! {
            ul { class: "file-list",
                for entry in list {
                    {
                        let is_dir = entry.is_dir();
                        let p = entry.path.clone();
                        rsx! {
                            li {
                                key: "{entry.path}",
                                onclick: move |_| {
                                    if is_dir {
                                        path.set(p.clone());
                                        file.set(None);
                                    } else {
                                        file.set(Some(p.clone()));
                                    }
                                },
                                span { class: "file-relation", if is_dir { "dir" } else { "" } }
                                span { "{entry.name}" }
                            }
                        }
                    }
                }
            }
        },
    };

    rsx! {
        div { class: "gh-browser",
            if !description.is_empty() {
                div { class: "muted small-text", "{description}" }
            }
            div { class: "crumbs",
                button { class: "crumb", onclick: move |_| { path.set(String::new()); file.set(None); }, "{repo}" }
                for (name, full) in segments {
                    span { key: "{full}", class: "crumb-item",
                        span { class: "crumb-sep", "/" }
                        button {
                            class: "crumb",
                            onclick: move |_| {
                                path.set(full.clone());
                                file.set(None);
                            },
                            "{name}"
                        }
                    }
                }
            }
            {file_view}
            if open_file.is_none() {
                {entries}
            }
        }
    }
}
