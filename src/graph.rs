use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const GRAPH_FILE: &str = ".workbench/graph.json";
pub const BASELINE_FILE: &str = ".workbench/graph.baseline.json";

pub const NODE_W: f64 = 190.0;
pub const NODE_H: f64 = 58.0;
const COL_W: f64 = 260.0;
const ROW_H: f64 = 84.0;

pub const KNOWN_KINDS: [&str; 15] = [
    "app", "api", "route", "handler", "service", "model", "store", "db", "external", "ui", "screen",
    "component", "job", "module", "reference",
];

pub const REFERENCE_KIND: &str = "reference";

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RefNode {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Reference {
    #[serde(default)]
    pub source: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<RefNode>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<Reference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pos: Option<[f64; 2]>,
}

impl Node {
    pub fn is_reference(&self) -> bool {
        self.reference.is_some() || self.kind.trim().eq_ignore_ascii_case(REFERENCE_KIND)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Module {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(flatten)]
    pub net: Graph,
}

impl Module {
    pub fn as_node(&self) -> Node {
        let mut files: Vec<String> = Vec::new();
        for f in self.net.nodes.iter().flat_map(|n| n.files.iter()) {
            if !files.contains(f) {
                files.push(f.clone());
            }
        }
        Node {
            id: self.id.clone(),
            title: self.title.clone(),
            kind: "module".into(),
            description: self.description.clone(),
            files,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Flow {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub nodes: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Graph {
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub flows: Vec<Flow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modules: Vec<Module>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Title,
    Kind,
    Description,
    Links,
    Module,
    Reference,
}

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::Title => "title",
            Field::Kind => "kind",
            Field::Description => "description",
            Field::Links => "links",
            Field::Module => "module",
            Field::Reference => "reference",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    Node,
    Module,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified(Vec<Field>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub scope: Option<String>,
    pub subject: Subject,
    pub id: String,
    pub kind: ChangeKind,
    pub before: Option<Node>,
    pub after: Option<Node>,
}

impl Change {
    pub fn node(&self) -> &Node {
        self.after.as_ref().or(self.before.as_ref()).expect("change without node")
    }

    pub fn key(&self) -> String {
        let subject = match self.subject {
            Subject::Node => "node",
            Subject::Module => "module",
        };
        format!("{}:{subject}:{}", self.scope.as_deref().unwrap_or(""), self.id)
    }
}

pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "node".into() } else { out }
}

impl Graph {
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn node_mut(&mut self, id: &str) -> Option<&mut Node> {
        self.nodes.iter_mut().find(|n| n.id == id)
    }

    pub fn title_of(&self, id: &str) -> String {
        self.node(id).map(|n| n.title.clone()).unwrap_or_else(|| id.to_string())
    }

    pub fn incoming(&self, id: &str) -> Vec<&Node> {
        self.nodes.iter().filter(|n| n.links.iter().any(|l| l == id)).collect()
    }

    pub fn linked(&self, a: &str, b: &str) -> bool {
        let has = |x: &str, y: &str| self.node(x).is_some_and(|n| n.links.iter().any(|l| l == y));
        has(a, b) || has(b, a)
    }

    pub fn unique_id(&self, base: &str) -> String {
        let base = slugify(base);
        let taken = |id: &str| self.nodes.iter().any(|n| n.id == id) || self.flows.iter().any(|f| f.id == id);
        if !taken(&base) {
            return base;
        }
        (2..).map(|i| format!("{base}-{i}")).find(|id| !taken(id)).unwrap()
    }

    pub fn remove_node(&mut self, id: &str) {
        self.nodes.retain(|n| n.id != id);
        for n in &mut self.nodes {
            n.links.retain(|l| l != id);
        }
        for f in &mut self.flows {
            f.nodes.retain(|n| n != id);
        }
        self.flows.retain(|f| !f.nodes.is_empty());
    }

    pub fn positions(&self) -> HashMap<String, [f64; 2]> {
        let auto = auto_layout(self);
        let mut placed: HashMap<String, [f64; 2]> = self.nodes.iter().filter_map(|n| n.pos.map(|p| (n.id.clone(), p))).collect();
        if placed.is_empty() {
            return auto;
        }
        let mut free: Vec<&Node> = self.nodes.iter().filter(|n| n.pos.is_none()).collect();
        if free.is_empty() {
            return placed;
        }
        free.sort_by(|a, b| {
            let (pa, pb) = (auto[&a.id], auto[&b.id]);
            pa[0].total_cmp(&pb[0]).then(pa[1].total_cmp(&pb[1]))
        });
        let x0 = placed.values().map(|p| p[0]).fold(f64::MAX, f64::min);
        let below = placed.values().map(|p| p[1]).fold(f64::MIN, f64::max) + ROW_H * 2.0;
        for n in free {
            let callers: Vec<[f64; 2]> = self.incoming(&n.id).iter().filter_map(|c| placed.get(&c.id).copied()).collect();
            let callees: Vec<[f64; 2]> = n.links.iter().filter_map(|l| placed.get(l).copied()).collect();
            let mean_y = |ps: &[[f64; 2]]| ps.iter().map(|p| p[1]).sum::<f64>() / ps.len() as f64;
            let want = if !callers.is_empty() {
                [callers.iter().map(|p| p[0]).fold(f64::MIN, f64::max) + COL_W, mean_y(&callers)]
            } else if !callees.is_empty() {
                [callees.iter().map(|p| p[0]).fold(f64::MAX, f64::min) - COL_W, mean_y(&callees)]
            } else {
                let a = auto[&n.id];
                [x0 + a[0], below + a[1]]
            };
            let spot = free_spot(want, &placed);
            placed.insert(n.id.clone(), spot);
        }
        placed
    }

    pub fn clear_positions(&mut self) -> HashMap<String, [f64; 2]> {
        self.nodes.iter_mut().filter_map(|n| n.pos.take().map(|p| (n.id.clone(), p))).collect()
    }

    pub fn pin_all(&mut self) {
        let pos = self.positions();
        for n in self.nodes.iter_mut().filter(|n| n.pos.is_none()) {
            n.pos = pos.get(&n.id).copied();
        }
    }

    pub fn edges(&self) -> Vec<(String, String)> {
        let ids: HashSet<&str> = self.nodes.iter().map(|n| n.id.as_str()).collect();
        self.nodes
            .iter()
            .flat_map(|n| {
                n.links
                    .iter()
                    .filter(|l| ids.contains(l.as_str()))
                    .map(|l| (n.id.clone(), l.clone()))
            })
            .collect()
    }

    pub fn references(&self) -> Vec<&Reference> {
        self.nets().into_iter().flat_map(|(_, net)| net.nodes.iter().filter_map(|n| n.reference.as_ref())).collect()
    }

    pub fn all_nodes(&self) -> Vec<(Option<&str>, &Node)> {
        self.nets().into_iter().flat_map(|(scope, net)| net.nodes.iter().map(move |n| (scope, n))).collect()
    }

    pub fn module(&self, id: &str) -> Option<&Module> {
        self.modules.iter().find(|m| m.id == id)
    }

    pub fn module_mut(&mut self, id: &str) -> Option<&mut Module> {
        self.modules.iter_mut().find(|m| m.id == id)
    }

    pub fn module_title(&self, id: &str) -> String {
        self.module(id).map(|m| m.title.clone()).unwrap_or_else(|| id.to_string())
    }

    pub fn net(&self, scope: Option<&str>) -> Option<&Graph> {
        match scope {
            None => Some(self),
            Some(id) => self.module(id).map(|m| &m.net),
        }
    }

    pub fn net_mut(&mut self, scope: Option<&str>) -> Option<&mut Graph> {
        match scope {
            None => Some(self),
            Some(id) => self.module_mut(id).map(|m| &mut m.net),
        }
    }

    pub fn nets(&self) -> Vec<(Option<&str>, &Graph)> {
        let mut out = vec![(None, self)];
        out.extend(self.modules.iter().map(|m| (Some(m.id.as_str()), &m.net)));
        out
    }

    pub fn unique_module_id(&self, base: &str) -> String {
        let base = slugify(base);
        let taken = |id: &str| self.modules.iter().any(|m| m.id == id);
        if !taken(&base) {
            return base;
        }
        (2..).map(|i| format!("{base}-{i}")).find(|id| !taken(id)).unwrap()
    }

    pub fn module_uses(&self, id: &str) -> Vec<(Option<String>, String)> {
        self.nets()
            .into_iter()
            .flat_map(|(scope, net)| {
                net.nodes
                    .iter()
                    .filter(|n| n.module.as_deref() == Some(id))
                    .map(move |n| (scope.map(String::from), n.id.clone()))
            })
            .collect()
    }

    pub fn module_reaches(&self, from: &str, target: &str) -> bool {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut stack = vec![from];
        while let Some(id) = stack.pop() {
            if id == target {
                return true;
            }
            if !seen.insert(id) {
                continue;
            }
            if let Some(m) = self.module(id) {
                stack.extend(m.net.nodes.iter().filter_map(|n| n.module.as_deref()));
            }
        }
        false
    }

    pub fn can_attach(&self, scope: Option<&str>, module: &str) -> bool {
        self.module(module).is_some() && scope.is_none_or(|s| !self.module_reaches(module, s))
    }

    pub fn remove_module(&mut self, id: &str) {
        self.modules.retain(|m| m.id != id);
        let mut refs: Vec<(Option<String>, String)> = Vec::new();
        for (scope, net) in self.nets() {
            for n in net.nodes.iter().filter(|n| n.module.as_deref() == Some(id)) {
                refs.push((scope.map(String::from), n.id.clone()));
            }
        }
        for (scope, node) in refs {
            if let Some(net) = self.net_mut(scope.as_deref()) {
                net.remove_node(&node);
            }
        }
    }
}

pub fn free_spot(want: [f64; 2], placed: &HashMap<String, [f64; 2]>) -> [f64; 2] {
    let taken = |x: f64, y: f64| {
        placed
            .values()
            .any(|p| (p[0] - x).abs() < NODE_W + 24.0 && (p[1] - y).abs() < NODE_H + 18.0)
    };
    let [x, y] = [want[0].round(), want[1].round()];
    std::iter::once(0.0)
        .chain((1..400).flat_map(|i| [i as f64, -(i as f64)]))
        .map(|k| y + k * ROW_H / 2.0)
        .find(|&cy| !taken(x, cy))
        .map(|cy| [x, cy])
        .unwrap_or([x, y])
}

pub fn auto_layout(g: &Graph) -> HashMap<String, [f64; 2]> {
    let n = g.nodes.len();
    let index: HashMap<&str, usize> = g.nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();
    let index = &index;
    let edges: Vec<(usize, usize)> = g
        .nodes
        .iter()
        .enumerate()
        .flat_map(|(i, node)| node.links.iter().filter_map(move |l| index.get(l.as_str())).map(move |&j| (i, j)))
        .filter(|(i, j)| i != j)
        .collect();

    let mut depth = vec![0usize; n];
    for _ in 0..n {
        let mut changed = false;
        for &(u, v) in &edges {
            if depth[u] + 1 > depth[v] && depth[u] + 1 < n {
                depth[v] = depth[u] + 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let max_depth = depth.iter().copied().max().unwrap_or(0);
    let mut row_of = vec![0f64; n];
    let mut out = HashMap::new();
    for d in 0..=max_depth {
        let mut layer: Vec<usize> = (0..n).filter(|&i| depth[i] == d).collect();
        let bary = |i: usize| {
            let parents: Vec<f64> = edges.iter().filter(|e| e.1 == i && depth[e.0] < d).map(|e| row_of[e.0]).collect();
            if parents.is_empty() { f64::MAX } else { parents.iter().sum::<f64>() / parents.len() as f64 }
        };
        layer.sort_by(|&a, &b| bary(a).partial_cmp(&bary(b)).unwrap_or(std::cmp::Ordering::Equal).then(a.cmp(&b)));
        for (row, &i) in layer.iter().enumerate() {
            row_of[i] = row as f64;
            out.insert(g.nodes[i].id.clone(), [d as f64 * COL_W, row as f64 * ROW_H]);
        }
    }
    out
}

fn sorted(v: &[String]) -> Vec<&String> {
    let mut s: Vec<&String> = v.iter().collect();
    s.sort();
    s.dedup();
    s
}

pub fn diff(base: &Graph, cur: &Graph) -> Vec<Change> {
    let mut out = diff_net(base, cur, None);
    let empty = Graph::default();
    let mut ids: Vec<&str> = cur.modules.iter().map(|m| m.id.as_str()).collect();
    ids.extend(base.modules.iter().map(|m| m.id.as_str()).filter(|id| cur.module(id).is_none()));
    for id in ids {
        let scope = Some(id.to_string());
        let change = |kind, before: Option<&Module>, after: Option<&Module>| Change {
            scope: scope.clone(),
            subject: Subject::Module,
            id: id.to_string(),
            kind,
            before: before.map(Module::as_node),
            after: after.map(Module::as_node),
        };
        match (base.module(id), cur.module(id)) {
            (None, Some(c)) => {
                out.push(change(ChangeKind::Added, None, Some(c)));
                out.extend(diff_net(&empty, &c.net, Some(id)));
            }
            (Some(b), None) => out.push(change(ChangeKind::Removed, Some(b), None)),
            (Some(b), Some(c)) => {
                let mut fields = Vec::new();
                if b.title.trim() != c.title.trim() {
                    fields.push(Field::Title);
                }
                if b.description.trim() != c.description.trim() {
                    fields.push(Field::Description);
                }
                if !fields.is_empty() {
                    out.push(change(ChangeKind::Modified(fields), Some(b), Some(c)));
                }
                out.extend(diff_net(&b.net, &c.net, Some(id)));
            }
            (None, None) => {}
        }
    }
    out
}

fn diff_net(base: &Graph, cur: &Graph, scope: Option<&str>) -> Vec<Change> {
    let scope = scope.map(String::from);
    let mut out = Vec::new();
    for after in &cur.nodes {
        match base.node(&after.id) {
            None => out.push(Change {
                scope: scope.clone(),
                subject: Subject::Node,
                id: after.id.clone(),
                kind: ChangeKind::Added,
                before: None,
                after: Some(after.clone()),
            }),
            Some(before) => {
                let mut fields = Vec::new();
                if before.title.trim() != after.title.trim() {
                    fields.push(Field::Title);
                }
                if before.kind.trim() != after.kind.trim() {
                    fields.push(Field::Kind);
                }
                if before.description.trim() != after.description.trim() {
                    fields.push(Field::Description);
                }
                if sorted(&before.links) != sorted(&after.links) {
                    fields.push(Field::Links);
                }
                if before.module != after.module {
                    fields.push(Field::Module);
                }
                if before.reference != after.reference {
                    fields.push(Field::Reference);
                }
                if !fields.is_empty() {
                    out.push(Change {
                        scope: scope.clone(),
                        subject: Subject::Node,
                        id: after.id.clone(),
                        kind: ChangeKind::Modified(fields),
                        before: Some(before.clone()),
                        after: Some(after.clone()),
                    });
                }
            }
        }
    }
    for before in &base.nodes {
        if cur.node(&before.id).is_none() {
            out.push(Change {
                scope: scope.clone(),
                subject: Subject::Node,
                id: before.id.clone(),
                kind: ChangeKind::Removed,
                before: Some(before.clone()),
                after: None,
            });
        }
    }
    out
}

pub fn absorb(base: &mut Graph, changes: &[Change], cur: &Graph) {
    for c in changes.iter().filter(|c| c.subject == Subject::Module) {
        match (&c.kind, cur.module(&c.id)) {
            (ChangeKind::Removed, _) => base.modules.retain(|m| m.id != c.id),
            (_, Some(m)) => match base.module_mut(&c.id) {
                Some(b) => {
                    b.title = m.title.clone();
                    b.description = m.description.clone();
                }
                None => base.modules.push(Module { net: Graph::default(), ..m.clone() }),
            },
            (_, None) => {}
        }
    }
    for c in changes.iter().filter(|c| c.subject == Subject::Node) {
        let Some(net) = base.net_mut(c.scope.as_deref()) else { continue };
        match cur.net(c.scope.as_deref()).and_then(|n| n.node(&c.id)) {
            Some(node) if c.kind != ChangeKind::Removed => match net.node_mut(&c.id) {
                Some(b) => *b = node.clone(),
                None => net.nodes.push(node.clone()),
            },
            _ => net.nodes.retain(|n| n.id != c.id),
        }
    }
}

pub fn load(cwd: &Path, rel: &str) -> Result<Option<Graph>, String> {
    match std::fs::read_to_string(cwd.join(rel)) {
        Ok(text) => serde_json::from_str(&text).map(Some).map_err(|e| format!("{rel}: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{rel}: {e}")),
    }
}

pub fn save(cwd: &Path, rel: &str, g: &Graph) -> Result<(), String> {
    let path = cwd.join(rel);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut text = serde_json::to_string_pretty(g).map_err(|e| e.to_string())?;
    text.push('\n');
    if std::fs::read_to_string(&path).is_ok_and(|old| old == text) {
        return Ok(());
    }
    std::fs::write(&path, text).map_err(|e| format!("{rel}: {e}"))
}

pub fn kind_color(kind: &str) -> &'static str {
    match kind.trim().to_ascii_lowercase().as_str() {
        "app" | "cli" => "#d97757",
        "api" | "server" | "gateway" => "#e3b341",
        "route" | "endpoint" | "command" => "#56d364",
        "handler" | "controller" => "#3fb950",
        "service" | "domain" | "logic" => "#58a6ff",
        "model" | "schema" | "type" => "#a5d6ff",
        "store" | "state" | "cache" => "#bc8cff",
        "db" | "database" | "storage" => "#d2a8ff",
        "external" | "integration" | "third-party" => "#ff7b72",
        "ui" | "screen" | "page" | "view" => "#f778ba",
        "component" | "widget" => "#ffa198",
        "job" | "worker" | "queue" | "event" => "#79c0ff",
        "module" | "package" | "feature" => "#56b6c2",
        "reference" | "ref" | "docs" => "#e5c07b",
        _ => "#8a909c",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, links: &[&str]) -> Node {
        Node {
            id: id.into(),
            title: id.to_uppercase(),
            kind: "service".into(),
            description: format!("{id} does things"),
            links: links.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn diff_detects_added_removed_modified() {
        let base = Graph { nodes: vec![node("a", &["b"]), node("b", &[]), node("c", &[])], ..Default::default() };
        let mut cur = base.clone();
        cur.remove_node("c");
        cur.node_mut("a").unwrap().description = "a does other things".into();
        cur.node_mut("b").unwrap().pos = Some([5.0, 5.0]);
        cur.nodes.push(node("d", &[]));
        cur.node_mut("a").unwrap().links.push("d".into());

        let changes = diff(&base, &cur);
        assert_eq!(changes.len(), 3);
        assert!(matches!(&changes[0].kind, ChangeKind::Modified(f) if f == &vec![Field::Description, Field::Links]));
        assert_eq!(changes[1].kind, ChangeKind::Added);
        assert_eq!(changes[1].id, "d");
        assert_eq!(changes[2].kind, ChangeKind::Removed);
        assert_eq!(changes[2].id, "c");
    }

    #[test]
    fn diff_ignores_link_order_and_whitespace() {
        let base = Graph { nodes: vec![node("a", &["b", "c"]), node("b", &[]), node("c", &[])], ..Default::default() };
        let mut cur = base.clone();
        cur.node_mut("a").unwrap().links = vec!["c".into(), "b".into()];
        cur.node_mut("a").unwrap().description.push_str("\n\n");
        assert!(diff(&base, &cur).is_empty());
    }

    #[test]
    fn layout_layers_by_depth_and_survives_cycles() {
        let g = Graph { nodes: vec![node("a", &["b"]), node("b", &["c"]), node("c", &["a"]), node("d", &[])], ..Default::default() };
        let pos = auto_layout(&g);
        assert_eq!(pos.len(), 4);
        let g = Graph { nodes: vec![node("api", &["r1", "r2"]), node("r1", &["svc"]), node("r2", &["svc"]), node("svc", &[])], ..Default::default() };
        let pos = auto_layout(&g);
        assert_eq!(pos["api"][0], 0.0);
        assert_eq!(pos["r1"][0], COL_W);
        assert_eq!(pos["svc"][0], 2.0 * COL_W);
    }

    #[test]
    fn unpinned_nodes_slot_in_next_to_pinned_neighbours() {
        let mut g = Graph { nodes: vec![node("a", &["b", "c"]), node("b", &[]), node("c", &[]), node("d", &["a"])], ..Default::default() };
        g.node_mut("a").unwrap().pos = Some([500.0, 300.0]);
        g.node_mut("b").unwrap().pos = Some([760.0, 300.0]);
        let pos = g.positions();
        assert_eq!(pos["a"], [500.0, 300.0]);
        assert_eq!(pos["c"][0], 760.0);
        assert!((pos["c"][1] - 300.0).abs() >= NODE_H);
        assert_eq!(pos["d"], [240.0, 300.0]);

        let mut lone = g.clone();
        lone.nodes.push(node("e", &[]));
        let e = lone.positions()["e"];
        assert!(e[1] > 300.0 + NODE_H);

        let cleared = g.clear_positions();
        assert_eq!(cleared.len(), 2);
        assert_eq!(g.positions(), auto_layout(&g));
        g.pin_all();
        assert!(g.nodes.iter().all(|n| n.pos.is_some()));
    }

    #[test]
    fn remove_node_cleans_links_and_flows() {
        let mut g = Graph {
            nodes: vec![node("a", &["b"]), node("b", &[])],
            flows: vec![Flow { id: "f".into(), name: "F".into(), nodes: vec!["b".into()], note: String::new() }],
            ..Default::default()
        };
        g.remove_node("b");
        assert!(g.nodes[0].links.is_empty());
        assert!(g.flows.is_empty());
    }

    #[test]
    fn unique_ids() {
        let g = Graph { nodes: vec![node("get-users", &[])], ..Default::default() };
        assert_eq!(g.unique_id("GET /users"), "get-users-2");
        assert_eq!(g.unique_id("Auth Service"), "auth-service");
        assert_eq!(slugify("  "), "node");
    }

    #[test]
    fn parses_minimal_json() {
        let g: Graph = serde_json::from_str(r#"{"nodes":[{"id":"x","title":"X"}]}"#).unwrap();
        assert_eq!(g.nodes[0].kind, "");
        assert!(g.flows.is_empty());
        assert!(g.modules.is_empty());
    }

    fn with_module() -> Graph {
        let mut g = Graph { nodes: vec![node("app", &["gh-a"]), node("gh-a", &[])], ..Default::default() };
        g.node_mut("gh-a").unwrap().module = Some("github".into());
        g.modules.push(Module {
            id: "github".into(),
            title: "GitHub".into(),
            description: "PRs".into(),
            net: Graph { nodes: vec![node("view", &["svc"]), node("svc", &[])], ..Default::default() },
        });
        g
    }

    #[test]
    fn modules_round_trip_through_json() {
        let g = with_module();
        let text = serde_json::to_string(&g).unwrap();
        let back: Graph = serde_json::from_str(&text).unwrap();
        assert_eq!(back, g);
        assert_eq!(back.net(Some("github")).unwrap().nodes.len(), 2);
        assert!(back.net(Some("nope")).is_none());
        let plain = serde_json::to_string(&Graph { nodes: vec![node("x", &[])], ..Default::default() }).unwrap();
        assert!(!plain.contains("modules"));
    }

    #[test]
    fn diff_tracks_module_nodes_and_metadata() {
        let base = with_module();
        let mut cur = base.clone();
        cur.module_mut("github").unwrap().description = "PRs and related repos".into();
        cur.net_mut(Some("github")).unwrap().nodes.push(node("related", &[]));
        let changes = diff(&base, &cur);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].subject, Subject::Module);
        assert!(matches!(&changes[0].kind, ChangeKind::Modified(f) if f == &vec![Field::Description]));
        assert_eq!(changes[1].scope.as_deref(), Some("github"));
        assert_eq!(changes[1].id, "related");

        let mut gone = base.clone();
        gone.remove_module("github");
        let changes = diff(&base, &gone);
        assert!(changes.iter().any(|c| c.subject == Subject::Module && c.kind == ChangeKind::Removed));
        assert!(gone.node("gh-a").is_none());
        assert!(gone.node("app").unwrap().links.is_empty());
    }

    #[test]
    fn references_round_trip_and_diff() {
        let mut base = with_module();
        let mut r = node("shared", &[]);
        r.kind = REFERENCE_KIND.into();
        r.reference = Some(Reference { source: "acme/shared".into(), nodes: vec![] });
        base.net_mut(Some("github")).unwrap().nodes.push(r);
        let text = serde_json::to_string(&base).unwrap();
        let back: Graph = serde_json::from_str(&text).unwrap();
        assert_eq!(back, base);
        assert_eq!(back.references().len(), 1);
        assert_eq!(back.all_nodes().len(), 5);
        assert!(!serde_json::to_string(&node("x", &[])).unwrap().contains("reference"));

        let mut cur = base.clone();
        let picked = RefNode { id: "auth".into(), module: Some("core".into()) };
        cur.net_mut(Some("github")).unwrap().node_mut("shared").unwrap().reference.as_mut().unwrap().nodes.push(picked);
        let changes = diff(&base, &cur);
        assert_eq!(changes.len(), 1);
        assert!(matches!(&changes[0].kind, ChangeKind::Modified(f) if f == &vec![Field::Reference]));
        assert!(cur.net(Some("github")).unwrap().node("shared").unwrap().is_reference());
    }

    #[test]
    fn attaching_a_module_inside_itself_is_refused() {
        let mut g = with_module();
        g.modules.push(Module { id: "outer".into(), title: "Outer".into(), ..Default::default() });
        let mut r = node("uses-gh", &[]);
        r.module = Some("github".into());
        g.net_mut(Some("outer")).unwrap().nodes.push(r);
        assert!(g.can_attach(None, "github"));
        assert!(g.can_attach(Some("outer"), "github"));
        assert!(!g.can_attach(Some("github"), "outer"));
        assert!(!g.can_attach(Some("github"), "github"));
        assert_eq!(g.module_uses("github").len(), 2);
    }

    #[test]
    fn absorb_moves_only_the_given_changes_into_the_baseline() {
        let base = with_module();
        let mut start = base.clone();
        start.node_mut("app").unwrap().description = "user's pending design edit".into();

        let mut cur = start.clone();
        cur.nodes.push(node("cache", &[]));
        cur.node_mut("gh-a").unwrap().links.push("cache".into());
        cur.net_mut(Some("github")).unwrap().remove_node("svc");
        cur.modules.push(Module {
            id: "billing".into(),
            title: "Billing".into(),
            description: "Charges".into(),
            net: Graph { nodes: vec![node("invoice", &[])], ..Default::default() },
        });

        let made = diff(&start, &cur);
        let mut absorbed = base.clone();
        absorb(&mut absorbed, &made, &cur);

        let pending = diff(&absorbed, &cur);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "app");
        assert!(absorbed.node("cache").is_some());
        assert!(absorbed.net(Some("github")).unwrap().node("svc").is_none());
        assert!(absorbed.net(Some("billing")).unwrap().node("invoice").is_some());

        let mut gone = cur.clone();
        gone.remove_module("billing");
        absorb(&mut absorbed, &diff(&cur, &gone), &gone);
        assert!(absorbed.module("billing").is_none());
    }
}
