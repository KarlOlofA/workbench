//! Three-way merge of `graph.json` at node level, for intent-conflict review.
//!
//! Intent changed on one side only is taken from that side. A node or module whose
//! intent both sides changed differently is a conflict a person must resolve; it is
//! never merged automatically. Layout (`pos`) and `files` follow our side.

use std::path::Path;

use crate::git;
use crate::graph::{self, Change, ChangeKind, Graph, Node, Subject};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Base,
    Ours,
    Theirs,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    pub scope: Option<String>,
    pub subject: Subject,
    pub id: String,
    pub base: Option<Node>,
    pub ours: Option<Node>,
    pub theirs: Option<Node>,
}

impl Conflict {
    pub fn side(&self, side: Side) -> Option<&Node> {
        match side {
            Side::Base => self.base.as_ref(),
            Side::Ours => self.ours.as_ref(),
            Side::Theirs => self.theirs.as_ref(),
        }
    }

    pub fn title(&self) -> String {
        self.ours.as_ref().or(self.theirs.as_ref()).or(self.base.as_ref()).map(|n| n.title.clone()).unwrap_or_else(|| self.id.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Merge {
    /// Our graph with their one-sided changes applied; conflicts still hold our version.
    pub graph: Graph,
    pub conflicts: Vec<Conflict>,
}

fn same_intent(a: &Change, b: &Change) -> bool {
    match (&a.after, &b.after) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            let one = |n: &Node| Graph { nodes: vec![n.clone()], ..Default::default() };
            graph::diff(&one(x), &one(y)).is_empty()
        }
        _ => false,
    }
}

pub fn merge(base: &Graph, ours: &Graph, theirs: &Graph) -> Merge {
    let mine = graph::diff(base, ours);
    let mut out = Merge { graph: ours.clone(), conflicts: Vec::new() };
    for c in graph::diff(base, theirs) {
        match mine.iter().find(|m| m.key() == c.key()) {
            None => graph::absorb(&mut out.graph, std::slice::from_ref(&c), theirs),
            Some(m) if same_intent(m, &c) => {}
            Some(m) => out.conflicts.push(Conflict {
                scope: c.scope.clone(),
                subject: c.subject,
                id: c.id.clone(),
                base: c.before.clone(),
                ours: m.after.clone(),
                theirs: c.after.clone(),
            }),
        }
    }
    for f in &theirs.flows {
        if !out.graph.flows.iter().any(|x| x.id == f.id) {
            out.graph.flows.push(f.clone());
        }
    }
    out
}

/// Puts `side`'s version of the conflicting node or module into `g`.
pub fn resolve(g: &mut Graph, c: &Conflict, side: Side) {
    let chosen = c.side(side).cloned();
    let kind = if chosen.is_some() { ChangeKind::Added } else { ChangeKind::Removed };
    let change = Change { scope: c.scope.clone(), subject: c.subject, id: c.id.clone(), kind, before: None, after: chosen.clone() };
    let mut source = Graph::default();
    match (c.subject, &chosen) {
        (Subject::Module, Some(n)) => {
            source.modules.push(graph::Module { id: n.id.clone(), title: n.title.clone(), description: n.description.clone(), net: Graph::default() });
        }
        (Subject::Node, Some(n)) => match &c.scope {
            Some(m) => source.modules.push(graph::Module { id: m.clone(), net: Graph { nodes: vec![n.clone()], ..Default::default() }, ..Default::default() }),
            None => source.nodes.push(n.clone()),
        },
        (_, None) => {}
    }
    // Keep our layout for a node both sides still have.
    let pos = g.net(c.scope.as_deref()).and_then(|n| n.node(&c.id)).and_then(|n| n.pos);
    graph::absorb(g, &[change], &source);
    if let Some(n) = g.net_mut(c.scope.as_deref()).and_then(|n| n.node_mut(&c.id)) {
        n.pos = n.pos.or(pos);
    }
}

fn stage(cwd: &Path, n: u8, path: &str) -> Result<Graph, String> {
    match git::show_file(cwd, &format!(":{n}"), path)? {
        Some(text) => serde_json::from_str(&text).map_err(|e| format!("{path} (merge stage {n}): {e}")),
        None => Ok(Graph::default()),
    }
}

/// Merges the three git merge stages of `path` (base, ours, theirs). Blocking: runs git.
pub fn load(cwd: &Path, path: &str) -> Result<Merge, String> {
    Ok(merge(&stage(cwd, 1, path)?, &stage(cwd, 2, path)?, &stage(cwd, 3, path)?))
}

/// The baseline records what the code implements. For a conflicting node, the merged code
/// matches neither side for sure, so the baseline goes back to the common base and the node
/// shows up as pending work.
pub fn resolve_baseline(cwd: &Path) -> Result<Option<Graph>, String> {
    if !git::unmerged(cwd, graph::BASELINE_FILE) {
        return Ok(None);
    }
    let mut m = load(cwd, graph::BASELINE_FILE)?;
    for c in m.conflicts.clone() {
        resolve(&mut m.graph, &c, Side::Base);
    }
    Ok(Some(m.graph))
}

/// A commit message that marks the merge as an intent resolution.
pub fn commit_message(conflicts: &[Conflict], choices: &[Side]) -> String {
    let lines: Vec<String> = conflicts
        .iter()
        .zip(choices)
        .map(|(c, s)| format!("- {}: kept {}", c.title(), if *s == Side::Ours { "mine" } else { "theirs" }))
        .collect();
    let ids: Vec<&str> = conflicts.iter().map(|c| c.id.as_str()).collect();
    format!("Resolve intent conflicts in the architecture graph\n\n{}\n\nIntent-Resolution: {}", lines.join("\n"), ids.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, desc: &str) -> Node {
        Node { id: id.into(), title: id.into(), description: desc.into(), ..Default::default() }
    }

    fn graph(nodes: Vec<Node>) -> Graph {
        Graph { nodes, ..Default::default() }
    }

    fn base() -> Graph {
        graph(vec![node("api", "serves"), node("db", "stores"), node("ui", "shows")])
    }

    #[test]
    fn one_sided_changes_merge_without_conflicts() {
        let mut ours = base();
        ours.node_mut("api").unwrap().description = "serves fast".into();
        let mut theirs = base();
        theirs.node_mut("db").unwrap().description = "stores less".into();
        theirs.nodes.push(node("cache", "caches"));
        theirs.remove_node("ui");

        let m = merge(&base(), &ours, &theirs);
        assert!(m.conflicts.is_empty());
        assert_eq!(m.graph.node("api").unwrap().description, "serves fast");
        assert_eq!(m.graph.node("db").unwrap().description, "stores less");
        assert!(m.graph.node("cache").is_some());
        assert!(m.graph.node("ui").is_none());
    }

    #[test]
    fn the_same_edit_on_both_sides_is_not_a_conflict() {
        let mut ours = base();
        ours.node_mut("db").unwrap().description = "stores less".into();
        assert!(merge(&base(), &ours, &ours.clone()).conflicts.is_empty());
    }

    #[test]
    fn different_descriptions_conflict_and_resolve_by_choice() {
        let mut ours = base();
        ours.node_mut("db").unwrap().description = "stores more".into();
        ours.node_mut("db").unwrap().pos = Some([1.0, 2.0]);
        let mut theirs = base();
        theirs.node_mut("db").unwrap().description = "stores less".into();

        let mut m = merge(&base(), &ours, &theirs);
        assert_eq!(m.conflicts.len(), 1);
        let c = m.conflicts[0].clone();
        assert_eq!(c.id, "db");
        resolve(&mut m.graph, &c, Side::Theirs);
        let db = m.graph.node("db").unwrap();
        assert_eq!(db.description, "stores less");
        assert_eq!(db.pos, Some([1.0, 2.0]));
        assert!(commit_message(&m.conflicts, &[Side::Theirs]).ends_with("Intent-Resolution: db"));
    }

    /// Two branches edit the same node: the claim shows up before merging, and the merge
    /// conflict loads as one intent conflict.
    #[test]
    fn claims_and_conflicts_from_a_real_repo() {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("wb-intent-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".workbench")).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git").args(["-c", "user.name=Anna", "-c", "user.email=a@x", "-c", "init.defaultBranch=main"]).args(args).current_dir(&dir).output().unwrap();
            assert!(out.status.success() || args[0] == "merge", "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        let write = |g: &Graph| graph::save(&dir, graph::GRAPH_FILE, g).unwrap();
        let mut g = base();
        g.node_mut("db").unwrap().files = vec!["src/db.rs".into()];
        git(&["init", "-q"]);
        write(&g);
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        git(&["switch", "-qc", "feat"]);
        g.node_mut("db").unwrap().description = "stores less".into();
        write(&g);
        git(&["commit", "-qam", "theirs"]);
        git(&["switch", "-q", "main"]);
        let mut mine = base();
        mine.node_mut("db").unwrap().description = "stores more".into();
        write(&mine);
        git(&["commit", "-qam", "mine"]);

        let open = crate::claims::open_claims(&dir);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].branch, "feat");
        assert_eq!(open[0].authors, vec!["Anna".to_string()]);
        assert!(open[0].claim.nodes.contains(&(None, "db".into())));

        git(&["merge", "-q", "feat"]);
        assert!(git::unmerged(&dir, graph::GRAPH_FILE));
        let m = load(&dir, graph::GRAPH_FILE).unwrap();
        assert_eq!(m.conflicts.len(), 1);
        assert_eq!(m.conflicts[0].theirs.as_ref().unwrap().description, "stores less");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_against_removal_conflicts() {
        let mut ours = base();
        ours.node_mut("ui").unwrap().description = "shows more".into();
        let mut theirs = base();
        theirs.remove_node("ui");

        let mut m = merge(&base(), &ours, &theirs);
        assert_eq!(m.conflicts.len(), 1);
        let c = m.conflicts[0].clone();
        assert!(c.theirs.is_none());
        resolve(&mut m.graph, &c, Side::Theirs);
        assert!(m.graph.node("ui").is_none());
        resolve(&mut m.graph, &c, Side::Base);
        assert_eq!(m.graph.node("ui").unwrap().description, "shows");
    }
}
