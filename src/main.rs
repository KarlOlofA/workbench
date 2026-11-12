mod activity;
mod agent;
mod api_chat;
mod app;
mod chat;
mod chat_window;
mod claude;
mod code;
mod codex;
mod config;
mod dropdown;
mod editor;
mod ghost;
mod git;
mod github;
mod github_view;
mod graph;
mod graph_view;
mod highlight;
mod history;
mod logos;
mod markdown;
mod mcp;
mod mcp_view;
mod node_history;
mod platform;
mod project_view;
mod projects;
mod prompts;
mod providers;
mod providers_view;
mod reference;
mod rules;
mod rules_view;
mod session;
mod sessions;
mod settings_view;
mod state;
mod update;
mod update_view;
mod vibe;
mod workbench;
mod worktree_view;

use dioxus::desktop::{Config, LogicalSize, WindowBuilder};

/// The theme (variables and base controls) first, then the component styles that use it.
pub const STYLE: &str = concat!(include_str!("theme.css"), include_str!("style.css"));

fn main() {
    let window = WindowBuilder::new()
        .with_title("Workbench")
        .with_inner_size(LogicalSize::new(1400.0, 900.0))
        .with_min_inner_size(LogicalSize::new(800.0, 500.0))
        // Drop the OS title bar (title + minimize/maximize/close) on Linux.
        // The app header's empty space acts as the drag handle instead.
        .with_decorations(!cfg!(target_os = "linux"));

    let config = Config::new()
        .with_window(window)
        .with_custom_head(format!("<style>{STYLE}</style>"));

    dioxus::LaunchBuilder::desktop().with_cfg(config).launch(app::App);
}
