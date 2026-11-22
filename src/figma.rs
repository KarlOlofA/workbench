//! The repository's Figma files. The MCP tab binds a repository to Figma files pasted by URL;
//! Figma no longer gives personal access tokens the projects:read scope, so projects can't be
//! listed with them. The binding is stored in `.workbench/figma.json`, which is git-ignored so file keys are not published;
//! the token is stored in the user's config folder. Agents read the designs themselves through the
//! Figma MCP preset (figma-developer-mcp), which gets the same token on each message.

use std::path::Path;

use crate::mcp::{Server, Transport};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const FIGMA_FILE: &str = ".workbench/figma.json";
/// The personal access token, in the user's config folder.
pub const TOKEN_FILE: &str = "figma-token.json";
/// The graph node kind that stands for the repository's Figma files.
pub const FIGMA_KIND: &str = "figma";
/// The name of the Figma MCP preset server.
pub const SERVER_NAME: &str = "figma";

const API: &str = "https://api.figma.com/v1";

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FigmaFile {
    pub key: String,
    pub name: String,
}

/// The Figma files a repository is bound to.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Binding {
    #[serde(default)]
    pub files: Vec<FigmaFile>,
    /// The key of the file that holds the design system: colours, fonts, radii.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_file: Option<String>,
}

impl Binding {
    pub fn theme(&self) -> Option<&FigmaFile> {
        let key = self.theme_file.as_deref()?;
        self.files.iter().find(|f| f.key == key)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Token {
    pub token: String,
}

pub fn saved_token() -> String {
    crate::config::load_json::<Token>(TOKEN_FILE).token
}

/// Hands the token to the Figma MCP server as FIGMA_API_KEY, so it never has to be written
/// into the project's committed `.workbench/mcp.json`.
pub fn with_token(mut servers: Vec<Server>, token: &str) -> Vec<Server> {
    if token.trim().is_empty() {
        return servers;
    }
    for server in servers.iter_mut().filter(|s| s.name.trim() == SERVER_NAME && s.transport == Transport::Stdio) {
        server.env.push(format!("FIGMA_API_KEY={}", token.trim()));
    }
    servers
}

pub fn file_url(key: &str) -> String {
    format!("https://www.figma.com/design/{key}")
}

/// The file key in a file URL such as `https://www.figma.com/design/AbC123/Name?node-id=1-2`
/// (also `/file/` and `/board/` URLs). A bare key is accepted as well.
pub fn file_key_from_url(text: &str) -> Option<String> {
    let text = text.trim();
    let after = ["/design/", "/file/", "/board/"].iter().find_map(|kind| text.split_once(kind).map(|(_, rest)| rest));
    let key = match after {
        Some(rest) => rest.split(['/', '?', '#']).next().unwrap_or_default(),
        None => text,
    };
    let is_key = !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric());
    is_key.then(|| key.to_string())
}

pub fn load(cwd: &Path) -> Result<Option<Binding>, String> {
    match std::fs::read_to_string(cwd.join(FIGMA_FILE)) {
        Ok(text) => serde_json::from_str(&text).map(Some).map_err(|e| format!("{FIGMA_FILE}: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{FIGMA_FILE}: {e}")),
    }
}

/// Writes the binding, or removes the file when the repository has none.
pub fn save(cwd: &Path, binding: Option<&Binding>) -> Result<(), String> {
    let path = cwd.join(FIGMA_FILE);
    let Some(binding) = binding else {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{FIGMA_FILE}: {e}")),
            _ => Ok(()),
        };
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{FIGMA_FILE}: {e}"))?;
    }
    let mut text = serde_json::to_string_pretty(binding).map_err(|e| format!("{FIGMA_FILE}: {e}"))?;
    text.push('\n');
    std::fs::write(path, text).map_err(|e| format!("{FIGMA_FILE}: {e}"))
}

/// Looks up a file's name. `depth=1` keeps the response to the file's pages; it needs only the
/// file_content:read scope.
pub async fn file(token: &str, key: &str) -> Result<FigmaFile, String> {
    let url = format!("{API}/files/{key}?depth=1");
    let call = reqwest::Client::new().get(url).header("X-Figma-Token", token.trim());
    let response = call.send().await.map_err(|e| format!("Figma request failed: {e}"))?;
    let status = response.status();
    let body: Value = response.json().await.map_err(|e| format!("unreadable Figma response ({status}): {e}"))?;
    if status.is_success() {
        return Ok(FigmaFile { key: key.to_string(), name: body["name"].as_str().unwrap_or(key).to_string() });
    }
    let message = body["err"].as_str().or(body["message"].as_str()).map(String::from).unwrap_or_else(|| body.to_string());
    Err(format!("Figma {status}: {message}"))
}

/// What the agent is told about the repository's Figma files.
pub fn prompt_section(binding: &Binding) -> String {
    let files = binding.files.iter().map(|f| format!("“{}” (key `{}`)", f.name, f.key)).collect::<Vec<_>>().join(", ");
    let theme = match binding.theme() {
        Some(f) => format!(" The design system (colours, fonts, radii) is in “{}” (key `{}`).", f.name, f.key),
        None => String::new(),
    };
    format!(
        "This repository's Figma files: {files}.{theme} \
         Whenever the user mentions Figma, a design or a screen's look, read these files with the Figma MCP tools \
         (get_figma_data takes a file key, and optionally a node id, and returns its layout, styles and text).\n\n"
    )
}

/// The task the Figma node's Sync theme button gives the agent.
pub fn sync_theme_prompt(binding: &Binding, theme_files: &[String]) -> String {
    let source = match binding.theme() {
        Some(f) => format!("the Figma file “{}” ({})", f.name, file_url(&f.key)),
        None => "the repository's Figma files (listed in the system prompt)".to_string(),
    };
    let targets = if theme_files.is_empty() {
        "the project's theme files (find them first)".to_string()
    } else {
        theme_files.iter().map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", ")
    };
    format!(
        "Bring the project's theme in line with its Figma design.\n\n\
         Read the styles (colours, fonts, sizes, radii, shadows) of {source} with the Figma MCP tool get_figma_data. \
         Compare them with {targets}. \
         First list every difference as a table: token, Figma value, code value. \
         Then update the theme files so they match Figma, keeping their existing names and structure.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_file_keys_from_urls() {
        assert_eq!(file_key_from_url("https://www.figma.com/design/AbC123/Design-system?node-id=1-2").as_deref(), Some("AbC123"));
        assert_eq!(file_key_from_url("https://www.figma.com/file/XyZ9/Old").as_deref(), Some("XyZ9"));
        assert_eq!(file_key_from_url("https://www.figma.com/board/Q1w2/Ideas").as_deref(), Some("Q1w2"));
        assert_eq!(file_key_from_url(" AbC123 ").as_deref(), Some("AbC123"));
        assert_eq!(file_key_from_url("https://www.figma.com/files/team/1/Acme"), None);
        assert_eq!(file_key_from_url(""), None);
    }

    #[test]
    fn token_goes_only_to_the_figma_server() {
        let figma = crate::mcp::PRESETS.iter().find(|p| p.name == SERVER_NAME).unwrap().server();
        let other = Server { name: "linear".into(), transport: Transport::Stdio, command: "x".into(), ..Default::default() };
        let out = with_token(vec![figma, other], " secret ");
        assert_eq!(out[0].env, vec!["FIGMA_API_KEY=secret".to_string()]);
        assert!(out[1].env.is_empty());
        assert!(with_token(out.clone(), "").iter().all(|s| s.env.len() <= 1));
    }

    #[test]
    fn binding_round_trips_and_removes() {
        let dir = std::env::temp_dir().join(format!("wb-figma-{}", std::process::id()));
        let binding = Binding {
            files: vec![FigmaFile { key: "k".into(), name: "Design system".into() }],
            theme_file: Some("k".into()),
        };
        save(&dir, Some(&binding)).unwrap();
        assert_eq!(load(&dir).unwrap(), Some(binding.clone()));
        assert_eq!(binding.theme().map(|f| f.name.as_str()), Some("Design system"));
        save(&dir, None).unwrap();
        assert_eq!(load(&dir).unwrap(), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
