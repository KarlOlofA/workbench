//! Direct chat calls to provider APIs. One request per message, with the whole
//! conversation so far. These providers only answer: they can't edit files,
//! run commands or call MCP tools.

use serde_json::{Value, json};

use crate::agent::TurnOptions;
use crate::claude::ClaudeEvent;
use crate::state::ChatItem;
use crate::providers::ApiStyle;

/// Anthropic requires an output limit on every request.
const ANTHROPIC_MAX_TOKENS: u32 = 16_000;
const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    /// "user" or "assistant".
    pub role: &'static str,
    pub content: String,
}

/// The conversation so far, as the API expects it. Tool calls and notes are left out.
pub fn history(items: &[ChatItem]) -> Vec<ChatMessage> {
    items
        .iter()
        .filter_map(|item| match item {
            ChatItem::User(m) => Some(ChatMessage { role: "user", content: m.prompt.clone() }),
            ChatItem::Prompt { text, .. } => Some(ChatMessage { role: "user", content: text.clone() }),
            ChatItem::Assistant { text, .. } => Some(ChatMessage { role: "assistant", content: text.clone() }),
            _ => None,
        })
        .collect()
}

pub struct Request {
    pub base_url: &'static str,
    pub style: ApiStyle,
    pub key: String,
    pub opts: TurnOptions,
}

/// Sends the conversation and returns the answer as chat events, ending with `Done`.
pub async fn run(request: Request) -> Vec<ClaudeEvent> {
    match ask(&request).await {
        Ok(text) => vec![
            ClaudeEvent::AssistantText(text.clone()),
            ClaudeEvent::Done { cost_usd: None, is_error: false, text: Some(text) },
        ],
        Err(e) => vec![ClaudeEvent::Done { cost_usd: None, is_error: true, text: Some(e) }],
    }
}

async fn ask(request: &Request) -> Result<String, String> {
    let opts = &request.opts;
    let mut messages: Vec<Value> = opts.history.iter().map(|m| json!({ "role": m.role, "content": m.content })).collect();
    messages.push(json!({ "role": "user", "content": opts.prompt }));

    let client = reqwest::Client::new();
    let call = match request.style {
        ApiStyle::Anthropic => client
            .post(format!("{}/messages", request.base_url))
            .header("x-api-key", &request.key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&json!({
                "model": opts.model,
                "max_tokens": ANTHROPIC_MAX_TOKENS,
                "system": opts.system_prompt,
                "messages": messages,
            })),
        ApiStyle::OpenAi => {
            messages.insert(0, json!({ "role": "system", "content": opts.system_prompt }));
            client
                .post(format!("{}/chat/completions", request.base_url))
                .bearer_auth(&request.key)
                .json(&json!({ "model": opts.model, "messages": messages }))
        }
    };
    let body = send(call).await?;
    let text = match request.style {
        ApiStyle::Anthropic => body["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        ApiStyle::OpenAi => body["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_string(),
    };
    Ok(text)
}

/// Lists the model ids the provider offers to this key.
pub async fn list_models(base_url: &str, style: ApiStyle, key: Option<String>) -> Result<Vec<String>, String> {
    let mut call = reqwest::Client::new().get(format!("{base_url}/models"));
    if let Some(key) = key {
        call = match style {
            ApiStyle::Anthropic => call.header("x-api-key", key).header("anthropic-version", ANTHROPIC_VERSION),
            ApiStyle::OpenAi => call.bearer_auth(key),
        };
    }
    let body = send(call).await?;
    let mut ids: Vec<String> = body["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(String::from)).collect();
    ids.sort();
    Ok(ids)
}

/// Sends a request and returns its JSON body, or the provider's error message.
async fn send(call: reqwest::RequestBuilder) -> Result<Value, String> {
    let response = call.send().await.map_err(|e| format!("request failed: {e}"))?;
    let status = response.status();
    let body: Value = response.json().await.map_err(|e| format!("unreadable response ({status}): {e}"))?;
    if status.is_success() {
        return Ok(body);
    }
    let message = body["error"]["message"].as_str().or(body["message"].as_str()).map(String::from).unwrap_or_else(|| body.to_string());
    Err(format!("{status}: {message}"))
}
