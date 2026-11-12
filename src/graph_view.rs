use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;

use crate::claims;
use crate::claude::ChatMode;
use crate::dropdown::{Choice, ComboBox, Dropdown};
use crate::ghost::{self, GhostPick};
use crate::git;
use crate::graph::{self, ChangeKind, Flow, Graph, KNOWN_KINDS, Module, NODE_H, NODE_W, Node, REFERENCE_KIND, RefNode, Reference, Subject};
use crate::prompts::{self, DebugMode};
use crate::reference::{self, RefState, Source};
use crate::node_history::{self, Entry, Op, Row};
use crate::state::{AppState, Cmd, GraphPane, Job, JobKind, Layout};

fn attach_module(mut st: AppState, module: String) {
    let scope = st.scope.peek().last().cloned();
    let Some(m) = st.graph.peek().module(&module).cloned() else { return };
    if !st.graph.peek().can_attach(scope.as_deref(), &module) {
        st.workbench_error.set(Some(format!("“{}” can't be attached inside itself", m.title)));
        return;
    }
    let mut created = None;
    st.update_net(|g| {
        let id = g.unique_id(&m.title);
        g.nodes.push(Node {
            id: id.clone(),
            title: m.title.clone(),
            kind: "module".into(),
            module: Some(m.id.clone()),
            ..Default::default()
        });
        created = Some(id);
    });
    if let Some(id) = created {
        st.select(Some(id));
        st.graph_pane.set(GraphPane::Node);
    }
}

fn new_module(mut st: AppState) {
    let mut module_id = String::new();
    st.update_graph(|g| {
        module_id = g.unique_module_id("new module");
        g.modules.push(Module { id: module_id.clone(), title: "New module".into(), ..Default::default() });
    });
    if !module_id.is_empty() {
        attach_module(st, module_id);
    }
}

fn add_reference(mut st: AppState) {
    let mut created = None;
    st.update_net(|g| {
        let id = g.unique_id("external reference");
        g.nodes.push(Node {
            id: id.clone(),
            title: "External reference".into(),
            kind: REFERENCE_KIND.into(),
            reference: Some(Reference::default()),
            ..Default::default()
        });
        created = Some(id);
    });
    if let Some(id) = created {
        st.select(Some(id));
        st.graph_pane.set(GraphPane::Node);
    }
}

fn edit_reference(mut st: AppState, id: &str, f: impl FnOnce(&mut Node)) {
    st.update_net(|g| {
        if let Some(n) = g.node_mut(id) {
            n.reference.get_or_insert_with(Reference::default);
            f(n);
        }
    });
}

fn open_source(src: &Source) {
    match src {
        Source::Local(p) if p.exists() => {
            crate::platform::reveal(p);
        }
        Source::Local(_) => {}
        _ => {
            if let Some(url) = src.url().filter(|u| u.starts_with("https://") || u.starts_with("http://")) {
                crate::platform::open_url(&url);
            }
        }
    }
}

fn add_node(mut st: AppState, parent: Option<String>) {
    let mut created = None;
    st.update_net(|g| {
        let id = g.unique_id("new node");
        if let Some(parent) = parent.as_deref().and_then(|p| g.node_mut(p)) {
            parent.links.push(id.clone());
        }
        g.nodes.push(Node {
            id: id.clone(),
            title: "New node".into(),
            kind: "module".into(),
            ..Default::default()
        });
        created = Some(id);
    });
    if let Some(id) = created {
        st.select(Some(id));
        st.graph_pane.set(GraphPane::Node);
    }
}

/// The graph view of the workbench: toolbar, breadcrumbs, the canvas with the floating chat,
/// and the node menu (node, flows, changes, history, prompt) on the right.
#[component]
pub fn GraphTab() -> Element {
    let mut st = use_context::<AppState>();
    let job = *st.job.read();
    let busy = st.chat_busy();
    let has_nodes = !st.graph.read().nodes.is_empty();
    let n_changes = st.changes().len();
    let net = st.net();
    let net_empty = net.nodes.is_empty();
    let n_flows = net.flows.len();
    let pane = *st.graph_pane.read();
    let n_history = st.node_history.read().entries.len();
    let layout = *st.layout.read();
    let mut row_el = use_signal(|| None::<Rc<MountedData>>);
    let mut split_drag = use_signal(|| None::<(f64, f64, Option<f64>)>);
    let splitting = split_drag.read().is_some();
    let drafting = st.flow_draft.read().is_some();
    let path = st.scope.read().clone();
    let scope = path.last().cloned();
    let crumbs: Vec<(usize, String)> = {
        let g = st.graph.read();
        path.iter().enumerate().map(|(i, m)| (i + 1, g.module_title(m))).collect()
    };
    let at_root = path.is_empty();
    let project = st.current_project();
    let project_name = project.as_ref().map(|p| p.name.clone());
    let cwd = st.cwd.read().clone();
    let cwd_label = cwd.display().to_string();
    let repo_label = project
        .as_ref()
        .and_then(|p| p.repos.iter().find(|r| r.is_at(&cwd)).map(|r| r.label()))
        .unwrap_or_else(|| cwd.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_else(|| cwd_label.clone()));
    let attachable: Vec<(String, String)> = {
        let g = st.graph.read();
        g.modules
            .iter()
            .filter(|m| g.can_attach(scope.as_deref(), &m.id))
            .map(|m| (m.id.clone(), m.title.clone()))
            .collect()
    };

    let generate = move |_| {
        if st.chat_busy() || st.job.peek().is_some() {
            return;
        }
        let existing = st.graph.peek().clone();
        let text = prompts::generate(Some(&existing), &st.rules.peek());
        let label = if existing.nodes.is_empty() { "Map this codebase into a graph" } else { "Rescan codebase and update the graph" };
        st.start_job(JobKind::Generate);
        st.chat_or_new().send(Cmd::Prompt { label: label.into(), text });
    };

    let rescan_blocked = has_nodes && n_changes > 0;
    let pane_btn = move |p: GraphPane, label: String| {
        rsx! {
            button {
                class: if pane == p { "subtab active" } else { "subtab" },
                onclick: move |_| st.show_pane(p),
                "{label}"
            }
        }
    };

    // Dragging the splitter left widens the node menu; the drag holds (start x, start %, row width).
    let start_split = move |e: MouseEvent| {
        if e.trigger_button() != Some(MouseButton::Primary) {
            return;
        }
        e.prevent_default();
        let x = e.client_coordinates().x;
        let pct = st.layout.peek().node_menu_pct;
        split_drag.set(Some((x, pct, None)));
        let Some(m) = row_el.peek().clone() else { return };
        spawn(async move {
            if let Ok(rect) = m.get_client_rect().await
                && let Some(d) = split_drag.write().as_mut()
            {
                d.2 = Some(rect.size.width);
            }
        });
    };
    let mut end_split = move || {
        if split_drag.peek().is_some() {
            split_drag.set(None);
            st.save_layout();
        }
    };
    let menu_open = layout.node_menu_open || drafting;
    let menu_style = format!("flex: 0 0 {:.2}%;", Layout::clamp_node_menu(layout.node_menu_pct));

    rsx! {
        div { class: "graph-tab",
            div { class: "graph-toolbar",
                button {
                    disabled: busy || rescan_blocked || job.is_some(),
                    title: if rescan_blocked { "Process or revert your graph changes before rescanning" } else { "Let Workbench explore the code and (re)build the graph" },
                    onclick: generate,
                    if has_nodes { "Rescan code" } else { "Map codebase" }
                }
                button { disabled: job.is_some(), onclick: move |_| add_node(st, None), "+ Node" }
                button {
                    disabled: job.is_some(),
                    title: "Reference an external project, file, website or package to give Workbench more context",
                    onclick: move |_| add_reference(st),
                    "+ Reference"
                }
                Dropdown {
                    class: "add-module",
                    disabled: job.is_some(),
                    title: "Attach a module (an isolated node network) here as a single node",
                    value: "",
                    placeholder: "+ Module…",
                    choices: std::iter::once(Choice::new("__new__", "New module"))
                        .chain(attachable.into_iter().map(|(mid, title)| Choice::new(mid, format!("Attach “{title}”"))))
                        .collect::<Vec<_>>(),
                    onchange: move |v: String| match v.as_str() {
                        "__new__" => new_module(st),
                        id => attach_module(st, id.to_string()),
                    },
                }
                button {
                    disabled: job.is_some(),
                    title: "Describe what you want and let Workbench add the nodes",
                    onclick: move |_| st.show_pane(GraphPane::Prompt),
                    "+ From prompt"
                }
                div { class: "spacer" }
                match job {
                    Some(Job { awaiting: true, .. }) => rsx! {
                        span { class: "badge warn", title: "Answer Workbench's questions in the chat to continue", "Waiting for your answers" }
                        button { class: "small", onclick: move |_| st.finish_job(false), "Cancel" }
                    },
                    Some(Job { kind: JobKind::Generate, .. }) => rsx! { span { class: "badge busy", "Mapping…" } },
                    Some(Job { kind: JobKind::Process, .. }) => rsx! { span { class: "badge busy", "Processing…" } },
                    Some(Job { kind: JobKind::Draft, .. }) => rsx! { span { class: "badge busy", "Drafting nodes…" } },
                    None => rsx! {},
                }
                if n_changes > 0 && job.is_none() {
                    button {
                        class: "primary",
                        onclick: move |_| st.show_pane(GraphPane::Changes),
                        "Commit {n_changes} change{plural(n_changes)}"
                    }
                }
            }
            div { class: "crumbs",
                button {
                    class: "crumb project",
                    title: "Show every repository in the project",
                    onclick: move |_| st.show_project(None),
                    match project_name.clone() {
                        Some(name) => rsx! { "◈ {name}" },
                        None => rsx! { "◈ Projects" },
                    }
                }
                span { class: "crumb-sep", "›" }
                button {
                    class: if at_root { "crumb current" } else { "crumb" },
                    title: "{cwd_label}",
                    onclick: move |_| st.set_scope(Vec::new()),
                    "{repo_label}"
                }
                for (depth, title) in crumbs {
                    {
                        let target: Vec<String> = path[..depth].to_vec();
                        let current = depth == path.len();
                        rsx! {
                            span { key: "{depth}", class: "crumb-item",
                                span { class: "crumb-sep", "›" }
                                button {
                                    class: if current { "crumb current" } else { "crumb" },
                                    onclick: move |_| st.set_scope(target.clone()),
                                    "▣ {title}"
                                }
                            }
                        }
                    }
                }
                div { class: "spacer" }
                button {
                    class: "small",
                    title: if at_root { "Show the other repositories in this project" } else { "Leave this module" },
                    onclick: move |_| {
                        let mut p = st.scope.peek().clone();
                        if p.pop().is_none() {
                            st.show_project(None);
                        } else {
                            st.set_scope(p);
                        }
                    },
                    "↑ Up"
                }
            }
            div { class: "graph-row", onmounted: move |e| row_el.set(Some(e.data())),
                div { class: "graph-canvas-wrap",
                    GraphCanvas {}
                    if scope.is_some() && net_empty && job.is_none() {
                        div { class: "graph-empty",
                            p { "This module has no nodes yet." }
                            p { class: "muted",
                                "Add nodes with + Node, or describe the feature in the Prompt tab and Workbench drafts the module's network."
                            }
                        }
                    }
                    if scope.is_none() && !has_nodes && job.is_none() {
                        div { class: "graph-empty",
                            p { "No graph for this project yet." }
                            p { class: "muted",
                                "Map codebase lets Workbench explore the code and draw its architecture: entry points, APIs, routes, services, data and integrations. Or add nodes yourself."
                            }
                            button { class: "primary", disabled: busy, onclick: generate, "Map codebase" }
                        }
                    }
                }
                if menu_open {
                    div {
                        class: "splitter",
                        title: "Drag to resize the node menu · double-click to hide it",
                        onmousedown: start_split,
                        ondoubleclick: move |_| st.set_layout(|l| l.node_menu_open = false),
                    }
                    div { class: "node-menu", style: "{menu_style}",
                        if drafting {
                            FlowDraft {}
                        } else {
                            div { class: "subtabs",
                                {pane_btn(GraphPane::Node, "Node".into())}
                                {pane_btn(GraphPane::Flows, format!("Flows ({n_flows})"))}
                                {pane_btn(GraphPane::Changes, format!("Changes ({n_changes})"))}
                                {pane_btn(GraphPane::History, format!("History ({n_history})"))}
                                {pane_btn(GraphPane::Prompt, "Prompt".into())}
                                div { class: "spacer" }
                                button {
                                    class: "pane-toggle",
                                    title: "Hide the node menu to give the graph more room",
                                    onclick: move |_| st.set_layout(|l| l.node_menu_open = false),
                                    "▸"
                                }
                            }
                            div { class: "pane-body",
                                match pane {
                                    GraphPane::Node => rsx! { NodePane {} },
                                    GraphPane::Flows => rsx! { FlowsPane {} },
                                    GraphPane::Changes => rsx! { ChangesPane {} },
                                    GraphPane::History => rsx! { HistoryPane {} },
                                    GraphPane::Prompt => rsx! { PromptPane {} },
                                }
                            }
                        }
                    }
                } else {
                    div {
                        class: "screen-strip",
                        title: "Show the node menu",
                        onclick: move |_| st.set_layout(|l| l.node_menu_open = true),
                        span { "Node menu" }
                    }
                }
            }
            if splitting {
                div {
                    class: "drag-overlay",
                    onmousemove: move |e: MouseEvent| {
                        let Some((start_x, start_pct, Some(width))) = *split_drag.peek() else { return };
                        if width <= 0.0 {
                            return;
                        }
                        let x = e.client_coordinates().x;
                        st.layout.write().node_menu_pct = Layout::clamp_node_menu(start_pct - (x - start_x) / width * 100.0);
                    },
                    onmouseup: move |_| end_split(),
                    onmouseleave: move |_| end_split(),
                }
            }
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[derive(Clone, PartialEq)]
enum Drag {
    Pan { sx: f64, sy: f64, ox: f64, oy: f64, moved: bool },
    Node { id: String, sx: f64, sy: f64, group: Vec<(String, f64, f64)>, moved: bool, shift: bool },
    Select { sx: f64, sy: f64, cx: f64, cy: f64, additive: bool },
    Link { anchor: String, outgoing: bool, sx: f64, sy: f64, wx: f64, wy: f64, cx: f64, cy: f64 },
}

fn toggle_link(mut st: AppState, from: String, to: String) {
    if from == to {
        return;
    }
    st.update_net(|g| {
        if let Some(n) = g.node_mut(&from) {
            if n.links.contains(&to) {
                n.links.retain(|l| *l != to);
            } else {
                n.links.push(to);
            }
        }
    });
}

fn pending_link_svg(d: &Drag) -> String {
    let Drag::Link { outgoing, wx, wy, cx, cy, .. } = d else { return String::new() };
    let ((x1, y1), (x2, y2)) = if *outgoing { ((*wx, *wy), (*cx, *cy)) } else { ((*cx, *cy), (*wx, *wy)) };
    let dx = ((x2 - x1).abs() / 2.0).max(50.0);
    format!(
        r#"<svg class="edges-svg" width="1" height="1"><path class="edge pending" d="M{x1:.1},{y1:.1} C{:.1},{y1:.1} {:.1},{y2:.1} {x2:.1},{y2:.1}" marker-end="url(#wb-arrow-on)"/></svg>"#,
        x1 + dx,
        x2 - dx,
    )
}

pub type View = (f64, f64, f64);

fn bounds(positions: &HashMap<String, [f64; 2]>) -> Option<(f64, f64, f64, f64)> {
    positions.values().fold(None, |acc, p| {
        let (x0, y0, x1, y1) = acc.unwrap_or((p[0], p[1], p[0] + NODE_W, p[1] + NODE_H));
        Some((x0.min(p[0]), y0.min(p[1]), x1.max(p[0] + NODE_W), y1.max(p[1] + NODE_H)))
    })
}

fn fit_view(g: &Graph, w: f64, h: f64) -> View {
    let Some((x0, y0, x1, y1)) = bounds(&g.positions()) else { return (40.0, 40.0, 1.0) };
    let pad = 32.0;
    let z = ((w - 2.0 * pad) / (x1 - x0)).min((h - 2.0 * pad) / (y1 - y0)).clamp(0.25, 1.1);
    ((w - (x1 - x0) * z) / 2.0 - x0 * z, (h - (y1 - y0) * z) / 2.0 - y0 * z, z)
}

pub fn zoom_at(view: View, cx: f64, cy: f64, factor: f64) -> View {
    let (ox, oy, z) = view;
    let nz = (z * factor).clamp(0.2, 2.5);
    let wx = (cx - ox) / z;
    let wy = (cy - oy) / z;
    (cx - wx * nz, cy - wy * nz, nz)
}

fn node_clicked(mut st: AppState, id: String, shift: bool) {
    if st.flow_draft.peek().is_some() {
        let mut draft = st.flow_draft.write();
        let steps = draft.as_mut().unwrap();
        if steps.last() == Some(&id) {
            steps.pop();
        } else {
            steps.push(id);
        }
        return;
    }
    if shift {
        st.toggle_selected(id);
    } else {
        st.select(Some(id));
        st.ghost_picks.set(Vec::new());
    }
    if *st.graph_pane.peek() != GraphPane::History {
        st.graph_pane.set(GraphPane::Node);
    }
}

fn ghost_clicked(mut st: AppState, pick: GhostPick, shift: bool) {
    if st.flow_draft.peek().is_some() {
        return;
    }
    if shift {
        st.toggle_ghost_pick(pick);
    } else {
        st.selected.set(Vec::new());
        st.ghost_picks.set(vec![pick]);
    }
    st.graph_pane.set(GraphPane::Node);
}

fn ghost_edges_svg(edges: &[(String, String, usize)], positions: &HashMap<String, [f64; 2]>) -> String {
    let mut out = String::from(r#"<svg class="edges-svg" width="1" height="1">"#);
    for (from, to, bi) in edges {
        let (Some(a), Some(b)) = (positions.get(from), positions.get(to)) else { continue };
        let (x1, y1) = (a[0] + NODE_W, a[1] + NODE_H / 2.0);
        let (x2, y2) = (b[0], b[1] + NODE_H / 2.0);
        let dx = ((x2 - x1).abs() / 2.0).max(50.0);
        out.push_str(&format!(
            r#"<path class="edge ghost" style="stroke: {}" d="M{x1:.1},{y1:.1} C{:.1},{y1:.1} {:.1},{y2:.1} {x2:.1},{y2:.1}" marker-end="url(#wb-arrow)"/>"#,
            ghost::color(*bi),
            x1 + dx,
            x2 - dx,
        ));
    }
    out.push_str("</svg>");
    out
}

fn mark_text(kind: &ChangeKind) -> String {
    match kind {
        ChangeKind::Modified(f) if f.is_empty() => "also added on that branch, differently".into(),
        k => ghost::describe(k),
    }
}

fn nodes_in_box(g: &Graph, positions: &HashMap<String, [f64; 2]>, (x0, y0, x1, y1): (f64, f64, f64, f64)) -> Vec<String> {
    g.nodes
        .iter()
        .filter(|n| {
            positions
                .get(&n.id)
                .is_some_and(|p| p[0] < x1 && p[0] + NODE_W > x0 && p[1] < y1 && p[1] + NODE_H > y0)
        })
        .map(|n| n.id.clone())
        .collect()
}

const HUB_MIN_CALLERS: usize = 5;

fn hubs(g: &Graph) -> HashMap<String, usize> {
    let mut count: HashMap<String, usize> = HashMap::new();
    for (_, to) in g.edges() {
        *count.entry(to).or_default() += 1;
    }
    count.retain(|_, n| *n >= HUB_MIN_CALLERS);
    count
}

#[derive(Clone, Copy, PartialEq)]
enum Emphasis {
    None,
    Path { dim_rest: bool },
    Focus { hide_rest: bool },
    Faint,
}

struct EdgeView<'a> {
    path: &'a [String],
    focus: &'a HashSet<String>,
    emphasis: Emphasis,
    hubs: Option<&'a HashMap<String, usize>>,
}

fn edges_svg(g: &Graph, positions: &HashMap<String, [f64; 2]>, view: EdgeView) -> String {
    let on_path = |a: &str, b: &str| view.path.windows(2).any(|w| (w[0] == a && w[1] == b) || (w[0] == b && w[1] == a));
    let mut out = String::from(
        r##"<svg class="edges-svg" width="1" height="1"><defs>
<marker id="wb-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto"><path d="M0,0 L10,5 L0,10 z" fill="#5c6370"/></marker>
<marker id="wb-arrow-on" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto"><path d="M0,0 L10,5 L0,10 z" fill="#d97757"/></marker>
<marker id="wb-arrow-hl" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto"><path d="M0,0 L10,5 L0,10 z" fill="#c8ced8"/></marker>
</defs>"##,
    );
    for (from, to) in g.edges() {
        let (Some(a), Some(b)) = (positions.get(&from), positions.get(&to)) else { continue };
        let (class, marker) = match view.emphasis {
            _ if on_path(&from, &to) => ("edge on", "wb-arrow-on"),
            Emphasis::Path { dim_rest: true } => ("edge dim", "wb-arrow"),
            Emphasis::Focus { .. } if view.focus.contains(&from) || view.focus.contains(&to) => ("edge hl", "wb-arrow-hl"),
            Emphasis::Focus { hide_rest: true } => continue,
            Emphasis::Focus { hide_rest: false } => ("edge dim", "wb-arrow"),
            Emphasis::Faint => ("edge faint", "wb-arrow"),
            _ => ("edge", "wb-arrow"),
        };
        let (x1, y1) = (a[0] + NODE_W, a[1] + NODE_H / 2.0);
        let stub = class != "edge on" && class != "edge hl" && view.hubs.is_some_and(|h| h.contains_key(&to));
        if stub {
            let color = g.node(&to).map(|n| graph::kind_color(&n.kind)).unwrap_or("#8a909c");
            out.push_str(&format!(
                r#"<path class="{class} stub" style="stroke: {color}" d="M{x1:.1},{y1:.1} L{:.1},{y1:.1}"/><circle class="{class} stub-end" style="fill: {color}" cx="{:.1}" cy="{y1:.1}" r="3"/>"#,
                x1 + 22.0,
                x1 + 25.0,
            ));
            continue;
        }
        let (x2, y2) = (b[0], b[1] + NODE_H / 2.0);
        let dx = ((x2 - x1).abs() / 2.0).max(50.0);
        out.push_str(&format!(
            r#"<path class="{class}" d="M{x1:.1},{y1:.1} C{:.1},{y1:.1} {:.1},{y2:.1} {x2:.1},{y2:.1}" marker-end="url(#{marker})"/>"#,
            x1 + dx,
            x2 - dx,
        ));
    }
    out.push_str("</svg>");
    out
}

#[component]
fn GraphCanvas() -> Element {
    let mut st = use_context::<AppState>();
    let mut view = use_signal(|| (40.0f64, 40.0f64, 1.0f64));
    let mut drag = use_signal(|| None::<Drag>);
    let mut mounted = use_signal(|| None::<Rc<MountedData>>);
    let mut origin = use_signal(|| (0.0f64, 0.0f64));
    let mut hovered = use_signal(|| None::<String>);
    let mut focus_only = use_signal(|| false);
    let mut collapse_hubs = use_signal(|| true);
    let mut arrange_undo = use_signal(|| None::<(Option<String>, HashMap<String, [f64; 2]>)>);
    let mut ghost_menu = use_signal(|| false);
    let mut branch_list = use_signal(|| None::<Result<Vec<String>, String>>);

    let refresh_branches = move || {
        let cwd = st.cwd.peek().clone();
        spawn(async move {
            let r = tokio::task::spawn_blocking(move || git::branches(&cwd)).await.unwrap_or_else(|e| Err(e.to_string()));
            branch_list.set(Some(r));
        });
    };

    let arrange = move |_| {
        let scope = st.scope.peek().last().cloned();
        let mut prev = HashMap::new();
        st.update_net(|g| prev = g.clear_positions());
        if !prev.is_empty() {
            arrange_undo.set(Some((scope, prev)));
            *st.fit_request.write() += 1;
        }
    };

    let undo_arrange = move |_| {
        let Some((_, prev)) = arrange_undo.peek().clone() else { return };
        st.update_net(|g| {
            for n in g.nodes.iter_mut() {
                if let Some(p) = prev.get(&n.id) {
                    n.pos = Some(*p);
                }
            }
        });
        arrange_undo.set(None);
        *st.fit_request.write() += 1;
    };

    let refresh_origin = move || {
        let Some(m) = mounted.peek().clone() else { return };
        spawn(async move {
            if let Ok(rect) = m.get_client_rect().await {
                origin.set((rect.origin.x, rect.origin.y));
            }
        });
    };

    use_effect(move || {
        let _ = st.fit_request.read();
        let Some(m) = mounted.read().clone() else { return };
        spawn(async move {
            if let Ok(rect) = m.get_client_rect().await {
                view.set(fit_view(&st.net_peek(), rect.size.width, rect.size.height));
            }
        });
    });

    let zoom_center = move |factor: f64| {
        let Some(m) = mounted.peek().clone() else { return };
        spawn(async move {
            if let Ok(rect) = m.get_client_rect().await {
                let v = *view.peek();
                view.set(zoom_at(v, rect.size.width / 2.0, rect.size.height / 2.0, factor));
            }
        });
    };

    let mut end_drag = move || {
        let d = drag.peek().clone();
        drag.set(None);
        match d {
            Some(Drag::Pan { moved: false, .. }) => {
                if st.flow_draft.peek().is_none() {
                    st.clear_context();
                }
            }
            Some(Drag::Node { moved: true, .. }) => {
                arrange_undo.set(None);
                st.save_graph();
            }
            Some(Drag::Node { id, moved: false, shift, .. }) => node_clicked(st, id, shift),
            Some(Drag::Select { sx, sy, cx, cy, additive }) => {
                let (ox, oy) = *origin.peek();
                let (px, py, z) = *view.peek();
                let world = |x: f64, y: f64| ((x - ox - px) / z, (y - oy - py) / z);
                let (x0, y0) = world(sx.min(cx), sy.min(cy));
                let (x1, y1) = world(sx.max(cx), sy.max(cy));
                let g = st.net_peek();
                let hits = nodes_in_box(&g, &g.positions(), (x0, y0, x1, y1));
                if additive {
                    let mut sel = st.selected.write();
                    for id in hits {
                        if !sel.contains(&id) {
                            sel.push(id);
                        }
                    }
                } else {
                    st.selected.set(hits);
                }
                st.graph_pane.set(GraphPane::Node);
            }
            _ => {}
        }
    };

    let on_move = move |e: MouseEvent| {
        let Some(d) = drag.peek().clone() else { return };
        if !e.held_buttons().contains(MouseButton::Primary) {
            end_drag();
            return;
        }
        let p = e.client_coordinates();
        match d {
            Drag::Pan { sx, sy, ox, oy, moved } => {
                let (dx, dy) = (p.x - sx, p.y - sy);
                if !moved && dx.abs() + dy.abs() < 3.0 {
                    return;
                }
                let mut v = view.write();
                v.0 = ox + dx;
                v.1 = oy + dy;
                drag.set(Some(Drag::Pan { sx, sy, ox, oy, moved: true }));
            }
            Drag::Node { id, sx, sy, mut group, moved, shift } => {
                let z = view.peek().2;
                let (dx, dy) = ((p.x - sx) / z, (p.y - sy) / z);
                if (!moved && dx.abs() + dy.abs() < 3.0 / z) || st.job.peek().is_some() {
                    return;
                }
                {
                    let scope = st.scope.peek().last().cloned();
                    let mut root = st.graph.write();
                    let Some(g) = root.net_mut(scope.as_deref()) else { return };
                    if !moved {
                        g.pin_all();
                        let auto = g.positions();
                        let sel = st.selected.peek();
                        let ids: Vec<String> = if sel.contains(&id) { sel.clone() } else { vec![id.clone()] };
                        group = ids.into_iter().filter_map(|i| auto.get(&i).map(|p| (i, p[0], p[1]))).collect();
                    }
                    for (gid, ox, oy) in &group {
                        if let Some(n) = g.node_mut(gid) {
                            n.pos = Some([(ox + dx).round(), (oy + dy).round()]);
                        }
                    }
                }
                drag.set(Some(Drag::Node { id, sx, sy, group, moved: true, shift }));
            }
            Drag::Select { sx, sy, additive, .. } => {
                drag.set(Some(Drag::Select { sx, sy, cx: p.x, cy: p.y, additive }));
            }
            Drag::Link { anchor, outgoing, sx, sy, wx, wy, .. } => {
                let z = view.peek().2;
                let (cx, cy) = (wx + (p.x - sx) / z, wy + (p.y - sy) / z);
                drag.set(Some(Drag::Link { anchor, outgoing, sx, sy, wx, wy, cx, cy }));
            }
        }
    };

    let on_wheel = move |e: WheelEvent| {
        e.prevent_default();
        let d = e.delta().strip_units();
        let m = e.modifiers();
        if m.ctrl() || m.meta() {
            zoom_center((-d.y * 0.01).exp());
        } else {
            let mut v = view.write();
            v.0 -= d.x;
            v.1 -= d.y;
        }
    };

    let g = st.net();
    let scope = st.current_scope();
    let mut positions = g.positions();
    let ghosts = st.ghosts.read().clone();
    let ghost_picks = st.ghost_picks.read().clone();
    let ghost_layer = ghost::layer(&g, scope.as_deref(), &ghosts, &mut positions);
    let ghost_svg = ghost_edges_svg(&ghost_layer.edges, &positions);
    // Nodes I'm editing that a ghost branch also changes or depends on.
    let invalid = {
        let empty = Graph::default();
        let base = st.baseline.read();
        let base = base.as_ref().unwrap_or(&empty);
        let root = st.graph.read();
        let mine = claims::claim(base, &root, &graph::diff(base, &root));
        ghost::invalidated(&mine, &ghosts, scope.as_deref())
    };
    let picked_branches = st.ghost_branches.read().clone();
    let branch_status: HashMap<String, (usize, Result<usize, String>)> = ghosts
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let status = match &b.error {
                Some(e) => Err(e.clone()),
                None => Ok(ghost_layer.nodes.iter().filter(|n| n.branch == i).count()),
            };
            (b.branch.clone(), (i, status))
        })
        .collect();
    let menu_open = *ghost_menu.read();
    let (px, py, z) = *view.read();
    let selected = st.selected.read().clone();
    let draft = st.flow_draft.read().clone();
    let select_box = match drag.read().as_ref() {
        Some(Drag::Select { sx, sy, cx, cy, .. }) => {
            let (ox, oy) = *origin.read();
            Some((sx.min(*cx) - ox, sy.min(*cy) - oy, (cx - sx).abs(), (cy - sy).abs()))
        }
        _ => None,
    };
    let active_flow = st.active_flow.read().clone();
    let path: Vec<String> = draft
        .clone()
        .or_else(|| active_flow.and_then(|id| g.flows.iter().find(|f| f.id == id).map(|f| f.nodes.clone())))
        .unwrap_or_default();
    let changes = st.changes();
    let changed_modules: Vec<String> = changes.iter().filter_map(|c| c.scope.clone()).collect();
    let status: HashMap<String, &'static str> = changes
        .into_iter()
        .filter(|c| c.subject == Subject::Node && c.scope == scope)
        .filter_map(|c| match c.kind {
            ChangeKind::Added => Some((c.id, "added")),
            ChangeKind::Modified(_) => Some((c.id, "modified")),
            ChangeKind::Removed => None,
        })
        .collect();
    let module_info: HashMap<String, (String, usize, bool)> = {
        let root = st.graph.read();
        g.nodes
            .iter()
            .filter_map(|n| n.module.as_ref())
            .map(|m| {
                let (title, count) = root.module(m).map(|x| (x.title.clone(), x.net.nodes.len())).unwrap_or_else(|| (format!("missing: {m}"), 0));
                (m.clone(), (title, count, changed_modules.contains(m)))
            })
            .collect()
    };
    let ref_info: HashMap<String, String> = {
        let cwd = st.cwd.read().clone();
        let loaded = st.refs.read();
        g.nodes
            .iter()
            .filter(|n| n.is_reference())
            .map(|n| {
                let r = n.reference.clone().unwrap_or_default();
                let label = reference::parse(&cwd, &r.source).map(|s| s.label()).unwrap_or_else(|| "no source".into());
                let wb = matches!(loaded.get(&reference::key(&r.source)), Some(RefState::Ready(Some(_))));
                let picked = if r.nodes.is_empty() { String::new() } else { format!(" · {}", r.nodes.len()) };
                (n.id.clone(), format!("↗ {label}{}{picked}", if wb { " ◆" } else { "" }))
            })
            .collect()
    };
    let focus: HashSet<String> = match hovered.read().clone() {
        Some(h) if g.node(&h).is_some() => HashSet::from([h]),
        _ => selected.iter().cloned().collect(),
    };
    let hide_rest = *focus_only.read();
    let emphasis = if !path.is_empty() {
        Emphasis::Path { dim_rest: draft.is_none() }
    } else if !focus.is_empty() {
        Emphasis::Focus { hide_rest }
    } else if hide_rest {
        Emphasis::Faint
    } else {
        Emphasis::None
    };
    let lit: Option<HashSet<String>> = match emphasis {
        Emphasis::Path { dim_rest: true } => Some(path.iter().cloned().collect()),
        Emphasis::Focus { .. } => Some(
            g.edges()
                .into_iter()
                .filter(|(a, b)| focus.contains(a) || focus.contains(b))
                .flat_map(|(a, b)| [a, b])
                .chain(focus.iter().cloned())
                .collect(),
        ),
        _ => None,
    };
    let hub_count = if *collapse_hubs.read() { hubs(&g) } else { HashMap::new() };
    let svg = edges_svg(
        &g,
        &positions,
        EdgeView { path: &path, focus: &focus, emphasis, hubs: (!hub_count.is_empty()).then_some(&hub_count) },
    );
    let has_pins = g.nodes.iter().any(|n| n.pos.is_some());
    let can_undo_arrange = arrange_undo.read().as_ref().is_some_and(|(s, _)| *s == scope);
    let busy_job = st.job.read().is_some();
    let grid = 24.0 * z;
    let nodes: Vec<Node> = g.nodes.clone();
    drop(g);
    let current_drag = drag.read().clone();
    let linking = matches!(current_drag, Some(Drag::Link { .. }));
    let pending_svg = current_drag.as_ref().map(pending_link_svg).unwrap_or_default();
    let can_link = draft.is_none() && st.job.read().is_none();
    let canvas_class = if draft.is_some() {
        "graph-canvas picking"
    } else if linking {
        "graph-canvas linking"
    } else {
        "graph-canvas"
    };

    rsx! {
        div {
            class: "{canvas_class}",
            style: "background-size: {grid}px {grid}px; background-position: {px}px {py}px;",
            onmounted: move |e| {
                mounted.set(Some(e.data()));
                refresh_origin();
            },
            oncontextmenu: move |e: MouseEvent| {
                if e.modifiers().ctrl() {
                    e.prevent_default();
                }
            },
            onmousedown: move |e: MouseEvent| {
                if e.trigger_button() != Some(MouseButton::Primary) {
                    return;
                }
                st.fade_chat();
                let p = e.client_coordinates();
                let m = e.modifiers();
                if (m.ctrl() || m.meta()) && st.flow_draft.peek().is_none() {
                    refresh_origin();
                    hovered.set(None);
                    drag.set(Some(Drag::Select { sx: p.x, sy: p.y, cx: p.x, cy: p.y, additive: m.shift() }));
                    return;
                }
                let (ox, oy, _) = *view.peek();
                drag.set(Some(Drag::Pan { sx: p.x, sy: p.y, ox, oy, moved: false }));
            },
            onmousemove: on_move,
            onmouseup: move |_| end_drag(),
            onmouseleave: move |_| end_drag(),
            onwheel: on_wheel,
            div { class: "graph-world", style: "transform: translate({px}px, {py}px) scale({z});",
                div { class: "edges", dangerous_inner_html: "{svg}" }
                if !ghost_layer.edges.is_empty() {
                    div { class: "edges", dangerous_inner_html: "{ghost_svg}" }
                }
                if linking {
                    div { class: "edges", dangerous_inner_html: "{pending_svg}" }
                }
                for n in nodes {
                    {
                        let [x, y] = positions.get(&n.id).copied().unwrap_or([0.0, 0.0]);
                        let module = n.module.as_ref().and_then(|m| module_info.get(m).cloned().map(|i| (m.clone(), i)));
                        let inner_changed = module.as_ref().is_some_and(|(_, (_, _, changed))| *changed);
                        let st_class = status.get(&n.id).copied().unwrap_or(if inner_changed { "modified" } else { "" });
                        let ext = ref_info.get(&n.id).cloned();
                        let mod_class = if module.is_some() {
                            "module-ref"
                        } else if ext.is_some() {
                            "ext-ref"
                        } else {
                            ""
                        };
                        let enter_id = n.module.clone();
                        let tip = if module.is_some() {
                            format!("{}\n\nDouble-click to open the module", n.description)
                        } else {
                            n.description.clone()
                        };
                        let sel = if selected.contains(&n.id) { "selected" } else { "" };
                        let dim = if lit.as_ref().is_some_and(|l| !l.contains(&n.id)) { "dim" } else { "" };
                        let hub = hub_count.get(&n.id).copied();
                        let marks = ghost_layer.marks.get(&n.id).cloned().unwrap_or_default();
                        let invalid_by = invalid.get(&n.id).cloned().unwrap_or_default();
                        let ghost_class = if !invalid_by.is_empty() {
                            "ghost-conflict"
                        } else if marks.iter().any(|m| m.kind == ChangeKind::Removed) {
                            "ghost-removed"
                        } else if !marks.is_empty() {
                            "ghost-touched"
                        } else {
                            ""
                        };
                        let mark_color = marks.first().map(|m| ghost::color(m.branch)).unwrap_or("");
                        let mark_tip = marks
                            .iter()
                            .map(|m| format!("{}: {}", ghosts[m.branch].who(), mark_text(&m.kind)))
                            .chain(invalid_by.iter().map(|bi| format!("{}: invalidates your edit to this node", ghosts[*bi].who())))
                            .collect::<Vec<_>>()
                            .join("\n");
                        let n_marks = marks.len();
                        let enter_hover = n.id.clone();
                        let leave_hover = n.id.clone();
                        let steps = path
                            .iter()
                            .enumerate()
                            .filter(|(_, p)| **p == n.id)
                            .map(|(i, _)| (i + 1).to_string())
                            .collect::<Vec<_>>()
                            .join(",");
                        let on = if steps.is_empty() { "" } else { "on-path" };
                        let color = graph::kind_color(&n.kind);
                        let id = n.id.clone();
                        let drop_id = n.id.clone();
                        let in_id = n.id.clone();
                        let out_id = n.id.clone();
                        let mut start_link = move |e: MouseEvent, anchor: String, outgoing: bool| {
                            if e.trigger_button() != Some(MouseButton::Primary) {
                                return;
                            }
                            e.stop_propagation();
                            let p = e.client_coordinates();
                            let (wx, wy) = (if outgoing { x + NODE_W } else { x }, y + NODE_H / 2.0);
                            drag.set(Some(Drag::Link { anchor, outgoing, sx: p.x, sy: p.y, wx, wy, cx: wx, cy: wy }));
                        };
                        rsx! {
                            div {
                                key: "{n.id}",
                                class: "gnode {st_class} {sel} {on} {mod_class} {dim} {ghost_class}",
                                style: "left: {x}px; top: {y}px; width: {NODE_W}px; height: {NODE_H}px; --kind: {color}; --ghost: {mark_color};",
                                title: "{tip}",
                                onmouseenter: move |e: MouseEvent| {
                                    e.stop_propagation();
                                    if matches!(*drag.peek(), Some(Drag::Select { .. })) {
                                        return;
                                    }
                                    hovered.set(Some(enter_hover.clone()));
                                },
                                onmouseleave: move |e: MouseEvent| {
                                    e.stop_propagation();
                                    if hovered.peek().as_deref() == Some(leave_hover.as_str()) {
                                        hovered.set(None);
                                    }
                                },
                                ondoubleclick: move |e: MouseEvent| {
                                    e.stop_propagation();
                                    if let Some(m) = enter_id.as_deref()
                                        && st.flow_draft.peek().is_none()
                                    {
                                        st.enter_module(m);
                                    }
                                },
                                onmousedown: move |e: MouseEvent| {
                                    let m = e.modifiers();
                                    if e.trigger_button() != Some(MouseButton::Primary) || m.ctrl() || m.meta() {
                                        return;
                                    }
                                    e.stop_propagation();
                                    st.fade_chat();
                                    let p = e.client_coordinates();
                                    drag.set(Some(Drag::Node { id: id.clone(), sx: p.x, sy: p.y, group: Vec::new(), moved: false, shift: m.shift() }));
                                },
                                onmouseup: move |_| {
                                    let Some(Drag::Link { anchor, outgoing, .. }) = drag.peek().clone() else { return };
                                    drag.set(None);
                                    if outgoing {
                                        toggle_link(st, anchor, drop_id.clone());
                                    } else {
                                        toggle_link(st, drop_id.clone(), anchor);
                                    }
                                },
                                if can_link {
                                    div {
                                        class: "gnode-port in",
                                        title: "Drag to a node to make it call this one (drop on an existing caller to remove the link)",
                                        onmousedown: move |e| start_link(e, in_id.clone(), false),
                                    }
                                    div {
                                        class: "gnode-port out",
                                        title: "Drag to a node to make this one call it (drop on an existing callee to remove the link)",
                                        onmousedown: move |e| start_link(e, out_id.clone(), true),
                                    }
                                }
                                div { class: "gnode-title", "{n.title}" }
                                div { class: "gnode-meta",
                                    match (&module, &ext) {
                                        (Some((_, (title, count, _))), _) => rsx! {
                                            span { class: "gnode-kind", "▣ {title} · {count}" }
                                        },
                                        (None, Some(label)) => rsx! { span { class: "gnode-kind", "{label}" } },
                                        (None, None) => rsx! { span { class: "gnode-kind", "{n.kind}" } },
                                    }
                                    match st_class {
                                        "added" => rsx! { span { class: "gnode-status", "new" } },
                                        "modified" => rsx! { span { class: "gnode-status", "edited" } },
                                        _ => rsx! {},
                                    }
                                    if n_marks > 0 {
                                        span { class: "gnode-ghost-tag", title: "{mark_tip}", "⧉ {n_marks}" }
                                    }
                                }
                                if !steps.is_empty() {
                                    span { class: "gnode-step", "{steps}" }
                                }
                                if let Some(callers) = hub {
                                    span {
                                        class: "gnode-hub",
                                        title: "Called by {callers} nodes. Their links are shortened to coloured stubs; hover or select a node to see them in full.",
                                        "⇠ {callers}"
                                    }
                                }
                            }
                        }
                    }
                }
                for gn in ghost_layer.nodes.clone() {
                    {
                        let [x, y] = positions.get(&gn.key).copied().unwrap_or([0.0, 0.0]);
                        let branch = ghosts[gn.branch].branch.clone();
                        let who = ghosts[gn.branch].who();
                        let bcolor = ghost::color(gn.branch);
                        let color = graph::kind_color(&gn.node.kind);
                        let pick = GhostPick { branch: branch.clone(), scope: scope.clone(), id: gn.node.id.clone() };
                        let sel = if ghost_picks.contains(&pick) { "selected" } else { "" };
                        let tip = format!(
                            "{}\n\nWork in progress on {who}. Click to send it to Workbench as context, Shift+click to add it to the context.",
                            gn.node.description
                        );
                        rsx! {
                            div {
                                key: "{gn.key}",
                                class: "gnode ghost {sel}",
                                style: "left: {x}px; top: {y}px; width: {NODE_W}px; height: {NODE_H}px; --kind: {color}; --ghost: {bcolor};",
                                title: "{tip}",
                                onmousedown: move |e: MouseEvent| {
                                    let m = e.modifiers();
                                    if e.trigger_button() == Some(MouseButton::Primary) && !m.ctrl() && !m.meta() {
                                        e.stop_propagation();
                                    }
                                },
                                onclick: move |e: MouseEvent| {
                                    let m = e.modifiers();
                                    if m.ctrl() || m.meta() {
                                        return;
                                    }
                                    e.stop_propagation();
                                    ghost_clicked(st, pick.clone(), m.shift());
                                },
                                div { class: "gnode-title", "{gn.node.title}" }
                                div { class: "gnode-meta",
                                    span { class: "gnode-kind", "{gn.node.kind}" }
                                    span { class: "gnode-branch", title: "{who}", "⧉ {who}" }
                                }
                            }
                        }
                    }
                }
            }
            if let Some((bx, by, bw, bh)) = select_box {
                div { class: "select-box", style: "left: {bx}px; top: {by}px; width: {bw}px; height: {bh}px;" }
            }
            div { class: "zoom-controls", onmousedown: move |e| e.stop_propagation(),
                button { onclick: move |_| zoom_center(1.0 / 1.2), "−" }
                button { onclick: move |_| zoom_center(1.2), "+" }
                button { onclick: move |_| *st.fit_request.write() += 1, "Fit" }
                span { class: "zoom-sep" }
                button {
                    class: if hide_rest { "toggle on" } else { "toggle" },
                    title: "Show only the links of the hovered or selected nodes",
                    onclick: move |_| {
                        let v = *focus_only.peek();
                        focus_only.set(!v);
                    },
                    "Focus"
                }
                button {
                    class: if *collapse_hubs.read() { "toggle on" } else { "toggle" },
                    title: "Shorten links into nodes with {HUB_MIN_CALLERS} or more callers to coloured stubs",
                    onclick: move |_| {
                        let v = *collapse_hubs.peek();
                        collapse_hubs.set(!v);
                    },
                    "Hubs"
                }
                button {
                    class: if picked_branches.is_empty() { "toggle" } else { "toggle on" },
                    title: "Show work from other git branches as ghost nodes",
                    onclick: move |_| {
                        let open = !*ghost_menu.peek();
                        ghost_menu.set(open);
                        if open {
                            refresh_branches();
                        }
                    },
                    if picked_branches.is_empty() { "Ghosts" } else { "Ghosts ({picked_branches.len()})" }
                }
                span { class: "zoom-sep" }
                if can_undo_arrange {
                    button { disabled: busy_job, title: "Restore the positions from before Arrange", onclick: undo_arrange, "Undo arrange" }
                }
                button {
                    disabled: busy_job || !has_pins,
                    title: if has_pins { "Forget the manual positions and lay the graph out automatically" } else { "The graph is already laid out automatically" },
                    onclick: arrange,
                    "Arrange"
                }
            }
            if menu_open {
                div {
                    class: "ghost-menu",
                    onmousedown: move |e| e.stop_propagation(),
                    onwheel: move |e| e.stop_propagation(),
                    div { class: "ghost-menu-head",
                        b { "Ghost branches" }
                        div { class: "spacer" }
                        button {
                            class: "small",
                            title: "Reload the branch list and the ghost graphs",
                            onclick: move |_| {
                                refresh_branches();
                                st.load_ghosts();
                            },
                            "↻"
                        }
                        button {
                            class: "small",
                            title: "Show every branch that has work this branch doesn't have yet",
                            onclick: move |_| st.show_team(),
                            "Team"
                        }
                        button { class: "small", onclick: move |_| ghost_menu.set(false), "✕" }
                    }
                    div { class: "muted small-text",
                        "Show the graph work on other branches as ghost nodes. Click a ghost to send it to Workbench as context."
                    }
                    div { class: "ghost-branches",
                        match branch_list.read().clone() {
                            None => rsx! { div { class: "muted small-text", "Loading branches…" } },
                            Some(Err(e)) => rsx! { div { class: "muted small-text", "{e}" } },
                            Some(Ok(list)) if list.is_empty() => rsx! { div { class: "muted small-text", "No other branches." } },
                            Some(Ok(list)) => rsx! {
                                for name in list {
                                    {
                                        let on = picked_branches.contains(&name);
                                        let status = branch_status.get(&name).cloned();
                                        let dot = status.as_ref().map(|(i, _)| ghost::color(*i)).unwrap_or("transparent");
                                        let ids: Vec<String> = status
                                            .as_ref()
                                            .map(|(i, _)| ghost_layer.nodes.iter().filter(|n| n.branch == *i).map(|n| n.node.id.clone()).collect())
                                            .unwrap_or_default();
                                        let toggle_name = name.clone();
                                        let pick_name = name.clone();
                                        let pick_scope = scope.clone();
                                        rsx! {
                                            div { key: "{name}", class: "ghost-branch",
                                                label {
                                                    input {
                                                        r#type: "checkbox",
                                                        checked: on,
                                                        onchange: move |_| {
                                                            let mut list = st.ghost_branches.peek().clone();
                                                            match list.iter().position(|b| *b == toggle_name) {
                                                                Some(i) => {
                                                                    list.remove(i);
                                                                }
                                                                None => list.push(toggle_name.clone()),
                                                            }
                                                            st.set_ghost_branches(list);
                                                        },
                                                    }
                                                    span { class: "ghost-dot", style: "background: {dot}" }
                                                    span { class: "ghost-name", title: "{name}", "{name}" }
                                                }
                                                if on {
                                                    match status.map(|(_, s)| s) {
                                                        None => rsx! { span { class: "muted small-text", "loading…" } },
                                                        Some(Err(e)) => rsx! { span { class: "muted small-text", title: "{e}", "no graph" } },
                                                        Some(Ok(0)) => rsx! { span { class: "muted small-text", "no new nodes here" } },
                                                        Some(Ok(n)) => rsx! {
                                                            span { class: "muted small-text", "{n} node{plural(n)}" }
                                                            button {
                                                                class: "small",
                                                                title: "Add all of this branch's ghost nodes here to Workbench's context",
                                                                onclick: move |_| {
                                                                    for id in &ids {
                                                                        let pick = GhostPick { branch: pick_name.clone(), scope: pick_scope.clone(), id: id.clone() };
                                                                        if !st.ghost_picks.peek().contains(&pick) {
                                                                            st.ghost_picks.write().push(pick);
                                                                        }
                                                                    }
                                                                    st.graph_pane.set(GraphPane::Node);
                                                                },
                                                                "◇ Context"
                                                            }
                                                        },
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            },
                        }
                    }
                }
            }
            if draft.is_some() {
                div { class: "picking-hint", "Click nodes in order to build the flow" }
            }
            if linking {
                div { class: "picking-hint", "Drop on a node to link it, or on an already linked node to unlink" }
            }
        }
    }
}

#[component]
fn GhostPane() -> Element {
    let mut st = use_context::<AppState>();
    let picks = st.ghost_picks.read().clone();
    let ghosts = st.ghosts.read().clone();
    let Some(pick) = picks.last().cloned() else { return rsx! {} };
    let found = ghosts
        .iter()
        .enumerate()
        .find(|(_, b)| b.branch == pick.branch)
        .and_then(|(i, b)| b.node(pick.scope.as_deref(), &pick.id).map(|n| (i, b, n.clone())));
    let Some((bi, b, node)) = found else {
        return rsx! { div { class: "muted pad", "This ghost node is no longer on branch {pick.branch}." } };
    };
    let empty = Graph::default();
    let net = b.graph.net(pick.scope.as_deref()).unwrap_or(&empty);
    let change = b.change(pick.scope.as_deref(), &pick.id).map(|k| ghost::describe(&k)).unwrap_or_else(|| "on that branch".into());
    let list = |v: Vec<String>| if v.is_empty() { "none".to_string() } else { v.join(", ") };
    let calls = list(node.links.iter().map(|l| net.title_of(l)).collect());
    let callers = list(net.incoming(&node.id).iter().map(|n| n.title.clone()).collect());
    let others = st.ghost_titles();
    let others: Vec<String> = others[..others.len().saturating_sub(1)].to_vec();
    let others_text = others.join(", ");
    let branch = b.branch.clone();
    let color = ghost::color(bi);
    let kind_color = graph::kind_color(&node.kind);
    let files = node.files.clone();
    let remove = pick.clone();

    rsx! {
        div { class: "inspector",
            div { class: "inspector-head",
                span { class: "kind-dot", style: "background: {kind_color}" }
                code { "{node.id}" }
                span { class: "chip ghost-chip", style: "--ghost: {color}", "⧉ {branch}" }
                div { class: "spacer" }
                button { class: "small", onclick: move |_| st.toggle_ghost_pick(remove.clone()), "Remove from context" }
            }
            div { class: "muted small-text",
                "Ghost node: {change}. It only exists on branch {branch}, so it is read-only here. It is sent to Workbench as context with your messages."
            }
            label { "Title" }
            div { class: "ghost-field", "{node.title}" }
            label { "Kind" }
            div { class: "ghost-field", "{node.kind}" }
            label { "Description" }
            div { class: "ghost-field ghost-desc", "{node.description}" }
            if !files.is_empty() {
                label { "Files on {branch}" }
                div { class: "ghost-field",
                    for f in files {
                        div { key: "{f}", code { "{f}" } }
                    }
                }
            }
            label { "Calls" }
            div { class: "ghost-field", "{calls}" }
            label { "Called by" }
            div { class: "ghost-field", "{callers}" }
            if !others.is_empty() {
                label { "Also in context" }
                div { class: "ghost-field", "{others_text}" }
            }
        }
    }
}

#[component]
fn NodePane() -> Element {
    let mut st = use_context::<AppState>();
    let locked = st.graph_locked();

    let scope = st.current_scope();
    let selection = st.selected.read().clone();
    if selection.len() > 1 {
        return rsx! { SelectionPane {} };
    }
    let Some(id) = selection.into_iter().next() else {
        if !st.ghost_picks.read().is_empty() {
            return rsx! { GhostPane {} };
        }
        if let Some(m) = scope {
            return rsx! { ModulePane { id: m } };
        }
        return rsx! {
            div { class: "muted pad",
                "Select a node to read and edit what it does. Edit descriptions, links or add and remove nodes, then commit the changes so Workbench updates the code to match. "
                "Shift+click or Ctrl+drag to select several nodes; selected nodes are sent to Workbench as context."
            }
        };
    };
    let g = st.net();
    let Some(node) = g.node(&id).cloned() else {
        return rsx! { div { class: "muted pad", "Node no longer exists." } };
    };
    let attached: Option<(String, String, usize)> = node.module.as_ref().map(|m| {
        let root = st.graph.read();
        let uses = root.module_uses(m).len();
        (m.clone(), root.module_title(m), uses)
    });
    let module_choices: Vec<(String, String)> = {
        let root = st.graph.read();
        root.modules
            .iter()
            .filter(|m| root.can_attach(scope.as_deref(), &m.id))
            .map(|m| (m.id.clone(), m.title.clone()))
            .collect()
    };
    let incoming: Vec<(String, String)> = g.incoming(&id).iter().map(|n| (n.id.clone(), n.title.clone())).collect();
    let outgoing: Vec<(String, String)> = node.links.iter().map(|l| (l.clone(), g.title_of(l))).collect();
    let candidates: Vec<(String, String)> = g
        .nodes
        .iter()
        .filter(|n| n.id != id && !node.links.contains(&n.id))
        .map(|n| (n.id.clone(), n.title.clone()))
        .collect();
    let input_candidates: Vec<(String, String)> = g
        .nodes
        .iter()
        .filter(|n| n.id != id && !n.links.contains(&id))
        .map(|n| (n.id.clone(), n.title.clone()))
        .collect();
    let mut related_files: Vec<(String, &'static str, String)> = Vec::new();
    let outputs = node.links.iter().filter_map(|l| g.node(l)).map(|n| ("dependency", n));
    let inputs = g.incoming(&id).into_iter().map(|n| ("used by", n));
    for (relation, other) in outputs.chain(inputs) {
        for f in &other.files {
            if !node.files.contains(f) && !related_files.iter().any(|(rf, rr, _)| rf == f && *rr == relation) {
                related_files.push((f.clone(), relation, other.title.clone()));
            }
        }
    }
    drop(g);
    let is_ref = node.is_reference();
    let reference = node.reference.clone().unwrap_or_default();
    let change = st
        .changes()
        .into_iter()
        .find(|c| c.id == id && c.subject == Subject::Node && c.scope == scope)
        .map(|c| c.kind);
    let n_history = st.node_history.read().count_for(scope.as_deref(), &id);
    let rev_scope = scope.clone();
    let id_mod = id.clone();

    let id_title = id.clone();
    let id_kind = id.clone();
    let id_desc = id.clone();
    let id_link = id.clone();
    let id_rev = id.clone();
    let id_child = id.clone();
    let id_flow = id.clone();
    let id_del = id.clone();

    rsx! {
        div { class: "inspector",
            div { class: "inspector-head",
                span { class: "kind-dot", style: "background: {graph::kind_color(&node.kind)}" }
                code { "{node.id}" }
                match &change {
                    Some(ChangeKind::Added) => rsx! { span { class: "chip added", "new" } },
                    Some(ChangeKind::Modified(fields)) => rsx! {
                        span { class: "chip modified",
                            "edited: "
                            {fields.iter().map(|f| f.label()).collect::<Vec<_>>().join(", ")}
                        }
                    },
                    _ => rsx! {},
                }
                div { class: "spacer" }
                if n_history > 0 {
                    button {
                        class: "small",
                        title: "Show how this node changed over time",
                        onclick: move |_| st.show_pane(GraphPane::History),
                        "History ({n_history})"
                    }
                }
                if change.is_some() {
                    button {
                        class: "small",
                        disabled: locked,
                        onclick: move |_| st.revert_change(rev_scope.clone(), Subject::Node, &id_rev),
                        "Revert"
                    }
                }
            }
            if let Some((mid, mtitle, uses)) = attached {
                div { class: "module-box",
                    span { "▣ Attaches module " }
                    b { "{mtitle}" }
                    span { class: "muted small-text", " · used in {uses} place{plural(uses)}" }
                    div { class: "spacer" }
                    button {
                        class: "small primary",
                        onclick: move |_| st.enter_module(&mid),
                        "Open module"
                    }
                }
            }
            label { "Title" }
            input {
                value: "{node.title}",
                disabled: locked,
                oninput: move |e| {
                    let v = e.value();
                    let id = id_title.clone();
                    st.update_net(|g| if let Some(n) = g.node_mut(&id) { n.title = v });
                },
            }
            label { "Kind" }
            ComboBox {
                value: node.kind.clone(),
                suggestions: KNOWN_KINDS.iter().map(|k| k.to_string()).collect::<Vec<_>>(),
                disabled: locked,
                onchange: move |v: String| {
                    let id = id_kind.clone();
                    st.update_net(|g| if let Some(n) = g.node_mut(&id) { n.kind = v });
                },
            }
            if is_ref {
                ReferenceBox { id: id.clone(), reference: reference.clone(), title: node.title.clone(), locked }
            } else {
                label { "Module" }
                Dropdown {
                    disabled: locked,
                    value: node.module.clone().unwrap_or_default(),
                    choices: std::iter::once(Choice::new("", "None — a plain node"))
                        .chain(module_choices.into_iter().map(|(mid, mtitle)| Choice::new(mid, format!("Attaches “{mtitle}”"))))
                        .collect::<Vec<_>>(),
                    onchange: move |v: String| {
                        let id = id_mod.clone();
                        st.update_net(|g| {
                            if let Some(n) = g.node_mut(&id) {
                                n.module = if v.is_empty() { None } else { Some(v) };
                            }
                        });
                    },
                }
            }
            label { "What it does" }
            textarea {
                class: "desc",
                value: "{node.description}",
                disabled: locked,
                placeholder: if is_ref { "Why this reference matters here, e.g. “Our API client must follow this project's auth flow”." } else { "Describe what this part should do. Changes here are turned into code changes when you commit." },
                oninput: move |e| {
                    let v = e.value();
                    let id = id_desc.clone();
                    st.update_net(|g| if let Some(n) = g.node_mut(&id) { n.description = v });
                },
            }
            label { "Inputs — called by" }
            div { class: "chips",
                for (iid, title) in incoming {
                    span { key: "{iid}", class: "chip link",
                        span {
                            class: "chip-click",
                            onclick: {
                                let iid = iid.clone();
                                move |_| st.select(Some(iid.clone()))
                            },
                            "{title}"
                        }
                        if !locked {
                            span {
                                class: "chip-x",
                                onclick: {
                                    let iid = iid.clone();
                                    let id = id_link.clone();
                                    move |_| st.update_net(|g| if let Some(n) = g.node_mut(&iid) { n.links.retain(|l| *l != id) })
                                },
                                "✕"
                            }
                        }
                    }
                }
                if !locked && !input_candidates.is_empty() {
                    Dropdown {
                        class: "add-link",
                        value: "",
                        placeholder: "+ input from…",
                        choices: input_candidates.into_iter().map(|(cid, title)| Choice::new(cid, title)).collect::<Vec<_>>(),
                        onchange: {
                            let id = id_link.clone();
                            move |source: String| {
                                let id = id.clone();
                                st.update_net(|g| if let Some(n) = g.node_mut(&source) { n.links.push(id) });
                            }
                        },
                    }
                }
            }
            label { "Outputs — calls / depends on" }
            div { class: "chips",
                for (lid, title) in outgoing {
                    span { key: "{lid}", class: "chip link",
                        span {
                            class: "chip-click",
                            onclick: {
                                let lid = lid.clone();
                                move |_| st.select(Some(lid.clone()))
                            },
                            "{title}"
                        }
                        if !locked {
                            span {
                                class: "chip-x",
                                onclick: {
                                    let lid = lid.clone();
                                    let id = id_link.clone();
                                    move |_| st.update_net(|g| if let Some(n) = g.node_mut(&id) { n.links.retain(|l| *l != lid) })
                                },
                                "✕"
                            }
                        }
                    }
                }
                if !locked && !candidates.is_empty() {
                    Dropdown {
                        class: "add-link",
                        value: "",
                        placeholder: "+ output to…",
                        choices: candidates.into_iter().map(|(cid, title)| Choice::new(cid, title)).collect::<Vec<_>>(),
                        onchange: {
                            let id = id_link.clone();
                            move |target: String| {
                                st.update_net(|g| if let Some(n) = g.node_mut(&id) { n.links.push(target) });
                            }
                        },
                    }
                }
            }
            if !is_ref || !node.files.is_empty() {
                label { "Files" }
            }
            if node.files.is_empty() && !is_ref {
                div { class: "muted small-text", "Not mapped yet — Workbench fills this in when it maps or processes the node." }
            }
            ul { class: "file-list",
                for f in node.files.iter().cloned() {
                    li {
                        key: "{f}",
                        onclick: {
                            let f = f.clone();
                            move |_| st.open(&f)
                        },
                        "{f}"
                    }
                }
            }
            if !related_files.is_empty() {
                label { "Related files" }
                ul { class: "file-list",
                    for (f, relation, via) in related_files {
                        li {
                            key: "{relation}:{f}",
                            title: "{relation} · {via}",
                            onclick: {
                                let f = f.clone();
                                move |_| st.open(&f)
                            },
                            span { class: "file-relation", "{relation}" }
                            span { "{f}" }
                            span { class: "muted", "{via}" }
                        }
                    }
                }
            }
            div { class: "inspector-actions",
                button { disabled: locked, onclick: move |_| add_node(st, Some(id_child.clone())), "+ Linked node" }
                button {
                    onclick: move |_| st.flow_draft.set(Some(vec![id_flow.clone()])),
                    "Start flow here"
                }
                span { class: "muted small-text", "Chat messages include this node as context" }
                div { class: "spacer" }
                button {
                    class: "danger",
                    disabled: locked,
                    onclick: move |_| {
                        let id = id_del.clone();
                        st.update_net(|g| g.remove_node(&id));
                        st.selected.set(Vec::new());
                    },
                    "Delete"
                }
            }
        }
    }
}

#[component]
fn ReferenceBox(id: String, reference: Reference, title: String, locked: bool) -> Element {
    let mut st = use_context::<AppState>();
    let cwd = st.cwd.read().clone();
    let source = reference.source.trim().to_string();
    let parsed = reference::parse(&cwd, &source);
    let state = st.refs.read().get(&reference::key(&source)).cloned();
    let ext = match &state {
        Some(RefState::Ready(Some(g))) => Some(g.clone()),
        _ => None,
    };
    let ext_graph = ext.as_ref().map(|g| &g.graph);
    let picked: Vec<(usize, String, String)> = reference
        .nodes
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let found = ext_graph.and_then(|g| reference::find(g, r));
            let desc = match (found, ext_graph) {
                (Some(n), _) => n.description.clone(),
                (None, Some(_)) => "Not found in the referenced graph any more.".into(),
                (None, None) => String::new(),
            };
            (i, reference::label(ext_graph, r), desc)
        })
        .collect();
    let choices: Vec<(String, String)> = ext_graph
        .map(|g| {
            reference::choices(g)
                .into_iter()
                .filter(|(r, _)| !reference.nodes.contains(r))
                .filter_map(|(r, label)| serde_json::to_string(&r).ok().map(|v| (v, label)))
                .collect()
        })
        .unwrap_or_default();
    let open_target = parsed.clone();
    let id_src = id.clone();
    let id_commit = id.clone();
    let id_pick = id.clone();
    let reload_src = source.clone();

    rsx! {
        div { class: "ref-box",
            label { "Source" }
            input {
                value: "{reference.source}",
                disabled: locked,
                placeholder: "~/code/other-app, ./docs/spec.md, https://github.com/owner/repo/tree/main/src, https://docs.site, npm:react, crate:serde",
                oninput: move |e| {
                    let v = e.value();
                    edit_reference(st, &id_src, |n| n.reference.as_mut().unwrap().source = v);
                },
                onchange: move |e| {
                    let v = e.value();
                    let cwd = st.cwd.peek().clone();
                    let name = reference::parse(&cwd, &v).map(|s| s.short_name());
                    let rename = title == "External reference";
                    edit_reference(st, &id_commit, |n| {
                        if rename && let Some(name) = name {
                            n.title = name;
                        }
                    });
                    st.load_reference(&v, false);
                    st.touch_context();
                },
            }
            div { class: "ref-status",
                match &parsed {
                    None if source.is_empty() => rsx! {
                        span { class: "muted small-text", "A local folder or file, a GitHub repository (URL or owner/repo, optionally with a branch and path), a website, or a package (npm:, crate:, pypi:, go:)." }
                    },
                    None => rsx! { span { class: "warn small-text", "Not a recognised path, URL or package" } },
                    Some(src) => rsx! {
                        span { class: "chip ref-kind", "{src.label()}" }
                        span { class: "ref-display", title: "{src.display()}", "{src.display()}" }
                        div { class: "spacer" }
                        button {
                            class: "small",
                            onclick: move |_| if let Some(s) = &open_target { open_source(s) },
                            "Open"
                        }
                    },
                }
            }
            if parsed.is_some() {
                div { class: "ref-status",
                    match &state {
                        None => rsx! { span { class: "muted small-text", "Not checked for a Workbench graph yet." } },
                        Some(RefState::Loading) => rsx! { span { class: "muted small-text", "Looking for a Workbench graph…" } },
                        Some(RefState::Ready(None)) => rsx! {
                            span { class: "muted small-text", "No Workbench graph found. Workbench reads the source directly when it needs it." }
                        },
                        Some(RefState::Ready(Some(g))) => rsx! {
                            span { class: "chip ref-wb", "◆ Workbench" }
                            span { class: "muted small-text", title: "{g.location}",
                                "{g.graph.nodes.len()} nodes · {g.graph.modules.len()} modules"
                            }
                        },
                        Some(RefState::Failed(e)) => rsx! { span { class: "warn small-text", "{e}" } },
                    }
                    div { class: "spacer" }
                    button {
                        class: "small",
                        disabled: matches!(state, Some(RefState::Loading)),
                        onclick: move |_| st.load_reference(&reload_src, true),
                        "Reload"
                    }
                }
            }
            if ext.is_some() || !picked.is_empty() {
                label { "Referenced nodes" }
                if picked.is_empty() {
                    div { class: "muted small-text",
                        "Pick the nodes of the other project that matter here. Their descriptions and files are sent to Workbench as context."
                    }
                }
                ul { class: "ref-nodes",
                    for (i, label, desc) in picked {
                        li { key: "{i}:{label}",
                            div { class: "ref-node-head",
                                b { "{label}" }
                                div { class: "spacer" }
                                if !locked {
                                    span {
                                        class: "chip-x",
                                        title: "Stop referencing this node",
                                        onclick: {
                                            let id = id.clone();
                                            move |_| {
                                                edit_reference(st, &id, |n| {
                                                    let nodes = &mut n.reference.as_mut().unwrap().nodes;
                                                    if i < nodes.len() {
                                                        nodes.remove(i);
                                                    }
                                                });
                                                st.touch_context();
                                            }
                                        },
                                        "✕"
                                    }
                                }
                            }
                            if !desc.is_empty() {
                                div { class: "muted small-text", "{desc}" }
                            }
                        }
                    }
                }
                if !locked && !choices.is_empty() {
                    Dropdown {
                        class: "add-link",
                        value: "",
                        placeholder: "+ reference a node…",
                        choices: choices.into_iter().map(|(value, label)| Choice::new(value, label)).collect::<Vec<_>>(),
                        onchange: move |v: String| {
                            let Ok(r) = serde_json::from_str::<RefNode>(&v) else { return };
                            edit_reference(st, &id_pick, |n| {
                                let nodes = &mut n.reference.as_mut().unwrap().nodes;
                                if !nodes.contains(&r) {
                                    nodes.push(r);
                                }
                            });
                            st.touch_context();
                        },
                    }
                }
            }
        }
    }
}

#[component]
fn SelectionPane() -> Element {
    let mut st = use_context::<AppState>();
    let locked = st.graph_locked();
    let g = st.net();
    let nodes: Vec<(String, String, String)> = st
        .selected
        .read()
        .iter()
        .filter_map(|id| g.node(id))
        .map(|n| (n.id.clone(), n.title.clone(), graph::kind_color(&n.kind).to_string()))
        .collect();
    drop(g);
    let count = nodes.len();

    rsx! {
        div { class: "inspector",
            div { class: "inspector-head",
                b { "{count} nodes selected" }
                div { class: "spacer" }
                button { class: "small", onclick: move |_| st.selected.set(Vec::new()), "Clear selection" }
            }
            div { class: "muted small-text",
                "Chat messages and Prompt requests include these nodes as context. Shift+click a node to add or remove it, Ctrl+drag to box-select (add Shift to extend)."
            }
            div { class: "chips",
                for (nid, title, color) in nodes {
                    span { key: "{nid}", class: "chip link",
                        span { class: "kind-dot", style: "background: {color}" }
                        span {
                            class: "chip-click",
                            title: "Select only this node",
                            onclick: {
                                let nid = nid.clone();
                                move |_| st.select(Some(nid.clone()))
                            },
                            "{title}"
                        }
                        span {
                            class: "chip-x",
                            title: "Remove from the selection",
                            onclick: {
                                let nid = nid.clone();
                                move |_| st.toggle_selected(nid.clone())
                            },
                            "✕"
                        }
                    }
                }
            }
            div { class: "inspector-actions",
                button {
                    title: "Start a flow through the selected nodes in the order you selected them",
                    onclick: move |_| {
                        let steps = st.selected.peek().clone();
                        st.flow_draft.set(Some(steps));
                    },
                    "Flow from selection"
                }
                div { class: "spacer" }
                button {
                    class: "danger",
                    disabled: locked,
                    onclick: move |_| {
                        let ids = st.selected.peek().clone();
                        st.update_net(|g| {
                            for id in &ids {
                                g.remove_node(id);
                            }
                        });
                        st.selected.set(Vec::new());
                    },
                    "Delete {count} nodes"
                }
            }
        }
    }
}

#[component]
fn ModulePane(id: String) -> Element {
    let mut st = use_context::<AppState>();
    let locked = st.graph_locked();
    let root = st.graph.read().clone();
    let Some(module) = root.module(&id).cloned() else {
        return rsx! { div { class: "muted pad", "Module no longer exists." } };
    };
    let uses: Vec<(Option<String>, String, String)> = root
        .module_uses(&id)
        .into_iter()
        .map(|(scope, node)| {
            let place = match &scope {
                None => "Main".to_string(),
                Some(m) => format!("▣ {}", root.module_title(m)),
            };
            let title = root.net(scope.as_deref()).map(|n| n.title_of(&node)).unwrap_or_else(|| node.clone());
            (scope, node, format!("{place} › {title}"))
        })
        .collect();
    let change = st
        .changes()
        .into_iter()
        .find(|c| c.subject == Subject::Module && c.id == id)
        .map(|c| c.kind);
    let id_title = id.clone();
    let id_desc = id.clone();
    let id_rev = id.clone();
    let id_del = id.clone();
    let n_uses = uses.len();
    let color = graph::kind_color("module");

    rsx! {
        div { class: "inspector",
            div { class: "inspector-head",
                span { class: "kind-dot", style: "background: {color}" }
                b { "▣ Module" }
                code { "{module.id}" }
                match &change {
                    Some(ChangeKind::Added) => rsx! { span { class: "chip added", "new" } },
                    Some(ChangeKind::Modified(fields)) => rsx! {
                        span { class: "chip modified",
                            "edited: "
                            {fields.iter().map(|f| f.label()).collect::<Vec<_>>().join(", ")}
                        }
                    },
                    _ => rsx! {},
                }
                div { class: "spacer" }
                if change.is_some() {
                    button {
                        class: "small",
                        disabled: locked,
                        onclick: move |_| st.revert_change(Some(id_rev.clone()), Subject::Module, &id_rev),
                        "Revert"
                    }
                }
            }
            div { class: "muted small-text",
                "A module is an isolated node network. Attach it as a single node wherever it is used; select a node here to edit it."
            }
            label { "Title" }
            input {
                value: "{module.title}",
                disabled: locked,
                oninput: move |e| {
                    let v = e.value();
                    let id = id_title.clone();
                    st.update_graph(|g| if let Some(m) = g.module_mut(&id) { m.title = v });
                },
            }
            label { "What it does" }
            textarea {
                class: "desc",
                value: "{module.description}",
                disabled: locked,
                placeholder: "Describe the feature and the interface the rest of the system uses.",
                oninput: move |e| {
                    let v = e.value();
                    let id = id_desc.clone();
                    st.update_graph(|g| if let Some(m) = g.module_mut(&id) { m.description = v });
                },
            }
            label { "Used in ({n_uses})" }
            if uses.is_empty() {
                div { class: "muted small-text", "Not attached anywhere yet. Go up and use + Module… to attach it." }
            }
            ul { class: "file-list",
                for (scope, node, label) in uses {
                    li {
                        key: "{label}",
                        onclick: move |_| st.show_change(scope.clone(), Some(node.clone())),
                        "{label}"
                    }
                }
            }
            div { class: "inspector-actions",
                span { class: "muted small-text", "{module.net.nodes.len()} nodes · {module.net.flows.len()} flows" }
                div { class: "spacer" }
                button {
                    class: "danger",
                    disabled: locked,
                    title: "Delete the module and every node that attaches it",
                    onclick: move |_| {
                        let id = id_del.clone();
                        st.update_graph(|g| g.remove_module(&id));
                    },
                    "Delete module"
                }
            }
        }
    }
}

const HISTORY_PAGE: usize = 50;

#[derive(Clone, PartialEq)]
enum HistoryGroup {
    /// Not a git repository: one flat list without headers.
    NoGit,
    Uncommitted,
    Commit(git::CommitInfo),
}

/// Groups newest-first history entries: uncommitted ones first, then one group per commit,
/// newest commit first. Without git, everything goes in a single `NoGit` group.
fn group_by_commit(
    entries: impl Iterator<Item = Entry>,
    commits: Option<&HashMap<String, git::CommitInfo>>,
) -> Vec<(HistoryGroup, Vec<Entry>)> {
    let mut groups: Vec<(HistoryGroup, Vec<Entry>)> = Vec::new();
    for e in entries {
        let g = match commits {
            None => HistoryGroup::NoGit,
            Some(m) => m.get(&e.uid).cloned().map_or(HistoryGroup::Uncommitted, HistoryGroup::Commit),
        };
        match groups.iter_mut().find(|(k, _)| *k == g) {
            Some((_, items)) => items.push(e),
            None => groups.push((g, vec![e])),
        }
    }
    groups.sort_by_key(|(g, _)| match g {
        HistoryGroup::NoGit | HistoryGroup::Uncommitted => (0, 0),
        HistoryGroup::Commit(c) => (1, u64::MAX - c.at),
    });
    groups
}

#[component]
fn HistoryPane() -> Element {
    let mut st = use_context::<AppState>();
    let mut only_focus = use_signal(|| true);
    let mut limit = use_signal(|| HISTORY_PAGE);
    let scope = st.current_scope();
    let selected = st.selected.read().clone();
    let focus: Option<(String, String)> = match selected.as_slice() {
        [id] => Some((node_history::key(scope.as_deref(), false, id), st.net().title_of(id))),
        [] => scope.as_ref().map(|m| (node_history::key(Some(m), true, m), format!("▣ {}", st.graph.read().module_title(m)))),
        _ => None,
    };
    let filtering = focus.is_some() && *only_focus.read();
    let history = st.node_history.read();
    let entries: Vec<Entry> = history
        .entries
        .iter()
        .rev()
        .filter(|e| !filtering || focus.as_ref().is_some_and(|(k, _)| e.key() == *k))
        .cloned()
        .collect();
    drop(history);
    let shown = entries.len().min(*limit.read());
    let more = entries.len() - shown;
    // Which commit added each entry to the shared log (None outside a git repo).
    let commits = use_resource(move || async move {
        let _ = st.repo_version.read();
        let _ = st.node_history.read().entries.len();
        let cwd = st.cwd.read().clone();
        tokio::task::spawn_blocking(move || git::is_repo(&cwd).then(|| git::line_commits(&cwd, node_history::HISTORY_FILE).unwrap_or_default()))
            .await
            .ok()
            .flatten()
    });
    let commits = commits.read().clone().flatten();
    let groups = group_by_commit(entries.into_iter().take(shown), commits.as_ref());
    let me = st.me.read().clone();
    let module_titles: std::collections::HashMap<String, String> =
        st.graph.read().modules.iter().map(|m| (m.id.clone(), m.title.clone())).collect();

    rsx! {
        div { class: "history-pane",
            div { class: "inspector-actions",
                if let Some((_, title)) = focus.clone() {
                    button {
                        class: if filtering { "small toggle on" } else { "small toggle" },
                        title: "Show only the history of the selected node",
                        onclick: move |_| {
                            let v = *only_focus.peek();
                            only_focus.set(!v);
                        },
                        "Only {title}"
                    }
                }
                span { class: "muted small-text",
                    "Every change to a node's title, kind, links, files or description, by you, teammates, Workbench or edits outside Workbench. Shared with the repo in .workbench/node-history.jsonl."
                }
            }
            if groups.is_empty() {
                div { class: "muted pad",
                    if filtering { "No recorded changes for this node yet." } else { "No recorded changes yet. Edits to nodes show up here as diffs." }
                }
            }
            for (group, items) in groups {
                match &group {
                    HistoryGroup::NoGit => rsx! {},
                    HistoryGroup::Uncommitted => rsx! {
                        div { class: "history-group uncommitted",
                            b { "Uncommitted" }
                            span { class: "muted small-text", "{items.len()} change{plural(items.len())} not in a commit on this branch yet" }
                        }
                    },
                    HistoryGroup::Commit(c) => {
                        let when = crate::history::ago(std::time::UNIX_EPOCH + std::time::Duration::from_secs(c.at));
                        rsx! {
                            div { class: "history-group", title: "{c.sha}",
                                code { "{c.short}" }
                                span { class: "history-group-subject", "{c.subject}" }
                                div { class: "spacer" }
                                span { class: "muted small-text", "{c.author} · {when}" }
                            }
                        }
                    }
                }
                for e in items {
                {
                    let tag = match &group {
                        HistoryGroup::NoGit => None,
                        HistoryGroup::Uncommitted => Some(("chip modified".to_string(), "uncommitted".to_string(), "Not in a commit on this branch yet".to_string())),
                        HistoryGroup::Commit(c) => Some(("chip".to_string(), c.short.clone(), format!("Committed in {} · {}", c.short, c.subject))),
                    };
                    let (sign, cls) = match e.kind() {
                        "added" => ("+", "added"),
                        "removed" => ("−", "removed"),
                        _ => ("~", "modified"),
                    };
                    let rows = node_history::line_diff(e.before.as_deref().unwrap_or(""), e.after.as_deref().unwrap_or(""));
                    let place = match (&e.scope, e.module) {
                        (Some(m), false) => format!(" · in ▣ {}", module_titles.get(m).cloned().unwrap_or_else(|| m.clone())),
                        _ => String::new(),
                    };
                    let title = if e.module { format!("▣ {}", e.title) } else { e.title.clone() };
                    let what = if e.module { "module" } else { "node" };
                    let detail = format!("{what} {} by {}{place}", e.kind(), e.who(me.as_deref()));
                    let when = crate::history::ago(e.when());
                    let can_show = e.after.is_some();
                    let show_scope = e.scope.clone();
                    let show_id = (!e.module).then(|| e.id.clone());
                    let key = format!("{}:{}", e.at, e.key());
                    rsx! {
                        details { key: "{key}", class: "history-entry {cls}", open: true,
                            summary {
                                span { class: "change-sign", "{sign}" }
                                span { class: "change-title", "{title}" }
                                span { class: "muted small-text history-detail", "{detail}" }
                                div { class: "spacer" }
                                if let Some((chip_cls, chip, chip_title)) = tag {
                                    span { class: "{chip_cls}", title: "{chip_title}", "{chip}" }
                                }
                                span { class: "history-when", "{when}" }
                                if can_show {
                                    button {
                                        class: "small",
                                        onclick: move |ev: MouseEvent| {
                                            ev.prevent_default();
                                            ev.stop_propagation();
                                            st.show_change(show_scope.clone(), show_id.clone());
                                        },
                                        "Show"
                                    }
                                }
                            }
                            table { class: "hunk-lines node-diff",
                                tbody {
                                    for (i, row) in rows.into_iter().enumerate() {
                                        match row {
                                            Row::Line { op, old, new, text } => rsx! {
                                                tr {
                                                    key: "{i}",
                                                    class: match op {
                                                        Op::Add => "added",
                                                        Op::Del => "removed",
                                                        Op::Same => "context",
                                                    },
                                                    td { class: "ln", {old.map(|n| n.to_string()).unwrap_or_default()} }
                                                    td { class: "ln", {new.map(|n| n.to_string()).unwrap_or_default()} }
                                                    td { class: "sign",
                                                        match op {
                                                            Op::Add => "+",
                                                            Op::Del => "−",
                                                            Op::Same => " ",
                                                        }
                                                    }
                                                    td { class: "src", "{text}" }
                                                }
                                            },
                                            Row::Skip(n) => rsx! {
                                                tr { key: "{i}", class: "meta",
                                                    td { class: "ln" }
                                                    td { class: "ln" }
                                                    td { class: "sign" }
                                                    td { class: "src", "⋯ {n} unchanged line{plural(n)}" }
                                                }
                                            },
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                }
            }
            if more > 0 {
                button {
                    class: "small",
                    onclick: move |_| *limit.write() += HISTORY_PAGE,
                    "Show {more.min(HISTORY_PAGE)} more ({more} older)"
                }
            }
        }
    }
}

#[component]
fn PromptPane() -> Element {
    let mut st = use_context::<AppState>();
    let mut request = use_signal(String::new);
    let busy = st.chat_busy();
    let locked = st.graph_locked();
    let awaiting = st.awaiting_answers();
    let mode = (*st.active_chat.read()).map(|c| *c.chat_mode.read()).unwrap_or(ChatMode::Plan);
    let empty = request.read().trim().is_empty();
    let context = st.selected_titles().join(", ");

    let draft = move |_| {
        let text = request.peek().trim().to_string();
        if text.is_empty() || st.chat_busy() || st.job.peek().is_some() {
            return;
        }
        let mode = st.start_job(JobKind::Draft);
        let scope = st.scope.peek().last().cloned();
        let selection = match (st.selection_context(), st.ghost_context()) {
            (None, None) => None,
            (a, b) => Some(format!("{}{}", a.unwrap_or_default(), b.unwrap_or_default())),
        };
        let prompt = prompts::draft_nodes(&st.graph.peek(), scope.as_deref(), &text, selection.as_deref(), mode, &st.rules.peek());
        let n = st.selected.peek().len();
        let label = match selection {
            Some(_) => format!("Add nodes from prompt ({}, {n} selected node{})", mode.label(), plural(n)),
            None => format!("Add nodes from prompt ({})", mode.label()),
        };
        request.set(String::new());
        st.chat_or_new().send(Cmd::Prompt { label, text: prompt });
    };

    rsx! {
        div { class: "prompt-pane",
            if awaiting {
                div { class: "hint-box",
                    "Workbench has questions about your request. Answer them in the chat to continue, or cancel the job from the toolbar."
                }
            }
            textarea {
                class: "note",
                placeholder: "Describe the behaviour to add, e.g. \"Map every incoming and outgoing HTTP request as route and external nodes\"",
                value: "{request}",
                disabled: locked,
                oninput: move |e| request.set(e.value()),
            }
            if !context.is_empty() {
                div { class: "muted small-text", "Context: ◇ {context}" }
            }
            div { class: "inspector-actions",
                span { class: "muted small-text", "{mode.label()} mode: {mode.hint()}" }
                div { class: "spacer" }
                button {
                    class: "primary",
                    disabled: busy || locked || empty,
                    title: "Workbench adds nodes to the graph only. Commit them afterwards to generate the code.",
                    onclick: draft,
                    "Add nodes"
                }
            }
        }
    }
}

/// The other branches whose claims overlap the claim of `changes` (pending since the baseline).
fn claim_overlaps(st: AppState, changes: &[graph::Change], open: &[claims::BranchClaim]) -> Vec<claims::BranchClaim> {
    let empty = Graph::default();
    let base = st.baseline.peek();
    let mine = claims::claim(base.as_ref().unwrap_or(&empty), &st.graph.peek(), changes);
    claims::overlaps(&mine, open)
}

/// One line per overlapping branch, e.g. "feature/auth (Anna): Auth, depends on DB · 2 files".
fn overlap_text(st: AppState, o: &claims::BranchClaim) -> String {
    let g = st.graph.peek();
    let title = |(scope, id): &claims::NodeKey| g.net(scope.as_deref()).map(|n| n.title_of(id)).unwrap_or_else(|| id.clone());
    let mut parts: Vec<String> = o.claim.nodes.iter().map(title).collect();
    parts.extend(o.claim.dependents.iter().map(|k| format!("depends on {}", title(k))));
    if !o.claim.files.is_empty() {
        parts.push(format!("{} file{}", o.claim.files.len(), plural(o.claim.files.len())));
    }
    format!("{} ({}): {}", o.branch, o.authors.join(", "), parts.join(" · "))
}

#[component]
fn ChangesPane() -> Element {
    let mut st = use_context::<AppState>();
    let mut note = use_signal(String::new);
    let busy = st.chat_busy();
    let locked = st.graph_locked();
    let changes = st.changes();
    let has_baseline = st.baseline.read().is_some();
    let has_nodes = !st.graph.read().nodes.is_empty();
    // Claims of the other branches, read from git so overlaps show before anything runs.
    let mut open_claims = use_resource(move || async move {
        let cwd = st.cwd.read().clone();
        tokio::task::spawn_blocking(move || claims::open_claims(&cwd)).await.unwrap_or_default()
    });
    let overlaps = open_claims.read().as_ref().map(|open| claim_overlaps(st, &changes, open)).unwrap_or_default();
    let free = changes.iter().filter(|c| !overlaps.iter().any(|o| o.claim.touches(c))).count();

    // `only_free` leaves out the changes that overlap another branch's claim.
    let mut process = move |only_free: bool| {
        let mut changes = st.changes();
        let open = open_claims.peek().clone().unwrap_or_default();
        if only_free {
            let taken = claim_overlaps(st, &changes, &open);
            changes.retain(|c| !taken.iter().any(|o| o.claim.touches(c)));
        }
        if changes.is_empty() || st.chat_busy() || st.job.peek().is_some() {
            return;
        }
        let overlaps = claim_overlaps(st, &changes, &open);
        let mode = st.start_job(JobKind::Process);
        let text = st.with_refs(|refs| prompts::process(&st.graph.peek(), &changes, &st.rules.peek(), &note.peek(), mode, Some(refs), &overlaps));
        let label = format!("Process {} graph change{} ({})", changes.len(), plural(changes.len()), mode.label());
        note.set(String::new());
        st.chat_or_new().send(Cmd::Prompt { label, text });
    };

    if changes.is_empty() {
        return rsx! {
            div { class: "muted pad",
                if has_nodes {
                    "The graph matches the code. Edit node descriptions, links or nodes and your changes will show up here."
                } else {
                    "No graph yet."
                }
            }
        };
    }

    rsx! {
        div { class: "changes",
            if !has_baseline {
                div { class: "hint-box",
                    "This graph hasn't been processed yet, so every node counts as new. If it already describes the code, choose "
                    b { "Mark in sync" }
                    " instead of processing."
                }
            }
            for c in changes.iter() {
                {
                    let n = c.node().clone();
                    let is_module = c.subject == Subject::Module;
                    let what = if is_module { "module" } else { "node" };
                    let (sign, cls, detail) = match &c.kind {
                        ChangeKind::Added => ("+", "added", format!("new {what}")),
                        ChangeKind::Removed => ("−", "removed", format!("{what} removed")),
                        ChangeKind::Modified(f) => ("~", "modified", f.iter().map(|f| f.label()).collect::<Vec<_>>().join(", ")),
                    };
                    let place = match (&c.scope, is_module) {
                        (Some(m), false) => format!(" · in ▣ {}", st.graph.peek().module_title(m)),
                        _ => String::new(),
                    };
                    let title = if is_module { format!("▣ {}", n.title) } else { n.title.clone() };
                    let removed = c.kind == ChangeKind::Removed;
                    let key = c.key();
                    let show_scope = c.scope.clone();
                    let show_id = if is_module { None } else { Some(n.id.clone()) };
                    let rev_scope = c.scope.clone();
                    let subject = c.subject;
                    let id_rev = n.id.clone();
                    rsx! {
                        div { key: "{key}", class: "change {cls}",
                            span { class: "change-sign", "{sign}" }
                            div { class: "change-body",
                                div { class: "change-title", "{title}" }
                                div { class: "muted small-text", "{detail}{place}" }
                            }
                            if !removed {
                                button {
                                    class: "small",
                                    onclick: move |_| st.show_change(show_scope.clone(), show_id.clone()),
                                    "Show"
                                }
                            }
                            button {
                                class: "small",
                                disabled: locked,
                                onclick: move |_| st.revert_change(rev_scope.clone(), subject, &id_rev),
                                "Revert"
                            }
                        }
                    }
                }
            }
            if !overlaps.is_empty() {
                div { class: "hint-box claim-warning",
                    div { class: "claim-warning-head",
                        b { "Other branches are working on this too" }
                        div { class: "spacer" }
                        button { class: "small", title: "Read the other branches again", onclick: move |_| open_claims.restart(), "↻" }
                    }
                    for o in overlaps.iter() {
                        div { key: "{o.branch}", class: "small-text", "{overlap_text(st, o)}" }
                    }
                    div { class: "small-text",
                        "Process anyway to go first, process only the changes no one else touches, or wait until that branch is merged."
                    }
                }
            }
            textarea {
                class: "note",
                placeholder: "Extra instructions for this commit (optional)",
                value: "{note}",
                disabled: locked,
                oninput: move |e| note.set(e.value()),
            }
            div { class: "inspector-actions",
                button {
                    class: "primary",
                    disabled: busy || locked,
                    title: "Send the diff between the graph and the code to Workbench and let it change the code",
                    onclick: move |_| process(false),
                    if overlaps.is_empty() { "Commit & process" } else { "Process anyway" }
                }
                if !overlaps.is_empty() {
                    button {
                        disabled: busy || locked || free == 0,
                        title: "Process only the changes that no other branch touches",
                        onclick: move |_| process(true),
                        "Process {free} without overlap"
                    }
                }
                button {
                    disabled: busy || locked,
                    title: "Accept the graph as an accurate description of the code without changing any code",
                    onclick: move |_| st.mark_in_sync(),
                    "Mark in sync"
                }
            }
        }
    }
}

#[component]
fn FlowsPane() -> Element {
    let mut st = use_context::<AppState>();
    let mut debugging = use_signal(|| None::<String>);
    let mut mode = use_signal(|| DebugMode::Trace);
    let mut symptom = use_signal(String::new);
    let busy = st.chat_busy();
    let locked = st.graph_locked();
    let g = st.net();
    let active = st.active_flow.read().clone();
    let open_debug = debugging.read().clone();

    let mut start_debug = move |flow: Flow| {
        if st.chat_busy() {
            return;
        }
        let m = *mode.peek();
        let text = prompts::debug(&st.net_peek(), &flow, m, &symptom.peek(), &st.rules.peek());
        let label = format!("Debug flow “{}” — {}", flow.name, m.label());
        st.active_flow.set(Some(flow.id.clone()));
        symptom.set(String::new());
        debugging.set(None);
        st.chat_or_new().send(Cmd::Prompt { label, text });
    };

    rsx! {
        div { class: "flows",
            div { class: "inspector-actions",
                button {
                    disabled: g.nodes.is_empty(),
                    onclick: move |_| {
                        let start = st.selected.peek().clone();
                        st.flow_draft.set(Some(start));
                    },
                    "+ New flow"
                }
                span { class: "muted small-text", "A flow is a path through the graph you can trace, instrument or test." }
            }
            if g.flows.is_empty() {
                div { class: "muted pad", "No flows yet." }
            }
            for flow in g.flows.iter().cloned() {
                {
                    let is_active = active.as_ref() == Some(&flow.id);
                    let is_debug = open_debug.as_ref() == Some(&flow.id);
                    let path = flow.nodes.iter().map(|id| g.title_of(id)).collect::<Vec<_>>().join(" → ");
                    let fid = flow.id.clone();
                    let fid_dbg = flow.id.clone();
                    let fid_del = flow.id.clone();
                    let flow_run = flow.clone();
                    rsx! {
                        div { key: "{flow.id}", class: if is_active { "flow active" } else { "flow" },
                            div { class: "flow-head",
                                span { class: "flow-name", "{flow.name}" }
                                div { class: "spacer" }
                                button {
                                    class: "small",
                                    onclick: move |_| st.active_flow.set(if is_active { None } else { Some(fid.clone()) }),
                                    if is_active { "Hide" } else { "Show" }
                                }
                                button {
                                    class: if is_debug { "small primary" } else { "small" },
                                    onclick: move |_| debugging.set(if is_debug { None } else { Some(fid_dbg.clone()) }),
                                    "Debug…"
                                }
                                button {
                                    class: "small",
                                    disabled: locked,
                                    onclick: move |_| {
                                        let id = fid_del.clone();
                                        st.update_net(|g| g.flows.retain(|f| f.id != id));
                                    },
                                    "✕"
                                }
                            }
                            div { class: "flow-path", "{path}" }
                            if is_debug {
                                div { class: "debug-form",
                                    Dropdown {
                                        value: mode.read().key().to_string(),
                                        choices: DebugMode::ALL.iter().map(|m| Choice::new(m.key(), m.label())).collect::<Vec<_>>(),
                                        onchange: move |v: String| mode.set(DebugMode::from_key(&v)),
                                    }
                                    textarea {
                                        placeholder: "What's going wrong? (optional) e.g. “returns 500 when the list is empty”",
                                        value: "{symptom}",
                                        oninput: move |e| symptom.set(e.value()),
                                    }
                                    button {
                                        class: "primary",
                                        disabled: busy,
                                        onclick: move |_| start_debug(flow_run.clone()),
                                        "Start debugging"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn FlowDraft() -> Element {
    let mut st = use_context::<AppState>();
    let mut name = use_signal(String::new);
    let steps = st.flow_draft.read().clone().unwrap_or_default();
    let g = st.net();
    let locked = st.graph_locked();
    let can_save = !name.read().trim().is_empty() && !steps.is_empty() && !locked;

    let save = move |_| {
        let steps = st.flow_draft.peek().clone().unwrap_or_default();
        let n = name.peek().trim().to_string();
        if n.is_empty() || steps.is_empty() {
            return;
        }
        let mut new_id = String::new();
        st.update_net(|g| {
            new_id = g.unique_id(&format!("flow {n}"));
            g.flows.push(Flow { id: new_id.clone(), name: n, nodes: steps, note: String::new() });
        });
        st.flow_draft.set(None);
        st.active_flow.set(Some(new_id));
        st.graph_pane.set(GraphPane::Flows);
    };

    rsx! {
        div { class: "flow-draft",
            div { class: "flow-head",
                b { "New flow" }
                div { class: "spacer" }
                button { class: "small", onclick: move |_| st.flow_draft.set(None), "Cancel" }
                button { class: "small primary", disabled: !can_save, onclick: save, "Save flow" }
            }
            input {
                placeholder: "Flow name, e.g. “User signs up”",
                value: "{name}",
                oninput: move |e| name.set(e.value()),
            }
            if steps.is_empty() {
                div { class: "muted pad", "Click the first node on the graph." }
            }
            ol { class: "steps",
                for (i, sid) in steps.iter().enumerate() {
                    {
                        let unlinked = i > 0 && !g.linked(&steps[i - 1], sid);
                        rsx! {
                            li { key: "{i}",
                                span { "{g.title_of(sid)}" }
                                if unlinked {
                                    span { class: "warn small-text", title: "These two nodes aren't linked in the graph", " ⚠ not linked" }
                                }
                                span {
                                    class: "chip-x",
                                    onclick: move |_| {
                                        if let Some(s) = st.flow_draft.write().as_mut() {
                                            if i < s.len() {
                                                s.remove(i);
                                            }
                                        }
                                    },
                                    "✕"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
