use dioxus::prelude::*;

use crate::dropdown::{Choice, Dropdown};

use crate::prompts;
use crate::rules::{self, Rule};
use crate::state::{AppState, Cmd};

#[component]
pub fn RulesView() -> Element {
    let mut st = use_context::<AppState>();
    let mut filter = use_signal(String::new);
    let busy = st.chat_busy();
    let all = st.rules.read().clone();
    let nodes: Vec<(String, String)> = st.graph.read().nodes.iter().map(|n| (n.id.clone(), n.title.clone())).collect();
    let needle = filter.read().to_lowercase();
    let visible: Vec<Rule> = all
        .iter()
        .filter(|r| needle.is_empty() || r.title.to_lowercase().contains(&needle) || r.body.to_lowercase().contains(&needle))
        .cloned()
        .collect();
    let enabled = all.iter().filter(|r| r.enabled).count();

    let add = move |_| {
        st.update_rules(|rs| {
            let id = rules::new_id(rs);
            rs.insert(0, Rule { id, title: String::new(), body: String::new(), enabled: true, scope: None });
        });
    };

    let audit = move |_| {
        if st.chat_busy() {
            return;
        }
        let text = prompts::audit(&st.graph.peek(), &st.rules.peek());
        st.chat_or_new().send(Cmd::Prompt { label: "Audit code against project rules".into(), text });
    };

    rsx! {
        div { class: "rules",
            div { class: "graph-toolbar",
                button { class: "primary", onclick: add, "+ Rule" }
                button { disabled: busy || enabled == 0, title: "Ask Workbench to check the codebase against every enabled rule (read-only)", onclick: audit, "Audit code" }
                div { class: "spacer" }
                input { class: "filter", placeholder: "Filter rules…", value: "{filter}", oninput: move |e| filter.set(e.value()) }
            }
            div { class: "rules-intro muted small-text",
                "Enabled rules are given to Workbench on every message, graph commit and debug run in this project. Scope a rule to a node to apply it only there. Stored in "
                code { "{rules::RULES_FILE}" }
                "."
            }
            div { class: "rules-list",
                if all.is_empty() {
                    div { class: "muted pad",
                        "No rules yet. Rules capture how this project should be built, e.g. “All database access goes through the repository layer” or “Never log personal data”."
                    }
                }
                for rule in visible {
                    RuleCard { key: "{rule.id}", rule, nodes: nodes.clone() }
                }
            }
        }
    }
}

#[component]
fn RuleCard(rule: Rule, nodes: Vec<(String, String)>) -> Element {
    let mut st = use_context::<AppState>();
    let id = rule.id.clone();
    let scope = rule.scope.clone().unwrap_or_default();
    let scope_missing = rule.scope.as_ref().is_some_and(|s| !nodes.iter().any(|(id, _)| id == s));

    let edit = move |f: Box<dyn FnOnce(&mut Rule)>| {
        let id = id.clone();
        st.update_rules(move |rs| {
            if let Some(r) = rs.iter_mut().find(|r| r.id == id) {
                f(r);
            }
        });
    };
    let mut e_title = edit.clone();
    let mut e_body = edit.clone();
    let mut e_enabled = edit.clone();
    let mut e_scope = edit.clone();
    let rid = rule.id.clone();

    rsx! {
        div { class: if rule.enabled { "rule" } else { "rule disabled" },
            div { class: "rule-head",
                input {
                    r#type: "checkbox",
                    checked: rule.enabled,
                    title: "Enabled",
                    onchange: move |e| {
                        let v = e.checked();
                        e_enabled(Box::new(move |r| r.enabled = v));
                    },
                }
                input {
                    class: "rule-title",
                    placeholder: "Rule, e.g. “Handlers never talk to the database directly”",
                    value: "{rule.title}",
                    oninput: move |e| {
                        let v = e.value();
                        e_title(Box::new(move |r| r.title = v));
                    },
                }
                button {
                    class: "small",
                    title: "Delete rule",
                    onclick: move |_| {
                        let id = rid.clone();
                        st.update_rules(|rs| rs.retain(|r| r.id != id));
                    },
                    "✕"
                }
            }
            textarea {
                placeholder: "Details, rationale, examples (optional)",
                value: "{rule.body}",
                oninput: move |e| {
                    let v = e.value();
                    e_body(Box::new(move |r| r.body = v));
                },
            }
            div { class: "rule-scope",
                span { class: "muted small-text", "Applies to" }
                Dropdown {
                    value: scope.clone(),
                    choices: std::iter::once(Choice::new("", "Whole project"))
                        .chain(nodes.iter().map(|(nid, title)| Choice::new(nid.clone(), format!("Node: {title}"))))
                        .chain(scope_missing.then(|| Choice::new(scope.clone(), format!("Missing node: {scope}"))))
                        .collect::<Vec<_>>(),
                    onchange: move |v: String| {
                        e_scope(Box::new(move |r| r.scope = if v.is_empty() { None } else { Some(v) }));
                    },
                }
            }
        }
    }
}
