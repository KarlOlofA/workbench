use std::collections::HashMap;
use std::path::Path;

use crate::git;
use crate::graph::{self, ChangeKind, GRAPH_FILE, Graph, NODE_H, Node, Subject};

pub const COLORS: [&str; 5] = ["#a5d6ff", "#d2a8ff", "#ffa657", "#7ee787", "#f2cc60"];

pub fn color(i: usize) -> &'static str {
    COLORS[i % COLORS.len()]
}

#[derive(Debug, Clone, PartialEq)]
pub struct GhostBranch {
    pub branch: String,
    pub base: Graph,
    pub graph: Graph,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhostPick {
    pub branch: String,
    pub scope: Option<String>,
    pub id: String,
}

fn parse(text: &str, what: &str) -> Result<Graph, String> {
    serde_json::from_str(text).map_err(|e| format!("{what}: {e}"))
}

pub fn load(cwd: &Path, branch: &str) -> GhostBranch {
    let result = (|| -> Result<(Graph, Graph), String> {
        let text = git::show_file(cwd, branch, GRAPH_FILE)?.ok_or("no Workbench graph on this branch")?;
        let g = parse(&text, branch)?;
        let base = match git::merge_base(cwd, "HEAD", branch) {
            Some(rev) => match git::show_file(cwd, &rev, GRAPH_FILE)? {
                Some(t) => parse(&t, "merge base")?,
                None => Graph::default(),
            },
            None => Graph::default(),
        };
        Ok((base, g))
    })();
    match result {
        Ok((base, graph)) => GhostBranch { branch: branch.into(), base, graph, error: None },
        Err(e) => GhostBranch { branch: branch.into(), base: Graph::default(), graph: Graph::default(), error: Some(e) },
    }
}

pub fn key(branch: usize, id: &str) -> String {
    format!("⧉{branch}:{id}")
}

fn one(n: &Node) -> Graph {
    Graph { nodes: vec![n.clone()], ..Default::default() }
}

fn same(a: &Node, b: &Node) -> bool {
    graph::diff(&one(a), &one(b)).is_empty()
}

impl GhostBranch {
    pub fn change(&self, scope: Option<&str>, id: &str) -> Option<ChangeKind> {
        let base = self.base.net(scope).and_then(|n| n.node(id));
        let cur = self.graph.net(scope).and_then(|n| n.node(id));
        match (base, cur) {
            (None, Some(_)) => Some(ChangeKind::Added),
            (Some(_), None) => Some(ChangeKind::Removed),
            (Some(b), Some(c)) => graph::diff(&one(b), &one(c)).into_iter().next().map(|ch| ch.kind),
            (None, None) => None,
        }
    }

    pub fn node(&self, scope: Option<&str>, id: &str) -> Option<&Node> {
        self.graph.net(scope).and_then(|n| n.node(id))
    }

    pub fn ghost_ids(&self, net: &Graph, scope: Option<&str>) -> Vec<String> {
        let Some(bnet) = self.graph.net(scope) else { return Vec::new() };
        graph::diff(&self.base, &self.graph)
            .into_iter()
            .filter(|c| c.subject == Subject::Node && c.scope.as_deref() == scope && c.kind != ChangeKind::Removed)
            .filter(|c| net.node(&c.id).is_none())
            .filter(|c| bnet.node(&c.id).is_some())
            .map(|c| c.id)
            .collect()
    }
}

pub fn describe(kind: &ChangeKind) -> String {
    match kind {
        ChangeKind::Added => "added on that branch".into(),
        ChangeKind::Removed => "removed on that branch".into(),
        ChangeKind::Modified(f) => format!("changed on that branch: {}", f.iter().map(|f| f.label()).collect::<Vec<_>>().join(", ")),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GhostNode {
    pub key: String,
    pub branch: usize,
    pub node: Node,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Mark {
    pub branch: usize,
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Layer {
    pub nodes: Vec<GhostNode>,
    pub edges: Vec<(String, String, usize)>,
    pub marks: HashMap<String, Vec<Mark>>,
}

pub fn layer(net: &Graph, scope: Option<&str>, branches: &[GhostBranch], positions: &mut HashMap<String, [f64; 2]>) -> Layer {
    let mut out = Layer::default();
    for (bi, b) in branches.iter().enumerate() {
        let Some(bnet) = b.graph.net(scope) else { continue };
        let empty = Graph::default();
        let base_net = b.base.net(scope).unwrap_or(&empty);
        let ghosts = b.ghost_ids(net, scope);
        for c in graph::diff(&b.base, &b.graph).into_iter().filter(|c| c.subject == Subject::Node && c.scope.as_deref() == scope) {
            let Some(mine) = net.node(&c.id) else { continue };
            let kind = match (&c.kind, bnet.node(&c.id)) {
                (ChangeKind::Removed, _) => ChangeKind::Removed,
                (_, Some(theirs)) if same(mine, theirs) => continue,
                (ChangeKind::Added, Some(_)) => ChangeKind::Modified(Vec::new()),
                (k, _) => k.clone(),
            };
            out.marks.entry(c.id.clone()).or_default().push(Mark { branch: bi, kind });
        }

        let resolve = |id: &str| -> Option<String> {
            if net.node(id).is_some() {
                Some(id.to_string())
            } else if ghosts.iter().any(|g| g == id) {
                Some(key(bi, id))
            } else {
                None
            }
        };
        let mut order: Vec<&Node> = ghosts.iter().filter_map(|id| bnet.node(id)).collect();
        order.sort_by_key(|n| n.pos.is_none());
        for n in order {
            let near = |ids: Vec<&str>| -> Vec<[f64; 2]> { ids.into_iter().filter_map(|i| resolve(i)).filter_map(|k| positions.get(&k).copied()).collect() };
            let callers = near(bnet.incoming(&n.id).iter().map(|x| x.id.as_str()).collect());
            let callees = near(n.links.iter().map(String::as_str).collect());
            let mean_y = |ps: &[[f64; 2]]| ps.iter().map(|p| p[1]).sum::<f64>() / ps.len() as f64;
            let want = if let Some(p) = n.pos {
                p
            } else if !callers.is_empty() {
                [callers.iter().map(|p| p[0]).fold(f64::MIN, f64::max) + graph::NODE_W + 70.0, mean_y(&callers)]
            } else if !callees.is_empty() {
                [callees.iter().map(|p| p[0]).fold(f64::MAX, f64::min) - graph::NODE_W - 70.0, mean_y(&callees)]
            } else {
                let x0 = positions.values().map(|p| p[0]).fold(f64::MAX, f64::min);
                let y1 = positions.values().map(|p| p[1]).fold(f64::MIN, f64::max);
                if positions.is_empty() { [0.0, 0.0] } else { [x0, y1 + NODE_H * 2.0] }
            };
            let k = key(bi, &n.id);
            let spot = graph::free_spot(want, positions);
            positions.insert(k.clone(), spot);
            out.nodes.push(GhostNode { key: k, branch: bi, node: n.clone() });
        }

        for (from, to) in bnet.edges() {
            if base_net.node(&from).is_some_and(|n| n.links.contains(&to)) || net.node(&from).is_some_and(|n| n.links.contains(&to)) {
                continue;
            }
            if let (Some(a), Some(z)) = (resolve(&from), resolve(&to)) {
                out.edges.push((a, z, bi));
            }
        }
    }
    out
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

    fn graph(nodes: Vec<Node>) -> Graph {
        Graph { nodes, ..Default::default() }
    }

    #[test]
    fn layer_shows_only_the_branch_work() {
        let base = graph(vec![node("api", &["db"]), node("db", &[]), node("old", &[])]);
        let mut theirs = base.clone();
        theirs.remove_node("old");
        theirs.nodes.push(node("cache", &["db"]));
        theirs.node_mut("api").unwrap().links.push("cache".into());
        theirs.node_mut("db").unwrap().description = "db does other things".into();

        let mut mine = base.clone();
        mine.nodes.push(node("mine", &[]));
        let mut positions = mine.positions();
        let b = GhostBranch { branch: "feature".into(), base, graph: theirs, error: None };
        let l = layer(&mine, None, std::slice::from_ref(&b), &mut positions);

        assert_eq!(l.nodes.len(), 1);
        assert_eq!(l.nodes[0].node.id, "cache");
        let k = key(0, "cache");
        assert!(positions.contains_key(&k));
        assert!(l.edges.contains(&("api".into(), k.clone(), 0)));
        assert!(l.edges.contains(&(k, "db".into(), 0)));
        assert_eq!(l.edges.len(), 2);
        assert!(matches!(&l.marks["db"][0].kind, ChangeKind::Modified(f) if !f.is_empty()));
        assert_eq!(l.marks["old"][0].kind, ChangeKind::Removed);
        assert!(!l.marks.contains_key("mine"));
        assert_eq!(b.change(None, "cache"), Some(ChangeKind::Added));
    }

    #[test]
    fn work_already_in_the_working_tree_is_not_ghosted() {
        let base = graph(vec![node("api", &[])]);
        let mut theirs = base.clone();
        theirs.nodes.push(node("cache", &[]));
        let mine = theirs.clone();
        let mut positions = mine.positions();
        let b = GhostBranch { branch: "feature".into(), base, graph: theirs, error: None };
        let l = layer(&mine, None, &[b], &mut positions);
        assert!(l.nodes.is_empty() && l.marks.is_empty() && l.edges.is_empty());
    }
}
