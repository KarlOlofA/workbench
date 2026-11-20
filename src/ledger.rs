//! The intent ledger: `.workbench/ledger.ndjson`, an append-only log of graph operations
//! and the source of truth for the graph. `graph.json` is the ledger materialized, plus
//! the node positions (layout is not intent, so it stays out of the ledger).
//!
//! The schema and the closed op set are frozen in `docs/ledger-spec.md`.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::graph::{self, ChangeKind, Flow, Graph, Module, Node, Subject};

pub const LEDGER_FILE: &str = ".workbench/ledger.ndjson";

/// Who made an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Person,
    Agent,
    Outside,
}

/// One immutable line of the ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// A ULID: unique, and sorting by it sorts by creation time.
    pub id: String,
    /// Seconds since the Unix epoch.
    pub at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub source: Source,
    /// A suggested change: left out of the graph until a person accepts it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub proposed: bool,
    /// The proposal this entry accepts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolves: Option<String>,
    #[serde(flatten)]
    pub op: Op,
}

/// The closed op set. Node and flow ids are written as `target_id`, since `id` is the
/// entry's own ULID. `scope` is the module whose network the op applies to (None: the
/// top-level graph). Ops on modules themselves use `subject: module` and no scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Op {
    /// A new node (without links or position) or module (title and description only).
    #[serde(rename = "node.add")]
    NodeAdd {
        #[serde(default)]
        scope: Option<String>,
        subject: Subject,
        node: Node,
    },
    /// Sets the given fields of a node or module; a null value clears the field.
    #[serde(rename = "node.merge")]
    NodeMerge {
        #[serde(default)]
        scope: Option<String>,
        subject: Subject,
        #[serde(rename = "target_id")]
        id: String,
        patch: Map<String, Value>,
    },
    #[serde(rename = "node.remove")]
    NodeRemove {
        #[serde(default)]
        scope: Option<String>,
        subject: Subject,
        #[serde(rename = "target_id")]
        id: String,
    },
    /// One link: `from` calls or depends on `to`. Removed by a tombstone.
    #[serde(rename = "ref.add")]
    RefAdd {
        #[serde(default)]
        scope: Option<String>,
        from: String,
        to: String,
    },
    /// Adds a flow or replaces the flow with the same id.
    #[serde(rename = "flow.put")]
    FlowPut {
        #[serde(default)]
        scope: Option<String>,
        flow: Flow,
    },
    #[serde(rename = "flow.remove")]
    FlowRemove {
        #[serde(default)]
        scope: Option<String>,
        #[serde(rename = "target_id")]
        id: String,
    },
    /// Voids an earlier entry: removes a link or rejects a proposal.
    #[serde(rename = "tombstone")]
    Tombstone { target: String },
}

static IDS: Mutex<ulid::Generator> = Mutex::new(ulid::Generator::new());

fn new_id() -> String {
    let mut ids = IDS.lock().unwrap_or_else(|e| e.into_inner());
    ids.generate().unwrap_or_else(|_| ulid::Ulid::generate()).to_string()
}

/// Turns ops into entries with fresh ids, in order.
pub fn stamp(ops: Vec<Op>, source: Source, author: Option<&str>, at: u64) -> Vec<Entry> {
    ops.into_iter()
        .map(|op| Entry { id: new_id(), at, author: author.map(String::from), source, proposed: false, resolves: None, op })
        .collect()
}

pub fn load(cwd: &Path) -> Result<Vec<Entry>, String> {
    match std::fs::read_to_string(cwd.join(LEDGER_FILE)) {
        Ok(text) => parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{LEDGER_FILE}: {e}")),
    }
}

/// Parses the ledger, sorted by id. Any line that isn't a known op is an error: the op set
/// is closed, so an unknown op means a newer schema this build can't apply.
pub fn parse(text: &str) -> Result<Vec<Entry>, String> {
    let mut out: Vec<Entry> = Vec::new();
    for (i, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        out.push(serde_json::from_str(line).map_err(|e| format!("{LEDGER_FILE} line {}: {e}", i + 1))?);
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    // A git union merge can keep the same line from both sides.
    out.dedup_by(|a, b| a.id == b.id);
    Ok(out)
}

pub fn append(cwd: &Path, entries: &[Entry]) -> Result<(), String> {
    let path = cwd.join(LEDGER_FILE);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    crate::node_history::ensure_attribute(cwd, "ledger.ndjson merge=union")?;
    let mut text = String::new();
    for e in entries {
        text.push_str(&serde_json::to_string(e).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| format!("{LEDGER_FILE}: {e}"))?;
    file.write_all(text.as_bytes()).map_err(|e| format!("{LEDGER_FILE}: {e}"))
}

/// Appends the ops that bring the ledger on disk in line with `g`, and returns the whole
/// ledger. The ledger is read from disk each time, so entries that a git pull or another
/// Workbench window added count too.
///
/// Agents don't own intent: the description part of an agent's edit becomes a proposal, and so
/// do changes to decisions and invariants other than appending to them.
/// While that proposal is open, the same text in a later edit is still the agent's, so it is
/// not recorded as anyone else's change.
pub fn record(cwd: &Path, known: &[Entry], g: &Graph, source: Source, author: Option<&str>, at: u64) -> Result<Vec<Entry>, String> {
    let mut entries = load_checked(cwd, known)?;
    let base = materialize(&entries, g);
    let (ops, intent) = split_intent(&base, ops_for(&entries, &base, g));
    let open: Vec<Op> = open_proposals(&entries).into_iter().map(|e| e.op.clone()).collect();
    let intent: Vec<Op> = intent.into_iter().filter(|op| !open.contains(op)).collect();
    let mut new = stamp(ops, source, author, at);
    if source == Source::Agent {
        let mut proposals = stamp(intent, source, author, at);
        proposals.iter_mut().for_each(|e| e.proposed = true);
        new.extend(proposals);
    } else {
        new.extend(stamp(intent, source, author, at));
    }
    if !new.is_empty() {
        append(cwd, &new)?;
        entries.extend(new);
    }
    Ok(entries)
}

/// Moves the intent part of every `node.merge` into its own `node.merge`: the description,
/// and the decisions and invariants unless the merge only appends to them. Appending records
/// what happened; rewriting or dropping what was decided changes intent.
fn split_intent(base: &Graph, ops: Vec<Op>) -> (Vec<Op>, Vec<Op>) {
    let mut rest = Vec::new();
    let mut intent = Vec::new();
    for op in ops {
        let Op::NodeMerge { scope, subject, id, mut patch } = op else {
            rest.push(op);
            continue;
        };
        let old = merge_base(base, scope.as_deref(), subject, &id).map(|n| fields(&n)).unwrap_or_default();
        let mut moved = Map::new();
        for key in ["description", "decisions", "invariants"] {
            let Some(new) = patch.get(key) else { continue };
            if key == "description" || !only_appends(old.get(key), new) {
                moved.insert(key.to_string(), patch.remove(key).unwrap_or_default());
            }
        }
        if !moved.is_empty() {
            intent.push(Op::NodeMerge { scope: scope.clone(), subject, id: id.clone(), patch: moved });
        }
        if !patch.is_empty() {
            rest.push(Op::NodeMerge { scope, subject, id, patch });
        }
    }
    (rest, intent)
}

/// The node or module a merge applies to, as it is before the merge.
fn merge_base(base: &Graph, scope: Option<&str>, subject: Subject, id: &str) -> Option<Node> {
    match subject {
        Subject::Module => base.module(id).map(Module::as_node),
        Subject::Node => base.net(scope).and_then(|net| net.node(id)).cloned(),
    }
}

/// Whether `new` is the list `old` (missing means empty) with entries added at the end.
fn only_appends(old: Option<&Value>, new: &Value) -> bool {
    let old = old.and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    new.as_array().is_some_and(|new| new.len() > old.len() && new.starts_with(old))
}

/// The node or module a `node.merge` changes.
pub fn merge_target(op: &Op) -> Option<(Option<&str>, Subject, &str)> {
    match op {
        Op::NodeMerge { scope, subject, id, .. } => Some((scope.as_deref(), *subject, id.as_str())),
        _ => None,
    }
}

/// What a person's verdict on a proposal appends. Accepting applies its patch, in an entry
/// that resolves it. Every other open proposal for the same node is rejected with a tombstone,
/// and so is the proposal itself when it is not accepted.
pub fn verdict(entries: &[Entry], proposal: &str, accept: bool, author: Option<&str>, at: u64) -> Vec<Entry> {
    let open = open_proposals(entries);
    let Some(target) = open.iter().find(|e| e.id == proposal).and_then(|e| merge_target(&e.op)) else { return Vec::new() };
    let mut out = Vec::new();
    for e in open.iter().filter(|e| merge_target(&e.op) == Some(target)) {
        if accept && e.id == proposal {
            let mut accepted = stamp(vec![e.op.clone()], Source::Person, author, at);
            accepted[0].resolves = Some(e.id.clone());
            out.extend(accepted);
        } else {
            out.extend(stamp(vec![Op::Tombstone { target: e.id.clone() }], Source::Person, author, at));
        }
    }
    out
}

/// Checks that the ledger only grew: every entry in `known` (what was read before) is still
/// there, unchanged. Removing something from the graph appends a `node.remove` or a
/// `tombstone`; it never deletes or edits a line. A git union merge only adds lines, so it
/// passes; a rewrite, a rebase that drops lines or a hand edit fails.
pub fn extends(known: &[Entry], now: &[Entry]) -> Result<(), String> {
    let now: std::collections::HashMap<&str, &Entry> = now.iter().map(|e| (e.id.as_str(), e)).collect();
    for e in known {
        match now.get(e.id.as_str()) {
            Some(n) if *n == e => {}
            Some(_) => return Err(format!("{LEDGER_FILE} was rewritten: entry {} changed. The ledger is append-only; restore it from git.", e.id)),
            None => return Err(format!("{LEDGER_FILE} was rewritten: entry {} is gone. The ledger is append-only; restore it from git.", e.id)),
        }
    }
    Ok(())
}

/// Loads the ledger and checks that it extends `known`.
pub fn load_checked(cwd: &Path, known: &[Entry]) -> Result<Vec<Entry>, String> {
    let now = load(cwd)?;
    extends(known, &now)?;
    Ok(now)
}

fn tombstoned(entries: &[Entry]) -> HashSet<&str> {
    entries
        .iter()
        .filter_map(|e| match &e.op {
            Op::Tombstone { target } => Some(target.as_str()),
            _ => None,
        })
        .collect()
}

/// Proposals nobody has accepted or rejected yet.
pub fn open_proposals(entries: &[Entry]) -> Vec<&Entry> {
    let dead = tombstoned(entries);
    let resolved: HashSet<&str> = entries.iter().filter_map(|e| e.resolves.as_deref()).collect();
    entries.iter().filter(|e| e.proposed && !dead.contains(e.id.as_str()) && !resolved.contains(e.id.as_str())).collect()
}

/// The graph the ledger describes. Positions come from `layout` (the current graph.json).
pub fn materialize(entries: &[Entry], layout: &Graph) -> Graph {
    let dead = tombstoned(entries);
    let mut g = Graph::default();
    for e in entries.iter().filter(|e| !e.proposed && !dead.contains(e.id.as_str())) {
        apply(&mut g, &e.op);
    }
    let pos = |scope: Option<&str>, id: &str| layout.net(scope).and_then(|n| n.node(id)).and_then(|n| n.pos);
    for n in &mut g.nodes {
        n.pos = pos(None, &n.id);
    }
    for m in &mut g.modules {
        for n in &mut m.net.nodes {
            n.pos = pos(Some(&m.id), &n.id);
        }
    }
    g
}

fn fields(n: &Node) -> Map<String, Value> {
    match serde_json::to_value(n) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

/// The node with a `node.merge` patch applied; a null value clears the field.
pub fn with_patch(n: &Node, patch: &Map<String, Value>) -> Node {
    let mut f = fields(n);
    for (k, v) in patch {
        if v.is_null() {
            f.remove(k);
        } else {
            f.insert(k.clone(), v.clone());
        }
    }
    serde_json::from_value(Value::Object(f)).unwrap_or_else(|_| n.clone())
}

fn apply(g: &mut Graph, op: &Op) {
    match op {
        Op::NodeAdd { subject: Subject::Module, node, .. } => {
            if g.module(&node.id).is_none() {
                g.modules.push(Module {
                    id: node.id.clone(),
                    title: node.title.clone(),
                    description: node.description.clone(),
                    decisions: node.decisions.clone(),
                    invariants: node.invariants.clone(),
                    net: Graph::default(),
                });
            }
        }
        Op::NodeAdd { scope, node, .. } => {
            let Some(net) = g.net_mut(scope.as_deref()) else { return };
            match net.node_mut(&node.id) {
                Some(n) => *n = node.clone(),
                None => net.nodes.push(node.clone()),
            }
        }
        Op::NodeMerge { subject: Subject::Module, id, patch, .. } => {
            let Some(m) = g.module_mut(id) else { return };
            let text = |key: &str| patch.get(key).map(|v| v.as_str().unwrap_or_default().to_string());
            if let Some(t) = text("title") {
                m.title = t;
            }
            if let Some(d) = text("description") {
                m.description = d;
            }
            if let Some(v) = patch.get("decisions") {
                m.decisions = serde_json::from_value(v.clone()).unwrap_or_default();
            }
            if let Some(v) = patch.get("invariants") {
                m.invariants = serde_json::from_value(v.clone()).unwrap_or_default();
            }
        }
        Op::NodeMerge { scope, id, patch, .. } => {
            let Some(n) = g.net_mut(scope.as_deref()).and_then(|net| net.node_mut(id)) else { return };
            *n = with_patch(n, patch);
        }
        Op::NodeRemove { subject: Subject::Module, id, .. } => g.modules.retain(|m| m.id != *id),
        Op::NodeRemove { scope, id, .. } => {
            if let Some(net) = g.net_mut(scope.as_deref()) {
                net.nodes.retain(|n| n.id != *id);
            }
        }
        Op::RefAdd { scope, from, to } => {
            if let Some(n) = g.net_mut(scope.as_deref()).and_then(|net| net.node_mut(from))
                && !n.links.contains(to)
            {
                n.links.push(to.clone());
            }
        }
        Op::FlowPut { scope, flow } => {
            let Some(net) = g.net_mut(scope.as_deref()) else { return };
            match net.flows.iter_mut().find(|f| f.id == flow.id) {
                Some(f) => *f = flow.clone(),
                None => net.flows.push(flow.clone()),
            }
        }
        Op::FlowRemove { scope, id } => {
            if let Some(net) = g.net_mut(scope.as_deref()) {
                net.flows.retain(|f| f.id != *id);
            }
        }
        Op::Tombstone { .. } => {}
    }
}

/// A node as the ledger stores it: no links (those are `ref.add` ops) and no position.
/// A module only keeps its id, title and description.
fn bare(n: &Node, subject: Subject) -> Node {
    let mut n = Node { links: Vec::new(), pos: None, ..n.clone() };
    if subject == Subject::Module {
        n.files.clear();
        n.kind.clear();
    }
    n
}

/// The fields that differ between two nodes, with their new value (null when cleared).
pub fn patch(before: &Node, after: &Node) -> Map<String, Value> {
    let (b, a) = (fields(before), fields(after));
    let mut out = Map::new();
    for k in b.keys().chain(a.keys()) {
        if b.get(k) != a.get(k) {
            out.insert(k.clone(), a.get(k).cloned().unwrap_or(Value::Null));
        }
    }
    out
}

/// The live `ref.add` entry for a link, so a tombstone can remove it.
fn live_ref<'a>(entries: &'a [Entry], scope: &Option<String>, from: &str, to: &str) -> Option<&'a str> {
    let dead = tombstoned(entries);
    entries
        .iter()
        .rev()
        .filter(|e| !dead.contains(e.id.as_str()))
        .find(|e| matches!(&e.op, Op::RefAdd { scope: s, from: f, to: t } if s == scope && f == from && t == to))
        .map(|e| e.id.as_str())
}

/// The ops that turn `base` (the materialized ledger) into `cur`. `entries` is the ledger,
/// used to find the `ref.add` entries that removed links tombstone.
pub fn ops_for(entries: &[Entry], base: &Graph, cur: &Graph) -> Vec<Op> {
    let mut out = Vec::new();
    for c in graph::diff(base, cur) {
        let subject = c.subject;
        let scope = if subject == Subject::Module { None } else { c.scope.clone() };
        let id = c.id.clone();
        match (&c.kind, &c.before, &c.after) {
            (ChangeKind::Added, _, Some(after)) => {
                out.push(Op::NodeAdd { scope: scope.clone(), subject, node: bare(after, subject) });
                for to in &after.links {
                    out.push(Op::RefAdd { scope: scope.clone(), from: id.clone(), to: to.clone() });
                }
            }
            (ChangeKind::Removed, _, _) => out.push(Op::NodeRemove { scope, subject, id }),
            (ChangeKind::Modified(_), Some(before), Some(after)) => {
                let p = patch(&bare(before, subject), &bare(after, subject));
                if !p.is_empty() {
                    out.push(Op::NodeMerge { scope: scope.clone(), subject, id: id.clone(), patch: p });
                }
                for to in after.links.iter().filter(|l| !before.links.contains(l)) {
                    out.push(Op::RefAdd { scope: scope.clone(), from: id.clone(), to: to.clone() });
                }
                for to in before.links.iter().filter(|l| !after.links.contains(l)) {
                    if let Some(target) = live_ref(entries, &scope, &id, to) {
                        out.push(Op::Tombstone { target: target.to_string() });
                    }
                }
            }
            _ => {}
        }
    }
    let empty = Graph::default();
    for (scope, net) in cur.nets() {
        let old = base.net(scope).unwrap_or(&empty);
        let scope = scope.map(String::from);
        for f in &net.flows {
            if old.flows.iter().find(|o| o.id == f.id) != Some(f) {
                out.push(Op::FlowPut { scope: scope.clone(), flow: f.clone() });
            }
        }
        for f in old.flows.iter().filter(|o| !net.flows.iter().any(|f| f.id == o.id)) {
            out.push(Op::FlowRemove { scope: scope.clone(), id: f.id.clone() });
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
            title: id.into(),
            kind: "service".into(),
            description: format!("{id} does things"),
            files: vec![format!("src/{id}.rs")],
            links: links.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn sample() -> Graph {
        let mut g = Graph { nodes: vec![node("api", &["db"]), node("db", &[])], ..Default::default() };
        g.nodes[0].pos = Some([10.0, 20.0]);
        g.flows.push(Flow { id: "read".into(), name: "Read".into(), nodes: vec!["api".into(), "db".into()], note: String::new() });
        g.modules.push(Module {
            id: "billing".into(),
            title: "Billing".into(),
            description: "Charges customers".into(),
            net: Graph { nodes: vec![node("invoice", &[])], ..Default::default() },
            ..Default::default()
        });
        g
    }

    /// Records the ops that turn the materialized ledger into `cur`, like Workbench does.
    fn ingest(entries: &mut Vec<Entry>, cur: &Graph) {
        let base = materialize(entries, cur);
        let ops = ops_for(entries, &base, cur);
        entries.extend(stamp(ops, Source::Person, None, 0));
    }

    #[test]
    fn genesis_round_trips_the_graph() {
        let g = sample();
        let mut entries = Vec::new();
        ingest(&mut entries, &g);
        assert_eq!(materialize(&entries, &g), g);
        let text: String = entries.iter().map(|e| serde_json::to_string(e).unwrap() + "\n").collect();
        assert_eq!(parse(&text).unwrap(), entries);
    }

    #[test]
    fn edits_append_ops_and_replay() {
        let g = sample();
        let mut entries = Vec::new();
        ingest(&mut entries, &g);
        let n = entries.len();

        let mut cur = g.clone();
        cur.nodes[0].description = "new intent".into();
        cur.nodes[0].links.clear();
        cur.nodes[1].files = vec!["src/store.rs".into()];
        cur.flows.clear();
        cur.modules[0].title = "Payments".into();
        cur.nodes.push(node("cache", &["db"]));
        ingest(&mut entries, &cur);

        assert!(entries.len() > n);
        assert!(entries[n..].iter().any(|e| matches!(e.op, Op::Tombstone { .. })));
        assert_eq!(materialize(&entries, &cur), cur);
        let text: String = entries.iter().map(|e| serde_json::to_string(e).unwrap() + "\n").collect();
        assert_eq!(parse(&text).unwrap(), entries);

        // Nothing changed: nothing to record.
        let before = entries.len();
        ingest(&mut entries, &cur);
        assert_eq!(entries.len(), before);
    }

    #[test]
    fn removing_a_module_and_a_node_replays() {
        let g = sample();
        let mut entries = Vec::new();
        ingest(&mut entries, &g);
        let mut cur = g.clone();
        cur.modules.clear();
        cur.nodes.retain(|n| n.id != "db");
        cur.nodes[0].links.clear();
        cur.flows[0].nodes = vec!["api".into()];
        ingest(&mut entries, &cur);
        assert_eq!(materialize(&entries, &cur), cur);
    }

    #[test]
    fn proposals_stay_out_until_accepted() {
        let g = sample();
        let mut entries = Vec::new();
        ingest(&mut entries, &g);
        let mut proposal = stamp(
            vec![Op::NodeMerge {
                scope: None,
                subject: Subject::Node,
                id: "db".into(),
                patch: Map::from_iter([("description".to_string(), Value::from("rewritten"))]),
            }],
            Source::Agent,
            None,
            0,
        );
        proposal[0].proposed = true;
        entries.extend(proposal.clone());
        assert_eq!(materialize(&entries, &g), g);
        assert_eq!(open_proposals(&entries).len(), 1);

        let reject = stamp(vec![Op::Tombstone { target: proposal[0].id.clone() }], Source::Person, None, 0);
        let mut rejected = entries.clone();
        rejected.extend(reject);
        assert!(open_proposals(&rejected).is_empty());
    }

    #[test]
    fn agent_appends_decisions_but_only_proposes_rewrites() {
        let dir = std::env::temp_dir().join(format!("wb-ledger-{}", new_id()));
        let g = sample();
        record(&dir, &[], &g, Source::Person, None, 0).unwrap();
        let decision = |text: &str| graph::Decision { decision: text.into(), why: "reasons".into(), ..Default::default() };

        let mut appended = g.clone();
        appended.nodes[1].decisions.push(decision("Use Postgres"));
        appended.nodes[1].invariants.push("Never drops rows".into());
        let entries = record(&dir, &[], &appended, Source::Agent, None, 0).unwrap();
        let m = materialize(&entries, &appended);
        assert_eq!(m.nodes[1].decisions, vec![decision("Use Postgres")]);
        assert_eq!(m.nodes[1].invariants, vec!["Never drops rows".to_string()]);
        assert!(open_proposals(&entries).is_empty());

        let mut rewritten = appended.clone();
        rewritten.nodes[1].decisions[0] = decision("Use SQLite");
        let entries = record(&dir, &entries, &rewritten, Source::Agent, None, 0).unwrap();
        assert_eq!(materialize(&entries, &rewritten).nodes[1].decisions, vec![decision("Use Postgres")]);
        assert_eq!(open_proposals(&entries).len(), 1);
    }

    #[test]
    fn agent_description_edits_become_proposals() {
        let dir = std::env::temp_dir().join(format!("wb-ledger-{}", new_id()));
        let g = sample();
        record(&dir, &[], &g, Source::Person, None, 0).unwrap();

        let mut cur = g.clone();
        cur.nodes[1].description = "agent rewrote this".into();
        cur.nodes[1].title = "Database".into();
        let entries = record(&dir, &[], &cur, Source::Agent, None, 0).unwrap();
        // The title is committed, the description only proposed.
        let m = materialize(&entries, &cur);
        assert_eq!(m.nodes[1].title, "Database");
        assert_eq!(m.nodes[1].description, "db does things");
        assert_eq!(open_proposals(&entries).len(), 1);

        // Seeing the same graph again, even from a person, records nothing new.
        let again = record(&dir, &entries, &cur, Source::Person, None, 0).unwrap();
        assert_eq!(again.len(), entries.len());

        let id = open_proposals(&again)[0].id.clone();
        let accepted = verdict(&again, &id, true, None, 0);
        assert_eq!(accepted[0].resolves.as_deref(), Some(id.as_str()));
        let mut all = again.clone();
        all.extend(accepted);
        assert_eq!(materialize(&all, &cur).nodes[1].description, "agent rewrote this");
        assert!(open_proposals(&all).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wb-{name}-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn file(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(LEDGER_FILE)).unwrap_or_default()
    }

    fn find(entries: &[Entry], f: impl Fn(&Op) -> bool) -> &Entry {
        entries.iter().find(|e| f(&e.op)).expect("entry in the ledger")
    }

    /// Every step of a history that removes things only ever appends to the file: the old
    /// bytes stay a prefix of the new ones, and what was removed is still in the log.
    #[test]
    fn removals_only_ever_append() {
        let dir = temp_dir("append");
        let mut known: Vec<Entry> = Vec::new();
        let step = |g: &Graph, source: Source, known: &mut Vec<Entry>| {
            let before = file(&dir);
            *known = record(&dir, known, g, source, None, 0).unwrap();
            let after = file(&dir);
            assert!(after.starts_with(&before), "the ledger was rewritten instead of appended to");
            assert_eq!(parse(&after).unwrap(), *known);
        };

        let mut g = sample();
        step(&g, Source::Person, &mut known);
        let genesis = known.clone();
        let db_added = find(&genesis, |op| matches!(op, Op::NodeAdd { node, .. } if node.id == "db")).clone();
        let link = find(&genesis, |op| matches!(op, Op::RefAdd { from, to, .. } if from == "api" && to == "db")).clone();

        g.nodes[0].description = "Serves the public API".into();
        step(&g, Source::Person, &mut known);
        g.nodes[0].links.clear();
        step(&g, Source::Person, &mut known);
        g.remove_node("db");
        step(&g, Source::Person, &mut known);
        g.modules.clear();
        step(&g, Source::Outside, &mut known);

        // An agent's rewrite is proposed and rejected, then proposed again and accepted.
        let mut agent = g.clone();
        agent.nodes[0].description = "Agent version one".into();
        step(&agent, Source::Agent, &mut known);
        let rejected = open_proposals(&known)[0].clone();
        let before = file(&dir);
        let reject = verdict(&known, &rejected.id, false, None, 0);
        append(&dir, &reject).unwrap();
        known.extend(reject);
        assert!(file(&dir).starts_with(&before));
        agent.nodes[0].description = "Agent version two".into();
        step(&agent, Source::Agent, &mut known);
        let accepted = open_proposals(&known)[0].clone();
        let before = file(&dir);
        let accept = verdict(&known, &accepted.id, true, None, 0);
        append(&dir, &accept).unwrap();
        known.extend(accept);
        assert!(file(&dir).starts_with(&before));

        g.nodes[0].description = "Agent version two".into();
        g.flows.clear();
        step(&g, Source::Person, &mut known);

        // The graph no longer has what was removed, but the log still holds it, unchanged.
        let all = load(&dir).unwrap();
        extends(&genesis, &all).unwrap();
        assert!(all.contains(&db_added) && all.contains(&link) && all.contains(&rejected));
        assert!(all.iter().any(|e| e.op == Op::Tombstone { target: link.id.clone() }));
        assert!(all.iter().any(|e| e.op == Op::Tombstone { target: rejected.id.clone() }));
        assert!(all.iter().any(|e| e.resolves.as_deref() == Some(accepted.id.as_str())));
        let m = materialize(&all, &g);
        assert_eq!(m, g);
        assert!(m.node("db").is_none() && m.modules.is_empty() && m.nodes[0].links.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extends_rejects_rewrites() {
        let mut entries = Vec::new();
        ingest(&mut entries, &sample());
        let lines: Vec<String> = entries.iter().map(|e| serde_json::to_string(e).unwrap()).collect();

        let dropped = parse(&lines[1..].join("\n")).unwrap();
        assert!(extends(&entries, &dropped).unwrap_err().contains("is gone"));
        let mut edited = entries.clone();
        edited[0].author = Some("someone else".into());
        assert!(extends(&entries, &edited).unwrap_err().contains("changed"));

        let mut grown = entries.clone();
        grown.extend(stamp(vec![Op::FlowRemove { scope: None, id: "read".into() }], Source::Person, None, 0));
        extends(&entries, &grown).unwrap();
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn commit(dir: &Path, msg: &str) {
        git(dir, &["add", "-A"]);
        git(dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", msg]);
    }

    /// Two branches that both appended merge without conflicts and keep every entry, and the
    /// graph is the same whichever side is merged into the other.
    #[test]
    fn union_merge_keeps_every_entry() {
        let dir = temp_dir("merge");
        git(&dir, &["init", "-q", "-b", "main"]);
        let g = sample();
        let base = record(&dir, &[], &g, Source::Person, None, 0).unwrap();
        commit(&dir, "base");

        let branch = |name: &str, extra: Node| {
            git(&dir, &["checkout", "-q", "-b", name, "main"]);
            let mut cur = g.clone();
            cur.nodes.push(extra);
            let entries = record(&dir, &base, &cur, Source::Person, None, 0).unwrap();
            commit(&dir, name);
            entries
        };
        let a = branch("a", node("cache", &["db"]));
        let b = branch("b", node("queue", &["api"]));

        let mut merged_graphs = Vec::new();
        for (into, from, merged) in [("a", "b", "ab"), ("b", "a", "ba")] {
            git(&dir, &["checkout", "-q", "-b", merged, into]);
            git(&dir, &["-c", "user.name=t", "-c", "user.email=t@t", "merge", "-q", "--no-edit", from]);
            let all = load(&dir).unwrap();
            extends(&a, &all).unwrap();
            extends(&b, &all).unwrap();
            assert_eq!(all.len(), base.len() + (a.len() - base.len()) + (b.len() - base.len()));
            merged_graphs.push(materialize(&all, &Graph::default()));
        }
        assert_eq!(merged_graphs[0], merged_graphs[1]);
        assert!(merged_graphs[0].node("cache").is_some() && merged_graphs[0].node("queue").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_ops_are_rejected() {
        let line = r#"{"id":"01J","at":0,"source":"person","op":"node.rename","id2":"x"}"#;
        assert!(parse(line).unwrap_err().contains("line 1"));
    }
}
