//! Intent-conflict review: shown instead of the graph while `graph.json` has a merge conflict.

use dioxus::prelude::*;

use crate::git;
use crate::graph::{self, BASELINE_FILE, GRAPH_FILE, Graph, Node};
use crate::intent_merge::{self, Conflict, Merge, Side};
use crate::state::AppState;

#[component]
pub fn IntentMergeView(on_resolved: EventHandler<()>, on_close: EventHandler<()>) -> Element {
    let mut st = use_context::<AppState>();
    let merge = use_resource(move || async move {
        let cwd = st.cwd.read().clone();
        tokio::task::spawn_blocking(move || intent_merge::load(&cwd, GRAPH_FILE)).await.unwrap_or_else(|e| Err(e.to_string()))
    });
    let mut choices = use_signal(Vec::<Option<Side>>::new);
    let mut message = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);

    if let Some(msg) = message.read().clone() {
        return rsx! {
            div { class: "merge-view",
                h3 { "Intent conflicts resolved" }
                p { class: "muted", "The graph is saved and staged. Finish the merge with this commit message so the history shows it as an intent decision:" }
                pre { class: "merge-message", "{msg}" }
                button { class: "primary", onclick: move |_| on_close.call(()), "Back to the graph" }
            }
        };
    }

    let m: Merge = match merge.read().clone() {
        None => return rsx! { div { class: "merge-view muted", "Reading the merge…" } },
        Some(Err(e)) => return rsx! { div { class: "merge-view", div { class: "wb-error", "{e}" } } },
        Some(Ok(m)) => m,
    };
    let chosen: Vec<Option<Side>> = (0..m.conflicts.len()).map(|i| choices.read().get(i).copied().flatten()).collect();
    let ready = chosen.iter().all(Option::is_some);
    let n = m.conflicts.len();

    let save = {
        let m = m.clone();
        let chosen = chosen.clone();
        move |_| {
            let sides: Vec<Side> = chosen.iter().map(|s| s.unwrap_or(Side::Ours)).collect();
            let mut g = m.graph.clone();
            for (c, s) in m.conflicts.iter().zip(&sides) {
                intent_merge::resolve(&mut g, c, *s);
            }
            let cwd = st.cwd.peek().clone();
            let result = (|| -> Result<(), String> {
                graph::save(&cwd, GRAPH_FILE, &g)?;
                let mut staged = vec![GRAPH_FILE];
                if let Some(base) = intent_merge::resolve_baseline(&cwd)? {
                    graph::save(&cwd, BASELINE_FILE, &base)?;
                    staged.push(BASELINE_FILE);
                }
                git::add(&cwd, &staged)
            })();
            match result {
                Ok(()) => {
                    message.set(Some(intent_merge::commit_message(&m.conflicts, &sides)));
                    on_resolved.call(());
                    st.load_workbench();
                }
                Err(e) => error.set(Some(e)),
            }
        }
    };

    rsx! {
        div { class: "merge-view",
            h3 { "Intent conflicts in the architecture graph" }
            p { class: "muted",
                "This merge changes the graph on both sides. Changes made on one side only are already merged. "
                "For each node below, both sides describe different intent: choose the one the system should follow."
            }
            if let Some(e) = error.read().clone() {
                div { class: "wb-error", "{e}" }
            }
            if n == 0 {
                p { "No node has conflicting intent. Save to write the merged graph." }
            }
            for (i, c) in m.conflicts.iter().enumerate() {
                div { key: "{c.id}", class: "merge-conflict",
                    div { class: "merge-conflict-head",
                        b { "{c.title()}" }
                        span { class: "muted small-text", "{dependents(&m.graph, c)}" }
                    }
                    div { class: "merge-sides",
                        for (side, label) in [(Side::Ours, "Mine"), (Side::Theirs, "Theirs")] {
                            div {
                                key: "{label}",
                                class: if chosen[i] == Some(side) { "merge-side chosen" } else { "merge-side" },
                                b { "{label}" }
                                {side_body(&m.graph, c, c.side(side))}
                                button {
                                    class: "small",
                                    onclick: move |_| {
                                        let mut v = choices.write();
                                        v.resize(n, None);
                                        v[i] = Some(side);
                                    },
                                    if side == Side::Ours { "Keep mine" } else { "Take theirs" }
                                }
                            }
                        }
                    }
                }
            }
            div { class: "inspector-actions",
                span { class: "muted small-text", "Open the node afterwards to refine the chosen description." }
                div { class: "spacer" }
                button { class: "primary", disabled: !ready, onclick: save, "Save resolution" }
            }
        }
    }
}

fn dependents(g: &Graph, c: &Conflict) -> String {
    let deps: Vec<String> = g.net(c.scope.as_deref()).map(|n| n.incoming(&c.id).iter().map(|d| d.title.clone()).collect()).unwrap_or_default();
    if deps.is_empty() { String::new() } else { format!("Used by: {}", deps.join(", ")) }
}

fn side_body(g: &Graph, c: &Conflict, n: Option<&Node>) -> Element {
    let Some(n) = n else {
        return rsx! { p { class: "muted", "Removed on this side." } };
    };
    let net = g.net(c.scope.as_deref());
    let links: Vec<String> = n.links.iter().map(|l| net.map(|g| g.title_of(l)).unwrap_or_else(|| l.clone())).collect();
    rsx! {
        p { class: "merge-desc", "{n.description}" }
        if !links.is_empty() {
            div { class: "muted small-text", "Links: {links.join(\", \")}" }
        }
        if !n.files.is_empty() {
            div { class: "muted small-text", "Files: {n.files.join(\", \")}" }
        }
    }
}
