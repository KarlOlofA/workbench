//! Direct chat calls to provider APIs, with the whole conversation so far. The model can
//! call the tools of the user's MCP servers (through `mcp_manager`); each call is run and
//! its result sent back until the model answers. These providers can't edit files or run
//! commands themselves.

use serde_json::{Value, json};

use crate::agent::TurnOptions;
use crate::claude::{ChatMode, ClaudeEvent};
use crate::mcp_manager::{self, ToolSpec};
use crate::providers::ApiStyle;
use crate::state::ChatItem;

/// Anthropic requires an output limit on every request.
const ANTHROPIC_MAX_TOKENS: u32 = 16_000;
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// How many times one message may go back and forth between tool calls and the model.
const MAX_TOOL_ROUNDS: usize = 25;

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

/// Sends the conversation and reports the answer as chat events, ending with `Done`.
/// Tool calls the model makes along the way are reported as `ToolUse` and `ToolResult`.
pub async fn run(request: Request, emit: impl Fn(ClaudeEvent)) {
    match ask(&request, &emit).await {
        Ok(text) => {
            emit(ClaudeEvent::AssistantText(text.clone()));
            emit(ClaudeEvent::Done { cost_usd: None, is_error: false, text: Some(text) });
        }
        Err(e) => emit(ClaudeEvent::Done { cost_usd: None, is_error: true, text: Some(e) }),
    }
}

/// Asks the model, runs the MCP tools it calls and asks again with their results,
/// until it answers without calling a tool.
async fn ask(request: &Request, emit: &impl Fn(ClaudeEvent)) -> Result<String, String> {
    let opts = &request.opts;
    let (tools, statuses) = mcp_manager::tools(&opts.cwd, &opts.mcp_servers).await;
    emit(ClaudeEvent::McpStatus(statuses));
    // Plan mode is read only, so it only gets the tools the server marks as read only.
    let tools: Vec<ToolSpec> = tools.into_iter().filter(|t| opts.mode != ChatMode::Plan || t.read_only).collect();

    let mut messages: Vec<Value> = opts.history.iter().map(|m| json!({ "role": m.role, "content": m.content })).collect();
    messages.push(json!({ "role": "user", "content": opts.prompt }));
    if request.style == ApiStyle::OpenAi {
        messages.insert(0, json!({ "role": "system", "content": opts.system_prompt }));
    }

    let client = reqwest::Client::new();
    for _ in 0..MAX_TOOL_ROUNDS {
        let body = send(build_call(&client, request, &messages, &tools)).await?;
        let reply = parse_reply(request.style, &body);
        if reply.calls.is_empty() {
            return Ok(reply.text);
        }
        emit(ClaudeEvent::AssistantText(reply.text));
        messages.push(reply.message);
        let mut results = Vec::new();
        for call in reply.calls {
            emit(ClaudeEvent::ToolUse { id: call.id.clone(), name: call.name.clone(), input: call.input.clone() });
            let result = match tools.iter().find(|t| t.name == call.name) {
                Some(tool) => mcp_manager::call(tool, call.input).await,
                None => Err(format!("no tool named {}", call.name)),
            };
            let (content, is_error) = match result {
                Ok(text) => (text, false),
                Err(text) => (text, true),
            };
            emit(ClaudeEvent::ToolResult { id: call.id.clone(), content: content.clone(), is_error });
            results.push(ToolOutcome { id: call.id, content, is_error });
        }
        messages.extend(tool_result_messages(request.style, results));
    }
    Err(format!("stopped after {MAX_TOOL_ROUNDS} rounds of tool calls"))
}

fn build_call(client: &reqwest::Client, request: &Request, messages: &[Value], tools: &[ToolSpec]) -> reqwest::RequestBuilder {
    let opts = &request.opts;
    match request.style {
        ApiStyle::Anthropic => {
            let mut body = json!({
                "model": opts.model,
                "max_tokens": ANTHROPIC_MAX_TOKENS,
                "system": opts.system_prompt,
                "messages": messages,
            });
            if !tools.is_empty() {
                body["tools"] = tools
                    .iter()
                    .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.input_schema }))
                    .collect();
            }
            client
                .post(format!("{}/messages", request.base_url))
                .header("x-api-key", &request.key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .json(&body)
        }
        ApiStyle::OpenAi => {
            let mut body = json!({ "model": opts.model, "messages": messages });
            // OpenAI-style APIs reject an empty tools list.
            if !tools.is_empty() {
                body["tools"] = tools
                    .iter()
                    .map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.input_schema } }))
                    .collect();
            }
            client.post(format!("{}/chat/completions", request.base_url)).bearer_auth(&request.key).json(&body)
        }
    }
}

/// One answer from the model.
#[derive(Debug, PartialEq)]
struct Reply {
    text: String,
    calls: Vec<ToolCall>,
    /// The answer as a message, to send back with the tool results.
    message: Value,
}

#[derive(Debug, PartialEq)]
struct ToolCall {
    id: String,
    name: String,
    input: Value,
}

struct ToolOutcome {
    id: String,
    content: String,
    is_error: bool,
}

fn parse_reply(style: ApiStyle, body: &Value) -> Reply {
    match style {
        ApiStyle::Anthropic => {
            let blocks = body["content"].as_array().cloned().unwrap_or_default();
            let text = blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("");
            let calls = blocks
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .map(|b| ToolCall { id: str_of(&b["id"]), name: str_of(&b["name"]), input: b["input"].clone() })
                .collect();
            Reply { text, calls, message: json!({ "role": "assistant", "content": blocks }) }
        }
        ApiStyle::OpenAi => {
            let message = body["choices"][0]["message"].clone();
            let calls = message["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| ToolCall {
                    id: str_of(&c["id"]),
                    name: str_of(&c["function"]["name"]),
                    // OpenAI-style APIs send the arguments as a JSON string.
                    input: serde_json::from_str(c["function"]["arguments"].as_str().unwrap_or("{}")).unwrap_or_else(|_| json!({})),
                })
                .collect();
            Reply { text: str_of(&message["content"]), calls, message }
        }
    }
}

/// The tool results as the messages that follow the model's tool calls.
fn tool_result_messages(style: ApiStyle, results: Vec<ToolOutcome>) -> Vec<Value> {
    match style {
        ApiStyle::Anthropic => {
            let blocks: Vec<Value> = results
                .into_iter()
                .map(|r| json!({ "type": "tool_result", "tool_use_id": r.id, "content": r.content, "is_error": r.is_error }))
                .collect();
            vec![json!({ "role": "user", "content": blocks })]
        }
        ApiStyle::OpenAi => results
            .into_iter()
            .map(|r| {
                let content = if r.is_error { format!("Error: {}", r.content) } else { r.content };
                json!({ "role": "tool", "tool_call_id": r.id, "content": content })
            })
            .collect(),
    }
}

fn str_of(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_anthropic_tool_calls() {
        let body = json!({ "content": [
            { "type": "text", "text": "Looking." },
            { "type": "tool_use", "id": "t1", "name": "mcp__files__read", "input": { "path": "a" } },
        ] });
        let reply = parse_reply(ApiStyle::Anthropic, &body);
        assert_eq!(reply.text, "Looking.");
        assert_eq!(reply.calls, vec![ToolCall { id: "t1".into(), name: "mcp__files__read".into(), input: json!({ "path": "a" }) }]);
        assert_eq!(reply.message["content"], body["content"]);
        let results = tool_result_messages(ApiStyle::Anthropic, vec![ToolOutcome { id: "t1".into(), content: "x".into(), is_error: false }]);
        assert_eq!(results, vec![json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "x", "is_error": false }] })]);
    }

    #[test]
    fn reads_openai_tool_calls() {
        let body = json!({ "choices": [{ "message": { "role": "assistant", "content": null, "tool_calls": [
            { "id": "c1", "type": "function", "function": { "name": "mcp__files__read", "arguments": "{\"path\":\"a\"}" } },
        ] } }] });
        let reply = parse_reply(ApiStyle::OpenAi, &body);
        assert_eq!(reply.text, "");
        assert_eq!(reply.calls[0].input, json!({ "path": "a" }));
        let results = tool_result_messages(ApiStyle::OpenAi, vec![ToolOutcome { id: "c1".into(), content: "gone".into(), is_error: true }]);
        assert_eq!(results, vec![json!({ "role": "tool", "tool_call_id": "c1", "content": "Error: gone" })]);
    }

    #[test]
    fn plain_answers_have_no_tool_calls() {
        let body = json!({ "choices": [{ "message": { "role": "assistant", "content": "Hi" } }] });
        assert!(parse_reply(ApiStyle::OpenAi, &body).calls.is_empty());
    }
}
