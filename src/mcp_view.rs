use dioxus::prelude::*;

use crate::dropdown::{Choice, Dropdown};

use crate::config;
use crate::figma::{self, Token};
use crate::mcp::{self, Scope, Server, Transport};
use crate::logos::LogoIcon;
use crate::mcp_oauth;
use crate::state::AppState;

#[component]
pub fn McpView() -> Element {
    let st = use_context::<AppState>();
    let servers = st.mcp_servers.read().clone();
    let figma_on = servers.iter().any(|s| s.enabled && mcp::preset(s).is_some_and(|p| p.logo == crate::logos::Logo::Figma));
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
                if figma_on {
                    FigmaBinding {}
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
                list.push(preset.server());
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
                span { "{preset.label}" }
                span { class: "switch" }
            }
            if let Some(server) = server.filter(|s| s.enabled) {
                if server.transport != Transport::Stdio {
                    SignIn { server: server.clone() }
                }
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

/// Binds this repository to Figma files pasted by URL. Figma no longer gives personal access
/// tokens the projects:read scope, so files can't be picked from a project list.
#[component]
fn FigmaBinding() -> Element {
    let mut st = use_context::<AppState>();
    let binding = st.figma.read().clone().unwrap_or_default();
    let mut token = use_signal(|| config::load_json::<Token>(figma::TOKEN_FILE).token);
    let mut file_url = use_signal(String::new);
    let mut note = use_signal(String::new);

    let save_token = move |e: Event<FormData>| {
        let value = e.value();
        token.set(value.clone());
        if let Err(e) = config::save_json(figma::TOKEN_FILE, &Token { token: value }) {
            note.set(e);
        }
    };
    let add_file = {
        let binding = binding.clone();
        move |_| {
            let Some(key) = figma::file_key_from_url(&file_url.read()) else {
                note.set("Paste a file URL such as https://www.figma.com/design/AbC123/Design-system".into());
                return;
            };
            if binding.files.iter().any(|f| f.key == key) {
                note.set("That file is already added".into());
                return;
            }
            let mut binding = binding.clone();
            note.set("Looking up the file…".into());
            spawn(async move {
                match figma::file(&token.peek(), &key).await {
                    Ok(file) => {
                        binding.files.push(file);
                        file_url.set(String::new());
                        note.set(String::new());
                        st.set_figma(Some(binding));
                    }
                    Err(e) => note.set(e),
                }
            });
        }
    };
    let pick_theme = {
        let binding = binding.clone();
        move |key: String| {
            let mut b = binding.clone();
            b.theme_file = if key.is_empty() { None } else { Some(key) };
            st.set_figma(Some(b));
        }
    };

    let file_choices: Vec<Choice> = std::iter::once(Choice::new("", "No theme file"))
        .chain(binding.files.iter().map(|f| Choice::new(f.key.clone(), f.name.clone())))
        .collect();

    rsx! {
        div { class: "rule",
            strong { "Figma files for this repository" }
            div { class: "muted small-text",
                "Uses a Figma personal access token (Figma → Settings → Security) with File content: read. It is stored in your user config, never in the project, and handed to the Figma MCP server on each message. The files you add are saved in "
                code { "{figma::FIGMA_FILE}" }
                " (git-ignored, so file keys stay on this machine) and tell every agent which Figma files and theme file belong to this repository."
            }
            input {
                r#type: "password",
                placeholder: "Figma personal access token",
                value: "{token}",
                oninput: save_token,
            }
            div { class: "rule-head",
                input {
                    placeholder: "Figma file URL, e.g. https://www.figma.com/design/AbC123/Design-system",
                    value: "{file_url}",
                    oninput: move |e| file_url.set(e.value()),
                }
                button { class: "small", disabled: token.read().trim().is_empty(), onclick: add_file, "Add" }
            }
            for file in binding.files.clone() {
                div { key: "{file.key}", class: "rule-head",
                    a { class: "small-text", href: "{figma::file_url(&file.key)}", target: "_blank", "{file.name}" }
                    button {
                        class: "small",
                        title: "Remove this file",
                        onclick: {
                            let binding = binding.clone();
                            move |_| {
                                let mut b = binding.clone();
                                b.files.retain(|f| f.key != file.key);
                                if b.theme_file.as_deref() == Some(file.key.as_str()) {
                                    b.theme_file = None;
                                }
                                st.set_figma(if b.files.is_empty() { None } else { Some(b) });
                            }
                        },
                        "✕"
                    }
                }
            }
            if !binding.files.is_empty() {
                label { "Theme file (the design system with the colours, fonts and radii)" }
                Dropdown {
                    value: binding.theme_file.clone().unwrap_or_default(),
                    choices: file_choices,
                    onchange: pick_theme,
                }
            }
            if !note.read().is_empty() {
                span { class: "muted small-text", "{note}" }
            }
        }
    }
}
