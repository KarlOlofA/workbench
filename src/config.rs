//! Small JSON files in the user's config folder: projects, layout, provider settings and so on.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

fn config_path(name: &str) -> Option<PathBuf> {
    crate::platform::config_dir().map(|d| d.join(name))
}

pub fn load_json<T: for<'de> Deserialize<'de> + Default>(name: &str) -> T {
    config_path(name)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_json<T: Serialize>(name: &str, value: &T) -> Result<(), String> {
    let path = config_path(name).ok_or("config: no home folder (HOME is not set)")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{name}: {e}"))?;
    }
    let text = serde_json::to_string_pretty(value).map_err(|e| format!("{name}: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("{name}: {e}"))
}
