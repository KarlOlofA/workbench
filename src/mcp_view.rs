use dioxus::prelude::*;

use crate::dropdown::{Choice, Dropdown};

use crate::mcp::{self, Scope, Server, Transport};
use crate::logos::LogoIcon;
use crate::mcp_oauth;
use crate::state::AppState;

#[component]
pub fn McpView() -> Element {
    let st = use_context::<AppState>();
    let servers = st.mcp_servers.read().clone();
    // Cards edit the list by index, so each section keeps the servers' places in the whole list.
    // Servers added from a preset show as their toggle instead of as a card.
    let of = |scope: Scope| {
        servers.iter().cloned().enumerate().filter(|(_, s)| s.scope == scope && mcp::preset(s).is_none()).collect::<Vec<_>>()
    };

    rsx! {
        div { class: "rules",
            div { class: "rules-intro muted small-text",
                "Every provider can call the tools of the enabled servers: the agent CLIs connect to them themselves, and the API providers through Workbench. Changes apply from the next message. Your servers are used in every project and stored in your user config. Project servers are stored in "
                code { "{mcp::MCP_FILE}" }
                ", which is committed with the project: write secrets as "
                code { "${{ENV_VAR}}" }
                " so they are read from your environment. A project server replaces your server of the same name."
            }
            div { class: "graph-toolbar", strong { "Integrations" } }
            div { class: "mcp-presets",
                for (index, preset) in mcp::PRESETS.iter().enumerate() {
                    PresetToggle { key: "{preset.name}", index }
                }
            }
            ServerSection {
                title: "Your servers (all projects)",
                scope: Scope::User,
                servers: of(Scope::User),
                empty: "No servers of your own yet. Add one to use it in every project.",
            }
            ServerSection {
                title: "This project",
                scope: Scope::Project,
                servers: of(Scope::Project),
                empty: "No project servers yet. Add a local server (a command such as “npx -y @modelcontextprotocol/server-filesystem .”) or a remote one by URL.",
            }
        }
    }
}

#[component]
fn ServerSection(title: &'static str, scope: Scope, servers: Vec<(usize, Server)>, empty: &'static str) -> Element {
    let mut st = use_context::<AppState>();
    let add = move |_| {
        st.update_mcp(move |list| {
            // New servers go first in their section, so they show at the top.
            let at = list.iter().position(|s| s.scope == scope).unwrap_or(list.len());
            list.insert(at, Server { enabled: true, scope, ..Default::default() });
        });
    };

    rsx! {
        div { class: "graph-toolbar",
            strong { "{title}" }
            button { class: "primary", onclick: add, "+ MCP server" }
        }
        div { class: "rules-list",
            if servers.is_empty() {
                div { class: "muted pad", "{empty}" }
            }
            for (index, server) in servers.into_iter() {
                ServerCard { key: "{index}", index, server }
            }
        }
    }
}

/// Splits a textarea into one entry per line. Blank lines are kept so the user can keep typing.
fn lines(text: &str) -> Vec<String> {
    text.split('\n').map(String::from).collect()
}

/// What the card says about a server: why it isn't used, or the status its last connection reported.
fn status_text(st: &AppState, server: &Server) -> String {
    let status = st.mcp_status.read().iter().find(|s| s.name == server.name.trim()).map(|s| s.status.clone());
    let overridden = mcp::is_overridden(server, &st.mcp_servers.read());
    match (server.is_usable(), status) {
        (false, _) if !server.enabled => "disabled".to_string(),
        (true, _) if overridden => "overridden by the project server of the same name".to_string(),
        (false, _) => "incomplete: needs a name and a command or URL".to_string(),
        (true, Some(status)) => status,
        (true, None) => "connects on the next message".to_string(),
    }
}

/// A preset server as an on/off toggle with its logo. On adds it as a project server; off removes it.
#[component]
fn PresetToggle(index: usize) -> Element {
    let mut st = use_context::<AppState>();
    let preset = &mcp::PRESETS[index];
    let server = st.mcp_servers.read().iter().find(|s| mcp::preset(s).is_some_and(|p| p.name == preset.name)).cloned();
    let on = server.as_ref().is_some_and(|s| s.enabled);
    let toggle = move |_| {
        st.update_mcp(move |list| {
            list.retain(|s| mcp::preset(s).is_none_or(|p| p.name != preset.name));
            if !on {
                let url = preset.url.to_string();
                list.push(Server { name: preset.name.into(), enabled: true, transport: Transport::Http, url, ..Default::default() });
            }
        });
    };

    rsx! {
        div { class: if on { "mcp-preset on" } else { "mcp-preset" },
            button {
                class: "mcp-preset-toggle",
                role: "switch",
                aria_checked: "{on}",
                title: if on { "Turn off" } else { "Turn on for this project" },
                onclick: toggle,
                LogoIcon { logo: preset.logo }
                span { "{preset.logo.name()}" }
                span { class: "switch" }
            }
            if let Some(server) = server.filter(|s| s.enabled) {
                SignIn { server: server.clone() }
                span { class: "muted small-text", "{status_text(&st, &server)}" }
            }
        }
    }
}

/// Sign in to (or out of) a remote server that uses OAuth.
#[component]
fn SignIn(server: Server) -> Element {
    // What the last sign-in attempt said; changing it also re-reads whether the server is signed in.
    let mut note = use_signal(String::new);
    let signed_in = server.is_usable() && mcp_oauth::is_signed_in(&server);
    let usable = server.is_usable();
    let on_sign_out = {
        let server = server.clone();
        move |_| note.set(mcp_oauth::sign_out(&server).err().unwrap_or_default())
    };
    let on_sign_in = move |_| {
        let server = server.clone();
        note.set("Waiting for the browser…".into());
        spawn(async move {
            match mcp_oauth::sign_in(&server).await {
                Ok(()) => note.set("Signed in; connects on the next message".into()),
                Err(e) => note.set(format!("Sign-in failed: {e}")),
            }
        });
    };

    rsx! {
        if signed_in {
            span { class: "muted small-text", "Signed in" }
            button { class: "small", onclick: on_sign_out, "Sign out" }
        } else {
            button {
                class: "small",
                title: "Sign in in the browser, for servers that use OAuth",
                disabled: !usable,
                onclick: on_sign_in,
                "Sign in"
            }
        }
        if !note.read().is_empty() {
            span { class: "muted small-text", "{note}" }
        }
    }
}

/// A custom server: the head row with its name and status, and the fields once expanded.
#[component]
fn ServerCard(index: usize, server: Server) -> Element {
    let mut st = use_context::<AppState>();
    // A new, still empty server opens expanded so it can be filled in.
    let mut expanded = use_signal(|| server.name.trim().is_empty());

    let mut edit = move |f: Box<dyn FnOnce(&mut Server)>| {
        st.update_mcp(move |list| {
            if let Some(s) = list.get_mut(index) {
                f(s);
            }
        });
    };

    let status = status_text(&st, &server);
    let is_remote = server.transport != Transport::Stdio;

    rsx! {
        div { class: if server.enabled { "rule" } else { "rule disabled" },
            div { class: "rule-head",
                button {
                    class: "small expand",
                    title: if expanded() { "Collapse" } else { "Expand" },
                    onclick: move |_| expanded.toggle(),
                    if expanded() { "▾" } else { "▸" }
                }
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
                button {
                    class: "small",
                    title: if server.scope == Scope::User { "Use only in this project" } else { "Use in all your projects" },
                    onclick: move |_| {
                        st.update_mcp(move |list| {
                            let mut moved = list.remove(index);
                            moved.scope = if moved.scope == Scope::User { Scope::Project } else { Scope::User };
                            list.push(moved);
                        });
                    },
                    if server.scope == Scope::User { "→ project" } else { "→ mine" }
                }
                button {
                    class: "small",
                    title: "Delete server",
                    onclick: move |_| st.update_mcp(|list| { list.remove(index); }),
                    "✕"
                }
            }
            if expanded() {
                Dropdown {
                    value: server.transport.as_str().to_string(),
                    choices: Transport::ALL.iter().map(|t| Choice::new(t.as_str(), t.as_str())).collect::<Vec<_>>(),
                    onchange: move |v: String| {
                        let v = Transport::from_str(&v);
                        edit(Box::new(move |s| s.transport = v));
                    },
                }
            }
            if expanded() && is_remote {
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
                div { class: "rule-head", SignIn { server: server.clone() } }
            }
            if expanded() && !is_remote {
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
            div { class: "muted small-text", "Status: {status}" }
        }
    }
}
