//! Workbench's own MCP client, for the providers that have none (the chat APIs).
//!
//! It keeps one connection per active MCP server, shared by every chat, so a server is
//! started once and stays connected across turns. A connection is opened the first time
//! a turn asks for the tools, and reopened when the server's config or project changes.
//! The agent CLIs (Workbench Code, Codex, Vibe) connect to the servers themselves instead.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::service::{Peer, RoleClient, RunningService, ServiceExt};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::claude::McpStatus;
use crate::mcp::{self, Server, Transport};
use crate::platform;

/// How long a server may take to start and answer the MCP handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Providers only accept tool names of letters, digits, `_` and `-`, up to 64 characters.
const MAX_TOOL_NAME: usize = 64;

/// One tool of one server, as offered to a model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    /// The name the model sees: `mcp__<server>__<tool>`, cleaned up for the provider APIs.
    pub name: String,
    pub description: String,
    /// The tool's JSON schema for its arguments.
    pub input_schema: Value,
    /// The server says the tool doesn't change anything, so Plan mode may use it.
    pub read_only: bool,
    server: String,
    tool: String,
}

struct Connection {
    /// The config and project folder the connection was opened with.
    server: Server,
    cwd: PathBuf,
    /// Dropping the service closes the connection and stops a stdio server.
    service: RunningService<RoleClient, ()>,
    tools: Vec<ToolSpec>,
}

/// The open connections, by server name.
static CONNECTIONS: LazyLock<Mutex<HashMap<String, Connection>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Connects to every active server that isn't connected yet and returns all their tools,
/// plus a status per server for the MCP tab. Connections to servers that are gone or changed are closed.
pub async fn tools(cwd: &Path, servers: &[Server]) -> (Vec<ToolSpec>, Vec<McpStatus>) {
    let active = mcp::active(servers);
    let mut connections = CONNECTIONS.lock().await;
    connections.retain(|name, c| active.iter().any(|s| s.name.trim() == name && **s == c.server && c.cwd == cwd));

    let mut statuses = Vec::new();
    for server in active {
        let name = server.name.trim().to_string();
        if !connections.contains_key(&name) {
            match connect(cwd, server).await {
                Ok(connection) => {
                    connections.insert(name.clone(), connection);
                }
                Err(e) => {
                    statuses.push(McpStatus { name, status: format!("failed: {e}") });
                    continue;
                }
            }
        }
        statuses.push(McpStatus { name, status: "connected".into() });
    }
    let tools = connections.values().flat_map(|c| c.tools.clone()).collect();
    (tools, statuses)
}

/// Calls a tool and returns its text output. A tool that reports an error returns `Err`.
pub async fn call(tool: &ToolSpec, arguments: Value) -> Result<String, String> {
    let peer: Peer<RoleClient> = {
        let connections = CONNECTIONS.lock().await;
        let connection = connections.get(&tool.server).ok_or_else(|| format!("MCP server {} is not connected", tool.server))?;
        connection.service.peer().clone()
    };
    let mut params = CallToolRequestParams::new(tool.tool.clone());
    if let Value::Object(arguments) = arguments {
        params = params.with_arguments(arguments);
    }
    let result = peer.call_tool(params).await.map_err(|e| e.to_string())?;
    let result = serde_json::to_value(result).unwrap_or_default();
    let text = result_text(&result);
    if result["isError"].as_bool() == Some(true) { Err(text) } else { Ok(text) }
}

async fn connect(cwd: &Path, server: &Server) -> Result<Connection, String> {
    let service = match server.transport {
        Transport::Stdio => {
            let mut cmd = tokio::process::Command::new(platform::program(&mcp::expand_env(server.command.trim())));
            cmd.args(server.args_for_cli().into_iter().map(mcp::expand_env))
                .envs(server.env_map().into_iter().map(|(k, v)| (k, mcp::expand_env(v.as_str().unwrap_or_default()))))
                .env("PATH", platform::augmented_path())
                .current_dir(cwd);
            let (transport, _stderr) =
                TokioChildProcess::builder(cmd).stderr(std::process::Stdio::null()).spawn().map_err(|e| e.to_string())?;
            tokio::time::timeout(CONNECT_TIMEOUT, ().serve(transport)).await
        }
        Transport::Http => {
            let transport = StreamableHttpClientTransport::from_config(http_config(server)?);
            tokio::time::timeout(CONNECT_TIMEOUT, ().serve(transport)).await
        }
        Transport::Sse => return Err("SSE servers only work with the agent CLIs; use the server's http URL instead".into()),
    }
    .map_err(|_| "timed out while connecting".to_string())?
    .map_err(|e| e.to_string())?;

    let listed = service.peer().list_all_tools().await.map_err(|e| e.to_string())?;
    let name = server.name.trim().to_string();
    let tools = listed
        .into_iter()
        .map(|t| serde_json::to_value(t).unwrap_or_default())
        .map(|t| tool_spec(&name, &t))
        .collect();
    Ok(Connection { server: server.clone(), cwd: cwd.to_path_buf(), service, tools })
}

/// The URL and headers of an http server, with `${VAR}` references filled in.
fn http_config(server: &Server) -> Result<StreamableHttpClientTransportConfig, String> {
    let mut headers = HashMap::new();
    for (key, value) in server.header_map() {
        let name = key.parse().map_err(|_| format!("bad header name: {key}"))?;
        let value = mcp::expand_env(value.as_str().unwrap_or_default()).parse().map_err(|_| format!("bad value for header {key}"))?;
        headers.insert(name, value);
    }
    Ok(StreamableHttpClientTransportConfig::with_uri(mcp::expand_env(server.url.trim())).custom_headers(headers))
}

/// A tool as the MCP server listed it (JSON), turned into what a model is offered.
fn tool_spec(server: &str, tool: &Value) -> ToolSpec {
    let tool_name = tool["name"].as_str().unwrap_or_default();
    ToolSpec {
        name: model_tool_name(server, tool_name),
        description: tool["description"].as_str().unwrap_or_default().to_string(),
        input_schema: tool.get("inputSchema").cloned().unwrap_or_else(|| json!({ "type": "object" })),
        read_only: tool["annotations"]["readOnlyHint"].as_bool() == Some(true),
        server: server.to_string(),
        tool: tool_name.to_string(),
    }
}

fn model_tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .take(MAX_TOOL_NAME)
        .collect()
}

/// The text blocks of a tool result. Other blocks (images, resources) are passed on as JSON.
fn result_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|block| block["text"].as_str().map(String::from).unwrap_or_else(|| block.to_string()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_valid_for_the_provider_apis() {
        assert_eq!(model_tool_name("my files", "read.file"), "mcp__my_files__read_file");
        assert_eq!(model_tool_name(&"s".repeat(80), "t").len(), MAX_TOOL_NAME);
    }

    #[test]
    fn tool_spec_reads_schema_and_read_only_hint() {
        let tool = json!({
            "name": "read",
            "description": "Reads",
            "inputSchema": { "type": "object", "properties": { "path": { "type": "string" } } },
            "annotations": { "readOnlyHint": true },
        });
        let spec = tool_spec("files", &tool);
        assert_eq!(spec.name, "mcp__files__read");
        assert!(spec.read_only);
        assert_eq!(spec.input_schema["properties"]["path"]["type"], "string");
        assert!(!tool_spec("files", &json!({ "name": "write" })).read_only);
    }

    #[test]
    fn result_text_joins_text_blocks() {
        let result = json!({ "content": [{ "type": "text", "text": "a" }, { "type": "text", "text": "b" }] });
        assert_eq!(result_text(&result), "a\nb");
    }
}
