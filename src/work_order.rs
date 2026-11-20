//! Work orders: the explicit taxonomy of graph diffs. Each change between the baseline and
//! the graph becomes one work order per kind of edit, with its own prompt instruction and a
//! declared blast radius (its claim, see `claims.rs`). After the run, Workbench checks the
//! changed files against the union of the blast radii (see `scope.rs`).

use serde::Serialize;

use crate::claims::{self, BranchClaim, Claim, NodeKey};
use crate::graph::{Change, ChangeKind, Field, Graph, Subject};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// What a node is meant to do changed: its description, title, kind or external reference.
    Description,
    /// What a node calls or depends on changed, including the module it attaches.
    Link,
    /// Which files implement a node changed.
    Files,
    NodeAdd,
    NodeRemove,
}

impl Kind {
    /// The instruction the prompt gives for this kind of work order.
    pub fn instruction(self) -> &'static str {
        match self {
            Kind::Description => {
                "change the node's code so it does what the description now says; adjust the dependents only where they rely on the old behaviour"
            }
            Kind::Link => "add or remove the calls and imports between this node and the linked nodes; the linked nodes' own code stays as it is",
            Kind::Files => "move the node's code so exactly the listed files implement it, without changing what it does",
            Kind::NodeAdd => "implement the new node in its files and wire up the dependents that link to it",
            Kind::NodeRemove => "remove the node's code and keep the dependents compiling and working without it",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkOrder {
    pub kind: Kind,
    pub scope: Option<String>,
    pub subject: Subject,
    pub id: String,
    /// The nodes, files and dependents this work order may touch.
    pub blast_radius: Claim,
    /// Other branches whose claims overlap this work order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contended_by: Vec<String>,
    #[serde(skip)]
    pub change: Change,
}

fn kinds(c: &Change) -> Vec<Kind> {
    let ChangeKind::Modified(fields) = &c.kind else {
        return vec![if c.kind == ChangeKind::Added { Kind::NodeAdd } else { Kind::NodeRemove }];
    };
    let mut out = Vec::new();
    for f in fields {
        let k = match f {
            Field::Title | Field::Kind | Field::Description | Field::Decisions | Field::Invariants | Field::Reference => Kind::Description,
            Field::Links | Field::Module => Kind::Link,
            Field::Files => Kind::Files,
        };
        if !out.contains(&k) {
            out.push(k);
        }
    }
    out
}

/// The work orders for `changes` (between `base` and `cur`), given the other branches' open claims.
pub fn from_changes(base: &Graph, cur: &Graph, changes: &[Change], open: &[BranchClaim]) -> Vec<WorkOrder> {
    let mut out = Vec::new();
    for c in changes {
        let claim = claims::claim(base, cur, std::slice::from_ref(c));
        for kind in kinds(c) {
            let mut blast_radius = claim.clone();
            // Link and file work stays inside the node itself.
            if matches!(kind, Kind::Link | Kind::Files) {
                blast_radius.dependents.clear();
            }
            let contended_by = open.iter().filter(|b| !blast_radius.overlap(&b.claim).is_empty()).map(|b| b.branch.clone()).collect();
            out.push(WorkOrder {
                kind,
                scope: c.scope.clone(),
                subject: c.subject,
                id: c.id.clone(),
                blast_radius,
                contended_by,
                change: c.clone(),
            });
        }
    }
    out
}

/// The changes behind `orders`, each once, in order.
pub fn changes(orders: &[WorkOrder]) -> Vec<Change> {
    let mut out: Vec<Change> = Vec::new();
    for o in orders {
        if !out.iter().any(|c| c.key() == o.change.key()) {
            out.push(o.change.clone());
        }
    }
    out
}

/// The nodes a run on `orders` may touch: the union of their blast radii.
pub fn scope_nodes(orders: &[WorkOrder]) -> Vec<NodeKey> {
    let mut out: Vec<NodeKey> = Vec::new();
    for k in orders.iter().flat_map(|o| o.blast_radius.nodes.iter().chain(&o.blast_radius.dependents)) {
        if !out.contains(k) {
            out.push(k.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{self, Node};

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

    #[test]
    fn a_decision_edit_is_an_intent_change() {
        let base = Graph { nodes: vec![node("api", &[], &["src/api.rs"])], ..Default::default() };
        let mut cur = base.clone();
        cur.node_mut("api").unwrap().decisions.push(crate::graph::Decision { decision: "Cache reads".into(), ..Default::default() });
        let changes = graph::diff(&base, &cur);
        assert_eq!(kinds(&changes[0]), vec![Kind::Description]);
    }

    #[test]
    fn each_kind_of_edit_gets_its_own_work_order_and_blast_radius() {
        let base = Graph {
            nodes: vec![node("api", &["db"], &["src/api.rs"]), node("db", &[], &["src/db.rs"]), node("ui", &["api"], &["src/ui.rs"])],
            ..Default::default()
        };
        let mut cur = base.clone();
        cur.node_mut("api").unwrap().description = "new intent".into();
        cur.node_mut("api").unwrap().links.clear();
        cur.node_mut("db").unwrap().files = vec!["src/store.rs".into()];
        cur.nodes.push(node("cache", &[], &["src/cache.rs"]));
        let open = vec![BranchClaim { branch: "feat".into(), authors: vec![], claim: Claim { files: vec!["src/db.rs".into()], ..Default::default() } }];
        let orders = from_changes(&base, &cur, &graph::diff(&base, &cur), &open);

        let kinds: Vec<(Kind, &str)> = orders.iter().map(|o| (o.kind, o.id.as_str())).collect();
        assert_eq!(kinds, vec![(Kind::Description, "api"), (Kind::Link, "api"), (Kind::Files, "db"), (Kind::NodeAdd, "cache")]);
        // Intent work may reach the callers; link work stays in the node.
        assert_eq!(orders[0].blast_radius.dependents, vec![(None, "ui".to_string())]);
        assert!(orders[1].blast_radius.dependents.is_empty());
        assert_eq!(orders[2].blast_radius.files, vec!["src/db.rs".to_string(), "src/store.rs".to_string()]);
        assert_eq!(orders[2].contended_by, vec!["feat".to_string()]);
        assert_eq!(changes(&orders).len(), 3);

        let json = serde_json::to_string(&orders[0]).unwrap();
        assert!(json.contains(r#""kind":"description""#) && json.contains(r#""blast_radius""#) && !json.contains("change"));
    }
}
