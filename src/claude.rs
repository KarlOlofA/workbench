use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};

use crate::platform;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone)]
pub enum ClaudeEvent {
    Init { session_id: String, model: String, mcp_servers: Vec<McpStatus> },
    TextDelta(String),
    AssistantText(String),
    ToolUse { id: String, name: String, input: Value },
    ToolResult { id: String, content: String, is_error: bool },
    Done { cost_usd: Option<f64>, is_error: bool, text: Option<String> },
    Permission(PermissionRequest),
    ControlRequest { request_id: String, subtype: String },
    ControlCancel { request_id: String },
    Stderr(String),
    Exited,
}

/// One MCP server as reported by the agent when a session starts, e.g. status "connected" or "failed".
#[derive(Debug, Clone, PartialEq)]
pub struct McpStatus {
    pub name: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub request_id: String,
    pub tool: String,
    pub input: Value,
    pub suggestions: Vec<Value>,
    pub blocked_path: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionDecision {
    Allow { input: Value, permissions: Vec<Value> },
    Deny { message: String, interrupt: bool },
}

impl PermissionRequest {
    pub fn always_rules(&self) -> (Vec<Value>, String) {
        if !self.suggestions.is_empty() {
            let labels: Vec<String> = self.suggestions.iter().filter_map(suggestion_label).collect();
            let label = if labels.is_empty() { format!("{} like this", self.tool) } else { labels.join(", ") };
            return (self.suggestions.clone(), label);
        }
        let rule = json!({
            "type": "addRules",
            "rules": [{ "toolName": self.tool }],
            "behavior": "allow",
            "destination": "session",
        });
        (vec![rule], format!("any {} call for the rest of this session", self.tool))
    }
}

fn suggestion_label(s: &Value) -> Option<String> {
    let place = match s.get("destination").and_then(Value::as_str) {
        Some("session") => " for this session",
        Some("localSettings") => " in this project (local settings)",
        Some("projectSettings") => " in this project (shared settings)",
        Some("userSettings") => " in all projects",
        _ => "",
    };
    let what = match s.get("type").and_then(Value::as_str)? {
        "addRules" | "replaceRules" => s
            .get("rules")?
            .as_array()?
            .iter()
            .filter_map(|r| {
                let tool = r.get("toolName").and_then(Value::as_str)?;
                Some(match r.get("ruleContent").and_then(Value::as_str) {
                    Some(c) if !c.is_empty() => format!("{tool}({c})"),
                    _ => tool.to_string(),
                })
            })
            .collect::<Vec<_>>()
            .join(", "),
        "setMode" => format!("switch to {} mode", s.get("mode").and_then(Value::as_str)?),
        "addDirectories" => format!(
            "access to {}",
            s.get("directories")?.as_array()?.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")
        ),
        _ => return None,
    };
    (!what.is_empty()).then(|| format!("{what}{place}"))
}

/// What the agent may do on its own. It decides both how the agent acts and when it asks.
///
/// - `Plan` (the base mode): read only. The agent asks questions until it knows exactly what
///   to work on, then presents a plan; Workbench then asks which mode to run it in.
/// - `AcceptEdits`: the agent asks before every change to the code and every command.
/// - `Auto`: the agent edits and runs commands itself, but asks before deleting files.
/// - `FullAuto`: the agent never asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatMode {
    Plan,
    AcceptEdits,
    Auto,
    FullAuto,
}

/// Plan mode tool for presenting a finished plan; using it asks to leave plan mode.
pub const EXIT_PLAN_TOOL: &str = "ExitPlanMode";

impl ChatMode {
    pub const ALL: [ChatMode; 4] = [ChatMode::Plan, ChatMode::AcceptEdits, ChatMode::Auto, ChatMode::FullAuto];
    /// The modes a finished plan can be run in.
    pub const RUN_PLAN: [ChatMode; 3] = [ChatMode::Auto, ChatMode::FullAuto, ChatMode::AcceptEdits];

    pub fn label(self) -> &'static str {
        match self {
            ChatMode::Plan => "Plan",
            ChatMode::AcceptEdits => "Accept edits",
            ChatMode::Auto => "Auto",
            ChatMode::FullAuto => "Full auto",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            ChatMode::Plan => "Read only: the agent asks questions until it knows exactly what to do, then presents a plan for you to approve",
            ChatMode::AcceptEdits => "You accept every change to the code and every command the agent runs",
            ChatMode::Auto => "The agent edits and runs commands itself, but asks before deleting files (rm, rmdir, unlink, shred, git rm, find -delete)",
            ChatMode::FullAuto => "The agent acts on the task with full permission and never asks",
        }
    }

    /// The `--permission-mode` the agent runs in. Auto and Full auto run in `acceptEdits`
    /// and Workbench answers the remaining permission requests itself (see `auto_allows`).
    pub fn cli_arg(self) -> &'static str {
        match self {
            ChatMode::Plan => "plan",
            ChatMode::AcceptEdits => "default",
            ChatMode::Auto | ChatMode::FullAuto => "acceptEdits",
        }
    }

    /// Whether Workbench allows a permission request without asking. A plan is never
    /// approved without asking.
    pub fn auto_allows(self, req: &PermissionRequest) -> bool {
        if req.tool == EXIT_PLAN_TOOL {
            return false;
        }
        match self {
            ChatMode::Plan | ChatMode::AcceptEdits => false,
            ChatMode::Auto => !deletes_files(req),
            ChatMode::FullAuto => true,
        }
    }
}

const DELETE_COMMANDS: [&str; 4] = ["rm", "rmdir", "unlink", "shred"];

pub fn deletes_files(req: &PermissionRequest) -> bool {
    if req.tool != "Bash" {
        return false;
    }
    let command = req.input.get("command").and_then(Value::as_str).unwrap_or_default();
    command_deletes_files(command)
}

fn command_deletes_files(command: &str) -> bool {
    let words: Vec<&str> = command
        .split(|c: char| c.is_whitespace() || ";&|()<>`$'\"{}\\".contains(c))
        .filter(|w| !w.is_empty())
        .collect();
    words.iter().enumerate().any(|(i, w)| {
        let name = w.rsplit('/').next().unwrap_or(w);
        DELETE_COMMANDS.contains(&name) || (name == "-delete") || (name == "rm" && i > 0 && words[i - 1] == "git")
    })
}

pub struct SpawnOptions {
    pub cwd: PathBuf,
    pub resume: Option<String>,
    pub mode: ChatMode,
    pub append_system_prompt: Option<String>,
    pub mcp_config: Option<String>,
    /// Empty means the CLI's own default model.
    pub model: String,
}

pub struct ClaudeProcess {
    child: Child,
    stdin: ChildStdin,
    request_counter: u64,
}

pub fn find_claude_binary() -> PathBuf {
    // The native installer's locations win over whatever else is on PATH.
    let exe = if cfg!(windows) { "claude.exe" } else { "claude" };
    platform::home_dir()
        .into_iter()
        .flat_map(|h| [h.join(".local").join("bin").join(exe), h.join(".claude").join("local").join(exe)])
        .find(|p| p.is_file())
        .unwrap_or_else(|| platform::program("claude"))
}

pub use crate::platform::augmented_path;

impl ClaudeProcess {
    pub fn spawn(opts: &SpawnOptions, generation: u64, tx: UnboundedSender<(u64, ClaudeEvent)>) -> Result<Self> {
        let mut cmd = Command::new(find_claude_binary());
        cmd.args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-mode",
            opts.mode.cli_arg(),
            "--permission-prompt-tool",
            "stdio",
            // Workbench can't answer the agent's question form, so it asks questions in the reply.
            "--disallowedTools",
            "AskUserQuestion",
        ]);
        if let Some(id) = &opts.resume {
            cmd.args(["--resume", id]);
        }
        if let Some(extra) = &opts.append_system_prompt {
            cmd.args(["--append-system-prompt", extra]);
        }
        if !opts.model.trim().is_empty() {
            cmd.args(["--model", opts.model.trim()]);
        }
        if let Some(config) = &opts.mcp_config {
            cmd.args(["--mcp-config", config]);
        }
        cmd.current_dir(&opts.cwd)
            .env("PATH", augmented_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().context("failed to start claude")?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;

        let out_tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                for ev in parse_line(&line) {
                    if out_tx.send((generation, ev)).is_err() {
                        return;
                    }
                }
            }
            let _ = out_tx.send((generation, ClaudeEvent::Exited));
        });

        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() && tx.send((generation, ClaudeEvent::Stderr(line))).is_err() {
                    return;
                }
            }
        });

        Ok(Self { child, stdin, request_counter: 0 })
    }

    async fn write_json(&mut self, value: Value) -> Result<()> {
        let mut line = serde_json::to_string(&value)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    pub async fn send_user(&mut self, text: &str) -> Result<()> {
        self.write_json(json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": text }]
            }
        }))
        .await
    }

    pub async fn interrupt(&mut self) -> Result<()> {
        self.request_counter += 1;
        let id = format!("ui-{}", self.request_counter);
        self.write_json(json!({
            "type": "control_request",
            "request_id": id,
            "request": { "subtype": "interrupt" }
        }))
        .await
    }

    pub async fn respond_permission(&mut self, request_id: &str, decision: &PermissionDecision) -> Result<()> {
        let response = match decision {
            PermissionDecision::Allow { input, permissions } => {
                let mut r = json!({ "behavior": "allow", "updatedInput": input });
                if !permissions.is_empty() {
                    r["updatedPermissions"] = Value::Array(permissions.clone());
                }
                r
            }
            PermissionDecision::Deny { message, interrupt } => {
                let mut r = json!({ "behavior": "deny", "message": message });
                if *interrupt {
                    r["interrupt"] = Value::Bool(true);
                }
                r
            }
        };
        self.write_json(json!({
            "type": "control_response",
            "response": { "subtype": "success", "request_id": request_id, "response": response }
        }))
        .await
    }

    pub async fn respond_error(&mut self, request_id: &str, error: &str) -> Result<()> {
        self.write_json(json!({
            "type": "control_response",
            "response": { "subtype": "error", "request_id": request_id, "error": error }
        }))
        .await
    }

    pub async fn kill(mut self) {
        let _ = self.child.kill().await;
    }

    pub fn exit_code(&mut self) -> Option<i32> {
        self.child.try_wait().ok().flatten().and_then(|s| s.code())
    }
}

fn content_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|i| match i.get("type").and_then(Value::as_str) {
                Some("text") => i.get("text").and_then(Value::as_str).map(String::from),
                Some(other) => Some(format!("[{other}]")),
                None => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn parse_line(line: &str) -> Vec<ClaudeEvent> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return if line.trim().is_empty() {
            vec![]
        } else {
            vec![ClaudeEvent::Stderr(line.to_string())]
        };
    };
    let str_at = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).unwrap_or_default().to_string();

    match v.get("type").and_then(Value::as_str) {
        Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => {
            let mcp_servers = v["mcp_servers"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|m| McpStatus { name: str_at(m, "name"), status: str_at(m, "status") })
                .collect();
            vec![ClaudeEvent::Init { session_id: str_at(&v, "session_id"), model: str_at(&v, "model"), mcp_servers }]
        }
        Some("stream_event") => {
            let ev = &v["event"];
            if ev.get("type").and_then(Value::as_str) == Some("content_block_delta")
                && ev["delta"].get("type").and_then(Value::as_str) == Some("text_delta")
            {
                vec![ClaudeEvent::TextDelta(str_at(&ev["delta"], "text"))]
            } else {
                vec![]
            }
        }
        Some("assistant") => v["message"]["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                        Some("text") => Some(ClaudeEvent::AssistantText(str_at(b, "text"))),
                        Some("tool_use") => Some(ClaudeEvent::ToolUse {
                            id: str_at(b, "id"),
                            name: str_at(b, "name"),
                            input: b.get("input").cloned().unwrap_or(Value::Null),
                        }),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        Some("user") => v["message"]["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                    .map(|b| ClaudeEvent::ToolResult {
                        id: str_at(b, "tool_use_id"),
                        content: content_to_string(b.get("content").unwrap_or(&Value::Null)),
                        is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        Some("control_request") => {
            let req = &v["request"];
            let request_id = str_at(&v, "request_id");
            match req.get("subtype").and_then(Value::as_str) {
                Some("can_use_tool") => vec![ClaudeEvent::Permission(PermissionRequest {
                    request_id,
                    tool: str_at(req, "tool_name"),
                    input: req.get("input").cloned().unwrap_or(Value::Null),
                    suggestions: req.get("permission_suggestions").and_then(Value::as_array).cloned().unwrap_or_default(),
                    blocked_path: req.get("blocked_path").and_then(Value::as_str).map(String::from),
                    reason: req.get("decision_reason").and_then(Value::as_str).map(String::from),
                })],
                other => vec![ClaudeEvent::ControlRequest { request_id, subtype: other.unwrap_or_default().to_string() }],
            }
        }
        Some("control_cancel_request") => vec![ClaudeEvent::ControlCancel { request_id: str_at(&v, "request_id") }],
        Some("result") => vec![ClaudeEvent::Done {
            cost_usd: v.get("total_cost_usd").and_then(Value::as_f64),
            is_error: v.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            text: v.get("result").and_then(Value::as_str).map(String::from),
        }],
        _ => vec![],
    }
}

pub fn tool_summary(name: &str, input: &Value, cwd: &Path) -> String {
    let rel = |p: &str| {
        Path::new(p)
            .strip_prefix(cwd)
            .map(|r| r.display().to_string())
            .unwrap_or_else(|_| p.to_string())
    };
    let s = |k: &str| input.get(k).and_then(Value::as_str);
    match name {
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            s("file_path").or(s("notebook_path")).map(rel).unwrap_or_default()
        }
        "Bash" => s("command").unwrap_or_default().to_string(),
        "Grep" | "Glob" => s("pattern").unwrap_or_default().to_string(),
        "WebFetch" => s("url").unwrap_or_default().to_string(),
        "WebSearch" => s("query").unwrap_or_default().to_string(),
        "Task" | "Agent" => s("description").unwrap_or_default().to_string(),
        _ => String::new(),
    }
}

pub fn tool_touches_files(name: &str) -> bool {
    matches!(name, "Write" | "Edit" | "MultiEdit" | "NotebookEdit" | "Bash")
}

pub fn tool_file_path(input: &Value) -> Option<PathBuf> {
    input
        .get("file_path")
        .or_else(|| input.get("notebook_path"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_delta() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}}"#;
        assert!(matches!(&parse_line(line)[..], [ClaudeEvent::TextDelta(t)] if t == "hi"));
    }

    #[test]
    fn parses_tool_use_and_result() {
        let a = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"},{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/a/b.rs"}}]}}"#;
        let evs = parse_line(a);
        assert_eq!(evs.len(), 2);
        assert!(matches!(&evs[1], ClaudeEvent::ToolUse { name, .. } if name == "Edit"));
        let u = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"done"}],"is_error":false}]}}"#;
        assert!(matches!(&parse_line(u)[..], [ClaudeEvent::ToolResult { content, .. }] if content == "done"));
    }

    #[test]
    fn modes_decide_which_requests_are_allowed_without_asking() {
        for cmd in ["rm -rf target", "cargo build && rm x", "find . -name '*.o' -delete", "xargs /bin/rm", "bash -c \"rm a\"", "git rm src/a.rs", "sudo rmdir d", "ls | xargs rm"] {
            assert!(command_deletes_files(cmd), "{cmd}");
        }
        for cmd in ["cargo test", "docker run --rm img", "ls -la", "git status", "npm run format", "echo perm"] {
            assert!(!command_deletes_files(cmd), "{cmd}");
        }
        let req = |tool: &str, input: Value| PermissionRequest {
            request_id: "r".into(),
            tool: tool.into(),
            input,
            suggestions: Vec::new(),
            blocked_path: None,
            reason: None,
        };
        let build = req("Bash", serde_json::json!({"command": "cargo build"}));
        let delete = req("Bash", serde_json::json!({"command": "rm -r build"}));
        let plan = req(EXIT_PLAN_TOOL, serde_json::json!({"plan": "1. do it"}));
        assert!(ChatMode::Auto.auto_allows(&build));
        assert!(!ChatMode::Auto.auto_allows(&delete));
        assert!(ChatMode::Auto.auto_allows(&req("WebFetch", serde_json::json!({"url": "https://example.com"}))));
        assert!(ChatMode::FullAuto.auto_allows(&delete));
        assert!(!ChatMode::AcceptEdits.auto_allows(&build));
        assert!(!ChatMode::Plan.auto_allows(&build));
        assert!(ChatMode::ALL.iter().all(|m| !m.auto_allows(&plan)));
        assert_eq!(ChatMode::AcceptEdits.cli_arg(), "default");
    }

    #[test]
    fn parses_permission_requests() {
        let line = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"npm test"},"permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"npm test:*"}],"behavior":"allow","destination":"localSettings"}]}}"#;
        let evs = parse_line(line);
        let [ClaudeEvent::Permission(p)] = &evs[..] else { panic!("no permission event") };
        assert_eq!(p.request_id, "r1");
        assert_eq!(p.tool, "Bash");
        let (rules, label) = p.always_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(label, "Bash(npm test:*) in this project (local settings)");

        let bare = PermissionRequest { tool: "WebFetch".into(), ..p.clone() };
        let bare = PermissionRequest { suggestions: vec![], ..bare };
        let (rules, label) = bare.always_rules();
        assert_eq!(rules[0]["destination"], "session");
        assert!(label.contains("WebFetch"));

        let other = r#"{"type":"control_request","request_id":"r2","request":{"subtype":"hook_callback"}}"#;
        assert!(matches!(&parse_line(other)[..], [ClaudeEvent::ControlRequest { subtype, .. }] if subtype == "hook_callback"));
        let cancel = r#"{"type":"control_cancel_request","request_id":"r1"}"#;
        assert!(matches!(&parse_line(cancel)[..], [ClaudeEvent::ControlCancel { request_id }] if request_id == "r1"));
    }

    #[test]
    fn parses_init_and_result() {
        let i = r#"{"type":"system","subtype":"init","session_id":"s1","model":"m","mcp_servers":[{"name":"docs","status":"connected"}]}"#;
        assert!(matches!(&parse_line(i)[..], [ClaudeEvent::Init { session_id, mcp_servers, .. }]
            if session_id == "s1" && mcp_servers[0] == McpStatus { name: "docs".into(), status: "connected".into() }));
        let r = r#"{"type":"result","subtype":"success","is_error":false,"result":"x","total_cost_usd":0.5}"#;
        assert!(matches!(&parse_line(r)[..], [ClaudeEvent::Done { cost_usd: Some(c), .. }] if *c == 0.5));
    }
}
