use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use crate::claude::tool_summary;
use crate::screens;
use crate::state::{ChatItem, Message};

const LAST_FILE: &str = "sessions.json";
const MAX_SESSIONS: usize = 60;
const TITLE_SCAN_LINES: usize = 400;
const LONG_PROMPT: usize = 600;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub modified: SystemTime,
}

fn claude_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude"))
}

pub fn project_key(cwd: &Path) -> String {
    cwd.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

fn transcripts_dir(cwd: &Path) -> Option<PathBuf> {
    claude_home().map(|h| h.join("projects").join(project_key(cwd)))
}

fn transcript(cwd: &Path, id: &str) -> Option<PathBuf> {
    let p = transcripts_dir(cwd)?.join(format!("{id}.jsonl"));
    p.is_file().then_some(p)
}

pub fn last_session(cwd: &Path) -> Option<String> {
    let map: HashMap<String, String> = screens::load_json(LAST_FILE);
    map.get(&cwd.to_string_lossy().to_string()).filter(|id| transcript(cwd, id).is_some()).cloned()
}

pub fn remember(cwd: &Path, id: &str) -> Result<(), String> {
    let key = cwd.to_string_lossy().to_string();
    let mut map: HashMap<String, String> = screens::load_json(LAST_FILE);
    if map.get(&key).map(String::as_str) == Some(id) {
        return Ok(());
    }
    map.insert(key, id.to_string());
    screens::save_json(LAST_FILE, &map)
}

pub fn forget(cwd: &Path) -> Result<(), String> {
    let mut map: HashMap<String, String> = screens::load_json(LAST_FILE);
    if map.remove(&cwd.to_string_lossy().to_string()).is_some() {
        screens::save_json(LAST_FILE, &map)?;
    }
    Ok(())
}

fn is_noise(text: &str) -> bool {
    let t = text.trim_start();
    t.is_empty()
        || t.starts_with("<system-reminder>")
        || t.starts_with("<command-")
        || t.starts_with("<local-command")
        || t.starts_with("Caveat:")
        || t.starts_with("[Request interrupted")
}

fn conversational(v: &Value) -> bool {
    !v.get("isSidechain").and_then(Value::as_bool).unwrap_or(false) && !v.get("isMeta").and_then(Value::as_bool).unwrap_or(false)
}

fn user_texts(v: &Value) -> Vec<String> {
    match &v["message"]["content"] {
        Value::String(s) => vec![s.clone()],
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str).map(String::from))
            .collect(),
        _ => Vec::new(),
    }
    .into_iter()
    .filter(|t| !is_noise(t))
    .collect()
}

fn one_line(text: &str, max: usize) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    match line.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}

fn title_of(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().take(TITLE_SCAN_LINES) {
        let Ok(line) = line else { continue };
        if !line.contains("\"type\":\"user\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("user") || !conversational(&v) {
            continue;
        }
        if let Some(text) = user_texts(&v).first() {
            return Some(one_line(text, 90));
        }
    }
    None
}

pub fn list(cwd: &Path) -> Vec<SessionInfo> {
    let Some(dir) = transcripts_dir(cwd) else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut files: Vec<(PathBuf, SystemTime)> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| {
            let modified = p.metadata().and_then(|m| m.modified()).ok()?;
            Some((p, modified))
        })
        .collect();
    files.sort_by(|a, b| b.1.cmp(&a.1));
    files
        .into_iter()
        .filter_map(|(p, modified)| {
            let id = p.file_stem()?.to_string_lossy().to_string();
            let title = title_of(&p)?;
            Some(SessionInfo { id, title, modified })
        })
        .take(MAX_SESSIONS)
        .collect()
}

pub fn load(cwd: &Path, id: &str) -> Vec<ChatItem> {
    let Some(path) = transcript(cwd, id) else { return Vec::new() };
    let Ok(file) = std::fs::File::open(path) else { return Vec::new() };
    let mut items: Vec<ChatItem> = Vec::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        if !line.contains("\"type\":\"user\"") && !line.contains("\"type\":\"assistant\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if !conversational(&v) {
            continue;
        }
        match v.get("type").and_then(Value::as_str) {
            Some("user") => {
                for text in user_texts(&v) {
                    items.push(user_item(text));
                }
                for b in v["message"]["content"].as_array().into_iter().flatten() {
                    if b.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let tid = b.get("tool_use_id").and_then(Value::as_str).unwrap_or_default();
                    let is_error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                    let content = result_text(b.get("content").unwrap_or(&Value::Null));
                    if let Some(ChatItem::Tool { result, .. }) =
                        items.iter_mut().rev().find(|i| matches!(i, ChatItem::Tool { id, .. } if id == tid))
                    {
                        *result = Some((content, is_error));
                    }
                }
            }
            Some("assistant") => {
                for b in v["message"]["content"].as_array().into_iter().flatten() {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let text = b.get("text").and_then(Value::as_str).unwrap_or_default();
                            if !text.trim().is_empty() {
                                items.push(ChatItem::Assistant { text: text.to_string(), streaming: false });
                            }
                        }
                        Some("tool_use") => {
                            let name = b.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                            let input = b.get("input").cloned().unwrap_or(Value::Null);
                            items.push(ChatItem::Tool {
                                id: b.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
                                summary: tool_summary(&name, &input, cwd),
                                input: serde_json::to_string_pretty(&input).unwrap_or_default(),
                                name,
                                result: None,
                            });
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    items
}

fn user_item(text: String) -> ChatItem {
    if text.len() > LONG_PROMPT {
        ChatItem::Prompt { label: one_line(&text, 80), text }
    } else {
        ChatItem::User(Message { text: text.clone(), context: Vec::new(), prompt: text })
    }
}

fn result_text(v: &Value) -> String {
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

pub fn ago(t: SystemTime) -> String {
    let s = SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86_399 => format!("{} h ago", s / 3600),
        86_400..=172_799 => "yesterday".into(),
        _ => format!("{} days ago", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_key_matches_claude_layout() {
        assert_eq!(project_key(Path::new("/Users/me/dev/rust-ui_test.v2")), "-Users-me-dev-rust-ui-test-v2");
    }

    #[test]
    fn loads_a_transcript_into_chat_items() {
        let dir = std::env::temp_dir().join(format!("wb-history-{}", std::process::id()));
        let cwd = Path::new("/tmp/wb-history-project");
        let tdir = dir.join("projects").join(project_key(cwd));
        std::fs::create_dir_all(&tdir).unwrap();
        let lines = [
            r#"{"type":"session-owner","sessionId":"s1"}"#,
            r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"type":"text","text":"Fix the tests"}]}}"#,
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<system-reminder>x</system-reminder>"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"On it."}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}]}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"subagent noise"}]}}"#,
        ];
        std::fs::write(tdir.join("s1.jsonl"), lines.join("\n")).unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &dir) };

        let sessions = list(cwd);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, "Fix the tests");

        let items = load(cwd, "s1");
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[0], ChatItem::User(m) if m.text == "Fix the tests"));
        assert!(matches!(&items[1], ChatItem::Assistant { text, .. } if text == "On it."));
        assert!(matches!(&items[2], ChatItem::Tool { summary, result: Some((out, false)), .. } if summary == "cargo test" && out == "ok"));

        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
