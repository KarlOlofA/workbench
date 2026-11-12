//! Claims: the blast radius of a piece of intent work, derived from git.
//!
//! A claim lists the nodes a graph diff changes, the files those nodes own (plus any
//! code files a branch changed), and the nodes that depend on the changed ones.
//! Nothing is stored: a branch's claim is its `graph.json` diffed against the merge
//! base, so it is visible to everyone as soon as the branch is pushed.

use std::path::Path;

use crate::ghost;
use crate::git;
use crate::graph::{self, Change, Graph, Subject};

/// A node in a graph: (module scope, node id).
pub type NodeKey = (Option<String>, String);

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Claim {
    pub nodes: Vec<NodeKey>,
    pub files: Vec<String>,
    pub dependents: Vec<NodeKey>,
}

/// Another branch's claim, with the people who committed to it.
#[derive(Debug, Clone, PartialEq)]
pub struct BranchClaim {
    pub branch: String,
    pub authors: Vec<String>,
    pub claim: Claim,
}

fn push_unique<T: PartialEq>(v: &mut Vec<T>, item: T) {
    if !v.contains(&item) {
        v.push(item);
    }
}

fn key(c: &Change) -> NodeKey {
    (c.scope.clone(), c.id.clone())
}

impl Claim {
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.files.is_empty() && self.dependents.is_empty()
    }

    /// What two claims contend over: shared nodes, shared files, and nodes one side
    /// changes that the other side depends on.
    pub fn overlap(&self, other: &Claim) -> Claim {
        let shared = |a: &[NodeKey], b: &[NodeKey]| -> Vec<NodeKey> { a.iter().filter(|k| b.contains(k)).cloned().collect() };
        let mut dependents = shared(&self.nodes, &other.dependents);
        for k in shared(&self.dependents, &other.nodes) {
            push_unique(&mut dependents, k);
        }
        Claim {
            nodes: shared(&self.nodes, &other.nodes),
            files: self.files.iter().filter(|f| other.files.contains(f)).cloned().collect(),
            dependents,
        }
    }

    /// Whether `c` is part of what this (overlap) claim covers.
    pub fn touches(&self, c: &Change) -> bool {
        let k = key(c);
        self.nodes.contains(&k) || self.dependents.contains(&k) || c.node().files.iter().any(|f| self.files.contains(f))
    }
}

/// The claim of the intent changes between `base` and `cur`: changed nodes, their files
/// (before and after), and the nodes that link to them in either graph.
pub fn claim(base: &Graph, cur: &Graph, changes: &[Change]) -> Claim {
    let mut out = Claim::default();
    for c in changes {
        out.nodes.push(key(c));
        for n in c.before.iter().chain(c.after.iter()) {
            for f in &n.files {
                push_unique(&mut out.files, f.clone());
            }
        }
    }
    for c in changes.iter().filter(|c| c.subject == Subject::Node) {
        for g in [base, cur] {
            let Some(net) = g.net(c.scope.as_deref()) else { continue };
            for n in net.incoming(&c.id) {
                let k = (c.scope.clone(), n.id.clone());
                if !out.nodes.contains(&k) {
                    push_unique(&mut out.dependents, k);
                }
            }
        }
    }
    out
}

/// The claims of every other branch that has work HEAD doesn't have yet. A remote branch
/// with a local copy is read once, through the local branch. Blocking: runs git.
pub fn open_claims(cwd: &Path) -> Vec<BranchClaim> {
    if !git::is_repo(cwd) {
        return Vec::new();
    }
    let targets = git::switch_targets(cwd).unwrap_or_default();
    let mut out = Vec::new();
    for branch in targets.into_iter().map(|t| t.remote.unwrap_or(t.name)) {
        let authors = git::authors_since_merge_base(cwd, &branch);
        if authors.is_empty() {
            continue;
        }
        let g = ghost::load(cwd, &branch);
        let mut c = claim(&g.base, &g.graph, &graph::diff(&g.base, &g.graph));
        for f in git::changed_since_merge_base(cwd, &branch).into_iter().filter(|f| !f.starts_with(".workbench/")) {
            push_unique(&mut c.files, f);
        }
        if !c.is_empty() {
            out.push(BranchClaim { branch, authors, claim: c });
        }
    }
    out
}

/// The branches whose claims overlap `mine`, with the overlap itself.
pub fn overlaps(mine: &Claim, open: &[BranchClaim]) -> Vec<BranchClaim> {
    open.iter()
        .map(|b| BranchClaim { claim: mine.overlap(&b.claim), ..b.clone() })
        .filter(|b| !b.claim.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Node;

    fn node(id: &str, links: &[&str], files: &[&str]) -> Node {
        Node {
            id: id.into(),
            title: id.into(),
            description: format!("{id} does things"),
            links: links.iter().map(|s| s.to_string()).collect(),
            files: files.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn graph(nodes: Vec<Node>) -> Graph {
        Graph { nodes, ..Default::default() }
    }

    fn base() -> Graph {
        graph(vec![node("api", &["db"], &["src/api.rs"]), node("db", &[], &["src/db.rs"]), node("ui", &[], &["src/ui.rs"])])
    }

    fn edit(id: &str) -> Claim {
        let b = base();
        let mut cur = b.clone();
        cur.node_mut(id).unwrap().description = "new intent".into();
        claim(&b, &cur, &graph::diff(&b, &cur))
    }

    fn k(id: &str) -> NodeKey {
        (None, id.into())
    }

    #[test]
    fn claim_lists_nodes_files_and_dependents() {
        let c = edit("db");
        assert_eq!(c.nodes, vec![k("db")]);
        assert_eq!(c.files, vec!["src/db.rs".to_string()]);
        assert_eq!(c.dependents, vec![k("api")]);
    }

    #[test]
    fn same_node_overlaps() {
        let o = edit("db").overlap(&edit("db"));
        assert_eq!(o.nodes, vec![k("db")]);
        assert_eq!(o.files, vec!["src/db.rs".to_string()]);
    }

    #[test]
    fn editing_a_dependency_overlaps_its_dependents() {
        let o = edit("api").overlap(&edit("db"));
        assert!(o.nodes.is_empty());
        assert_eq!(o.dependents, vec![k("api")]);
    }

    #[test]
    fn shared_files_overlap() {
        let mut theirs = Claim::default();
        theirs.files.push("src/ui.rs".into());
        let o = edit("ui").overlap(&theirs);
        assert_eq!(o.files, vec!["src/ui.rs".to_string()]);
    }

    #[test]
    fn unrelated_work_does_not_overlap() {
        assert!(edit("ui").overlap(&edit("db")).is_empty());
        let open = vec![BranchClaim { branch: "feat".into(), authors: vec!["Anna".into()], claim: edit("db") }];
        assert!(overlaps(&edit("ui"), &open).is_empty());
        assert_eq!(overlaps(&edit("api"), &open).len(), 1);
    }
}
