use dioxus::prelude::*;

use crate::api_chat;
use crate::dropdown::{Choice, ComboBox, Dropdown};
use crate::providers::{Access, Provider};
use crate::state::{AppState, Cmd};

/// Header control: which provider and model the chat uses.
#[component]
pub fn ProviderPicker() -> Element {
    let mut st = use_context::<AppState>();
    let settings = st.provider_settings.read().clone();
    let provider = settings.provider;
    let model = settings.model(provider);
    let choices = settings.model_choices(provider);

    rsx! {
        Dropdown {
            title: "Provider",
            value: provider.id().to_string(),
            choices: Provider::ALL.iter().map(|p| Choice::new(p.id(), p.label())).collect::<Vec<_>>(),
            onchange: move |id: String| {
                let next = Provider::from_id(&id);
                st.set_provider(next);
                // Sessions can't move between providers, so switching starts a new one.
                st.chat().send(Cmd::Restart { keep_session: false });
            },
        }
        ComboBox {
            class: "model-input",
            title: "Model. Leave empty to use the CLI's own default.",
            placeholder: if provider.is_api() { "model" } else { "default model" },
            value: model,
            suggestions: choices,
            onchange: move |value: String| {
                st.update_provider_settings(|s| {
                    s.models.insert(provider.id().to_string(), value);
                });
                st.chat().send(Cmd::Restart { keep_session: true });
            },
        }
    }
}

/// The Providers tab: what is installed or has a key, API keys, and live model lists.
#[component]
pub fn ProvidersView() -> Element {
    let settings = use_context::<AppState>().provider_settings.read().clone();

    rsx! {
        div { class: "rules",
            div { class: "rules-intro muted small-text",
                "CLI providers (Workbench, Codex, Vibe) are agents: they edit files, run commands and use your MCP servers. "
                "API providers only chat: they can't change the project, so graph commits and other code changes need a CLI provider. "
                "Settings and keys are stored in your user config, not in the project. An API key in the environment wins over one typed here."
            }
            div { class: "rules-list",
                for provider in Provider::ALL {
                    ProviderCard { key: "{provider.id()}", provider, active: provider == settings.provider }
                }
            }
        }
    }
}

#[component]
fn ProviderCard(provider: Provider, active: bool) -> Element {
    let mut st = use_context::<AppState>();
    let mut loading = use_signal(|| false);
    let mut load_error = use_signal(|| None::<String>);
    let settings = st.provider_settings.read().clone();
    let availability = provider.availability(&settings);
    let stored_key = settings.keys.get(provider.id()).cloned().unwrap_or_default();
    let model_count = settings.fetched_models.get(provider.id()).map(Vec::len);

    let load_models = move |_| {
        let Access::Api { base_url, style, .. } = provider.access() else { return };
        let key = st.provider_settings.peek().api_key(provider);
        loading.set(true);
        load_error.set(None);
        spawn(async move {
            match api_chat::list_models(base_url, style, key).await {
                Ok(models) => st.update_provider_settings(|s| {
                    s.fetched_models.insert(provider.id().to_string(), models);
                }),
                Err(e) => load_error.set(Some(e)),
            }
            loading.set(false);
        });
    };

    rsx! {
        div { class: if availability.is_ok() { "rule" } else { "rule disabled" },
            div { class: "rule-head",
                span { class: "rule-title", "{provider.label()}" }
                if active {
                    span { class: "badge", "in use" }
                } else {
                    button {
                        class: "small",
                        onclick: move |_| {
                            st.set_provider(provider);
                            st.chat().send(Cmd::Restart { keep_session: false });
                        },
                        "Use"
                    }
                }
            }
            div { class: "muted small-text",
                match &availability {
                    Ok(()) => "Ready".to_string(),
                    Err(why) => why.clone(),
                }
            }
            if let Access::Api { key_env, .. } = provider.access() {
                input {
                    r#type: "password",
                    placeholder: "API key (or set {key_env})",
                    value: "{stored_key}",
                    onchange: move |e| {
                        let key = e.value().trim().to_string();
                        st.update_provider_settings(|s| {
                            if key.is_empty() {
                                s.keys.remove(provider.id());
                            } else {
                                s.keys.insert(provider.id().to_string(), key);
                            }
                        });
                    },
                }
                div { class: "rule-scope",
                    button { class: "small", disabled: loading(), onclick: load_models, if loading() { "Loading…" } else { "Load models" } }
                    span { class: "muted small-text",
                        match (load_error(), model_count) {
                            (Some(e), _) => e,
                            (None, Some(n)) => format!("{n} models loaded"),
                            (None, None) => "Showing suggested models".to_string(),
                        }
                    }
                }
            }
        }
    }
}
