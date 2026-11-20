use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;

use crate::claims;
use crate::claude::ChatMode;
use crate::code::FileDiffView;
use crate::dropdown::{Choice, ComboBox, Dropdown};
use crate::ghost::{self, GhostPick};
use crate::git;
use crate::ledger;
use crate::graph::{self, ChangeKind, Decision, Flow, Graph, KNOWN_KINDS, Module, NODE_H, NODE_W, Node, REFERENCE_KIND, RefNode, Reference, Subject};
use crate::prompts::{self, DebugMode};
use crate::icons::{Icon, IconSvg};
use crate::routing;
use crate::rules;
use crate::reference::{self, RefState, Source};
use crate::node_history::{self, Entry, Op};
use crate::work_order;
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
    let n_decisions = decision_timeline(&st.graph.read()).len();
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
            div { class: "graph-row", onmounted: move |e| row_el.set(Some(e.data())),
                div { class: "graph-canvas-wrap",
                    GraphCanvas {}
                    div { class: "graph-float-top",
                        div { class: "graph-toolbar",
                            button {
                                disabled: busy || rescan_blocked || job.is_some(),
                                title: if rescan_blocked { "Process or revert your graph changes before rescanning" } else { "Let Workbench explore the code and (re)build the graph" },
                                onclick: generate,
                                if has_nodes { "Rescan code" } else { "Map codebase" }
                            }
                            button {
                                disabled: busy || rescan_blocked || job.is_some() || !has_nodes,
                                title: if rescan_blocked { "Process or revert your graph changes before mining decisions" } else { "Read commits, comments, docs, tickets and past chats, and record why each part is built the way it is" },
                                onclick: move |_| mine_decisions(st, Vec::new()),
                                "Mine decisions"
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
                                    button {
                                        class: "small",
                                        onclick: move |_| {
                                            st.finish_job(false);
                                        },
                                        "Cancel"
                                    }
                                },
                                Some(Job { kind: JobKind::Generate, .. }) => rsx! { span { class: "badge busy", "Mapping…" } },
                                Some(Job { kind: JobKind::Process, .. }) => rsx! { span { class: "badge busy", "Processing…" } },
                                Some(Job { kind: JobKind::Draft, .. }) => rsx! { span { class: "badge busy", "Drafting nodes…" } },
                                Some(Job { kind: JobKind::Mine, .. }) => rsx! { span { class: "badge busy", "Mining decisions…" } },
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
                                IconSvg { icon: Icon::ArrowUp }
                                " Up"
                            }
                        }
                    }
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
                                {pane_btn(GraphPane::Decisions, format!("Decisions ({n_decisions})"))}
                                {pane_btn(GraphPane::Prompt, "Prompt".into())}
                                div { class: "spacer" }
                                button {
                                    class: "pane-toggle",
                                    title: "Hide the node menu to give the graph more room",
                                    onclick: move |_| st.set_layout(|l| l.node_menu_open = false),
                                    IconSvg { icon: Icon::ChevronRight }
                                }
                            }
                            div { class: "pane-body",
                                match pane {
                                    GraphPane::Node => rsx! { NodePane {} },
                                    GraphPane::Flows => rsx! { FlowsPane {} },
                                    GraphPane::Changes => rsx! { ChangesPane {} },
                                    GraphPane::History => rsx! { HistoryPane {} },
                                    GraphPane::Decisions => rsx! { DecisionsPane {} },
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

/// Starts the job that records the decisions and invariants of `ids` (every node when empty)
/// from the project's history.
fn mine_decisions(mut st: AppState, ids: Vec<String>) {
    if st.chat_busy() || st.job.peek().is_some() {
        return;
    }
    let cwd = st.cwd.peek().clone();
    let scope = st.scope.peek().last().cloned();
    let transcripts = crate::history::transcripts_dir(&cwd).filter(|d| d.is_dir());
    let text = prompts::mine_decisions(&st.graph.peek(), scope.as_deref(), &ids, transcripts.as_deref());
    let label = if ids.is_empty() { "Mine decisions for the whole graph".to_string() } else { format!("Mine decisions for {}", ids.join(", ")) };
    st.start_job(JobKind::Mine);
    st.chat_or_new().send(Cmd::Prompt { label, text });
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

/// The view that fits `positions` into a `w` × `h` canvas, zoomed no further out than `min_zoom`.
fn fit_positions(positions: &HashMap<String, [f64; 2]>, w: f64, h: f64, min_zoom: f64) -> View {
    let Some((x0, y0, x1, y1)) = bounds(positions) else { return (40.0, 40.0, 1.0) };
    let pad = 32.0;
    let z = ((w - 2.0 * pad) / (x1 - x0)).min((h - 2.0 * pad) / (y1 - y0)).clamp(min_zoom, 1.1);
    ((w - (x1 - x0) * z) / 2.0 - x0 * z, (h - (y1 - y0) * z) / 2.0 - y0 * z, z)
}

/// The view that fits the whole network.
fn fit_view(g: &Graph, w: f64, h: f64) -> View {
    fit_positions(&g.positions(), w, h, 0.25)
}

/// The view that fits `id` and the nodes it links to or from, zoomed in far enough to read.
fn focus_view(g: &Graph, id: &str, w: f64, h: f64) -> View {
    let mut near = g.positions();
    near.retain(|other, _| other == id || g.linked(id, other));
    fit_positions(&near, w, h, 0.6)
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
    let pairs: Vec<(String, String)> = edges.iter().map(|(a, b, _)| (a.clone(), b.clone())).collect();
    for ((_, _, bi), route) in edges.iter().zip(routing::route(&pairs, positions)) {
        let Some(pts) = route else { continue };
        out.push_str(&format!(
            r#"<path class="edge ghost" style="stroke: {}" d="{}" marker-end="url(#wb-arrow)"/>"#,
            ghost::color(*bi),
            routing::svg_path(&pts, routing::CORNER),
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

/// Draws `edges` along their `routes` (one per edge, from `routing::route`).
fn edges_svg(
    edges: &[(String, String)],
    routes: &[Option<Vec<routing::Point>>],
    nodes: &HashMap<&str, &Node>,
    view: EdgeView,
) -> String {
    let on_path: HashSet<(&str, &str)> =
        view.path.windows(2).flat_map(|w| [(w[0].as_str(), w[1].as_str()), (w[1].as_str(), w[0].as_str())]).collect();
    let mut out = String::from(
        r##"<svg class="edges-svg" width="1" height="1"><defs>
<marker id="wb-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto"><path d="M0,0 L10,5 L0,10 z" fill="#5c6370"/></marker>
<marker id="wb-arrow-on" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto"><path d="M0,0 L10,5 L0,10 z" fill="#d97757"/></marker>
<marker id="wb-arrow-hl" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto"><path d="M0,0 L10,5 L0,10 z" fill="#c8ced8"/></marker>
</defs>"##,
    );
    let mut on_top = String::new();
    for ((from, to), route) in edges.iter().zip(routes) {
        let Some(pts) = route else { continue };
        let (class, marker) = match view.emphasis {
            _ if on_path.contains(&(from.as_str(), to.as_str())) => ("edge on", "wb-arrow-on"),
            Emphasis::Path { dim_rest: true } => ("edge dim", "wb-arrow"),
            Emphasis::Focus { .. } if view.focus.contains(from) || view.focus.contains(to) => ("edge hl", "wb-arrow-hl"),
            Emphasis::Focus { hide_rest: true } => continue,
            Emphasis::Focus { hide_rest: false } => ("edge dim", "wb-arrow"),
            Emphasis::Faint => ("edge faint", "wb-arrow"),
            _ => ("edge", "wb-arrow"),
        };
        let (x1, y1) = pts[0];
        let lit = class == "edge on" || class == "edge hl";
        let stub = !lit && view.hubs.is_some_and(|h| h.contains_key(to));
        if stub {
            let color = nodes.get(to.as_str()).map(|n| graph::kind_color(&n.kind)).unwrap_or("#8a909c");
            out.push_str(&format!(
                r#"<path class="{class} stub" style="stroke: {color}" d="M{x1:.1},{y1:.1} L{:.1},{y1:.1}"/><circle class="{class} stub-end" style="fill: {color}" cx="{:.1}" cy="{y1:.1}" r="3"/>"#,
                x1 + 22.0,
                x1 + 25.0,
            ));
            continue;
        }
        // Links share trunks, so lit links are drawn last to stay on top of the dimmed ones.
        let layer = if lit { &mut on_top } else { &mut out };
        layer.push_str(&format!(
            r#"<path class="{class}" d="{}" marker-end="url(#{marker})"/>"#,
            routing::svg_path(pts, routing::CORNER),
        ));
    }
    out.push_str(&on_top);
    out.push_str("</svg>");
    out
}

/// Where every node and ghost node sits. Recomputed only when the network or the ghosts change.
#[derive(PartialEq)]
struct Placed {
    positions: HashMap<String, [f64; 2]>,
    ghost: ghost::Layer,
}

/// What the canvas shows on nodes besides their title. Recomputed only when the graph, the
/// baseline, the ghosts or the loaded references change, not on pan, hover or drag.
#[derive(PartialEq)]
struct NodeMarks {
    status: HashMap<String, &'static str>,
    stale: HashMap<String, String>,
    invalid: HashMap<String, Vec<usize>>,
    modules: HashMap<String, (String, usize, bool)>,
    refs: HashMap<String, String>,
}

fn node_marks(st: AppState) -> NodeMarks {
    let g = st.net();
    let scope = st.current_scope();
    let root = st.graph.read();
    let changes = st.changes();
    // Nodes I'm editing that a ghost branch also changes or depends on.
    let invalid = {
        let empty = Graph::default();
        let base = st.baseline.read();
        let mine = claims::claim(base.as_ref().unwrap_or(&empty), &root, &changes);
        ghost::invalidated(&mine, &st.ghosts.read(), scope.as_deref())
    };
    let changed_modules: HashSet<&str> = changes.iter().filter_map(|c| c.scope.as_deref()).collect();
    let stale = claims::stale(&root, &changes).into_iter().filter(|s| s.node.0 == scope).map(|s| (s.node.1.clone(), stale_text(&s))).collect();
    let status = changes
        .iter()
        .filter(|c| c.subject == Subject::Node && c.scope == scope)
        .filter_map(|c| match c.kind {
            ChangeKind::Added => Some((c.id.clone(), "added")),
            ChangeKind::Modified(_) => Some((c.id.clone(), "modified")),
            ChangeKind::Removed => None,
        })
        .collect();
    let modules = g
        .nodes
        .iter()
        .filter_map(|n| n.module.as_ref())
        .map(|m| {
            let (title, count) = root.module(m).map(|x| (x.title.clone(), x.net.nodes.len())).unwrap_or_else(|| (format!("missing: {m}"), 0));
            (m.clone(), (title, count, changed_modules.contains(m.as_str())))
        })
        .collect();
    let refs = {
        let cwd = st.cwd.read();
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
    NodeMarks { status, stale, invalid, modules, refs }
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
    // While true the view pans smoothly to a focused node; otherwise it follows the mouse at once.
    let mut panning = use_signal(|| false);
    // The node that glows after the chat focused it.
    let mut pulse = use_signal(|| None::<String>);
    // Where the dragged nodes are now. Written to the graph once, when the drag ends.
    let mut dragged = use_signal(HashMap::<String, [f64; 2]>::new);
    let placed = use_memo(move || {
        let g = st.net();
        let scope = st.current_scope();
        let mut positions = g.positions();
        let ghost = ghost::layer(&g, scope.as_deref(), &st.ghosts.read(), &mut positions);
        Placed { positions, ghost }
    });

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

    // Fits the whole network, or zooms to the node the chat asked to focus and pulses it.
    use_effect(move || {
        let _ = st.fit_request.read();
        let Some(m) = mounted.read().clone() else { return };
        let target = st.focus_target.write().take();
        spawn(async move {
            let Ok(rect) = m.get_client_rect().await else { return };
            let (w, h) = (rect.size.width, rect.size.height);
            let g = st.net_peek();
            let Some(id) = target.filter(|id| g.node(id).is_some()) else {
                view.set(fit_view(&g, w, h));
                return;
            };
            view.set(focus_view(&g, &id, w, h));
            panning.set(true);
            pulse.set(Some(id.clone()));
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            panning.set(false);
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            if pulse.peek().as_deref() == Some(id.as_str()) {
                pulse.set(None);
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
                let moved = dragged.take();
                let scope = st.scope.peek().last().cloned();
                if let Some(g) = st.graph.write().net_mut(scope.as_deref()) {
                    g.pin_all();
                    for n in g.nodes.iter_mut() {
                        if let Some(p) = moved.get(&n.id) {
                            n.pos = Some(*p);
                        }
                    }
                }
                st.save_graph();
            }
            Some(Drag::Node { id, moved: false, shift, .. }) => node_clicked(st, id, shift),
            Some(Drag::Select { sx, sy, cx, cy, additive }) => {
                let (ox, oy) = *origin.peek();
                let (px, py, z) = *view.peek();
                let world = |x: f64, y: f64| ((x - ox - px) / z, (y - oy - py) / z);
                let (x0, y0) = world(sx.min(cx), sy.min(cy));
                let (x1, y1) = world(sx.max(cx), sy.max(cy));
                let hits = nodes_in_box(&st.net_peek(), &placed.peek().positions, (x0, y0, x1, y1));
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
                if !moved {
                    let placed = placed.peek();
                    let sel = st.selected.peek();
                    let ids: Vec<String> = if sel.contains(&id) { sel.clone() } else { vec![id.clone()] };
                    group = ids.into_iter().filter_map(|i| placed.positions.get(&i).map(|p| (i, p[0], p[1]))).collect();
                }
                dragged.set(group.iter().map(|(gid, ox, oy)| (gid.clone(), [(ox + dx).round(), (oy + dy).round()])).collect());
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

    let scope = st.current_scope();
    let picked_branches = st.ghost_branches.read().clone();
    let menu_open = *ghost_menu.read();
    let (px, py, z) = *view.read();
    let draft_on = st.flow_draft.read().is_some();
    let select_box = match drag.read().as_ref() {
        Some(Drag::Select { sx, sy, cx, cy, .. }) => {
            let (ox, oy) = *origin.read();
            Some((sx.min(*cx) - ox, sy.min(*cy) - oy, (cx - sx).abs(), (cy - sy).abs()))
        }
        _ => None,
    };
    // Per ghost branch: its colour index and how many ghost nodes it adds here (or its error).
    let (branch_status, branch_nodes) = {
        let placed = placed.read();
        let ghosts = st.ghosts.read();
        let status: HashMap<String, (usize, Result<usize, String>)> = ghosts
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let status = match &b.error {
                    Some(e) => Err(e.clone()),
                    None => Ok(placed.ghost.nodes.iter().filter(|n| n.branch == i).count()),
                };
                (b.branch.clone(), (i, status))
            })
            .collect();
        let nodes: Vec<(usize, String)> = placed.ghost.nodes.iter().map(|n| (n.branch, n.node.id.clone())).collect();
        (status, nodes)
    };
    let has_pins = st.net().nodes.iter().any(|n| n.pos.is_some());
    let can_undo_arrange = arrange_undo.read().as_ref().is_some_and(|(s, _)| *s == scope);
    let busy_job = st.job.read().is_some();
    let grid = 24.0 * z;
    let current_drag = drag.read().clone();
    let linking = matches!(current_drag, Some(Drag::Link { .. }));
    let pending_svg = current_drag.as_ref().map(pending_link_svg).unwrap_or_default();
    let canvas_class = if draft_on {
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
            div {
                class: if *panning.read() { "graph-world panning" } else { "graph-world" },
                style: "transform: translate({px}px, {py}px) scale({z});",
                if linking {
                    div { class: "edges", dangerous_inner_html: "{pending_svg}" }
                }
                GraphWorld { placed, dragged, drag, hovered, pulse, focus_only, collapse_hubs }
            }
            if let Some((bx, by, bw, bh)) = select_box {
                div { class: "select-box", style: "left: {bx}px; top: {by}px; width: {bw}px; height: {bh}px;" }
            }
            div { class: "zoom-controls", onmousedown: move |e| e.stop_propagation(),
                button { class: "icon-btn", title: "Zoom out", onclick: move |_| zoom_center(1.0 / 1.2), IconSvg { icon: Icon::ZoomOut } }
                button { class: "icon-btn", title: "Fit the graph", onclick: move |_| *st.fit_request.write() += 1, IconSvg { icon: Icon::Fit } }
                button { class: "icon-btn", title: "Zoom in", onclick: move |_| zoom_center(1.2), IconSvg { icon: Icon::ZoomIn } }
                span { class: "zoom-sep" }
                button {
                    class: if *focus_only.read() { "toggle on" } else { "toggle" },
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
                        button { class: "icon-btn", title: "Close", onclick: move |_| ghost_menu.set(false), IconSvg { icon: Icon::X } }
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
                                            .map(|(i, _)| branch_nodes.iter().filter(|(b, _)| b == i).map(|(_, id)| id.clone()).collect())
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
            if draft_on {
                div { class: "picking-hint", "Click nodes in order to build the flow" }
            }
            if linking {
                div { class: "picking-hint", "Drop on a node to link it, or on an already linked node to unlink" }
            }
        }
    }
}

/// The nodes and links inside the panned and zoomed world. It does not read the view, so
/// panning and zooming never re-render it.
#[component]
fn GraphWorld(
    placed: Memo<Placed>,
    dragged: Signal<HashMap<String, [f64; 2]>>,
    drag: Signal<Option<Drag>>,
    hovered: Signal<Option<String>>,
    pulse: Signal<Option<String>>,
    focus_only: Signal<bool>,
    collapse_hubs: Signal<bool>,
) -> Element {
    let st = use_context::<AppState>();
    let marks = use_memo(move || node_marks(st));
    let hub_count = use_memo(move || if *collapse_hubs.read() { hubs(&st.net()) } else { HashMap::new() });
    // Routing every link is the slow part of drawing, so it reruns only when nodes move or links
    // change, not on hover or selection. Links into collapsed hubs are routed last: they show as
    // stubs until lit, so the links always drawn get first pick of the gaps.
    let routed = use_memo(move || {
        let mut positions = placed.read().positions.clone();
        positions.extend(dragged.read().iter().map(|(id, pos)| (id.clone(), *pos)));
        let mut edges = st.net().edges();
        edges.sort_by_key(|(_, to)| hub_count.read().contains_key(to));
        let routes = routing::route(&edges, &positions);
        (edges, routes)
    });

    let g = st.net();
    let scope = st.current_scope();
    let placed = placed.read();
    let dragged = dragged.read();
    let positions: Cow<HashMap<String, [f64; 2]>> = if dragged.is_empty() {
        Cow::Borrowed(&placed.positions)
    } else {
        let mut p = placed.positions.clone();
        p.extend(dragged.iter().map(|(id, pos)| (id.clone(), *pos)));
        Cow::Owned(p)
    };
    let ghost_layer = &placed.ghost;
    let ghost_svg = ghost_edges_svg(&ghost_layer.edges, &positions);
    let selected: HashSet<String> = st.selected.read().iter().cloned().collect();
    let draft = st.flow_draft.read().clone();
    let active_flow = st.active_flow.read().clone();
    let path: Vec<String> = draft
        .clone()
        .or_else(|| active_flow.and_then(|id| g.flows.iter().find(|f| f.id == id).map(|f| f.nodes.clone())))
        .unwrap_or_default();
    // The step numbers of each node on the shown flow, like "2,5".
    let mut steps: HashMap<&str, Vec<String>> = HashMap::new();
    for (i, id) in path.iter().enumerate() {
        steps.entry(id.as_str()).or_default().push((i + 1).to_string());
    }
    let routed = routed.read();
    let (edges, routes) = (&routed.0, &routed.1);
    let by_id = g.by_id();
    let focus: HashSet<String> = match hovered.read().clone() {
        Some(h) if by_id.contains_key(h.as_str()) => HashSet::from([h]),
        _ => selected.clone(),
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
    let lit: Option<HashSet<&str>> = match emphasis {
        Emphasis::Path { dim_rest: true } => Some(path.iter().map(String::as_str).collect()),
        Emphasis::Focus { .. } => Some(
            edges
                .iter()
                .filter(|(a, b)| focus.contains(a) || focus.contains(b))
                .flat_map(|(a, b)| [a.as_str(), b.as_str()])
                .chain(focus.iter().map(String::as_str))
                .collect(),
        ),
        _ => None,
    };
    let hub_count = hub_count.read();
    let svg = edges_svg(
        edges,
        routes,
        &by_id,
        EdgeView { path: &path, focus: &focus, emphasis, hubs: (!hub_count.is_empty()).then_some(&hub_count) },
    );
    let marks = marks.read();
    let ghosts = st.ghosts.read();
    let pulse_id = pulse.read().clone();
    let can_link = draft.is_none() && st.job.read().is_none();

    let views: Vec<NodeView> = g
        .nodes
        .iter()
        .map(|n| {
            let [x, y] = positions.get(&n.id).copied().unwrap_or([0.0, 0.0]);
            let module = n.module.as_ref().and_then(|m| marks.modules.get(m));
            let ext = marks.refs.get(&n.id);
            let inner_changed = module.is_some_and(|(_, _, changed)| *changed);
            let status = marks.status.get(&n.id).copied().unwrap_or(if inner_changed { "modified" } else { "" });
            let mod_class = if module.is_some() {
                "module-ref"
            } else if ext.is_some() {
                "ext-ref"
            } else {
                ""
            };
            let label = match (module, ext) {
                (Some((title, count, _)), _) => format!("▣ {title} · {count}"),
                (None, Some(label)) => label.clone(),
                (None, None) => n.kind.clone(),
            };
            let mut tip = n.description.clone();
            if let Some(d) = n.decisions.last() {
                tip.push_str(&format!("\n\nLatest decision: {}", d.line()));
            }
            if module.is_some() {
                tip.push_str("\n\nDouble-click to open the module");
            }
            let sel = if selected.contains(&n.id) { "selected" } else { "" };
            let pulsing = if pulse_id.as_deref() == Some(n.id.as_str()) { "pulse" } else { "" };
            let editing = st.live_edits_of(scope.as_deref(), n);
            let dim = if lit.as_ref().is_some_and(|l| !l.contains(n.id.as_str())) { "dim" } else { "" };
            let ghost_marks = ghost_layer.marks.get(&n.id).map(Vec::as_slice).unwrap_or_default();
            let invalid_by = marks.invalid.get(&n.id).map(Vec::as_slice).unwrap_or_default();
            let ghost_class = if !invalid_by.is_empty() {
                "ghost-conflict"
            } else if ghost_marks.iter().any(|m| m.kind == ChangeKind::Removed) {
                "ghost-removed"
            } else if !ghost_marks.is_empty() {
                "ghost-touched"
            } else {
                ""
            };
            let ghost_tag = (!ghost_marks.is_empty()).then(|| {
                let tip = ghost_marks
                    .iter()
                    .map(|m| format!("{}: {}", ghosts[m.branch].who(), mark_text(&m.kind)))
                    .chain(invalid_by.iter().map(|bi| format!("{}: invalidates your edit to this node", ghosts[*bi].who())))
                    .collect::<Vec<_>>()
                    .join("\n");
                (ghost_marks.len(), tip)
            });
            let steps = steps.get(n.id.as_str()).map(|s| s.join(",")).unwrap_or_default();
            let on = if steps.is_empty() { "" } else { "on-path" };
            NodeView {
                id: n.id.clone(),
                x,
                y,
                title: n.title.clone(),
                class: format!("gnode {status} {sel} {on} {mod_class} {dim} {ghost_class} {pulsing}"),
                color: graph::kind_color(&n.kind),
                mark_color: ghost_marks.first().map(|m| ghost::color(m.branch)).unwrap_or(""),
                tip,
                label,
                status,
                stale: marks.stale.get(&n.id).cloned(),
                editing_tip: (!editing.is_empty()).then(|| format!("Being edited now:\n{}", editing.join("\n"))),
                ghost_tag,
                steps,
                hub: hub_count.get(&n.id).copied(),
                module: n.module.clone(),
                can_link,
                knowledge: (n.decisions.len(), n.invariants.iter().filter(|i| !i.trim().is_empty()).count()),
            }
        })
        .collect();
    let ghost_picks = st.ghost_picks.read();
    let ghost_views: Vec<GhostView> = ghost_layer
        .nodes
        .iter()
        .map(|gn| {
            let [x, y] = positions.get(&gn.key).copied().unwrap_or([0.0, 0.0]);
            let who = ghosts[gn.branch].who();
            let pick = GhostPick { branch: ghosts[gn.branch].branch.clone(), scope: scope.clone(), id: gn.node.id.clone() };
            GhostView {
                key: gn.key.clone(),
                x,
                y,
                color: graph::kind_color(&gn.node.kind),
                branch_color: ghost::color(gn.branch),
                tip: format!(
                    "{}\n\nWork in progress on {who}. Click to send it to Workbench as context, Shift+click to add it to the context.",
                    gn.node.description
                ),
                who,
                selected: ghost_picks.contains(&pick),
                pick,
                title: gn.node.title.clone(),
                kind: gn.node.kind.clone(),
            }
        })
        .collect();
    let has_ghost_edges = !ghost_layer.edges.is_empty();

    rsx! {
        div { class: "edges", dangerous_inner_html: "{svg}" }
        if has_ghost_edges {
            div { class: "edges", dangerous_inner_html: "{ghost_svg}" }
        }
        for view in views {
            GNode { key: "{view.id}", view, drag, hovered }
        }
        for GhostView { key, x, y, color, branch_color, tip, who, selected, pick, title, kind } in ghost_views {
            div {
                key: "{key}",
                class: if selected { "gnode ghost selected" } else { "gnode ghost" },
                style: "left: {x}px; top: {y}px; width: {NODE_W}px; height: {NODE_H}px; --kind: {color}; --ghost: {branch_color};",
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
                div { class: "gnode-title", "{title}" }
                div { class: "gnode-meta",
                    span { class: "gnode-kind", "{kind}" }
                    span { class: "gnode-branch", title: "{who}", "⧉ {who}" }
                }
            }
        }
    }
}

/// A node another branch adds, as `GraphWorld` draws it.
struct GhostView {
    key: String,
    x: f64,
    y: f64,
    color: &'static str,
    branch_color: &'static str,
    tip: String,
    who: String,
    selected: bool,
    pick: GhostPick,
    title: String,
    kind: String,
}

/// Everything one node card shows, worked out by `GraphWorld`. A card re-renders only when
/// this changes.
#[derive(Clone, PartialEq)]
struct NodeView {
    id: String,
    x: f64,
    y: f64,
    title: String,
    class: String,
    color: &'static str,
    mark_color: &'static str,
    tip: String,
    /// The kind, or the attached module, or the reference source.
    label: String,
    /// "added", "modified" or "".
    status: &'static str,
    stale: Option<String>,
    editing_tip: Option<String>,
    /// How many ghost branches touch the node, and what they do.
    ghost_tag: Option<(usize, String)>,
    /// The node's step numbers on the shown flow, like "2,5".
    steps: String,
    /// How many nodes call this hub.
    hub: Option<usize>,
    module: Option<String>,
    can_link: bool,
    /// How many decisions and invariants the node records.
    knowledge: (usize, usize),
}

#[component]
fn GNode(view: NodeView, drag: Signal<Option<Drag>>, hovered: Signal<Option<String>>) -> Element {
    let mut st = use_context::<AppState>();
    let mut drag = drag;
    let mut hovered = hovered;
    let NodeView { id, x, y, title, class, color, mark_color, tip, label, status, stale, editing_tip, ghost_tag, steps, hub, module, can_link, knowledge } =
        view;
    let mut start_link = move |e: MouseEvent, anchor: String, outgoing: bool| {
        if e.trigger_button() != Some(MouseButton::Primary) {
            return;
        }
        e.stop_propagation();
        let p = e.client_coordinates();
        let (wx, wy) = (if outgoing { x + NODE_W } else { x }, y + NODE_H / 2.0);
        drag.set(Some(Drag::Link { anchor, outgoing, sx: p.x, sy: p.y, wx, wy, cx: wx, cy: wy }));
    };
    let (enter_id, leave_id, down_id, drop_id, in_id, out_id) = (id.clone(), id.clone(), id.clone(), id.clone(), id.clone(), id.clone());

    rsx! {
        div {
            class: "{class}",
            style: "left: {x}px; top: {y}px; width: {NODE_W}px; height: {NODE_H}px; --kind: {color}; --ghost: {mark_color};",
            title: "{tip}",
            onmouseenter: move |e: MouseEvent| {
                e.stop_propagation();
                if matches!(*drag.peek(), Some(Drag::Select { .. })) {
                    return;
                }
                hovered.set(Some(enter_id.clone()));
            },
            onmouseleave: move |e: MouseEvent| {
                e.stop_propagation();
                if hovered.peek().as_deref() == Some(leave_id.as_str()) {
                    hovered.set(None);
                }
            },
            ondoubleclick: move |e: MouseEvent| {
                e.stop_propagation();
                if let Some(m) = module.as_deref()
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
                drag.set(Some(Drag::Node { id: down_id.clone(), sx: p.x, sy: p.y, group: Vec::new(), moved: false, shift: m.shift() }));
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
            div { class: "gnode-title", "{title}" }
            div { class: "gnode-meta",
                span { class: "gnode-kind", "{label}" }
                match status {
                    "added" => rsx! { span { class: "gnode-status", "new" } },
                    "modified" => rsx! { span { class: "gnode-status", "edited" } },
                    _ => rsx! {},
                }
                if let Some(tip) = stale {
                    span { class: "gnode-stale", title: "{tip}", "stale?" }
                }
                if let Some(tip) = editing_tip {
                    span { class: "gnode-editing", title: "{tip}", "● editing" }
                }
                if let Some((n, tip)) = ghost_tag {
                    span { class: "gnode-ghost-tag", title: "{tip}", "⧉ {n}" }
                }
                if knowledge.0 > 0 {
                    span { class: "gnode-knowledge", title: "{knowledge.0} recorded decision(s)", "◆ {knowledge.0}" }
                }
                if knowledge.1 > 0 {
                    span { class: "gnode-knowledge", title: "{knowledge.1} invariant(s)", "⊘ {knowledge.1}" }
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
    let live = st.live_edits_of(scope.as_deref(), &node);
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
        .iter()
        .find(|c| c.id == id && c.subject == Subject::Node && c.scope == scope)
        .map(|c| c.kind.clone());
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
    let id_know = id.clone();
    let id_mine = id.clone();
    let rules_here: Vec<(String, String)> = rules::relevant(&st.rules.read(), Some(std::slice::from_ref(&id)))
        .into_iter()
        .map(|r| (r.id.clone(), if r.title.trim().is_empty() { r.body.lines().next().unwrap_or_default().to_string() } else { r.title.clone() }))
        .collect();

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
                    st.update_net_typing(|g| if let Some(n) = g.node_mut(&id) { n.title = v });
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
                    st.update_net_typing(|g| if let Some(n) = g.node_mut(&id) { n.description = v });
                },
            }
            KnowledgeEditor {
                decisions: node.decisions.clone(),
                invariants: node.invariants.clone(),
                locked,
                onchange: move |(decisions, invariants): (Vec<Decision>, Vec<String>)| {
                    let id = id_know.clone();
                    st.update_net_typing(|g| if let Some(n) = g.node_mut(&id) { n.decisions = decisions; n.invariants = invariants });
                },
            }
            if !rules_here.is_empty() {
                label { "Rules that apply" }
                div { class: "chips",
                    for (rid, title) in rules_here {
                        span { key: "{rid}", class: "chip", title: "A project rule scoped to this node or the whole project", "{title}" }
                    }
                }
            }
            div { class: "inspector-actions",
                button {
                    class: "small",
                    disabled: locked || st.chat_busy() || st.job.read().is_some(),
                    title: "Read this node's commits, comments, tickets and past chats, and record its decisions and invariants",
                    onclick: move |_| mine_decisions(st, vec![id_mine.clone()]),
                    "Mine decisions"
                }
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
            if !live.is_empty() {
                label { "Being edited now" }
                ul { class: "file-list live-edits",
                    for f in live {
                        li {
                            key: "{f}",
                            onclick: {
                                let f = f.clone();
                                move |_| st.open(&f)
                            },
                            span { class: "live-dot", "●" }
                            span { "{f}" }
                        }
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

/// The decisions and invariants of a node or module: why it is built the way it is and what
/// must stay true of it. Each edit hands both lists back through `onchange`.
#[component]
fn KnowledgeEditor(decisions: Vec<Decision>, invariants: Vec<String>, locked: bool, onchange: EventHandler<(Vec<Decision>, Vec<String>)>) -> Element {
    let st = use_context::<AppState>();
    let mut editing = use_signal(|| None::<usize>);
    let open_at = *editing.read();
    let n = decisions.len();
    let set_decision = {
        let (decisions, invariants) = (decisions.clone(), invariants.clone());
        move |i: usize, d: Option<Decision>| {
            let mut all = decisions.clone();
            match d {
                Some(d) if i < all.len() => all[i] = d,
                Some(d) => all.push(d),
                None if i < all.len() => {
                    all.remove(i);
                }
                None => {}
            }
            onchange.call((all, invariants.clone()));
        }
    };
    let set_invariant = {
        let (decisions, invariants) = (decisions.clone(), invariants.clone());
        move |i: usize, text: Option<String>| {
            let mut all = invariants.clone();
            match text {
                Some(t) if i < all.len() => all[i] = t,
                Some(t) => all.push(t),
                None if i < all.len() => {
                    all.remove(i);
                }
                None => {}
            }
            onchange.call((decisions.clone(), all));
        }
    };

    rsx! {
        label { "Decisions — why it is built this way ({n})" }
        if decisions.is_empty() {
            div { class: "muted small-text", "No decisions recorded yet. Agents add them as they change this part; Mine decisions reads them from its history." }
        }
        div { class: "decisions",
            for (i, d) in decisions.iter().cloned().enumerate().rev() {
                if open_at == Some(i) && !locked {
                    DecisionForm {
                        key: "edit-{i}",
                        decision: d.clone(),
                        onsave: {
                            let set = set_decision.clone();
                            move |d: Decision| {
                                set(i, Some(d));
                                editing.set(None);
                            }
                        },
                        onremove: {
                            let set = set_decision.clone();
                            move |_| {
                                set(i, None);
                                editing.set(None);
                            }
                        },
                    }
                } else {
                    div { key: "view-{i}", class: "decision",
                        onclick: move |_| if !locked { editing.set(Some(i)) },
                        title: if locked { "" } else { "Click to edit" },
                        div { class: "decision-what", "{d.decision}" }
                        if !d.why.trim().is_empty() {
                            div { class: "decision-why", "{d.why}" }
                        }
                        if !d.instead_of.trim().is_empty() {
                            div { class: "decision-instead", "Instead of {d.instead_of}" }
                        }
                        div { class: "decision-meta",
                            if !d.source.trim().is_empty() {
                                {source_chip(st, d.source.clone())}
                            }
                            if !d.date.trim().is_empty() {
                                span { class: "muted", "{d.date}" }
                            }
                        }
                    }
                }
            }
        }
        if open_at == Some(n) && !locked {
            DecisionForm {
                decision: Decision { source: format!("chat {}", graph::today()), date: graph::today(), ..Default::default() },
                onsave: {
                    let set = set_decision.clone();
                    move |d: Decision| {
                        set(n, Some(d));
                        editing.set(None);
                    }
                },
                onremove: move |_| editing.set(None),
            }
        } else if !locked {
            button { class: "small add-row", onclick: move |_| editing.set(Some(n)), "+ Decision" }
        }
        label { "Invariants — must stay true ({invariants.len()})" }
        ul { class: "invariants",
            for (i, text) in invariants.iter().cloned().enumerate() {
                li { key: "{i}",
                    input {
                        value: "{text}",
                        disabled: locked,
                        placeholder: "e.g. Never blocks the UI thread",
                        onchange: {
                            let set = set_invariant.clone();
                            move |e: FormEvent| set(i, Some(e.value()))
                        },
                    }
                    if !locked {
                        span {
                            class: "chip-x",
                            title: "Remove this invariant",
                            onclick: {
                                let set = set_invariant.clone();
                                move |_| set(i, None)
                            },
                            "✕"
                        }
                    }
                }
            }
        }
        if !locked {
            button {
                class: "small add-row",
                onclick: {
                    let set = set_invariant.clone();
                    let at = invariants.len();
                    move |_| set(at, Some(String::new()))
                },
                "+ Invariant"
            }
        }
    }
}

/// A decision's source. When it names a file of the project, clicking opens the file.
fn source_chip(mut st: AppState, source: String) -> Element {
    let path = source.split_whitespace().find(|w| w.contains('/') || w.contains('.')).map(|w| w.trim_matches(|c: char| c == '`' || c == ',' || c == ':').to_string());
    let file = path.filter(|p| st.cwd.peek().join(p).is_file());
    match file {
        Some(f) => rsx! {
            span {
                class: "chip chip-click decision-source",
                title: "Open {f}",
                onclick: move |e: MouseEvent| {
                    e.stop_propagation();
                    st.open(&f);
                },
                "{source}"
            }
        },
        None => rsx! { span { class: "chip decision-source", "{source}" } },
    }
}

/// Edits one decision; saves on Done, and Remove drops it (or cancels a new one).
#[component]
fn DecisionForm(decision: Decision, onsave: EventHandler<Decision>, onremove: EventHandler<()>) -> Element {
    let mut d = use_signal(|| decision.clone());
    let field = move |label: &'static str, placeholder: &'static str, get: fn(&Decision) -> &String, set: fn(&mut Decision, String)| {
        rsx! {
            input {
                value: "{get(&d.read())}",
                placeholder: "{placeholder}",
                title: "{label}",
                oninput: move |e| set(&mut d.write(), e.value()),
            }
        }
    };
    rsx! {
        div { class: "decision editing",
            {field("Decision", "What was decided", |d| &d.decision, |d, v| d.decision = v)}
            {field("Why", "Why: the reason or the constraint that forced it", |d| &d.why, |d, v| d.why = v)}
            {field("Instead of", "Instead of: the alternative not taken (optional)", |d| &d.instead_of, |d, v| d.instead_of = v)}
            div { class: "decision-meta",
                {field("Source", "Source: commit, ticket, chat date or file", |d| &d.source, |d, v| d.source = v)}
                {field("Date", "YYYY-MM-DD", |d| &d.date, |d, v| d.date = v)}
            }
            div { class: "inspector-actions",
                button { class: "small primary", disabled: d.read().decision.trim().is_empty(), onclick: move |_| onsave.call(d.peek().clone()), "Done" }
                button { class: "small", onclick: move |_| onremove.call(()), "Remove" }
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
        .iter()
        .find(|c| c.subject == Subject::Module && c.id == id)
        .map(|c| c.kind.clone());
    let id_title = id.clone();
    let id_desc = id.clone();
    let id_rev = id.clone();
    let id_del = id.clone();
    let id_know = id.clone();
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
                    st.update_graph_typing(|g| if let Some(m) = g.module_mut(&id) { m.title = v });
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
                    st.update_graph_typing(|g| if let Some(m) = g.module_mut(&id) { m.description = v });
                },
            }
            KnowledgeEditor {
                decisions: module.decisions.clone(),
                invariants: module.invariants.clone(),
                locked,
                onchange: move |(decisions, invariants): (Vec<Decision>, Vec<String>)| {
                    let id = id_know.clone();
                    st.update_graph_typing(|g| if let Some(m) = g.module_mut(&id) { m.decisions = decisions; m.invariants = invariants });
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
                    let (facts, prose) = node_history::intent_changes(e.before.as_deref().unwrap_or(""), e.after.as_deref().unwrap_or(""));
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
                            {intent_list(facts, prose)}
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
    let changes = st.changes().clone();
    let has_baseline = st.baseline.read().is_some();
    let has_nodes = !st.graph.read().nodes.is_empty();
    // Claims of the other branches, read from git so overlaps show before anything runs.
    let mut open_claims = use_resource(move || async move {
        let cwd = st.cwd.read().clone();
        tokio::task::spawn_blocking(move || claims::open_claims(&cwd)).await.unwrap_or_default()
    });
    let overlaps = open_claims.read().as_ref().map(|open| claim_overlaps(st, &changes, open)).unwrap_or_default();
    let free = changes.iter().filter(|c| !overlaps.iter().any(|o| o.claim.touches(c))).count();
    let stale = claims::stale(&st.graph.read(), &changes);

    // `only_free` leaves out the changes that overlap another branch's claim.
    let mut process = move |only_free: bool| {
        let mut changes = st.graph_changes.peek().clone();
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
        let base = st.baseline.peek().clone().unwrap_or_default();
        let orders = work_order::from_changes(&base, &st.graph.peek(), &changes, &open);
        // The run may only change files inside the work orders' blast radii.
        st.scope_run(work_order::scope_nodes(&orders));
        let text = st.with_refs(|refs| prompts::process(&st.graph.peek(), &orders, &st.rules.peek(), &note.peek(), mode, Some(refs), &overlaps));
        let label = format!("Process {} graph change{} ({})", changes.len(), plural(changes.len()), mode.label());
        note.set(String::new());
        st.chat_or_new().send(Cmd::Prompt { label, text });
    };

    if changes.is_empty() {
        return rsx! {
            ProposalsList {}
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
            ProposalsList {}
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
                    let title = match (&c.scope, is_module) {
                        (_, true) => format!("▣ {}", n.title),
                        (Some(m), false) => format!("▣ {} › {}", st.graph.peek().module_title(m), n.title),
                        (None, false) => n.title.clone(),
                    };
                    let file = change_diff(st, c, &title);
                    let removed = c.kind == ChangeKind::Removed;
                    let key = c.key();
                    let show_scope = c.scope.clone();
                    let show_id = if is_module { None } else { Some(n.id.clone()) };
                    let rev_scope = c.scope.clone();
                    let subject = c.subject;
                    let id_rev = n.id.clone();
                    let intent = matches!(&c.kind, ChangeKind::Modified(f) if f.iter().any(|f| f.is_intent()));
                    let verification = st.verification.read().get(&key).cloned();
                    let actions = rsx! {
                        if intent {
                            span {
                                class: "intent-flag",
                                title: "This rewrites what the node is meant to do. Review it like a change to .env: the code will be changed to match it.",
                                "⚠ intent"
                            }
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
                    };
                    rsx! {
                        div { key: "{key}", class: "change-diff",
                            match verification {
                                Some(None) => rsx! { div { class: "muted small-text", "Verifying that the code matches the description…" } },
                                Some(Some(why)) => rsx! { div { class: "warn small-text", title: "The agent's work on this node stays pending until it passes", "⚠ Not verified: {why}" } },
                                None => rsx! {},
                            }
                            FileDiffView { file, local: false, actions }
                        }
                    }
                }
            }
            if !stale.is_empty() {
                div { class: "hint-box claim-warning",
                    div { class: "claim-warning-head", b { "Possibly stale ({stale.len()})" } }
                    div { class: "small-text", "These nodes depend on intent you changed. Check that their descriptions still agree with it." }
                    for s in stale.iter() {
                        div { key: "{s.node.0:?}{s.node.1}", class: "small-text",
                            b { {st.graph.peek().net(s.node.0.as_deref()).map(|n| n.title_of(&s.node.1)).unwrap_or_default()} }
                            " · {stale_text(s)}"
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
            label {
                class: "small-text muted",
                title: "Processing may only change the changed nodes' files and their callers'. In strict mode, changes to other existing files are undone.",
                input {
                    r#type: "checkbox",
                    checked: st.provider_settings.read().strict_scope,
                    onchange: move |e| st.update_provider_settings(|s| s.strict_scope = e.checked()),
                }
                " Strict scope: undo changes outside the changed nodes' files"
            }
        }
    }
}

/// A graph change as a code diff: the node (or module) written out as text the way the node
/// history does, before against after, under `title`.
fn change_diff(st: AppState, c: &graph::Change, title: &str) -> git::FileDiff {
    let (before, after) = match c.subject {
        Subject::Module => {
            let base = st.baseline.peek();
            let before = base.as_ref().and_then(|g| g.module(&c.id)).map(node_history::render_module);
            (before, st.graph.peek().module(&c.id).map(node_history::render_module))
        }
        Subject::Node => {
            let render = |n: &Node| node_history::links_one_per_line(&node_history::render_node(n));
            (c.before.as_ref().map(render), c.after.as_ref().map(render))
        }
    };
    node_history::line_diff(title, &before.unwrap_or_default(), &after.unwrap_or_default())
}

/// Why a node may be stale, for its badge and the Changes pane.
fn stale_text(s: &claims::Stale) -> String {
    match &s.flow {
        Some(flow) => format!("may be stale: it is on the flow “{flow}” with `{}`, whose intent changed", s.because),
        None => format!("may be stale: it depends on `{}`, whose intent changed", s.because),
    }
}

/// The descriptions agents proposed. Agents never change intent themselves (see
/// `ledger::record`), so a person accepts or rejects each proposal here.
#[component]
fn ProposalsList() -> Element {
    let mut st = use_context::<AppState>();
    let locked = st.graph_locked() || st.turn_start.read().is_some();
    let entries = st.ledger.read().clone();
    let g = st.graph.read().clone();
    let items: Vec<(String, String, String, (Vec<(Op, String)>, Vec<(Op, String)>))> = ledger::open_proposals(&entries)
        .into_iter()
        .filter_map(|e| {
            let (scope, subject, id) = ledger::merge_target(&e.op)?;
            let ledger::Op::NodeMerge { patch, .. } = &e.op else { return None };
            let (title, current) = match subject {
                Subject::Module => g.module(id).map(|m| (format!("▣ {}", m.title), m.as_node()))?,
                Subject::Node => g.net(scope).and_then(|n| n.node(id)).map(|n| (n.title.clone(), n.clone()))?,
            };
            let proposed = ledger::with_patch(&current, patch);
            let changes = node_history::intent_changes(&node_history::render_node(&current), &node_history::render_node(&proposed));
            let who = e.author.clone().map(|a| format!("an agent run by {a}")).unwrap_or_else(|| "an agent".into());
            Some((e.id.clone(), title, who, changes))
        })
        .collect();
    if items.is_empty() {
        return rsx! {};
    }
    rsx! {
        div { class: "hint-box claim-warning",
            div { class: "claim-warning-head", b { "⚠ Proposed intent changes ({items.len()})" } }
            div { class: "small-text",
                "Agents may not rewrite what a node is meant to do or what was decided for it. Accept a proposal to apply it to the node, or reject it."
            }
            for (id, title, who, (facts, prose)) in items {
                {
                    let accept_id = id.clone();
                    let reject_id = id.clone();
                    rsx! {
                        div { key: "{id}", class: "proposal",
                            div { class: "claim-warning-head",
                                span { class: "change-title", "{title}" }
                                span { class: "muted small-text", " · proposed by {who}" }
                                div { class: "spacer" }
                                button { class: "small", disabled: locked, onclick: move |_| st.decide_proposal(&accept_id, true), "Accept" }
                                button { class: "small", disabled: locked, onclick: move |_| st.decide_proposal(&reject_id, false), "Reject" }
                            }
                            {intent_list(facts, prose)}
                        }
                    }
                }
            }
        }
    }
}

/// One row of the decisions timeline: where the decision was made and the decision.
struct TimelineRow {
    module: Option<String>,
    /// The node it belongs to; None for a decision of a module itself.
    node: Option<String>,
    place: String,
    decision: Decision,
}

/// Every decision in the graph, newest first. Undated decisions go last, in graph order.
fn decision_timeline(root: &Graph) -> Vec<TimelineRow> {
    let mut rows = Vec::new();
    for m in &root.modules {
        for d in &m.decisions {
            rows.push(TimelineRow { module: Some(m.id.clone()), node: None, place: format!("▣ {}", m.title), decision: d.clone() });
        }
    }
    for (scope, net) in root.nets() {
        for n in &net.nodes {
            let place = match scope {
                Some(m) => format!("▣ {} › {}", root.module_title(m), n.title),
                None => n.title.clone(),
            };
            for d in &n.decisions {
                rows.push(TimelineRow { module: scope.map(String::from), node: Some(n.id.clone()), place: place.clone(), decision: d.clone() });
            }
        }
    }
    rows.sort_by(|a, b| b.decision.date.cmp(&a.decision.date));
    rows
}

/// How the app was steered: every recorded decision, newest first. Clicking one shows its node.
#[component]
fn DecisionsPane() -> Element {
    let mut st = use_context::<AppState>();
    let mut filter = use_signal(String::new);
    let needle = filter.read().to_lowercase();
    let rows: Vec<TimelineRow> = decision_timeline(&st.graph.read())
        .into_iter()
        .filter(|r| needle.is_empty() || format!("{} {}", r.place, r.decision.line()).to_lowercase().contains(&needle))
        .collect();
    rsx! {
        div { class: "history-pane",
            input { class: "filter", placeholder: "Filter decisions…", value: "{filter}", oninput: move |e| filter.set(e.value()) }
            if rows.is_empty() {
                div { class: "muted small-text",
                    "No decisions recorded yet. Agents record them as they change the app; Mine decisions in the toolbar reads them from commits, comments, tickets and past chats."
                }
            }
            for (i, r) in rows.into_iter().enumerate() {
                div {
                    key: "{i}",
                    class: if r.node.is_some() { "decision chip-click" } else { "decision" },
                    onclick: move |_| if let Some(id) = r.node.clone() { st.focus_node(r.module.clone(), id) },
                    div { class: "decision-meta",
                        b { "{r.place}" }
                        div { class: "spacer" }
                        span { class: "muted", "{r.decision.date}" }
                    }
                    div { class: "decision-what", "{r.decision.decision}" }
                    if !r.decision.why.trim().is_empty() {
                        div { class: "decision-why", "{r.decision.why}" }
                    }
                    if !r.decision.instead_of.trim().is_empty() {
                        div { class: "decision-instead", "Instead of {r.decision.instead_of}" }
                    }
                    if !r.decision.source.trim().is_empty() {
                        div { class: "decision-meta", span { class: "chip decision-source", "{r.decision.source}" } }
                    }
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

/// The facts of a node's intent that were added or removed, drawn like the rows of a code diff
/// but without the unchanged ones. Used by the node history and the description proposals.
/// A node's intent change: the facts that changed, one per row, then the description as prose.
fn intent_list(facts: Vec<(Op, String)>, prose: Vec<(Op, String)>) -> Element {
    if facts.is_empty() && prose.is_empty() {
        return rsx! { div { class: "muted small-text", "No change to the intent." } };
    }
    rsx! {
        if !facts.is_empty() {
            div { class: "intent-facts",
                for (i, (op, text)) in facts.into_iter().enumerate() {
                    match op {
                        Op::Add => rsx! { div { key: "{i}", class: "added", span { class: "sign", "+" } "{text}" } },
                        _ => rsx! { div { key: "{i}", class: "removed", span { class: "sign", "−" } "{text}" } },
                    }
                }
            }
        }
        if !prose.is_empty() { {prose_view(prose)} }
    }
}

/// Unchanged text longer than this is folded, keeping a little on each side of a change.
const PROSE_CONTEXT_CHARS: usize = 160;

/// A description diff that reads like the text itself: removed words struck through and dimmed,
/// new words marked, unchanged text plain, with long unchanged stretches folded.
fn prose_view(parts: Vec<(Op, String)>) -> Element {
    let last = parts.len().saturating_sub(1);
    rsx! {
        div { class: "prose-diff",
            for (i, (op, text)) in parts.into_iter().enumerate() {
                match op {
                    Op::Add => rsx! { ins { key: "{i}", "{text}" } },
                    Op::Del => rsx! { del { key: "{i}", "{text}" } },
                    Op::Same => {
                        let (head, middle, tail) = fold(&text, i > 0, i < last);
                        rsx! {
                            span { key: "{i}",
                                "{head}"
                                if !middle.is_empty() {
                                    details { class: "prose-more",
                                        summary { "show more context" }
                                        "{middle}"
                                    }
                                }
                                "{tail}"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Splits unchanged text into what stays shown and the folded middle, on word boundaries.
/// `keep_start` keeps about `PROSE_CONTEXT_CHARS` of its start (it follows a change) and
/// `keep_end` as much of its end (a change follows). Short text isn't folded.
fn fold(text: &str, keep_start: bool, keep_end: bool) -> (String, String, String) {
    let words: Vec<&str> = text.split_inclusive(char::is_whitespace).collect();
    let take = |words: &mut dyn Iterator<Item = &&str>| {
        let mut chars = 0;
        words.take_while(|w| {
            let more = chars < PROSE_CONTEXT_CHARS;
            chars += w.chars().count();
            more
        }).count()
    };
    let head = if keep_start { take(&mut words.iter()) } else { 0 };
    let tail = if keep_end { take(&mut words.iter().rev()) } else { 0 };
    let folded_chars: usize = words.iter().map(|w| w.chars().count()).sum::<usize>();
    if head + tail >= words.len() || folded_chars <= PROSE_CONTEXT_CHARS * 3 {
        return (text.to_string(), String::new(), String::new());
    }
    let tail_start = words.len() - tail;
    (words[..head].concat(), words[head..tail_start].concat(), words[tail_start..].concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_unchanged_prose_folds_but_keeps_context_round_a_change() {
        let text = "word ".repeat(200);
        let (head, middle, tail) = fold(&text, true, true);
        assert_eq!(format!("{head}{middle}{tail}"), text);
        assert!(head.len() >= PROSE_CONTEXT_CHARS && tail.len() >= PROSE_CONTEXT_CHARS && !middle.is_empty());
        // Text at the very start only keeps its end, next to the change after it.
        assert!(fold(&text, false, true).0.is_empty());
        // Short text stays as it is.
        assert_eq!(fold("A short line.", true, true), ("A short line.".to_string(), String::new(), String::new()));
    }
}
