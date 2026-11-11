//! The settings screen, opened from the button at the bottom left. It replaces the graph in
//! the main screen until it's closed.

use dioxus::prelude::*;

use crate::mcp_view::McpView;
use crate::providers_view::{ProviderPicker, ProvidersView};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Agent,
    Providers,
    Mcp,
}

impl Section {
    const ALL: [Section; 3] = [Section::Agent, Section::Providers, Section::Mcp];

    fn label(self) -> &'static str {
        match self {
            Section::Agent => "Agent",
            Section::Providers => "Providers",
            Section::Mcp => "MCP servers",
        }
    }
}

#[component]
pub fn SettingsView() -> Element {
    let mut section = use_signal(|| Section::Agent);
    let current = *section.read();

    rsx! {
        div { class: "settings",
            nav { class: "settings-nav",
                for s in Section::ALL {
                    button {
                        key: "{s.label()}",
                        class: if s == current { "settings-nav-item active" } else { "settings-nav-item" },
                        onclick: move |_| section.set(s),
                        "{s.label()}"
                    }
                }
            }
            div { class: "settings-body",
                match current {
                    Section::Agent => rsx! { AgentSettings {} },
                    Section::Providers => rsx! { ProvidersView {} },
                    Section::Mcp => rsx! { McpView {} },
                }
            }
        }
    }
}

#[component]
fn AgentSettings() -> Element {

    rsx! {
        div { class: "settings-page",
            div { class: "settings-row",
                label { "Provider and model" }
                div { class: "settings-controls", ProviderPicker {} }
                div { class: "muted small-text", "Switching provider starts a new session. Leave the model empty to use the CLI's own default." }
            }
            div { class: "settings-row",
                label { "Mode" }
                div { class: "muted small-text",
                    "Plan, Accept edits, Auto and Full auto are picked under the chat. Every session starts in Plan."
                }
            }
        }
    }
}
