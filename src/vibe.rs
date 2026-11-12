//! Runs one turn of the Mistral Vibe CLI (`vibe -p --output streaming`) and
//! turns its history entries into chat events.

use serde_json::{Value, json};
use tokio::process::Command;

use crate::agent::{TurnOptions, TurnParser};
use crate::claude::{ChatMode, ClaudeEvent};
use crate::mcp::{Server, Transport};

/// Adds the arguments and environment for one turn. The prompt itself is
/// written to stdin, which `vibe -p` reads when no prompt text follows `-p`.
pub fn configure(cmd: &mut Command, opts: &TurnOptions) {
    cmd.args(["-p", "--output", "streaming", "--trust", "--agent", agent(opts.mode)]);
    // A resumed session keeps the agent it was pinned to and ignores `--agent`, so the approval
    // flags carry the mode across turns: on a resumed session they approve calls (all of them,
    // or the ones the classifier judges safe) even though the pinned agent stays.
    match opts.mode {
        ChatMode::Auto => {
            cmd.arg("--smart-approve");
        }
        ChatMode::FullAuto => {
            cmd.arg("--auto-approve");
        }
        ChatMode::Plan | ChatMode::AcceptEdits => {}
    }
    if let Some(id) = &opts.resume {
        cmd.args(["--resume", id]);
    }
    if !opts.model.trim().is_empty() {
        cmd.env("VIBE_ACTIVE_MODEL", opts.model.trim());
    }
    let servers: Vec<Value> = crate::mcp::active(&opts.mcp_servers).into_iter().map(mcp_entry).collect();
    if !servers.is_empty() {
        cmd.env("VIBE_MCP_SERVERS", Value::Array(servers).to_string());
    }
}

/// Vibe has no flag for extra instructions, so a new session gets them at the top of its first prompt.
pub fn first_prompt(system_prompt: &str, prompt: &str) -> String {
    if system_prompt.is_empty() {
        prompt.to_string()
    } else {
        format!("<workbench-instructions>\n{system_prompt}\n</workbench-instructions>\n\n{prompt}")
    }
}

/// Vibe 3.x's builtin agents. `plan` was removed in Vibe 3.0, so Plan uses `ask`: a headless
/// run can't give approval, so every write is denied and the agent stays read-only, which is
/// what plan mode needs (Workbench's plan instructions make it ask questions and present a
/// plan in its reply instead). Vibe denies approvals in programmatic mode, so Accept edits
/// uses the accept-edits agent (file edits auto-approved, commands denied), Auto lets the
/// classifier approve the safe calls (deletes stay denied), and Full auto approves everything.
fn agent(mode: ChatMode) -> &'static str {
    match mode {
        ChatMode::Plan => "ask",
        ChatMode::AcceptEdits => "accept-edits",
        ChatMode::Auto => "smart-approve",
        ChatMode::FullAuto => "auto-approve",
    }
}

/// A server in Vibe's `mcp_servers` config format.
fn mcp_entry(server: &Server) -> Value {
    let name = server.name.trim();
    match server.transport {
        Transport::Stdio => json!({
            "name": name, "transport": "stdio", "command": server.command.trim(),
            "args": server.args_for_cli(), "env": server.env_map(),
        }),
        Transport::Http => json!({ "name": name, "transport": "streamable-http", "url": server.url.trim(), "headers": server.header_map() }),
        Transport::Sse => json!({ "name": name, "transport": "http", "url": server.url.trim(), "headers": server.header_map() }),
    }
}

pub struct Parser {
    model: String,
    /// A resumed session first replays its old entries; anything created before this run started is skipped.
    started_ms: i64,
    session_sent: bool,
    last_text: Option<String>,
}

impl Parser {
    pub fn new(model: &str) -> Self {
        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or_default();
        Self { model: model.to_string(), started_ms, session_sent: false, last_text: None }
    }

    fn effect(entry: &Value) -> Vec<ClaudeEvent> {
        let detail = &entry["detail"];
        let name = match str_at(detail, "kind").as_str() {
            "shell" => "Bash".to_string(),
            "file_edit" => "Edit".to_string(),
            "file_write" => "Write".to_string(),
            "file_read" => "Read".to_string(),
            "file_search" => "Grep".to_string(),
            "web_search" => "WebSearch".to_string(),
            "web_fetch" => "WebFetch".to_string(),
            _ => str_at(detail, "toolName"),
        };
        let state = &entry["state"];
        let (content, is_error) = match str_at(state, "status").as_str() {
            "completed" => (str_at(state, "outputText"), false),
            "failed" => (str_at(&state["error"], "message"), true),
            _ => (str_at(state, "reason"), true),
        };
        let id = format!("vibe-{}", str_at(entry, "id"));
        vec![
            ClaudeEvent::ToolUse { id: id.clone(), name, input: detail["input"].clone() },
            ClaudeEvent::ToolResult { id, content, is_error },
        ]
    }
}

impl TurnParser for Parser {
    fn line(&mut self, line: &str) -> Vec<ClaudeEvent> {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            return if line.trim().is_empty() { vec![] } else { vec![ClaudeEvent::Stderr(line.to_string())] };
        };
        let mut events = Vec::new();
        if !self.session_sent && let Some(id) = entry["sessionId"].as_str() {
            self.session_sent = true;
            let model = if self.model.is_empty() { "vibe".to_string() } else { self.model.clone() };
            events.push(ClaudeEvent::Init { session_id: id.to_string(), model, mcp_servers: Vec::new() });
        }
        if entry["createdAt"].as_i64().is_some_and(|t| t < self.started_ms) {
            return events;
        }
        match (str_at(&entry, "type").as_str(), str_at(&entry, "role").as_str()) {
            ("message", "assistant") => {
                let text: Vec<&str> = entry["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect();
                let text = text.join("\n\n");
                if !text.trim().is_empty() {
                    self.last_text = Some(text.clone());
                    events.push(ClaudeEvent::AssistantText(text));
                }
            }
            ("effect", _) => events.extend(Self::effect(&entry)),
            ("callback", _) => events.push(ClaudeEvent::Stderr(format!("Vibe asked to approve “{}”, which is denied in this mode", str_at(&entry, "title")))),
            ("notice", _) if entry["level"] != "info" => events.push(ClaudeEvent::Stderr(str_at(&entry, "message"))),
            _ => {}
        }
        events
    }

    fn finish(&mut self, success: bool) -> Vec<ClaudeEvent> {
        let text = if success { self.last_text.clone() } else { Some("vibe exited with an error (see the messages above)".into()) };
        vec![ClaudeEvent::Done { cost_usd: None, is_error: !success, text }]
    }
}

fn str_at(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn maps_modes_to_vibe3_builtin_agents() {
        // `plan` was removed in Vibe 3.0; these are the builtins `vibe --help` lists.
        assert_eq!(agent(ChatMode::Plan), "ask");
        assert_eq!(agent(ChatMode::AcceptEdits), "accept-edits");
        assert_eq!(agent(ChatMode::Auto), "smart-approve");
        assert_eq!(agent(ChatMode::FullAuto), "auto-approve");
    }

    #[test]
    fn approval_flags_carry_the_mode_onto_resumed_sessions() {
        let opts = |mode: ChatMode| TurnOptions {
            cwd: PathBuf::from("/tmp"),
            model: String::new(),
            resume: Some("s1".to_string()),
            mode,
            system_prompt: String::new(),
            mcp_servers: Vec::new(),
            prompt: String::new(),
            history: Vec::new(),
        };
        let args = |mode: ChatMode| {
            let mut cmd = Command::new("vibe");
            configure(&mut cmd, &opts(mode));
            cmd.as_std().get_args().map(|a| a.to_string_lossy().to_string()).collect::<Vec<_>>()
        };
        // A resumed session ignores `--agent`, so Auto and Full auto need their approval flag.
        assert!(args(ChatMode::Auto).contains(&"--smart-approve".to_string()));
        assert!(args(ChatMode::FullAuto).contains(&"--auto-approve".to_string()));
        assert!(!args(ChatMode::Plan).iter().any(|a| a.starts_with("--") && a.contains("approve")));
        assert!(!args(ChatMode::AcceptEdits).iter().any(|a| a.starts_with("--") && a.contains("approve")));
        assert!(args(ChatMode::Plan).windows(2).any(|w| w == ["--agent", "ask"]));
    }

    #[test]
    fn parses_new_entries_and_skips_replayed_ones() {
        let mut p = Parser::new("");
        let now = p.started_ms;
        let old = format!(r#"{{"type":"message","role":"assistant","id":"e0","sessionId":"s1","createdAt":{},"content":[{{"type":"text","text":"old"}}]}}"#, now - 5000);
        assert!(matches!(&p.line(&old)[..], [ClaudeEvent::Init { session_id, .. }] if session_id == "s1"));
        let shell = format!(
            r#"{{"type":"effect","id":"e1","sessionId":"s1","createdAt":{now},"title":"bash","detail":{{"kind":"shell","toolName":"bash","input":{{"command":"ls"}}}},"state":{{"status":"completed","outputText":"a"}}}}"#
        );
        assert!(matches!(&p.line(&shell)[..], [ClaudeEvent::ToolUse { name, .. }, ClaudeEvent::ToolResult { is_error: false, .. }] if name == "Bash"));
        let reply = format!(r#"{{"type":"message","role":"assistant","id":"e2","sessionId":"s1","createdAt":{now},"content":[{{"type":"text","text":"hi"}}]}}"#);
        assert!(matches!(&p.line(&reply)[..], [ClaudeEvent::AssistantText(t)] if t == "hi"));
        assert!(matches!(&p.finish(true)[..], [ClaudeEvent::Done { is_error: false, text: Some(t), .. }] if t == "hi"));
    }
}
