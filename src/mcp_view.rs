use dioxus::prelude::*;

use crate::dropdown::{Choice, Dropdown};

use crate::mcp::{self, Server, Transport};
use crate::state::AppState;

#[component]
pub fn McpView() -> Element {
    let mut st = use_context::<AppState>();
    let servers = st.mcp_servers.read().clone();

    let add = move |_| {
        st.update_mcp(|list| list.insert(0, Server { enabled: true, ..Default::default() }));
    };

    rsx! {
        div { class: "rules",
            div { class: "graph-toolbar",
                button { class: "primary", onclick: add, "+ MCP server" }
            }
            div { class: "rules-intro muted small-text",
                "Enabled servers are passed to Workbench, which can then call their tools from the chat. Workbench reconnects on the next message after a change. Stored in "
                code { "{mcp::MCP_FILE}" }
                ", which is committed with the project: write secrets as "
                code { "${{ENV_VAR}}" }
                " so Workbench reads them from your environment."
            }
            div { class: "rules-list",
                if servers.is_empty() {
                    div { class: "muted pad",
                        "No MCP servers yet. Add a local server (a command such as “npx -y @modelcontextprotocol/server-filesystem .”) or a remote one by URL."
                    }
                }
                for (index, server) in servers.into_iter().enumerate() {
                    ServerCard { key: "{index}", index, server }
                }
            }
        }
    }
}

/// Splits a textarea into one entry per line. Blank lines are kept so the user can keep typing.
fn lines(text: &str) -> Vec<String> {
    text.split('\n').map(String::from).collect()
}

#[component]
fn ServerCard(index: usize, server: Server) -> Element {
    let mut st = use_context::<AppState>();
    let status = st
        .mcp_status
        .read()
        .iter()
        .find(|s| s.name == server.name.trim())
        .map(|s| s.status.clone());

    let mut edit = move |f: Box<dyn FnOnce(&mut Server)>| {
        st.update_mcp(move |list| {
            if let Some(s) = list.get_mut(index) {
                f(s);
            }
        });
    };

    let status_text = match (server.is_usable(), status) {
        (false, _) if !server.enabled => "disabled".to_string(),
        (false, _) => "incomplete: needs a name and a command or URL".to_string(),
        (true, Some(status)) => status,
        (true, None) => "connects on the next message".to_string(),
    };
    let is_remote = server.transport != Transport::Stdio;

    rsx! {
        div { class: if server.enabled { "rule" } else { "rule disabled" },
            div { class: "rule-head",
                input {
                    r#type: "checkbox",
                    checked: server.enabled,
                    title: "Enabled",
                    onchange: move |e| {
                        let v = e.checked();
                        edit(Box::new(move |s| s.enabled = v));
                    },
                }
                input {
                    class: "rule-title",
                    placeholder: "Name, e.g. “filesystem”",
                    value: "{server.name}",
                    oninput: move |e| {
                        let v = e.value();
                        edit(Box::new(move |s| s.name = v));
                    },
                }
                Dropdown {
                    value: server.transport.as_str().to_string(),
                    choices: Transport::ALL.iter().map(|t| Choice::new(t.as_str(), t.as_str())).collect::<Vec<_>>(),
                    onchange: move |v: String| {
                        let v = Transport::from_str(&v);
                        edit(Box::new(move |s| s.transport = v));
                    },
                }
                button {
                    class: "small",
                    title: "Delete server",
                    onclick: move |_| st.update_mcp(|list| { list.remove(index); }),
                    "✕"
                }
            }
            if is_remote {
                input {
                    placeholder: "URL, e.g. https://example.com/mcp",
                    value: "{server.url}",
                    oninput: move |e| {
                        let v = e.value();
                        edit(Box::new(move |s| s.url = v));
                    },
                }
                textarea {
                    placeholder: "Headers, one per line, e.g. Authorization: Bearer ${{TOKEN}}",
                    value: "{server.headers.join(\"\\n\")}",
                    oninput: move |e| {
                        let v = lines(&e.value());
                        edit(Box::new(move |s| s.headers = v));
                    },
                }
            } else {
                input {
                    placeholder: "Command, e.g. npx",
                    value: "{server.command}",
                    oninput: move |e| {
                        let v = e.value();
                        edit(Box::new(move |s| s.command = v));
                    },
                }
                textarea {
                    placeholder: "Arguments, one per line",
                    value: "{server.args.join(\"\\n\")}",
                    oninput: move |e| {
                        let v = lines(&e.value());
                        edit(Box::new(move |s| s.args = v));
                    },
                }
                textarea {
                    placeholder: "Environment, one per line, e.g. API_KEY=${{API_KEY}}",
                    value: "{server.env.join(\"\\n\")}",
                    oninput: move |e| {
                        let v = lines(&e.value());
                        edit(Box::new(move |s| s.env = v));
                    },
                }
            }
            div { class: "muted small-text", "Status: {status_text}" }
        }
    }
}
