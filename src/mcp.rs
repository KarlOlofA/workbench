//! MCP server connections for this project.
//!
//! The connections live in `.workbench/mcp.json`. Every enabled connection is
//! handed to the agent CLI in use (Codex, Vibe or Workbench), so the agent can call its tools.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub const MCP_FILE: &str = ".workbench/mcp.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// A local program that speaks MCP over stdin/stdout.
    #[default]
    Stdio,
    /// A remote server reached over streamable HTTP.
    Http,
    /// A remote server reached over server-sent events (older servers).
    Sse,
}

impl Transport {
    pub const ALL: [Transport; 3] = [Transport::Stdio, Transport::Http, Transport::Sse];

    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Stdio => "stdio",
            Transport::Http => "http",
            Transport::Sse => "sse",
        }
    }

    pub fn from_str(s: &str) -> Self {
        Self::ALL.into_iter().find(|t| t.as_str() == s).unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Server {
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub transport: Transport,
    /// stdio only: the program to run.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    /// stdio only: one argument per entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// stdio only: `KEY=value` entries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    /// http/sse only: the server URL.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// http/sse only: `Name: value` entries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<String>,
}

fn yes() -> bool {
    true
}

impl Server {
    /// A server can only be passed to Workbench once it has a name and something to connect to.
    pub fn is_usable(&self) -> bool {
        let target = match self.transport {
            Transport::Stdio => &self.command,
            Transport::Http | Transport::Sse => &self.url,
        };
        self.enabled && !self.name.trim().is_empty() && !target.trim().is_empty()
    }

    /// The arguments without the blank lines the editor keeps.
    pub fn args_for_cli(&self) -> Vec<&str> {
        self.args.iter().map(|a| a.trim()).filter(|a| !a.is_empty()).collect()
    }

    pub fn env_map(&self) -> Map<String, Value> {
        pairs(&self.env, '=')
    }

    pub fn header_map(&self) -> Map<String, Value> {
        pairs(&self.headers, ':')
    }

    /// The entry for this server in Workbench's `mcpServers` config.
    fn claude_entry(&self) -> Value {
        match self.transport {
            Transport::Stdio => json!({
                "type": "stdio",
                "command": self.command.trim(),
                "args": self.args_for_cli(),
                "env": self.env_map(),
            }),
            Transport::Http | Transport::Sse => json!({
                "type": self.transport.as_str(),
                "url": self.url.trim(),
                "headers": self.header_map(),
            }),
        }
    }
}

/// Turns `KEY=value` (or `Name: value`) lines into a JSON object, skipping lines without the separator.
fn pairs(lines: &[String], separator: char) -> Map<String, Value> {
    lines
        .iter()
        .filter_map(|line| line.split_once(separator))
        .map(|(key, value)| (key.trim().to_string(), Value::String(value.trim().to_string())))
        .filter(|(key, _)| !key.is_empty())
        .collect()
}

/// The JSON passed to `claude --mcp-config`, or None when no server is usable.
pub fn claude_config(servers: &[Server]) -> Option<String> {
    let entries: Map<String, Value> = servers
        .iter()
        .filter(|s| s.is_usable())
        .map(|s| (s.name.trim().to_string(), s.claude_entry()))
        .collect();
    (!entries.is_empty()).then(|| json!({ "mcpServers": entries }).to_string())
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
struct McpFile {
    #[serde(default)]
    servers: Vec<Server>,
}

pub fn load(cwd: &Path) -> Result<Vec<Server>, String> {
    match std::fs::read_to_string(cwd.join(MCP_FILE)) {
        Ok(text) => serde_json::from_str::<McpFile>(&text)
            .map(|f| f.servers)
            .map_err(|e| format!("{MCP_FILE}: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{MCP_FILE}: {e}")),
    }
}

pub fn save(cwd: &Path, servers: &[Server]) -> Result<(), String> {
    let path = cwd.join(MCP_FILE);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut text = serde_json::to_string_pretty(&McpFile { servers: servers.to_vec() }).map_err(|e| e.to_string())?;
    text.push('\n');
    std::fs::write(path, text).map_err(|e| format!("{MCP_FILE}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_contains_only_usable_servers() {
        let servers = vec![
            Server {
                name: "files".into(),
                enabled: true,
                command: "npx".into(),
                args: vec!["-y".into(), "".into(), "server-files".into()],
                env: vec!["TOKEN=abc".into(), "broken".into()],
                ..Default::default()
            },
            Server {
                name: "docs".into(),
                enabled: true,
                transport: Transport::Http,
                url: "https://example.com/mcp".into(),
                headers: vec!["Authorization: Bearer x".into()],
                ..Default::default()
            },
            Server { name: "off".into(), enabled: false, command: "x".into(), ..Default::default() },
            Server { name: "empty".into(), enabled: true, ..Default::default() },
        ];
        let config: Value = serde_json::from_str(&claude_config(&servers).unwrap()).unwrap();
        let entries = config["mcpServers"].as_object().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries["files"]["args"], json!(["-y", "server-files"]));
        assert_eq!(entries["files"]["env"], json!({ "TOKEN": "abc" }));
        assert_eq!(entries["docs"]["type"], "http");
        assert_eq!(entries["docs"]["headers"], json!({ "Authorization": "Bearer x" }));
    }

    #[test]
    fn no_config_without_servers() {
        assert_eq!(claude_config(&[]), None);
    }
}
