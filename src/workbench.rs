//! The window body: a collapsible projects sidebar on the left (with the Code, Rules and
//! Settings buttons at its bottom) and the main screen on the right. The main screen shows the
//! graph by default; Code, Rules and Settings replace it until closed with the ✕ at its top
//! left. The floating chat (`ChatDock`) is rendered by the app over the whole window.

use std::collections::HashMap;
use std::path::PathBuf;

use dioxus::prelude::*;

use crate::activity::AgentProc;
use crate::app::open_folder;
use crate::logos::LogoIcon;
use crate::projects::Project;
use crate::chat::{self, ChatPane};
use crate::code::CodePane;
use crate::graph_view::GraphTab;
use crate::project_view::{ProjectView, create_project, pick_repo};
use crate::rules_view::RulesView;
use crate::settings_view::SettingsView;
use crate::worktree_view::{AgentDot, other_worktrees};
use crate::state::{AppState, ChatFocus, ChatItem, ChatRect, Cmd, WorkbenchView};

const PEEK_CHARS: usize = 180;

#[component]
pub fn Body() -> Element {
    rsx! {
        div { class: "main",
            div { class: "left-rail",
                ProjectSidebar {}
                RailFooter {}
            }
            Workbench {}
        }
    }
}

#[component]
fn ProjectSidebar() -> Element {
    let mut st = use_context::<AppState>();
    let claude = use_coroutine_handle::<Cmd>();
    let mut new_name = use_signal(|| None::<String>);

    if !st.layout.read().projects_open {
        return rsx! {
            div {
                class: "screen-strip",
                title: "Show projects",
                onclick: move |_| st.set_layout(|l| l.projects_open = true),
                span { "Projects" }
            }
        };
    }

    let projects = st.projects.read().clone();
    let cwd = st.cwd.read().clone();
    let viewing = st.project_view.read().clone();
    let activity = st.agent_activity();
    let mut create = move || {
        let name = new_name.peek().clone().unwrap_or_default();
        create_project(st, &name);
        st.view.set(WorkbenchView::Graph);
        new_name.set(None);
    };

    rsx! {
        div { class: "projects-sidebar",
            div { class: "sidebar-head",
                b { "Projects" }
                div { class: "spacer" }
                button { class: "small", title: "New project", onclick: move |_| new_name.set(Some(String::new())), "+" }
                button { class: "small", title: "Hide projects", onclick: move |_| st.set_layout(|l| l.projects_open = false), "«" }
            }
            if let Some(name) = new_name.read().clone() {
                input {
                    class: "sidebar-new",
                    placeholder: "Project name (Enter to create)",
                    value: "{name}",
                    onmounted: chat::focus,
                    oninput: move |e| new_name.set(Some(e.value())),
                    onkeydown: move |e: KeyboardEvent| match e.key() {
                        Key::Enter => create(),
                        Key::Escape => new_name.set(None),
                        _ => {}
                    },
                }
            }
            if projects.is_empty() {
                div { class: "muted small-text pad", "No projects yet. A project groups the repositories that make up one product." }
            }
            for p in projects {
                {
                    let show_id = p.id.clone();
                    let add_id = p.id.clone();
                    let name_class = if viewing.as_deref() == Some(p.id.as_str()) { "sidebar-project active" } else { "sidebar-project" };
                    rsx! {
                        div { key: "{p.id}", class: "sidebar-group",
                        div { class: "{name_class}",
                            title: "Show every repository in {p.name}",
                            onclick: move |_| {
                                st.view.set(WorkbenchView::Graph);
                                st.project_view.set(Some(show_id.clone()));
                            },
                            span { class: "sidebar-label", "◈ {p.name}" }
                            button {
                                class: "small",
                                title: "Add a local clone of a repository to {p.name}",
                                onclick: move |e: MouseEvent| {
                                    e.stop_propagation();
                                    pick_repo(st, add_id.clone());
                                },
                                "+"
                            }
                        }
                        ul { class: "sidebar-repos",
                            for r in p.repos.clone() {
                                {
                                    let worktrees = other_worktrees(&st, &r.path);
                                    let agents = activity.get(&r.dir()).cloned().unwrap_or_default();
                                    let dir = r.dir();
                                    rsx! {
                                        li {
                                            key: "{r.path}",
                                            class: if r.is_at(&cwd) { "current" } else { "" },
                                            title: "{r.path}",
                                            onclick: move |_| {
                                                st.view.set(WorkbenchView::Graph);
                                                open_folder(st, claude, dir.clone());
                                            },
                                            span { class: "sidebar-label", "{r.label()}" }
                                            AgentDot { agents }
                                        }
                                        for w in worktrees {
                                            {
                                                let agents = activity.get(&w.path).cloned().unwrap_or_default();
                                                let here = crate::projects::same_dir(&w.path, &cwd);
                                                let dir = w.path.clone();
                                                let missing = w.prunable;
                                                rsx! {
                                                    li {
                                                        key: "{w.path.display()}",
                                                        class: match (here, missing) {
                                                            (true, _) => "sidebar-worktree current",
                                                            (false, true) => "sidebar-worktree missing",
                                                            _ => "sidebar-worktree",
                                                        },
                                                        title: "Worktree at {w.path.display()}",
                                                        onclick: move |_| {
                                                            if !missing {
                                                                st.view.set(WorkbenchView::Graph);
                                                                open_folder(st, claude, dir.clone());
                                                            }
                                                        },
                                                        span { class: "sidebar-label", "⑂ {w.label()}" }
                                                        AgentDot { agents }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        ProjectAgents { agents: project_agents(&st, &p, &activity) }
                        }
                    }
                }
            }
        }
    }
}

/// An agent in a project, with the repository or worktree folder it runs in.
#[derive(Clone, PartialEq)]
struct ProjectAgent {
    folder: PathBuf,
    folder_label: String,
    agent: AgentProc,
}

/// Every agent running in a project's repositories and their worktrees, repository by repository.
fn project_agents(st: &AppState, project: &Project, activity: &HashMap<PathBuf, Vec<AgentProc>>) -> Vec<ProjectAgent> {
    let mut out = Vec::new();
    for r in &project.repos {
        let mut folders = vec![(r.dir(), r.label())];
        for w in other_worktrees(st, &r.path) {
            folders.push((w.path.clone(), format!("{} ⑂ {}", r.label(), w.label())));
        }
        for (folder, folder_label) in folders {
            for agent in activity.get(&folder).into_iter().flatten() {
                out.push(ProjectAgent { folder: folder.clone(), folder_label: folder_label.clone(), agent: agent.clone() });
            }
        }
    }
    out
}

/// The agents of one project, under its repositories: the provider's logo, the agent, the
/// folder it runs in, and whether it is working or waiting for a prompt. Clicking one opens
/// its folder.
#[component]
fn ProjectAgents(agents: Vec<ProjectAgent>) -> Element {
    let mut st = use_context::<AppState>();
    let claude = use_coroutine_handle::<Cmd>();
    if agents.is_empty() {
        return rsx! {};
    }
    rsx! {
        ul { class: "sidebar-agents",
            for pa in agents {
                {
                    let a = pa.agent.clone();
                    let state = if a.working { "working" } else { "waiting for a prompt" };
                    let who = if a.in_app { "Workbench's own session".to_string() } else { format!("Running outside Workbench, pid {}", a.pid) };
                    let folder = pa.folder.clone();
                    rsx! {
                        li {
                            key: "{a.pid}-{pa.folder.display()}",
                            class: if a.working { "working" } else { "" },
                            title: "{a.kind.label()} in {pa.folder.display()}\n{who} · {state}\nClick to open this folder",
                            onclick: move |_| {
                                st.view.set(WorkbenchView::Graph);
                                open_folder(st, claude, folder.clone());
                            },
                            LogoIcon { logo: a.kind.logo() }
                            span { class: "sidebar-label",
                                "{a.kind.label()}"
                                span { class: "muted", " · {pa.folder_label}" }
                            }
                            span { class: "agent-state", if a.working { "working" } else { "waiting" } }
                        }
                    }
                }
            }
        }
    }
}

/// The buttons at the bottom left that open a screen in place of the graph.
/// Clicking the button of the open screen goes back to the graph.
#[component]
fn RailFooter() -> Element {
    let mut st = use_context::<AppState>();
    let view = *st.view.read();
    let wide = st.layout.read().projects_open;
    let dirty = st.open_file.read().as_ref().is_some_and(|f| f.dirty());
    let n_rules = st.rules.read().iter().filter(|r| r.enabled).count();

    let item = move |v: WorkbenchView, icon: &'static str, label: String| {
        let on = view == v;
        rsx! {
            button {
                class: if on { "rail-btn active" } else { "rail-btn" },
                title: if on { format!("Close {}", v.title()) } else { format!("Open {}", v.title()) },
                onclick: move |_| st.view.set(if on { WorkbenchView::Graph } else { v }),
                span { class: "rail-icon", "{icon}" }
                if wide { span { "{label}" } }
            }
        }
    };

    rsx! {
        div { class: if wide { "rail-footer" } else { "rail-footer narrow" },
            {item(WorkbenchView::Code, "</>", if dirty { "Code ●".into() } else { "Code".into() })}
            {item(WorkbenchView::Rules, "☰", format!("Rules ({n_rules})"))}
            {item(WorkbenchView::Settings, "⚙", "Settings".into())}
        }
    }
}

/// The main screen: the graph, or the screen that replaced it with a ✕ to go back.
#[component]
fn Workbench() -> Element {
    let mut st = use_context::<AppState>();
    let view = *st.view.read();
    let error = st.workbench_error.read().clone();
    let project_view = st.project_view.read().clone();

    rsx! {
        div { class: "workbench",
            if view != WorkbenchView::Graph {
                div { class: "screen-bar",
                    button {
                        class: "screen-close",
                        title: "Close {view.title()} and go back to the graph",
                        onclick: move |_| st.view.set(WorkbenchView::Graph),
                        "✕"
                    }
                    span { class: "screen-title", "{view.title()}" }
                }
            }
            if let Some(e) = error {
                div { class: "wb-error",
                    span { "{e}" }
                    button { onclick: move |_| st.workbench_error.set(None), "✕" }
                }
            }
            match (view, project_view) {
                (WorkbenchView::Graph, Some(id)) => rsx! { ProjectView { key: "{id}", id } },
                (WorkbenchView::Graph, None) => rsx! { GraphTab {} },
                (WorkbenchView::Code, _) => rsx! { CodePane {} },
                (WorkbenchView::Rules, _) => rsx! { RulesView {} },
                (WorkbenchView::Settings, _) => rsx! { SettingsView {} },
            }
        }
    }
}

/// The start of Workbench's latest reply, shown while the chat is faded.
fn latest_reply(items: &[ChatItem]) -> String {
    let text = items.iter().rev().find_map(|i| match i {
        ChatItem::Assistant { text, .. } => Some(text.as_str()),
        _ => None,
    });
    let Some(text) = text else { return "No replies yet".into() };
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(PEEK_CHARS) {
        Some((end, _)) => format!("{}…", &flat[..end]),
        None => flat,
    }
}

// Which edges a resize drag moves.
const N: u8 = 1;
const S: u8 = 2;
const E: u8 = 4;
const W: u8 = 8;
const EDGES: [(u8, &str); 8] = [(N, "n"), (S, "s"), (E, "e"), (W, "w"), (N | E, "ne"), (N | W, "nw"), (S | E, "se"), (S | W, "sw")];

#[derive(Clone, Copy, PartialEq)]
struct ChatDrag {
    /// 0 moves the chat; otherwise the edges being dragged.
    edges: u8,
    mx: f64,
    my: f64,
    start: ChatRect,
}

/// The size of the current window in logical pixels.
pub fn window_size() -> (f64, f64) {
    let w = dioxus::desktop::window();
    let size = w.window.inner_size().to_logical::<f64>(w.window.scale_factor());
    (size.width, size.height)
}

/// The rect after dragging by (dx, dy), kept inside the window.
fn dragged(d: ChatDrag, dx: f64, dy: f64) -> ChatRect {
    let (win_w, win_h) = window_size();
    let mut r = d.start;
    if d.edges == 0 {
        r.x = (r.x + dx).clamp(0.0, (win_w - r.w).max(0.0));
        r.y = (r.y + dy).clamp(0.0, (win_h - r.h).max(0.0));
        return r;
    }
    let right = r.x + r.w;
    let bottom = r.y + r.h;
    if d.edges & E != 0 {
        r.w = (d.start.w + dx).clamp(ChatRect::MIN_W, (win_w - r.x).max(ChatRect::MIN_W));
    }
    if d.edges & W != 0 {
        r.x = (d.start.x + dx).clamp(0.0, right - ChatRect::MIN_W);
        r.w = right - r.x;
    }
    if d.edges & S != 0 {
        r.h = (d.start.h + dy).clamp(ChatRect::MIN_H, (win_h - r.y).max(ChatRect::MIN_H));
    }
    if d.edges & N != 0 {
        r.y = (d.start.y + dy).clamp(0.0, bottom - ChatRect::MIN_H);
        r.h = bottom - r.y;
    }
    r
}

/// The floating chat. It floats over the whole window: drag its top bar to move it and its
/// edges or corners to resize it. The place and size are saved in `Layout::chat_rect`; until
/// the chat is first moved it sits at the bottom centre. A click in the graph fades it to a
/// peek at the latest reply unless it's locked.
#[component]
pub fn ChatDock() -> Element {
    let mut st = use_context::<AppState>();
    let mut el = use_signal(|| None::<std::rc::Rc<MountedData>>);
    let mut drag = use_signal(|| None::<ChatDrag>);
    let layout = *st.layout.read();
    let waiting_for_permission = !st.permissions.read().is_empty() || st.plan_approval.read().is_some();
    let focus = if waiting_for_permission { ChatFocus::Active } else { *st.chat_focus.read() };
    let peek = latest_reply(&st.items.read());
    let busy = *st.busy.read();
    let dragging = *drag.read();
    if layout.chat_detached {
        return rsx! {
            div { class: "chat-detached-pill",
                span { class: if busy { "agent-dot working" } else { "agent-dot" }, if busy { "●" } else { "○" } }
                span { "Chat is in its own window" }
                button {
                    class: "small",
                    title: "Put the chat back inside this window",
                    onclick: move |_| st.set_layout(|l| l.chat_detached = false),
                    "⇲ Attach"
                }
            }
        };
    }

    // Starts a move (edges = 0) or resize from the chat's rect as drawn, so a chat that the
    // CSS kept inside a smaller window doesn't jump.
    let begin = move |e: MouseEvent, edges: u8| {
        e.prevent_default();
        e.stop_propagation();
        let p = e.client_coordinates();
        let Some(m) = el.peek().clone() else { return };
        spawn(async move {
            if let Ok(rect) = m.get_client_rect().await {
                let start = ChatRect { x: rect.origin.x, y: rect.origin.y, w: rect.size.width, h: rect.size.height };
                drag.set(Some(ChatDrag { edges, mx: p.x, my: p.y, start }));
            }
        });
    };
    let mut end = move || {
        if drag.peek().is_some() {
            drag.set(None);
            st.save_layout();
        }
    };

    let place = match layout.chat_rect {
        Some(r) => format!("--x: {}px; --y: {}px; --w: {}px; --h: {}px;", r.x, r.y, r.w, r.h),
        None => String::new(),
    };
    let placed = if layout.chat_rect.is_some() { "placed" } else { "" };
    let faded = if focus == ChatFocus::Faded && dragging.is_none() { "faded" } else { "" };
    let overlay_cursor = match dragging.map(|d| d.edges) {
        Some(0) | None => "move".to_string(),
        Some(edges) => {
            let name = EDGES.iter().find(|(b, _)| *b == edges).map(|(_, n)| *n).unwrap_or("se");
            format!("{name}-resize")
        }
    };

    rsx! {
        div {
            class: "chat-dock {placed} {faded}",
            style: "{place}",
            onmounted: move |e| el.set(Some(e.data())),
            onmousedown: move |_| {
                if *st.chat_focus.peek() != ChatFocus::Active {
                    st.chat_focus.set(ChatFocus::Active);
                }
            },
            div { class: if busy { "chat-peek busy" } else { "chat-peek" }, "{peek}" }
            div {
                class: "chat-dock-bar",
                title: "Drag to move the chat · double-click to put it back at the bottom",
                onmousedown: move |e| begin(e, 0),
                ondoubleclick: move |_| st.set_layout(|l| l.chat_rect = None),
                span { class: "chat-grip", "⠿" }
                span { class: "muted small-text", "Chat" }
                div { class: "spacer" }
                button {
                    class: "small",
                    title: "Move the chat into a window of its own",
                    onmousedown: move |e| e.stop_propagation(),
                    onclick: move |_| st.set_layout(|l| l.chat_detached = true),
                    "⧉ Detach"
                }
                button {
                    class: if layout.chat_locked { "small toggle on" } else { "small toggle" },
                    title: if layout.chat_locked { "Let clicks in the graph fade the chat again" } else { "Lock the chat so clicks in the graph don't fade it" },
                    onmousedown: move |e| e.stop_propagation(),
                    onclick: move |_| st.set_layout(|l| l.chat_locked = !l.chat_locked),
                    "Lock"
                }
            }
            ChatPane {}
            for (edges, name) in EDGES {
                div {
                    key: "{name}",
                    class: "chat-edge {name}",
                    onmousedown: move |e| begin(e, edges),
                }
            }
        }
        if dragging.is_some() {
            div {
                class: "drag-overlay",
                style: "cursor: {overlay_cursor};",
                onmousemove: move |e: MouseEvent| {
                    let Some(d) = *drag.peek() else { return };
                    let p = e.client_coordinates();
                    let r = dragged(d, p.x - d.mx, p.y - d.my);
                    st.layout.write().chat_rect = Some(r);
                },
                onmouseup: move |_| end(),
                onmouseleave: move |_| end(),
            }
        }
    }
}
