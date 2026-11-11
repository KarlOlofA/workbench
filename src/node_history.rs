//! Shared, append-only history of node and module changes.
//!
//! The log lives in `.workbench/node-history.jsonl` and is meant to be committed and shared:
//! one JSON entry per line, each with a unique id and an author, so logs written by several
//! people merge cleanly (`.workbench/.gitattributes` sets `merge=union` for it) and a future
//! sync host can merge them by id. What this client last saw of the graph is per-user state and
//! is kept outside the repo, in `~/.config/workbench/node-snapshots/`.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::graph::{Graph, Module, Node};
use crate::config;

pub const HISTORY_FILE: &str = ".workbench/node-history.jsonl";
const ATTRIBUTES_FILE: &str = ".workbench/.gitattributes";
const ATTRIBUTES: &str = "node-history.jsonl merge=union\n";
const MERGE_SECS: u64 = 120;
const CONTEXT: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// A person edited the graph in Workbench.
    Person,
    /// Workbench edited the graph during a chat turn or job started by the entry's author.
    Workbench,
    /// The graph changed on disk without Workbench (an editor, git, another tool).
    Outside,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tracked {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub module: bool,
    pub id: String,
    pub title: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub uid: String,
    pub at: u64,
    pub source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub module: bool,
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

impl Entry {
    pub fn key(&self) -> String {
        key(self.scope.as_deref(), self.module, &self.id)
    }

    pub fn when(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.at)
    }

    /// Who made the change, as seen by `me` (the local user).
    pub fn who(&self, me: Option<&str>) -> String {
        let mine = self.author.is_some() && self.author.as_deref() == me;
        match (self.source, self.author.as_deref()) {
            (Source::Person, _) if mine => "you".into(),
            (Source::Person, Some(a)) => a.into(),
            (Source::Person, None) => "someone".into(),
            (Source::Workbench, _) if mine => "Workbench".into(),
            (Source::Workbench, Some(a)) => format!("Workbench for {a}"),
            (Source::Workbench, None) => "Workbench".into(),
            (Source::Outside, _) => "outside Workbench".into(),
        }
    }

    pub fn kind(&self) -> &'static str {
        match (&self.before, &self.after) {
            (None, Some(_)) => "added",
            (Some(_), None) => "removed",
            _ => "modified",
        }
    }
}

pub type Snapshot = BTreeMap<String, Tracked>;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct NodeHistory {
    /// What this client last saw of the graph (local, never shared).
    pub known: Option<Snapshot>,
    /// The shared log, oldest first.
    pub entries: Vec<Entry>,
}

pub fn key(scope: Option<&str>, module: bool, id: &str) -> String {
    format!("{}:{}:{id}", scope.unwrap_or(""), if module { "module" } else { "node" })
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn new_uid(at: u64, salt: usize) -> String {
    use std::hash::{BuildHasher, Hasher};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(nanos);
    h.write_u32(std::process::id());
    h.write_usize(salt);
    format!("{at:x}-{:016x}", h.finish())
}

fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut cur = String::new();
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            cur.push(c);
            if matches!(c, '.' | '!' | '?') && chars.peek().is_some_and(|n| n.is_whitespace()) {
                out.push(cur.trim().to_string());
                cur.clear();
            }
        }
        if !cur.trim().is_empty() || line.trim().is_empty() {
            out.push(cur.trim().to_string());
        }
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

fn push_description(out: &mut Vec<String>, description: &str) {
    let lines = sentences(description.trim());
    if !lines.is_empty() {
        out.push(String::new());
        out.extend(lines);
    }
}

pub fn render_node(n: &Node) -> String {
    let mut out = vec![format!("Title: {}", n.title.trim()), format!("Kind: {}", n.kind.trim())];
    if let Some(m) = &n.module {
        out.push(format!("Attaches module: {m}"));
    }
    if let Some(r) = &n.reference {
        out.push(format!("Reference: {}", r.source.trim()));
        for rn in &r.nodes {
            match &rn.module {
                Some(m) => out.push(format!("  references {m} › {}", rn.id)),
                None => out.push(format!("  references {}", rn.id)),
            }
        }
    }
    let mut links = n.links.clone();
    links.sort();
    links.dedup();
    if !links.is_empty() {
        out.push(format!("Links: {}", links.join(", ")));
    }
    if !n.files.is_empty() {
        out.push("Files:".into());
        out.extend(n.files.iter().map(|f| format!("  {f}")));
    }
    push_description(&mut out, &n.description);
    out.join("\n")
}

pub fn render_module(m: &Module) -> String {
    let mut out = vec![format!("Title: {}", m.title.trim())];
    push_description(&mut out, &m.description);
    out.join("\n")
}

pub fn track(g: &Graph) -> BTreeMap<String, Tracked> {
    let mut out = BTreeMap::new();
    let add_net = |net: &Graph, scope: Option<&str>, out: &mut BTreeMap<String, Tracked>| {
        for n in &net.nodes {
            out.insert(
                key(scope, false, &n.id),
                Tracked { scope: scope.map(String::from), module: false, id: n.id.clone(), title: n.title.clone(), text: render_node(n) },
            );
        }
    };
    add_net(g, None, &mut out);
    for m in &g.modules {
        out.insert(
            key(Some(&m.id), true, &m.id),
            Tracked { scope: Some(m.id.clone()), module: true, id: m.id.clone(), title: m.title.clone(), text: render_module(m) },
        );
        add_net(&m.net, Some(&m.id), &mut out);
    }
    out
}

impl NodeHistory {
    /// Compares the graph with the last recorded state and appends an entry for every node or
    /// module whose information changed. Returns true when the history changed.
    pub fn record(&mut self, g: &Graph, source: Source, author: Option<&str>, at: u64) -> bool {
        let cur = track(g);
        let Some(known) = self.known.take() else {
            self.known = Some(cur);
            return true;
        };
        if known == cur {
            self.known = Some(known);
            return false;
        }
        let keys: Vec<&String> = cur.keys().chain(known.keys().filter(|k| !cur.contains_key(*k))).collect();
        for k in keys {
            let (before, after) = (known.get(k), cur.get(k));
            if before.map(|t| &t.text) == after.map(|t| &t.text) {
                continue;
            }
            let after_text = after.map(|t| &t.text);
            if self.entries.iter().rev().find(|e| e.key() == *k).is_some_and(|e| e.after.as_ref() == after_text) {
                // Already in the shared log, e.g. made by someone else and pulled in.
                continue;
            }
            let t = after.or(before).unwrap();
            let uid = new_uid(at, self.entries.len());
            self.push(Entry {
                uid,
                at,
                source,
                author: author.map(String::from),
                scope: t.scope.clone(),
                module: t.module,
                id: t.id.clone(),
                title: t.title.clone(),
                before: before.map(|t| t.text.clone()),
                after: after.map(|t| t.text.clone()),
            });
        }
        self.known = Some(cur);
        true
    }

    /// Replaces the log with the one on disk (after a pull or another writer), keeping the
    /// local snapshot. Returns true when it changed.
    pub fn adopt(&mut self, entries: Vec<Entry>) -> bool {
        if self.entries == entries {
            return false;
        }
        self.entries = entries;
        true
    }

    fn push(&mut self, e: Entry) {
        let k = e.key();
        if e.source == Source::Person
            && let Some(i) = self.entries.iter().rposition(|p| p.key() == k)
        {
            let prev = &mut self.entries[i];
            if prev.source == Source::Person && prev.author == e.author && e.at.saturating_sub(prev.at) <= MERGE_SECS {
                prev.after = e.after;
                prev.at = e.at;
                prev.title = e.title;
                if prev.before == prev.after {
                    self.entries.remove(i);
                }
                return;
            }
        }
        self.entries.push(e);
    }

    pub fn count_for(&self, scope: Option<&str>, id: &str) -> usize {
        let k = key(scope, false, id);
        self.entries.iter().filter(|e| e.key() == k).count()
    }
}

/// Reads the shared log. Lines are de-duplicated by uid and sorted by time, since a union merge
/// can interleave or repeat lines written on different branches. Unreadable lines are skipped.
pub fn load(cwd: &Path) -> Result<Vec<Entry>, String> {
    let text = match std::fs::read_to_string(cwd.join(HISTORY_FILE)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{HISTORY_FILE}: {e}")),
    };
    Ok(parse(&text))
}

pub fn parse(text: &str) -> Vec<Entry> {
    let mut seen = HashSet::new();
    let mut out: Vec<Entry> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
        .filter(|e| seen.insert(e.uid.clone()))
        .collect();
    out.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.uid.cmp(&b.uid)));
    out
}

pub fn exists(cwd: &Path) -> bool {
    cwd.join(HISTORY_FILE).exists()
}

pub fn save(cwd: &Path, entries: &[Entry]) -> Result<(), String> {
    let path = cwd.join(HISTORY_FILE);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let attrs = cwd.join(ATTRIBUTES_FILE);
    let current = std::fs::read_to_string(&attrs).unwrap_or_default();
    if !current.lines().any(|l| l.trim() == ATTRIBUTES.trim()) {
        let sep = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
        std::fs::write(&attrs, format!("{current}{sep}{ATTRIBUTES}")).map_err(|e| format!("{ATTRIBUTES_FILE}: {e}"))?;
    }
    let mut text = String::new();
    for e in entries {
        text.push_str(&serde_json::to_string(e).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    if std::fs::read_to_string(&path).is_ok_and(|old| old == text) {
        return Ok(());
    }
    std::fs::write(&path, text).map_err(|e| format!("{HISTORY_FILE}: {e}"))
}

fn snapshot_name(cwd: &Path) -> String {
    format!("node-snapshots/{}.json", crate::history::project_key(cwd))
}

/// The local "last seen" state of the graph for this folder, if this client has seen it before.
pub fn load_snapshot(cwd: &Path) -> Option<Snapshot> {
    let s: Option<Snapshot> = config::load_json(&snapshot_name(cwd));
    s
}

pub fn save_snapshot(cwd: &Path, s: &Snapshot) -> Result<(), String> {
    config::save_json(&snapshot_name(cwd), s)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Same,
    Add,
    Del,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Line { op: Op, old: Option<usize>, new: Option<usize>, text: String },
    Skip(usize),
}

/// Classic line diff (longest common subsequence) with unchanged runs collapsed to a few lines
/// of context around each change.
pub fn line_diff(before: &str, after: &str) -> Vec<Row> {
    let a: Vec<&str> = if before.is_empty() { Vec::new() } else { before.split('\n').collect() };
    let b: Vec<&str> = if after.is_empty() { Vec::new() } else { after.split('\n').collect() };
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    let mut ops: Vec<(Op, Option<usize>, Option<usize>, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            ops.push((Op::Same, Some(i + 1), Some(j + 1), a[i]));
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            ops.push((Op::Del, Some(i + 1), None, a[i]));
            i += 1;
        } else {
            ops.push((Op::Add, None, Some(j + 1), b[j]));
            j += 1;
        }
    }
    let changed: Vec<usize> = ops.iter().enumerate().filter(|(_, o)| o.0 != Op::Same).map(|(k, _)| k).collect();
    let near = |k: usize| changed.iter().any(|&c| c.abs_diff(k) <= CONTEXT);
    let mut rows = Vec::new();
    let mut skipped = 0;
    for (k, (op, old, new, text)) in ops.into_iter().enumerate() {
        if op == Op::Same && !near(k) {
            skipped += 1;
            continue;
        }
        if skipped > 0 {
            rows.push(Row::Skip(skipped));
            skipped = 0;
        }
        rows.push(Row::Line { op, old, new, text: text.to_string() });
    }
    if skipped > 0 {
        rows.push(Row::Skip(skipped));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, desc: &str) -> Node {
        Node { id: id.into(), title: id.to_uppercase(), kind: "service".into(), description: desc.into(), ..Default::default() }
    }

    fn graph(nodes: Vec<Node>) -> Graph {
        Graph { nodes, ..Default::default() }
    }

    #[test]
    fn first_record_only_takes_a_snapshot() {
        let mut h = NodeHistory::default();
        assert!(h.record(&graph(vec![node("a", "Does a.")]), Source::Outside, None, 10));
        assert!(h.entries.is_empty());
        assert!(!h.record(&graph(vec![node("a", "Does a.")]), Source::Person, None, 11));
    }

    #[test]
    fn records_added_modified_and_removed_nodes() {
        let mut h = NodeHistory::default();
        h.record(&graph(vec![node("a", "Does a."), node("b", "Does b.")]), Source::Outside, None, 0);
        h.record(&graph(vec![node("a", "Does a better."), node("c", "New.")]), Source::Workbench, None, 5);
        let kinds: Vec<(&str, &str)> = h.entries.iter().map(|e| (e.id.as_str(), e.kind())).collect();
        assert_eq!(kinds, vec![("a", "modified"), ("c", "added"), ("b", "removed")]);
        assert!(h.entries.iter().all(|e| e.source == Source::Workbench));
    }

    #[test]
    fn consecutive_user_edits_merge_and_cancel_out() {
        let mut h = NodeHistory::default();
        h.record(&graph(vec![node("a", "One.")]), Source::Outside, None, 0);
        h.record(&graph(vec![node("a", "One. T")]), Source::Person, None, 100);
        h.record(&graph(vec![node("a", "One. Two.")]), Source::Person, None, 130);
        assert_eq!(h.entries.len(), 1);
        assert!(h.entries[0].after.as_deref().unwrap().ends_with("One.\nTwo."));
        h.record(&graph(vec![node("a", "One.")]), Source::Person, None, 140);
        assert!(h.entries.is_empty());
    }

    #[test]
    fn edits_far_apart_or_by_claude_stay_separate() {
        let mut h = NodeHistory::default();
        h.record(&graph(vec![node("a", "One.")]), Source::Outside, None, 0);
        h.record(&graph(vec![node("a", "Two.")]), Source::Person, None, 0);
        h.record(&graph(vec![node("a", "Three.")]), Source::Person, None, 1000);
        h.record(&graph(vec![node("a", "Four.")]), Source::Workbench, None, 1001);
        h.record(&graph(vec![node("a", "Five.")]), Source::Person, None, 1002);
        assert_eq!(h.entries.len(), 4);
    }

    #[test]
    fn changes_pulled_from_teammates_are_not_logged_twice() {
        let mut alice = NodeHistory::default();
        alice.record(&graph(vec![node("a", "One.")]), Source::Outside, None, 0);
        alice.record(&graph(vec![node("a", "Two.")]), Source::Person, Some("alice"), 10);
        let mut bob = NodeHistory::default();
        bob.record(&graph(vec![node("a", "One.")]), Source::Outside, None, 0);
        bob.adopt(alice.entries.clone());
        bob.record(&graph(vec![node("a", "Two.")]), Source::Outside, Some("bob"), 20);
        assert_eq!(bob.entries, alice.entries);
        assert_eq!(bob.entries[0].who(Some("bob")), "alice");
        assert_eq!(bob.entries[0].who(Some("alice")), "you");
    }

    #[test]
    fn edits_by_different_people_are_not_merged() {
        let mut h = NodeHistory::default();
        h.record(&graph(vec![node("a", "One.")]), Source::Outside, None, 0);
        h.record(&graph(vec![node("a", "Two.")]), Source::Person, Some("alice"), 10);
        h.record(&graph(vec![node("a", "Three.")]), Source::Person, Some("bob"), 20);
        assert_eq!(h.entries.len(), 2);
        assert_ne!(h.entries[0].uid, h.entries[1].uid);
    }

    #[test]
    fn union_merged_log_is_deduplicated_and_sorted() {
        let e = |uid: &str, at: u64| {
            serde_json::to_string(&Entry {
                uid: uid.into(), at, source: Source::Person, author: None, scope: None, module: false,
                id: "a".into(), title: "A".into(), before: None, after: Some("x".into()),
            })
            .unwrap()
        };
        let text = format!("{}\n{}\n{}\nnot json\n\n{}\n", e("u1", 5), e("u3", 9), e("u2", 7), e("u1", 5));
        let uids: Vec<String> = parse(&text).into_iter().map(|e| e.uid).collect();
        assert_eq!(uids, vec!["u1", "u2", "u3"]);
    }

    #[test]
    fn descriptions_are_split_into_sentences() {
        assert_eq!(sentences("Loads it. Then saves e.g.x! Done?"), vec!["Loads it.", "Then saves e.g.x!", "Done?"]);
        assert_eq!(sentences("A.\n\nB."), vec!["A.", "", "B."]);
    }

    #[test]
    fn line_diff_marks_changes_and_collapses_context() {
        let before = "1\n2\n3\n4\n5\n6\n7\n8";
        let after = "1\n2\n3\n4\n5\nsix\n7\n8";
        let rows = line_diff(before, after);
        assert_eq!(rows[0], Row::Skip(3));
        let ops: Vec<Op> = rows.iter().filter_map(|r| match r { Row::Line { op, .. } => Some(*op), _ => None }).collect();
        assert_eq!(ops, vec![Op::Same, Op::Same, Op::Del, Op::Add, Op::Same, Op::Same]);
        assert!(line_diff("", "a\nb").iter().all(|r| matches!(r, Row::Line { op: Op::Add, .. })));
    }
}
