//! The LLM providers Workbench can talk to: agent CLIs installed on this machine
//! (Codex, Mistral Vibe, Workbench) and direct API calls (Anthropic, OpenAI,
//! Mistral, OpenRouter). The chosen provider, a model per provider and any API
//! keys typed into the app are stored per user, never in the project.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::claude::ChatMode;
use crate::{config, platform};

const SETTINGS_FILE: &str = "providers.json";

/// The modes an API provider offers: it only answers text, so the only choice is planning
/// first or answering directly.
const API_MODES: [ChatMode; 2] = [ChatMode::Plan, ChatMode::Auto];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    #[default]
    ClaudeCode,
    Codex,
    Vibe,
    AnthropicApi,
    OpenAiApi,
    MistralApi,
    OpenRouterApi,
}

/// How Workbench reaches a provider.
pub enum Access {
    /// An agent CLI that edits files and runs tools itself.
    Cli { binary: &'static str },
    /// A chat API: answers only, no file edits or tools.
    Api { base_url: &'static str, key_env: &'static str, style: ApiStyle },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ApiStyle {
    /// Anthropic's Messages API.
    Anthropic,
    /// The OpenAI chat completions API (also used by Mistral and OpenRouter).
    OpenAi,
}

impl Provider {
    pub const ALL: [Provider; 7] = [
        Provider::ClaudeCode,
        Provider::Codex,
        Provider::Vibe,
        Provider::AnthropicApi,
        Provider::OpenAiApi,
        Provider::MistralApi,
        Provider::OpenRouterApi,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Provider::ClaudeCode => "claude-code",
            Provider::Codex => "codex",
            Provider::Vibe => "vibe",
            Provider::AnthropicApi => "anthropic-api",
            Provider::OpenAiApi => "openai-api",
            Provider::MistralApi => "mistral-api",
            Provider::OpenRouterApi => "openrouter-api",
        }
    }

    pub fn from_id(id: &str) -> Self {
        Self::ALL.into_iter().find(|p| p.id() == id).unwrap_or_default()
    }

    pub fn label(self) -> &'static str {
        match self {
            Provider::ClaudeCode => "Workbench",
            Provider::Codex => "Codex (CLI)",
            Provider::Vibe => "Mistral Vibe (CLI)",
            Provider::AnthropicApi => "Anthropic API",
            Provider::OpenAiApi => "OpenAI API",
            Provider::MistralApi => "Mistral API",
            Provider::OpenRouterApi => "OpenRouter API",
        }
    }

    /// A short name for chat messages, e.g. "codex exited".
    pub fn short_name(self) -> &'static str {
        match self {
            Provider::ClaudeCode => "workbench",
            Provider::Codex => "codex",
            Provider::Vibe => "vibe",
            Provider::AnthropicApi | Provider::OpenAiApi | Provider::MistralApi | Provider::OpenRouterApi => "the API call",
        }
    }

    pub fn access(self) -> Access {
        match self {
            Provider::ClaudeCode => Access::Cli { binary: "claude" },
            Provider::Codex => Access::Cli { binary: "codex" },
            Provider::Vibe => Access::Cli { binary: "vibe" },
            Provider::AnthropicApi => Access::Api {
                base_url: "https://api.anthropic.com/v1",
                key_env: "ANTHROPIC_API_KEY",
                style: ApiStyle::Anthropic,
            },
            Provider::OpenAiApi => Access::Api {
                base_url: "https://api.openai.com/v1",
                key_env: "OPENAI_API_KEY",
                style: ApiStyle::OpenAi,
            },
            Provider::MistralApi => Access::Api {
                base_url: "https://api.mistral.ai/v1",
                key_env: "MISTRAL_API_KEY",
                style: ApiStyle::OpenAi,
            },
            Provider::OpenRouterApi => Access::Api {
                base_url: "https://openrouter.ai/api/v1",
                key_env: "OPENROUTER_API_KEY",
                style: ApiStyle::OpenAi,
            },
        }
    }

    pub fn is_api(self) -> bool {
        matches!(self.access(), Access::Api { .. })
    }

    /// The chat modes this provider can actually run, in the order the mode toggle shows them.
    /// CLI agents map every mode to their own permission mechanism; API providers only answer,
    /// so for them the modes reduce to planning first or answering directly.
    pub fn modes(self) -> &'static [ChatMode] {
        match self.access() {
            Access::Cli { .. } => &ChatMode::ALL,
            Access::Api { .. } => &API_MODES,
        }
    }

    /// The mode's name as this provider runs it. API providers can't edit files or run
    /// commands, so their direct mode is just "Answer".
    pub fn mode_label(self, mode: ChatMode) -> &'static str {
        if self.is_api() && mode != ChatMode::Plan { "Answer" } else { mode.label() }
    }

    /// The mode's tooltip as this provider runs it.
    pub fn mode_hint(self, mode: ChatMode) -> &'static str {
        if self.is_api() {
            match mode {
                ChatMode::Plan => "Ask questions until the request is clear, then reply with a plan for you to approve",
                _ => "Answer directly, filling in gaps with the most reasonable choice. API providers can't edit files or run commands",
            }
        } else {
            mode.hint()
        }
    }

    /// Suggested models. The model field accepts any other name too, and API
    /// providers can load their live model list in the Providers tab.
    pub fn suggested_models(self) -> &'static [&'static str] {
        match self {
            Provider::ClaudeCode => &["opus", "sonnet", "haiku", "claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-2"],
            Provider::Codex => &["gpt-5-codex", "gpt-5", "gpt-5-mini"],
            Provider::Vibe => &["mistral-medium-3.5", "local"],
            Provider::AnthropicApi => &["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-2"],
            Provider::OpenAiApi => &["gpt-5", "gpt-5-mini", "gpt-5-nano", "gpt-4.1"],
            Provider::MistralApi => &[
                "mistral-large-latest",
                "mistral-medium-latest",
                "mistral-small-latest",
                "codestral-latest",
                "devstral-medium-latest",
                "magistral-medium-latest",
            ],
            Provider::OpenRouterApi => &["anthropic/claude-sonnet-5.5", "openai/gpt-5", "mistralai/mistral-large", "google/gemini-2.5-pro"],
        }
    }

    /// API providers need a model; CLIs fall back to their own configured default.
    pub fn default_model(self) -> &'static str {
        if self.is_api() { self.suggested_models()[0] } else { "" }
    }

    /// Whether the CLI is installed, or the API key is set. Returns why not otherwise.
    pub fn availability(self, settings: &Settings) -> Result<(), String> {
        match self.access() {
            // Workbench is also looked for in its installer's own folders, see claude::find_claude_binary.
            Access::Cli { binary } if self == Provider::ClaudeCode || platform::find_program(binary).is_some() => Ok(()),
            Access::Cli { binary } => Err(format!("`{binary}` is not installed")),
            Access::Api { key_env, .. } => match settings.api_key(self) {
                Some(_) => Ok(()),
                None => Err(format!("no API key: set {key_env} or add a key in the Providers tab")),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub provider: Provider,
    /// The chosen model per provider id. Missing means the provider's default.
    #[serde(default)]
    pub models: BTreeMap<String, String>,
    /// API keys typed into the app, per provider id. The environment variable wins when both are set.
    #[serde(default)]
    pub keys: BTreeMap<String, String>,
    /// Model lists loaded from API providers in the Providers tab, per provider id.
    #[serde(default)]
    pub fetched_models: BTreeMap<String, Vec<String>>,
}

impl Settings {
    pub fn load() -> Self {
        config::load_json(SETTINGS_FILE)
    }

    pub fn save(&self) -> Result<(), String> {
        config::save_json(SETTINGS_FILE, self)
    }

    pub fn model(&self, provider: Provider) -> String {
        self.models
            .get(provider.id())
            .cloned()
            .unwrap_or_else(|| provider.default_model().to_string())
    }

    /// The models to offer for a provider: its loaded list if there is one, otherwise the suggestions.
    pub fn model_choices(&self, provider: Provider) -> Vec<String> {
        match self.fetched_models.get(provider.id()) {
            Some(list) if !list.is_empty() => list.clone(),
            _ => provider.suggested_models().iter().map(|m| m.to_string()).collect(),
        }
    }

    pub fn api_key(&self, provider: Provider) -> Option<String> {
        let Access::Api { key_env, .. } = provider.access() else { return None };
        std::env::var(key_env)
            .ok()
            .or_else(|| self.keys.get(provider.id()).cloned())
            .filter(|k| !k.trim().is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_providers_offer_every_mode_api_providers_only_plan_and_answer() {
        for p in [Provider::ClaudeCode, Provider::Codex, Provider::Vibe] {
            assert_eq!(p.modes(), &ChatMode::ALL, "{}", p.id());
            assert_eq!(p.mode_label(ChatMode::Plan), "Plan");
            assert_eq!(p.mode_label(ChatMode::FullAuto), "Full auto");
        }
        for p in [Provider::AnthropicApi, Provider::OpenAiApi, Provider::MistralApi, Provider::OpenRouterApi] {
            assert_eq!(p.modes(), &[ChatMode::Plan, ChatMode::Auto], "{}", p.id());
            assert_eq!(p.mode_label(ChatMode::Plan), "Plan");
            assert_eq!(p.mode_label(ChatMode::Auto), "Answer");
            assert!(p.mode_hint(ChatMode::Auto).contains("can't edit files"));
        }
        // Every provider offers Plan, so it is the safe reset when the provider changes.
        assert!(Provider::ALL.iter().all(|p| p.modes().contains(&ChatMode::Plan)));
    }
}
