//! Runs one turn of the OpenAI Codex CLI (`codex exec --json`) and turns its
//! JSONL events into chat events.

use serde_json::{Value, json};
use tokio::process::Command;

use crate::agent::{TurnOptions, TurnParser};
use crate::claude::{ChatMode, ClaudeEvent};
use crate::mcp::{Server, Transport};

/// Adds the arguments for one turn. The prompt itself is written to stdin (`-`).
pub fn configure(cmd: &mut Command, opts: &TurnOptions) {
    cmd.arg("exec");
    if opts.resume.is_some() {
        cmd.arg("resume");
    }
    cmd.args(["--json", "--skip-git-repo-check"]);
    if !opts.model.trim().is_empty() {
        cmd.args(["-m", opts.model.trim()]);
    }
    match opts.mode {
        ChatMode::Plan => {
            cmd.args(["-c", "sandbox_mode=\"read-only\""]);
        }
        ChatMode::FullAuto => {
            cmd.arg("--dangerously-bypass-approvals-and-sandbox");
        }
        // `codex exec` never stops to ask, so these modes may edit the workspace.
        ChatMode::AcceptEdits | ChatMode::Auto => {
            cmd.args(["-c", "sandbox_mode=\"workspace-write\""]);
        }
    }
    if !opts.system_prompt.is_empty() {
        cmd.arg("-c").arg(format!("developer_instructions={}", toml_value(&json!(opts.system_prompt))));
    }
    for server in crate::mcp::active(&opts.mcp_servers) {
        cmd.arg("-c").arg(format!("mcp_servers.{}={}", toml_value(&json!(server.name.trim())), toml_value(&mcp_entry(server))));
    }
    if let Some(id) = &opts.resume {
        cmd.arg(id);
    }
    cmd.arg("-");
}

/// A server in Codex's `mcp_servers` config format.
fn mcp_entry(server: &Server) -> Value {
    match server.transport {
        Transport::Stdio => json!({ "command": server.command.trim(), "args": server.args_for_cli(), "env": server.env_map() }),
        Transport::Http | Transport::Sse => json!({ "url": server.url.trim(), "http_headers": server.header_map() }),
    }
}

/// Writes a JSON value as an inline TOML value. JSON strings are valid TOML basic strings.
fn toml_value(v: &Value) -> String {
    match v {
        Value::Array(items) => format!("[{}]", items.iter().map(toml_value).collect::<Vec<_>>().join(", ")),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter().map(|(k, v)| format!("{} = {}", Value::String(k.clone()), toml_value(v))).collect::<Vec<_>>().join(", ")
        ),
        other => other.to_string(),
    }
}

pub struct Parser {
    model: String,
    /// Makes tool ids unique across turns, since Codex numbers items from 1 every turn.
    run_stamp: u128,
    started: Vec<String>,
    last_text: Option<String>,
    done: bool,
}

impl Parser {
    pub fn new(model: &str) -> Self {
        let run_stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or_default();
        Self { model: model.to_string(), run_stamp, started: Vec::new(), last_text: None, done: false }
    }

    fn tool_use(&mut self, item: &Value) -> Option<ClaudeEvent> {
        let id = self.item_id(item);
        if self.started.contains(&id) {
            return None;
        }
        let (name, input) = match str_at(item, "type").as_str() {
            "command_execution" => ("Bash".to_string(), json!({ "command": str_at(item, "command") })),
            "file_change" => {
                let first = item["changes"][0]["path"].as_str().unwrap_or_default();
                ("Edit".to_string(), json!({ "file_path": first, "changes": item["changes"] }))
            }
            "mcp_tool_call" => (format!("mcp__{}__{}", str_at(item, "server"), str_at(item, "tool")), item["arguments"].clone()),
            "web_search" => ("WebSearch".to_string(), json!({ "query": str_at(item, "query") })),
            _ => return None,
        };
        self.started.push(id.clone());
        Some(ClaudeEvent::ToolUse { id, name, input })
    }

    fn tool_result(&self, item: &Value) -> Option<ClaudeEvent> {
        let failed = str_at(item, "status") == "failed";
        let (content, is_error) = match str_at(item, "type").as_str() {
            "command_execution" => (str_at(item, "aggregated_output"), failed || item["exit_code"].as_i64().is_some_and(|c| c != 0)),
            "file_change" => {
                let lines: Vec<String> = item["changes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|c| format!("{} {}", str_at(c, "kind"), str_at(c, "path")))
                    .collect();
                (lines.join("\n"), failed)
            }
            "mcp_tool_call" => match item.get("error").filter(|e| !e.is_null()) {
                Some(e) => (e["message"].as_str().map(str::to_string).unwrap_or_else(|| e.to_string()), true),
                None => (item["result"].to_string(), failed),
            },
            "web_search" => (String::new(), failed),
            _ => return None,
        };
        Some(ClaudeEvent::ToolResult { id: self.item_id(item), content, is_error })
    }

    fn item_id(&self, item: &Value) -> String {
        format!("codex-{}-{}", self.run_stamp, str_at(item, "id"))
    }
}

impl TurnParser for Parser {
    fn line(&mut self, line: &str) -> Vec<ClaudeEvent> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return if line.trim().is_empty() { vec![] } else { vec![ClaudeEvent::Stderr(line.to_string())] };
        };
        let item = &v["item"];
        match str_at(&v, "type").as_str() {
            "thread.started" => vec![ClaudeEvent::Init {
                session_id: str_at(&v, "thread_id"),
                model: if self.model.is_empty() { "codex".into() } else { self.model.clone() },
                mcp_servers: Vec::new(),
            }],
            "item.started" => self.tool_use(item).into_iter().collect(),
            "item.completed" => match str_at(item, "type").as_str() {
                "agent_message" => {
                    let text = str_at(item, "text");
                    self.last_text = Some(text.clone());
                    vec![ClaudeEvent::AssistantText(text)]
                }
                "error" => vec![ClaudeEvent::Stderr(str_at(item, "message"))],
                _ => self.tool_use(item).into_iter().chain(self.tool_result(item)).collect(),
            },
            "turn.completed" => {
                self.done = true;
                vec![ClaudeEvent::Done { cost_usd: None, is_error: false, text: self.last_text.clone() }]
            }
            "turn.failed" => {
                self.done = true;
                vec![ClaudeEvent::Done { cost_usd: None, is_error: true, text: Some(str_at(&v["error"], "message")) }]
            }
            "error" => vec![ClaudeEvent::Stderr(str_at(&v, "message"))],
            _ => vec![],
        }
    }

    fn finish(&mut self, success: bool) -> Vec<ClaudeEvent> {
        if self.done {
            return vec![];
        }
        let text = if success { "codex ended without finishing the turn" } else { "codex exited with an error (see the messages above)" };
        vec![ClaudeEvent::Done { cost_usd: None, is_error: true, text: Some(text.into()) }]
    }
}

fn str_at(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_turn() {
        let mut p = Parser::new("");
        let init = p.line(r#"{"type":"thread.started","thread_id":"t1"}"#);
        assert!(matches!(&init[..], [ClaudeEvent::Init { session_id, model, .. }] if session_id == "t1" && model == "codex"));
        let started = p.line(r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"ls","status":"in_progress"}}"#);
        assert!(matches!(&started[..], [ClaudeEvent::ToolUse { name, .. }] if name == "Bash"));
        let done = p.line(r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"ls","aggregated_output":"a\n","exit_code":0,"status":"completed"}}"#);
        assert!(matches!(&done[..], [ClaudeEvent::ToolResult { content, is_error: false, .. }] if content == "a\n"));
        let edit = p.line(r#"{"type":"item.completed","item":{"id":"item_2","type":"file_change","changes":[{"path":"src/a.rs","kind":"update"}],"status":"completed"}}"#);
        assert!(matches!(&edit[..], [ClaudeEvent::ToolUse { name, .. }, ClaudeEvent::ToolResult { .. }] if name == "Edit"));
        p.line(r#"{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"done"}}"#);
        let end = p.line(r#"{"type":"turn.completed","usage":{}}"#);
        assert!(matches!(&end[..], [ClaudeEvent::Done { is_error: false, text: Some(t), .. }] if t == "done"));
        assert!(p.finish(true).is_empty());
    }

    #[test]
    fn missing_turn_end_is_an_error() {
        let mut p = Parser::new("gpt-5");
        assert!(matches!(&p.finish(false)[..], [ClaudeEvent::Done { is_error: true, .. }]));
    }

    #[test]
    fn writes_inline_toml() {
        let v = json!({ "command": "npx", "args": ["-y", "x"], "env": { "K": "v\"q" } });
        assert_eq!(toml_value(&v), r#"{"args" = ["-y", "x"], "command" = "npx", "env" = {"K" = "v\"q"}}"#);
    }
}
