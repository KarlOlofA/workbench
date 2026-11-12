//! MCP server connections, configured at two levels:
//! - user servers in `mcp.json` in the user's config folder, used in every project and kept across updates;
//! - project servers in `.workbench/mcp.json`, committed with the project.
//!
//! Every active server is handed to the agent CLI in use (Codex, Vibe or Workbench), and the
//! API providers reach them through `mcp_manager`, so every provider can call their tools.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// A remote MCP server Workbench knows, shown as a toggle with its logo in the MCP tab.
/// Turning it on adds it as a project server under its name.
pub struct Preset {
    pub name: &'static str,
    pub url: &'static str,
    pub logo: crate::logos::Logo,
}

pub const PRESETS: [Preset; 1] =
    [Preset { name: "linear", url: "https://mcp.linear.app/mcp", logo: crate::logos::Logo::Linear }];

/// The preset a server was added from: a project server with a preset's name.
pub fn preset(server: &Server) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| server.scope == Scope::Project && server.name.trim() == p.name)
}

pub const MCP_FILE: &str = ".workbench/mcp.json";
/// The user's servers, in the user's config folder.
pub const USER_MCP_FILE: &str = "mcp.json";

/// Which file a server is stored in. Not saved: it follows from the file the server was loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// The user's own servers, used in every project.
    User,
    /// The opened project's servers, committed with it.
    #[default]
    Project,
}

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
    #[serde(skip)]
    pub scope: Scope,
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

/// Whether a project server with the same name replaces this user server.
pub fn is_overridden(server: &Server, servers: &[Server]) -> bool {
    server.scope == Scope::User
        && servers.iter().any(|s| s.scope == Scope::Project && s.is_usable() && s.name.trim() == server.name.trim())
}

/// The servers every provider gets: the usable ones, where a project server replaces a user server of the same name.
pub fn active(servers: &[Server]) -> Vec<&Server> {
    servers.iter().filter(|s| s.is_usable() && !is_overridden(s, servers)).collect()
}

/// Replaces every `${VAR}` with the environment variable's value (empty when it is not set).
pub fn expand_env(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let Some(len) = rest[start + 2..].find('}') else { break };
        out.push_str(&rest[..start]);
        out.push_str(&std::env::var(&rest[start + 2..start + 2 + len]).unwrap_or_default());
        rest = &rest[start + 2 + len + 1..];
    }
    out.push_str(rest);
    out
}

/// The JSON passed to `claude --mcp-config`, or None when no server is active.
pub fn claude_config(servers: &[Server]) -> Option<String> {
    let entries: Map<String, Value> = active(servers)
        .into_iter()
        .map(|s| (s.name.trim().to_string(), s.claude_entry()))
        .collect();
    (!entries.is_empty()).then(|| json!({ "mcpServers": entries }).to_string())
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
struct McpFile {
    #[serde(default)]
    servers: Vec<Server>,
}

fn user_file() -> Option<PathBuf> {
    crate::config::config_path(USER_MCP_FILE)
}

/// The user's servers followed by the project's servers.
pub fn load(cwd: &Path) -> Result<Vec<Server>, String> {
    let mut servers = match user_file() {
        Some(path) => load_file(&path, Scope::User)?,
        None => Vec::new(),
    };
    servers.extend(load_file(&cwd.join(MCP_FILE), Scope::Project)?);
    Ok(servers)
}

/// Writes the user servers to the user file and the project servers to the project file.
pub fn save(cwd: &Path, servers: &[Server]) -> Result<(), String> {
    let of = |scope| servers.iter().filter(|s| s.scope == scope).cloned().collect::<Vec<_>>();
    if let Some(path) = user_file() {
        save_file(&path, of(Scope::User))?;
    }
    save_file(&cwd.join(MCP_FILE), of(Scope::Project))
}

fn load_file(path: &Path, scope: Scope) -> Result<Vec<Server>, String> {
    let shown = path.display();
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<McpFile>(&text)
            .map(|f| f.servers.into_iter().map(|s| Server { scope, ..s }).collect())
            .map_err(|e| format!("{shown}: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{shown}: {e}")),
    }
}

/// Saves the list, except that an empty list doesn't create a file that isn't there yet.
fn save_file(path: &Path, servers: Vec<Server>) -> Result<(), String> {
    let shown = path.display();
    if servers.is_empty() && !path.exists() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{shown}: {e}"))?;
    }
    let mut text = serde_json::to_string_pretty(&McpFile { servers }).map_err(|e| e.to_string())?;
    text.push('\n');
    std::fs::write(path, text).map_err(|e| format!("{shown}: {e}"))
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
    fn project_server_replaces_user_server_of_same_name() {
        let servers = vec![
            Server { name: "docs".into(), enabled: true, command: "user-docs".into(), scope: Scope::User, ..Default::default() },
            Server { name: "files".into(), enabled: true, command: "files".into(), scope: Scope::User, ..Default::default() },
            Server { name: "docs".into(), enabled: true, command: "project-docs".into(), scope: Scope::Project, ..Default::default() },
        ];
        let commands: Vec<&str> = active(&servers).iter().map(|s| s.command.as_str()).collect();
        assert_eq!(commands, ["files", "project-docs"]);
        assert!(is_overridden(&servers[0], &servers));
    }

    #[test]
    fn save_and_load_keep_each_scope_in_its_file() {
        let dir = std::env::temp_dir().join(format!("wb-mcp-test-{}", std::process::id()));
        let project = Server { name: "p".into(), command: "x".into(), scope: Scope::Project, ..Default::default() };
        save_file(&dir.join(MCP_FILE), vec![project.clone()]).unwrap();
        assert_eq!(load_file(&dir.join(MCP_FILE), Scope::Project).unwrap(), vec![project]);
        assert_eq!(load_file(&dir.join(MCP_FILE), Scope::User).unwrap()[0].scope, Scope::User);
        save_file(&dir.join("absent.json"), Vec::new()).unwrap();
        assert!(!dir.join("absent.json").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn expands_environment_variables() {
        // PATH is set in every test environment.
        let path = std::env::var("PATH").unwrap();
        assert_eq!(expand_env("a ${PATH} b"), format!("a {path} b"));
        assert_eq!(expand_env("${WB_SURELY_UNSET_VAR}x"), "x");
        assert_eq!(expand_env("no ${end"), "no ${end");
    }

    #[test]
    fn no_config_without_servers() {
        assert_eq!(claude_config(&[]), None);
    }
}
