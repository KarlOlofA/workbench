//! Canonical node naming (PRO-245): nodes in a graph that stand for the same thing carry one
//! name, so intent queries, drift checks and traversal see one concept, and agents don't
//! treat one thing as two targets.
//!
//! Two nodes stand for the same thing only when the graph says so structurally, never by
//! comparing titles:
//! - attachment nodes of the same module (`module`): the module's title is the name;
//! - reference nodes with the same `reference.source` (normalized): the title most of them
//!   use is the name, ties going to the first in graph order.
//!
//! Workbench lists every divergence and offers to rename. It never renames on its own: a
//! rename is a person's edit, recorded in the ledger as an ordinary `node.merge` of `title`.
//! Ids stay as they are, so links, claims and cross-repo refs (which use ids) keep working
//! and no aliases are needed.

use crate::graph::Graph;

/// One thing that several nodes stand for under different names.
#[derive(Debug, Clone, PartialEq)]
pub struct Divergence {
    /// What the nodes stand for, e.g. `module billing` or `reference github.com/acme/api`.
    pub thing: String,
    /// The name every node should carry.
    pub canonical: String,
    /// The nodes (scope, id, title) that carry another name, in graph order.
    pub off: Vec<Member>,
}

/// A node in a divergence: its scope (module id, or None for the top level), id and title.
pub type Member = (Option<String>, String, String);

/// The identity of what a node stands for, if it stands for a shared thing.
fn identity(node: &crate::graph::Node) -> Option<String> {
    if let Some(module) = node.module.as_deref().filter(|m| !m.trim().is_empty()) {
        return Some(format!("module {}", module.trim()));
    }
    let source = node.reference.as_ref().map(|r| normalize_source(&r.source)).filter(|s| !s.is_empty())?;
    Some(format!("reference {source}"))
}

/// A reference source in one spelling: no surrounding space, trailing slash or `.git`, no
/// URL scheme or `www.`, and lower case unless it is a local path (whose case can matter).
pub fn normalize_source(source: &str) -> String {
    let mut s = source.trim().trim_end_matches('/').to_string();
    if let Some(stripped) = s.strip_suffix(".git") {
        s = stripped.to_string();
    }
    let local = s.starts_with(['/', '.', '~']) || s.chars().nth(1) == Some(':');
    if local {
        return s;
    }
    let mut s = s.to_lowercase();
    for prefix in ["https://", "http://", "www."] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
        }
    }
    s
}

/// Every thing in `g` whose nodes carry more than one name, in graph order.
pub fn divergences(g: &Graph) -> Vec<Divergence> {
    let mut groups: Vec<(String, Vec<Member>)> = Vec::new();
    for (scope, node) in g.all_nodes() {
        let Some(thing) = identity(node) else { continue };
        let member = (scope.map(String::from), node.id.clone(), node.title.trim().to_string());
        match groups.iter_mut().find(|(t, _)| *t == thing) {
            Some((_, members)) => members.push(member),
            None => groups.push((thing, vec![member])),
        }
    }
    groups
        .into_iter()
        .filter_map(|(thing, members)| {
            let module_title = thing.strip_prefix("module ").and_then(|id| g.module(id)).map(|m| m.title.trim().to_string());
            let canonical = module_title.filter(|t| !t.is_empty()).unwrap_or_else(|| most_used(&members));
            let off: Vec<_> = members.into_iter().filter(|(_, _, title)| *title != canonical).collect();
            (!off.is_empty()).then_some(Divergence { thing, canonical, off })
        })
        .collect()
}

/// The title most members use; ties go to the one that comes first.
fn most_used(members: &[Member]) -> String {
    let mut best: Option<(&str, usize)> = None;
    for (_, _, title) in members {
        let count = members.iter().filter(|(_, _, t)| t == title).count();
        if best.is_none_or(|(_, c)| count > c) {
            best = Some((title, count));
        }
    }
    best.map(|(t, _)| t.to_string()).unwrap_or_default()
}

/// Gives every node of `d` its canonical name. Returns whether anything changed.
pub fn apply(g: &mut Graph, d: &Divergence) -> bool {
    let mut changed = false;
    for (scope, id, _) in &d.off {
        if let Some(n) = g.net_mut(scope.as_deref()).and_then(|net| net.node_mut(id))
            && n.title != d.canonical
        {
            n.title = d.canonical.clone();
            changed = true;
        }
    }
    changed
}

/// The rule for agents, so the graphs they write keep one name per thing.
pub fn prompt_rule() -> &'static str {
    "Nodes that stand for the same thing share one name: every attachment node of a module is titled exactly like the module, \
     and reference nodes with the same `reference.source` carry the same title. Reuse the existing name when you add one."
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Module, Node, Reference};

    fn attach(id: &str, title: &str, module: &str) -> Node {
        Node { id: id.into(), title: title.into(), kind: "module".into(), module: Some(module.into()), ..Default::default() }
    }

    fn reference(id: &str, title: &str, source: &str) -> Node {
        Node { id: id.into(), title: title.into(), kind: "reference".into(), reference: Some(Reference { source: source.into(), nodes: Vec::new() }), ..Default::default() }
    }

    fn graph() -> Graph {
        let mut g = Graph {
            nodes: vec![attach("pay", "Billing", "billing"), reference("api-a", "Backend", "https://github.com/Acme/api.git"), reference("api-b", "API server", "github.com/acme/api/")],
            ..Default::default()
        };
        g.modules.push(Module {
            id: "billing".into(),
            title: "Billing".into(),
            net: Graph { nodes: vec![reference("api-c", "Backend", "acme/api"), attach("pay2", "Payments", "billing")], ..Default::default() },
            ..Default::default()
        });
        g
    }

    #[test]
    fn module_attachments_take_the_module_title() {
        let d = divergences(&graph());
        let module = d.iter().find(|d| d.thing == "module billing").unwrap();
        assert_eq!(module.canonical, "Billing");
        assert_eq!(module.off, vec![(Some("billing".into()), "pay2".into(), "Payments".into())]);
    }

    #[test]
    fn references_to_one_source_take_the_most_used_title() {
        let d = divergences(&graph());
        let refs = d.iter().find(|d| d.thing == "reference github.com/acme/api").unwrap();
        assert_eq!(refs.canonical, "Backend");
        assert_eq!(refs.off, vec![(None, "api-b".into(), "API server".into())]);
        // `acme/api` is another spelling of a source, not provably the same one: left alone.
        assert!(!d.iter().any(|d| d.thing == "reference acme/api"));
    }

    #[test]
    fn similar_titles_alone_are_not_the_same_thing() {
        let g = Graph { nodes: vec![reference("a", "backend", "npm:a"), reference("b", "Backend", "npm:b")], ..Default::default() };
        assert!(divergences(&g).is_empty());
    }

    #[test]
    fn applying_renames_titles_but_keeps_ids() {
        let mut g = graph();
        for d in divergences(&g) {
            assert!(apply(&mut g, &d));
        }
        assert!(divergences(&g).is_empty());
        assert_eq!(g.module("billing").unwrap().net.node("pay2").unwrap().title, "Billing");
        assert_eq!(g.node("api-b").unwrap().title, "Backend");
    }

    #[test]
    fn local_paths_keep_their_case() {
        assert_eq!(normalize_source(" ../Shared/Lib/ "), "../Shared/Lib");
        assert_eq!(normalize_source("HTTPS://www.Example.com/Docs/"), "example.com/docs");
    }
}
