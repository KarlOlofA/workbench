//! Chat sessions and their agents, saved in the user's config folder.
//!
//! A session is a chat in one folder (a repository checkout or a worktree). It holds one or
//! more agents; each agent is its own conversation with its own agent process. The live side
//! of an agent (its messages, whether it is busy) is a `state::Chat`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config;
use crate::providers::Provider;

const FILE: &str = "chat_sessions.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedAgent {
    pub id: String,
    pub name: String,
    /// The provider's session id, so the conversation can be resumed after a restart.
    #[serde(default)]
    pub transcript: Option<String>,
    /// The provider's session id of the agent's discussion chat (PRO-248), to resume it too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discussion: Option<String>,
    /// The provider the agent was started on, when the chat asked for one. `None` uses the settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
    /// The model the agent was started on, when the chat asked for one. `None` uses the settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedSession {
    pub id: String,
    pub title: String,
    pub dir: PathBuf,
    /// Set for a project-level session: the id of its project. Its `dir` is then the folder
    /// that holds the project's repositories (`Project::folder`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub agents: Vec<SavedAgent>,
}

pub fn load() -> Vec<SavedSession> {
    config::load_json(FILE)
}

pub fn save(sessions: &[SavedSession]) -> Result<(), String> {
    config::save_json(FILE, &sessions)
}

/// A new id that sorts by creation time, e.g. `agent-1731400000123456`.
pub fn new_id(prefix: &str) -> String {
    let micros = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0);
    format!("{prefix}-{micros}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_saved_before_discussion_chats_still_load() {
        let agent: SavedAgent = serde_json::from_str(r#"{"id":"agent-1","name":"Agent","transcript":"abc"}"#).unwrap();
        assert_eq!(agent.transcript.as_deref(), Some("abc"));
        assert_eq!(agent.discussion, None);
        let json = serde_json::to_string(&agent).unwrap();
        assert!(!json.contains("discussion"), "{json}");
    }
}
