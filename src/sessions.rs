//! Chat sessions and their agents, saved in the user's config folder.
//!
//! A session is a chat in one folder (a repository checkout or a worktree). It holds one or
//! more agents; each agent is its own conversation with its own agent process. The live side
//! of an agent (its messages, whether it is busy) is a `state::Chat`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config;

const FILE: &str = "chat_sessions.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedAgent {
    pub id: String,
    pub name: String,
    /// The provider's session id, so the conversation can be resumed after a restart.
    #[serde(default)]
    pub transcript: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedSession {
    pub id: String,
    pub title: String,
    pub dir: PathBuf,
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
