use std::collections::HashSet;
use std::path::{Path, PathBuf};

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

use crate::claude::{PermissionDecision, PermissionMode, PermissionRequest};
use crate::ghost::{self, GhostBranch, GhostPick};
use crate::git;
use crate::github;
use crate::projects::{self, Project};
use crate::prompts;
use crate::graph::{self, BASELINE_FILE, Change, GRAPH_FILE, Graph, Subject};
use crate::node_history::{self, NodeHistory, Source};
use crate::reference::{self, RefGraphs, RefState, Refs};
use crate::rules::{self, Rule};
use crate::screens::{self, Layout, Screen, ScreenState, Sizes};

#[derive(Debug, Clone, PartialEq)]
pub enum ChatItem {
    User(Message),
    Prompt { label: String, text: String },
    Assistant { text: String, streaming: bool },
    Tool { id: String, name: String, summary: String, input: String, result: Option<(String, bool)> },
    Info(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Files,
    Editor,
    Diff,
    GitHub,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenFile {
    pub rel: String,
    pub content: String,
    pub saved: String,
}

impl OpenFile {
    pub fn dirty(&self) -> bool {
        self.content != self.saved
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub text: String,
    pub context: Vec<String>,
    pub prompt: String,
}

#[derive(Debug, Clone)]
pub enum Cmd {
    Send(Message),
    SendNow(Message),
    Prompt { label: String, text: String },
    Interrupt,
    Restart { keep_session: bool },
    Resume { id: String },
    Permission { request_id: String, decision: PermissionDecision },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Generate,
    Process,
    Draft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Plan,
    Auto,
}

impl RunMode {
    pub const ALL: [RunMode; 2] = [RunMode::Plan, RunMode::Auto];

    pub fn label(self) -> &'static str {
        match self {
            RunMode::Plan => "Plan",
            RunMode::Auto => "Auto",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            RunMode::Plan => "Claude asks clarifying questions before changing anything",
            RunMode::Auto => "Claude fills in the gaps itself and lists its assumptions",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Job {
    pub kind: JobKind,
    pub mode: RunMode,
    pub awaiting: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftTab {
    Graph,
    Rules,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphPane {
    Node,
    Flows,
    Changes,
    History,
    Prompt,
}

/// Size of the node/flows/changes area under the graph canvas, as a percentage of the graph
/// tab's height, and whether it is collapsed down to its tab bar.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BottomPane {
    pub collapsed: bool,
    pub pct: f64,
}

impl Default for BottomPane {
    fn default() -> Self {
        Self { collapsed: false, pct: BottomPane::DEFAULT_PCT }
    }
}

impl BottomPane {
    pub const DEFAULT_PCT: f64 = 45.0;
    pub const MIN_PCT: f64 = 15.0;
    pub const MAX_PCT: f64 = 85.0;
    const FILE: &str = "graph-bottom.json";

    pub fn clamp(pct: f64) -> f64 {
        pct.clamp(Self::MIN_PCT, Self::MAX_PCT)
    }
}

#[derive(Clone, Copy)]
pub struct AppState {
    pub graph: Signal<Graph>,
    pub baseline: Signal<Option<Graph>>,
    pub turn_start: Signal<Option<Graph>>,
    pub turn_user_edits: Signal<HashSet<String>>,
    pub rules: Signal<Vec<Rule>>,
    pub rules_version: Signal<u64>,
    pub selected: Signal<Vec<String>>,
    pub ghost_branches: Signal<Vec<String>>,
    pub ghosts: Signal<Vec<GhostBranch>>,
    pub ghost_picks: Signal<Vec<GhostPick>>,
    pub scope: Signal<Vec<String>>,
    pub related_repos: Signal<Vec<String>>,
    pub projects: Signal<Vec<Project>>,
    pub project_view: Signal<Option<String>>,
    pub refs: Signal<RefGraphs>,
    pub job: Signal<Option<Job>>,
    pub run_mode: Signal<RunMode>,
    pub workbench_error: Signal<Option<String>>,
    pub left_tab: Signal<LeftTab>,
    pub graph_pane: Signal<GraphPane>,
    pub graph_bottom: Signal<BottomPane>,
    pub node_history: Signal<NodeHistory>,
    pub me: Signal<Option<String>>,
    pub screens: Signal<Layout>,
    pub sizes: Signal<Sizes>,
    pub flow_draft: Signal<Option<Vec<String>>>,
    pub active_flow: Signal<Option<String>>,
    pub fit_request: Signal<u64>,
    pub cwd: Signal<PathBuf>,
    pub items: Signal<Vec<ChatItem>>,
    pub busy: Signal<bool>,
    pub queued: Signal<Vec<Message>>,
    pub permissions: Signal<Vec<PermissionRequest>>,
    pub session_id: Signal<Option<String>>,
    pub model: Signal<String>,
    pub last_cost: Signal<Option<f64>>,
    pub permission_mode: Signal<PermissionMode>,
    pub repo_version: Signal<u64>,
    pub chat_input: Signal<String>,
    pub tab: Signal<Tab>,
    pub open_file: Signal<Option<OpenFile>>,
    pub editor_error: Signal<Option<String>>,
    pub editor_load: Signal<(u64, bool)>,
}

impl AppState {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            graph: Signal::new(Graph::default()),
            baseline: Signal::new(None),
            turn_start: Signal::new(None),
            turn_user_edits: Signal::new(HashSet::new()),
            rules: Signal::new(Vec::new()),
            rules_version: Signal::new(0),
            selected: Signal::new(Vec::new()),
            ghost_branches: Signal::new(Vec::new()),
            ghosts: Signal::new(Vec::new()),
            ghost_picks: Signal::new(Vec::new()),
            scope: Signal::new(Vec::new()),
            related_repos: Signal::new(Vec::new()),
            projects: Signal::new(projects::load()),
            project_view: Signal::new(None),
            refs: Signal::new(RefGraphs::new()),
            job: Signal::new(None),
            run_mode: Signal::new(RunMode::Plan),
            workbench_error: Signal::new(None),
            left_tab: Signal::new(LeftTab::Graph),
            graph_pane: Signal::new(GraphPane::Node),
            graph_bottom: Signal::new(screens::load_json(BottomPane::FILE)),
            node_history: Signal::new(NodeHistory::default()),
            me: Signal::new(None),
            screens: Signal::new(screens::load()),
            sizes: Signal::new(screens::load_sizes()),
            flow_draft: Signal::new(None),
            active_flow: Signal::new(None),
            fit_request: Signal::new(0),
            cwd: Signal::new(cwd),
            items: Signal::new(Vec::new()),
            busy: Signal::new(false),
            queued: Signal::new(Vec::new()),
            permissions: Signal::new(Vec::new()),
            session_id: Signal::new(None),
            model: Signal::new(String::new()),
            last_cost: Signal::new(None),
            permission_mode: Signal::new(PermissionMode::AcceptEdits),
            repo_version: Signal::new(0),
            chat_input: Signal::new(String::new()),
            tab: Signal::new(Tab::Files),
            open_file: Signal::new(None),
            editor_error: Signal::new(None),
            editor_load: Signal::new((0, false)),
        }
    }

    pub fn request_editor_load(&mut self, keep_cursor: bool) {
        let next = self.editor_load.peek().0 + 1;
        self.editor_load.set((next, keep_cursor));
    }

    pub fn bump_repo(&mut self) {
        *self.repo_version.write() += 1;
    }

    pub fn rel_path(&self, p: &Path) -> String {
        let cwd = self.cwd.peek();
        p.strip_prefix(&*cwd).unwrap_or(p).display().to_string()
    }

    pub fn open(&mut self, rel: &str) {
        let full = self.cwd.peek().join(rel);
        match std::fs::read(&full) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => {
                    let same = self.open_file.peek().as_ref().is_some_and(|f| f.rel == rel);
                    self.open_file.set(Some(OpenFile { rel: rel.to_string(), content: text.clone(), saved: text }));
                    self.editor_error.set(None);
                    self.request_editor_load(same);
                }
                Err(_) => self.editor_error.set(Some(format!("{rel} is not UTF-8 text"))),
            },
            Err(e) => self.editor_error.set(Some(format!("{rel}: {e}"))),
        }
        self.tab.set(Tab::Editor);
    }

    pub fn save(&mut self) {
        let Some(file) = self.open_file.peek().clone() else { return };
        let full = self.cwd.peek().join(&file.rel);
        match std::fs::write(&full, &file.content) {
            Ok(()) => {
                if let Some(f) = self.open_file.write().as_mut() {
                    f.saved = file.content;
                }
                self.editor_error.set(None);
                self.bump_repo();
            }
            Err(e) => self.editor_error.set(Some(format!("save failed: {e}"))),
        }
    }

    pub fn reload_open_file_if_clean(&mut self) {
        let Some(file) = self.open_file.peek().clone() else { return };
        if file.dirty() {
            return;
        }
        let full = self.cwd.peek().join(&file.rel);
        match std::fs::read_to_string(&full) {
            Ok(text) if text != file.saved => {
                self.open_file.set(Some(OpenFile { rel: file.rel, content: text.clone(), saved: text }));
                self.request_editor_load(true);
            }
            Ok(_) => {}
            Err(_) => self.open_file.set(None),
        }
    }

    pub fn revert(&mut self) {
        if let Some(f) = self.open_file.write().as_mut() {
            f.content = f.saved.clone();
        }
        self.request_editor_load(true);
    }

    pub fn close_file(&mut self, force: bool) {
        if !force && self.open_file.peek().as_ref().is_some_and(|f| f.dirty()) {
            self.editor_error.set(Some("No write since last change (use :w first)".into()));
            return;
        }
        self.open_file.set(None);
        self.editor_error.set(None);
        self.tab.set(Tab::Files);
    }

    pub fn load_workbench(&mut self) {
        let cwd = self.cwd.peek().clone();
        let mut errors = Vec::new();
        let g = graph::load(&cwd, GRAPH_FILE).unwrap_or_else(|e| {
            errors.push(e);
            None
        });
        let base = graph::load(&cwd, BASELINE_FILE).unwrap_or_else(|e| {
            errors.push(e);
            None
        });
        let r = rules::load(&cwd).unwrap_or_else(|e| {
            errors.push(e);
            Vec::new()
        });
        let related = github::load_related(&cwd).unwrap_or_else(|e| {
            errors.push(e);
            Vec::new()
        });
        let entries = node_history::load(&cwd).unwrap_or_else(|e| {
            errors.push(e);
            Vec::new()
        });
        self.me.set(git::user_name(&cwd));
        self.node_history.set(NodeHistory { known: node_history::load_snapshot(&cwd), entries });
        self.graph.set(g.unwrap_or_default());
        self.record_history(Source::Outside);
        self.baseline.set(base);
        self.rules.set(r);
        self.related_repos.set(related);
        *self.rules_version.write() += 1;
        self.selected.set(Vec::new());
        self.ghost_branches.set(Vec::new());
        self.ghosts.set(Vec::new());
        self.ghost_picks.set(Vec::new());
        self.scope.set(Vec::new());
        self.flow_draft.set(None);
        self.active_flow.set(None);
        self.job.set(None);
        self.turn_start.set(None);
        self.turn_user_edits.write().clear();
        self.workbench_error.set(if errors.is_empty() { None } else { Some(errors.join("\n")) });
        *self.fit_request.write() += 1;
        self.refs.set(RefGraphs::new());
        self.load_references();
    }

    pub fn load_references(&mut self) {
        let sources: Vec<String> = self.graph.peek().references().iter().map(|r| r.source.clone()).collect();
        for s in sources {
            self.load_reference(&s, false);
        }
    }

    pub fn load_reference(&mut self, source: &str, force: bool) {
        let key = reference::key(source);
        if key.is_empty() || (!force && self.refs.peek().contains_key(&key)) {
            return;
        }
        let cwd = self.cwd.peek().clone();
        let Some(src) = reference::parse(&cwd, &key) else {
            self.refs.write().insert(key, RefState::Failed("Not a recognised path, URL or package".into()));
            return;
        };
        self.refs.write().insert(key.clone(), RefState::Loading);
        let mut st = *self;
        spawn(async move {
            let result = tokio::task::spawn_blocking(move || reference::load(&cwd, &src))
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            let found = matches!(result, Ok(Some(_)));
            st.refs.write().insert(key, match result {
                Ok(g) => RefState::Ready(g),
                Err(e) => RefState::Failed(e),
            });
            if found {
                st.touch_context();
            }
        });
    }

    pub fn touch_context(&mut self) {
        *self.rules_version.write() += 1;
    }

    pub fn with_refs<T>(&self, f: impl FnOnce(&Refs) -> T) -> T {
        let cwd = self.cwd.peek().clone();
        let graphs = self.refs.peek();
        f(&Refs { cwd: &cwd, graphs: &graphs })
    }

    pub fn sync_workbench_from_disk(&mut self) {
        let cwd = self.cwd.peek().clone();
        if let Ok(entries) = node_history::load(&cwd)
            && self.node_history.peek().entries != entries
        {
            self.node_history.write().adopt(entries);
        }
        match graph::load(&cwd, GRAPH_FILE) {
            Ok(Some(g)) => {
                if *self.graph.peek() != g {
                    let was_empty = self.graph.peek().nodes.is_empty();
                    let path = self.scope.peek().clone();
                    let valid = path.iter().take_while(|m| g.module(m).is_some()).count();
                    if valid < path.len() {
                        self.scope.write().truncate(valid);
                        self.selected.set(Vec::new());
                    }
                    let scope = self.scope.peek().last().cloned();
                    let net = g.net(scope.as_deref());
                    let sel = self.selected.peek().clone();
                    let kept: Vec<String> = sel.iter().filter(|id| net.is_some_and(|n| n.node(id).is_some())).cloned().collect();
                    if kept != sel {
                        self.selected.set(kept);
                    }
                    self.graph.set(g);
                    self.record_history(self.disk_source());
                    if was_empty {
                        *self.fit_request.write() += 1;
                    }
                    self.load_references();
                }
                if self.workbench_error.peek().as_ref().is_some_and(|e| e.starts_with(GRAPH_FILE)) {
                    self.workbench_error.set(None);
                }
            }
            Ok(None) => {}
            Err(e) => self.workbench_error.set(Some(e)),
        }
        if let Ok(r) = rules::load(&cwd)
            && *self.rules.peek() != r
        {
            self.rules.set(r);
            *self.rules_version.write() += 1;
        }
    }

    pub fn graph_locked(&self) -> bool {
        self.job.read().is_some()
    }

    pub fn update_graph(&mut self, f: impl FnOnce(&mut Graph)) {
        if self.job.peek().is_some() {
            return;
        }
        let before = self.turn_start.peek().is_some().then(|| self.graph.peek().clone());
        f(&mut self.graph.write());
        if let Some(before) = before {
            let keys: Vec<String> = graph::diff(&before, &self.graph.peek()).iter().map(Change::key).collect();
            self.turn_user_edits.write().extend(keys);
        }
        let path = self.scope.peek().clone();
        let valid = path.iter().take_while(|m| self.graph.peek().module(m).is_some()).count();
        if valid < path.len() {
            self.set_scope(path[..valid].to_vec());
        }
        self.save_graph();
        self.record_history(Source::Person);
    }

    fn disk_source(&self) -> Source {
        if *self.busy.peek() || self.job.peek().is_some() || self.turn_start.peek().is_some() {
            Source::Claude
        } else {
            Source::Outside
        }
    }

    /// Appends an entry to the shared node history for every node or module whose information
    /// differs from what this client last saw, saves the log in `.workbench/` and the local
    /// snapshot in the user's config folder.
    pub fn record_history(&mut self, source: Source) {
        let g = self.graph.peek().clone();
        let me = self.me.peek().clone();
        let before = self.node_history.peek().entries.len();
        let changed = self.node_history.write().record(&g, source, me.as_deref(), node_history::now());
        if !changed {
            return;
        }
        let cwd = self.cwd.peek().clone();
        let h = self.node_history.peek().clone();
        let mut result = Ok(());
        if h.entries.len() != before || node_history::exists(&cwd) {
            result = node_history::save(&cwd, &h.entries);
        }
        if let Some(known) = &h.known {
            result = result.and(node_history::save_snapshot(&cwd, known));
        }
        if let Err(e) = result {
            self.workbench_error.set(Some(e));
        }
    }

    pub fn set_bottom(&mut self, f: impl FnOnce(&mut BottomPane)) {
        f(&mut self.graph_bottom.write());
        self.save_bottom();
    }

    pub fn save_bottom(&mut self) {
        if let Err(e) = screens::save_json(BottomPane::FILE, &*self.graph_bottom.peek()) {
            self.workbench_error.set(Some(e));
        }
    }

    pub fn show_pane(&mut self, pane: GraphPane) {
        self.graph_pane.set(pane);
        if self.graph_bottom.peek().collapsed {
            self.set_bottom(|b| b.collapsed = false);
        }
    }

    pub fn current_scope(&self) -> Option<String> {
        self.scope.read().last().cloned()
    }

    pub fn net(&self) -> Graph {
        let scope = self.scope.read().last().cloned();
        self.graph.read().net(scope.as_deref()).cloned().unwrap_or_default()
    }

    pub fn net_peek(&self) -> Graph {
        let scope = self.scope.peek().last().cloned();
        self.graph.peek().net(scope.as_deref()).cloned().unwrap_or_default()
    }

    pub fn update_net(&mut self, f: impl FnOnce(&mut Graph)) {
        let scope = self.scope.peek().last().cloned();
        self.update_graph(|g| {
            if let Some(net) = g.net_mut(scope.as_deref()) {
                f(net);
            }
        });
    }

    pub fn set_scope(&mut self, path: Vec<String>) {
        if *self.scope.peek() == path {
            return;
        }
        self.scope.set(path);
        self.selected.set(Vec::new());
        self.flow_draft.set(None);
        self.active_flow.set(None);
        *self.fit_request.write() += 1;
    }

    pub fn enter_module(&mut self, id: &str) {
        if self.graph.peek().module(id).is_none() {
            return;
        }
        let mut path = self.scope.peek().clone();
        match path.iter().position(|m| m == id) {
            Some(i) => path.truncate(i + 1),
            None => path.push(id.to_string()),
        }
        self.set_scope(path);
    }

    pub fn show_change(&mut self, scope: Option<String>, id: Option<String>) {
        let current = self.scope.peek().last().cloned();
        if current != scope {
            self.set_scope(scope.into_iter().collect());
        }
        self.select(id);
        self.graph_pane.set(GraphPane::Node);
    }

    pub fn select(&mut self, id: Option<String>) {
        self.selected.set(id.into_iter().collect());
    }

    pub fn toggle_selected(&mut self, id: String) {
        let mut sel = self.selected.write();
        match sel.iter().position(|s| *s == id) {
            Some(i) => {
                sel.remove(i);
            }
            None => sel.push(id),
        }
    }

    pub fn selected_titles(&self) -> Vec<String> {
        let net = self.net();
        let mut out: Vec<String> = self.selected.read().iter().filter(|id| net.node(id).is_some()).map(|id| net.title_of(id)).collect();
        let _ = (self.ghost_picks.read(), self.ghosts.read());
        out.extend(self.ghost_titles());
        out
    }

    pub fn selection_context(&self) -> Option<String> {
        let scope = self.scope.peek().last().cloned();
        self.with_refs(|refs| prompts::selected_nodes(&self.graph.peek(), scope.as_deref(), &self.selected.peek(), Some(refs)))
    }

    fn picked_ghosts(&self) -> Vec<(GhostBranch, GhostPick, graph::Node)> {
        let ghosts = self.ghosts.peek();
        self.ghost_picks
            .peek()
            .iter()
            .filter_map(|p| {
                let b = ghosts.iter().find(|b| b.branch == p.branch)?;
                let n = b.node(p.scope.as_deref(), &p.id)?.clone();
                Some((b.clone(), p.clone(), n))
            })
            .collect()
    }

    pub fn ghost_titles(&self) -> Vec<String> {
        self.picked_ghosts().into_iter().map(|(b, _, n)| format!("⧉ {} ({})", n.title, b.branch)).collect()
    }

    pub fn ghost_context(&self) -> Option<String> {
        let picked = self.picked_ghosts();
        let rows: Vec<(&str, &Graph, Option<&str>, &graph::Node, String)> = picked
            .iter()
            .map(|(b, p, n)| {
                let change = b.change(p.scope.as_deref(), &p.id).map(|k| ghost::describe(&k)).unwrap_or_else(|| "on that branch".into());
                (b.branch.as_str(), &b.graph, p.scope.as_deref(), n, change)
            })
            .collect();
        prompts::ghost_nodes(&rows)
    }

    pub fn clear_context(&mut self) {
        self.selected.set(Vec::new());
        self.ghost_picks.set(Vec::new());
    }

    pub fn toggle_ghost_pick(&mut self, pick: GhostPick) {
        let mut picks = self.ghost_picks.write();
        match picks.iter().position(|p| *p == pick) {
            Some(i) => {
                picks.remove(i);
            }
            None => picks.push(pick),
        }
    }

    pub fn set_ghost_branches(&mut self, branches: Vec<String>) {
        self.ghost_picks.write().retain(|p| branches.contains(&p.branch));
        self.ghosts.write().retain(|g| branches.contains(&g.branch));
        self.ghost_branches.set(branches);
        self.load_ghosts();
    }

    pub fn load_ghosts(&mut self) {
        let wanted = self.ghost_branches.peek().clone();
        if wanted.is_empty() {
            self.ghosts.set(Vec::new());
            return;
        }
        let cwd = self.cwd.peek().clone();
        let mut st = *self;
        spawn(async move {
            let names = wanted.clone();
            let loaded = tokio::task::spawn_blocking(move || {
                if !git::is_repo(&cwd) {
                    return Vec::new();
                }
                names.iter().map(|b| ghost::load(&cwd, b)).collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            if *st.ghost_branches.peek() != wanted {
                return;
            }
            if *st.ghosts.peek() != loaded {
                st.ghosts.set(loaded);
            }
            let ghosts = st.ghosts.peek().clone();
            let keep = |p: &GhostPick| ghosts.iter().any(|b| b.branch == p.branch && b.node(p.scope.as_deref(), &p.id).is_some());
            if st.ghost_picks.peek().iter().any(|p| !keep(p)) {
                st.ghost_picks.write().retain(keep);
            }
        });
    }

    pub fn message(&self, text: String) -> Message {
        let context = self.selection_context();
        let ghosts = self.ghost_context();
        let mut titles: Vec<String> = match context {
            Some(_) => {
                let net = self.net_peek();
                self.selected.peek().iter().filter(|id| net.node(id).is_some()).map(|id| net.title_of(id)).collect()
            }
            None => Vec::new(),
        };
        if ghosts.is_some() {
            titles.extend(self.ghost_titles());
        }
        let prompt = prompts::with_selection(context.as_deref(), ghosts.as_deref(), &text);
        Message { text, context: titles, prompt }
    }

    pub fn save_graph(&mut self) {
        let cwd = self.cwd.peek().clone();
        if let Err(e) = graph::save(&cwd, GRAPH_FILE, &self.graph.peek()) {
            self.workbench_error.set(Some(e));
        }
    }

    pub fn changes(&self) -> Vec<Change> {
        let empty = Graph::default();
        let base = self.baseline.read();
        graph::diff(base.as_ref().unwrap_or(&empty), &self.graph.read())
    }

    pub fn mark_in_sync(&mut self) {
        let cwd = self.cwd.peek().clone();
        let g = self.graph.peek().clone();
        match graph::save(&cwd, BASELINE_FILE, &g) {
            Ok(()) => self.baseline.set(Some(g)),
            Err(e) => self.workbench_error.set(Some(e)),
        }
    }

    pub fn revert_change(&mut self, scope: Option<String>, subject: Subject, id: &str) {
        match subject {
            Subject::Module => self.revert_module(id),
            Subject::Node => self.revert_node(scope.as_deref(), id),
        }
    }

    fn revert_node(&mut self, scope: Option<&str>, id: &str) {
        let before = self.baseline.peek().as_ref().and_then(|b| b.net(scope).and_then(|n| n.node(id)).cloned());
        self.update_graph(|g| {
            let Some(g) = g.net_mut(scope) else { return };
            match before {
                Some(mut node) => {
                    let pos = g.node(&node.id).and_then(|n| n.pos);
                    node.pos = pos.or(node.pos);
                    match g.node_mut(&node.id) {
                        Some(n) => *n = node,
                        None => g.nodes.push(node),
                    }
                }
                None => g.remove_node(id),
            }
        });
    }

    fn revert_module(&mut self, id: &str) {
        let before = self.baseline.peek().as_ref().and_then(|b| b.module(id).cloned());
        self.update_graph(|g| match before {
            Some(m) => match g.module_mut(id) {
                Some(cur) => {
                    cur.title = m.title;
                    cur.description = m.description;
                }
                None => g.modules.push(m),
            },
            None => g.remove_module(id),
        });
    }

    pub fn update_related(&mut self, f: impl FnOnce(&mut Vec<String>)) {
        f(&mut self.related_repos.write());
        *self.rules_version.write() += 1;
        let cwd = self.cwd.peek().clone();
        if let Err(e) = github::save_related(&cwd, &self.related_repos.peek()) {
            self.workbench_error.set(Some(e));
        }
    }

    pub fn current_project(&self) -> Option<Project> {
        let cwd = self.cwd.read().clone();
        projects::containing(&self.projects.read(), &cwd).cloned()
    }

    pub fn update_projects(&mut self, f: impl FnOnce(&mut Vec<Project>)) {
        let unplaced = |list: &[Project]| {
            let mut list = list.to_vec();
            list.iter_mut().flat_map(|p| p.repos.iter_mut()).for_each(|r| r.pos = None);
            list
        };
        let before = unplaced(&self.projects.peek());
        f(&mut self.projects.write());
        if unplaced(&self.projects.peek()) != before {
            *self.rules_version.write() += 1;
        }
        if let Err(e) = projects::save(&self.projects.peek()) {
            self.workbench_error.set(Some(e));
        }
    }

    pub fn update_project(&mut self, id: &str, f: impl FnOnce(&mut Project)) {
        self.update_projects(|list| {
            if let Some(p) = list.iter_mut().find(|p| p.id == id) {
                f(p);
            }
        });
    }

    pub fn show_project(&mut self, id: Option<String>) {
        let cwd = self.cwd.peek().clone();
        let id = id
            .or_else(|| projects::containing(&self.projects.peek(), &cwd).map(|p| p.id.clone()))
            .or_else(|| self.projects.peek().first().map(|p| p.id.clone()))
            .unwrap_or_default();
        self.project_view.set(Some(id));
    }

    pub fn start_job(&mut self, kind: JobKind) -> RunMode {
        let mode = *self.run_mode.peek();
        self.job.set(Some(Job { kind, mode, awaiting: false }));
        mode
    }

    pub fn await_answers(&mut self) {
        if let Some(job) = self.job.write().as_mut() {
            job.awaiting = true;
        }
        self.sync_workbench_from_disk();
    }

    pub fn resume_job(&mut self) {
        if let Some(job) = self.job.write().as_mut().filter(|j| j.awaiting) {
            job.awaiting = false;
        }
    }

    pub fn awaiting_answers(&self) -> bool {
        self.job.read().is_some_and(|j| j.awaiting)
    }

    pub fn begin_chat_turn(&mut self) {
        if self.job.peek().is_some() || self.turn_start.peek().is_some() {
            return;
        }
        self.sync_workbench_from_disk();
        let g = self.graph.peek().clone();
        self.turn_start.set(Some(g));
        self.turn_user_edits.write().clear();
    }

    fn end_chat_turn(&mut self, success: bool) {
        if self.turn_start.peek().is_none() {
            return;
        }
        self.sync_workbench_from_disk();
        let Some(start) = self.turn_start.take() else { return };
        let user_edits = std::mem::take(&mut *self.turn_user_edits.write());
        if !success {
            return;
        }
        self.sync_workbench_from_disk();
        let cur = self.graph.peek().clone();
        let made: Vec<Change> = graph::diff(&start, &cur).into_iter().filter(|c| !user_edits.contains(&c.key())).collect();
        if made.is_empty() {
            return;
        }
        let was_in_sync = user_edits.is_empty() && self.baseline.peek().as_ref().is_some_and(|b| graph::diff(b, &start).is_empty());
        if was_in_sync {
            self.mark_in_sync();
            return;
        }
        let mut base = self.baseline.peek().clone().unwrap_or_default();
        graph::absorb(&mut base, &made, &cur);
        let cwd = self.cwd.peek().clone();
        match graph::save(&cwd, BASELINE_FILE, &base) {
            Ok(()) => self.baseline.set(Some(base)),
            Err(e) => self.workbench_error.set(Some(e)),
        }
    }

    pub fn finish_job(&mut self, success: bool) {
        self.end_chat_turn(success);
        let Some(job) = *self.job.peek() else { return };
        // Sync while the job is still set so its graph edits are credited to Claude.
        self.sync_workbench_from_disk();
        self.job.set(None);
        if success && job.kind != JobKind::Draft {
            self.mark_in_sync();
        }
    }

    pub fn update_rules(&mut self, f: impl FnOnce(&mut Vec<Rule>)) {
        f(&mut self.rules.write());
        *self.rules_version.write() += 1;
        let cwd = self.cwd.peek().clone();
        if let Err(e) = rules::save(&cwd, &self.rules.peek()) {
            self.workbench_error.set(Some(e));
        }
    }

    pub fn set_screen(&mut self, screen: Screen, state: ScreenState) {
        self.screens.write().insert(screen, state);
        if let Err(e) = screens::save(&self.screens.peek()) {
            self.workbench_error.set(Some(e));
        }
    }

    pub fn set_size(&mut self, screen: Screen, pct: f64) {
        self.sizes.write().insert(screen, pct);
        self.save_sizes();
    }

    pub fn save_sizes(&mut self) {
        if let Err(e) = screens::save_sizes(&self.sizes.peek()) {
            self.workbench_error.set(Some(e));
        }
    }

    pub fn append_to_input(&mut self, text: &str) {
        let mut input = self.chat_input.write();
        if !input.is_empty() && !input.ends_with('\n') {
            input.push('\n');
        }
        input.push_str(text);
    }
}
