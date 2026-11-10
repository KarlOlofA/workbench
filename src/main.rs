mod app;
mod chat;
mod claude;
mod code;
mod editor;
mod ghost;
mod git;
mod github;
mod github_view;
mod graph;
mod graph_view;
mod highlight;
mod history;
mod markdown;
mod node_history;
mod project_view;
mod projects;
mod prompts;
mod reference;
mod rules;
mod rules_view;
mod screens;
mod session;
mod state;

use dioxus::desktop::{Config, LogicalSize, WindowBuilder};

const STYLE: &str = include_str!("style.css");

fn main() {
    let window = WindowBuilder::new()
        .with_title("Workbench")
        .with_inner_size(LogicalSize::new(1400.0, 900.0))
        .with_min_inner_size(LogicalSize::new(800.0, 500.0));

    let config = Config::new()
        .with_window(window)
        .with_custom_head(format!("<style>{STYLE}</style>"));

    dioxus::LaunchBuilder::desktop().with_cfg(config).launch(app::App);
}
