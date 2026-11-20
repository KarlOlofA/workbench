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

use crate::graph::{Decision, Graph, Module, Node};
use crate::git::{DiffLine, FileDiff, Hunk, LineKind};
use crate::config;

pub const HISTORY_FILE: &str = ".workbench/node-history.jsonl";
const ATTRIBUTES_FILE: &str = ".workbench/.gitattributes";
const ATTRIBUTES: &str = "node-history.jsonl merge=union\n";
const MERGE_SECS: u64 = 120;

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

pub fn sentences(text: &str) -> Vec<String> {
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
    push_knowledge(&mut out, &n.decisions, &n.invariants);
    push_description(&mut out, &n.description);
    out.join("\n")
}

/// `text` from `render_node` with its links one per line under `Links:`, for diffs: a link
/// added or removed then shows as one line, not as the whole list changed. The history keeps the
/// one-line form, so nodes already tracked don't count as edited.
pub fn links_one_per_line(text: &str) -> String {
    text.lines()
        .map(|line| match line.strip_prefix("Links: ") {
            Some(links) => std::iter::once("Links:".to_string()).chain(links.split(", ").map(|l| format!("  {l}"))).collect::<Vec<_>>().join("\n"),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn render_module(m: &Module) -> String {
    let mut out = vec![format!("Title: {}", m.title.trim())];
    push_knowledge(&mut out, &m.decisions, &m.invariants);
    push_description(&mut out, &m.description);
    out.join("\n")
}

/// Decisions and invariants as facts: a heading, then one indented line each.
fn push_knowledge(out: &mut Vec<String>, decisions: &[Decision], invariants: &[String]) {
    if !decisions.is_empty() {
        out.push("Decisions:".into());
        out.extend(decisions.iter().map(|d| format!("  {}", d.line())));
    }
    let invariants: Vec<&String> = invariants.iter().filter(|i| !i.trim().is_empty()).collect();
    if !invariants.is_empty() {
        out.push("Invariants:".into());
        out.extend(invariants.iter().map(|i| format!("  {}", i.trim())));
    }
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

/// Adds `line` (e.g. `file merge=union`) to `.workbench/.gitattributes` unless it is there.
pub fn ensure_attribute(cwd: &Path, line: &str) -> Result<(), String> {
    let attrs = cwd.join(ATTRIBUTES_FILE);
    let current = std::fs::read_to_string(&attrs).unwrap_or_default();
    if !current.lines().any(|l| l.trim() == line.trim()) {
        let sep = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
        std::fs::write(&attrs, format!("{current}{sep}{}\n", line.trim())).map_err(|e| format!("{ATTRIBUTES_FILE}: {e}"))?;
    }
    Ok(())
}

pub fn save(cwd: &Path, entries: &[Entry]) -> Result<(), String> {
    let path = cwd.join(HISTORY_FILE);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    ensure_attribute(cwd, ATTRIBUTES)?;
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

/// The steps that turn `a` into `b`, from a longest common subsequence, so items that only
/// stayed in place count as `Same`.
fn diff<T: PartialEq + Clone>(a: &[T], b: &[T]) -> Vec<(Op, T)> {
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    let mut steps = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            steps.push((Op::Same, a[i].clone()));
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            steps.push((Op::Del, a[i].clone()));
            i += 1;
        } else {
            steps.push((Op::Add, b[j].clone()));
            j += 1;
        }
    }
    steps
}

/// What changed in a node's intent, as two parts:
/// - the facts (title, kind, links, each file) that are gone (`Del`) or new (`Add`), and
/// - the description as prose: its words in order, each `Same`, `Add` or `Del`. Empty when the
///   description didn't change.
///
/// Rendered nodes (`render_node`) hold one fact per line, then a blank line and the description.
pub fn intent_changes(before: &str, after: &str) -> (Vec<(Op, String)>, Vec<(Op, String)>) {
    let split = |text: &str| -> (String, String) {
        let (facts, description) = text.split_once("\n\n").unwrap_or((text, ""));
        (facts.to_string(), prose(description))
    };
    let ((facts_a, prose_a), (facts_b, prose_b)) = (split(before), split(after));
    let fact_changes = diff(&facts(&facts_a), &facts(&facts_b)).into_iter().filter(|(op, _)| *op != Op::Same).collect();
    let prose_changes = if prose_a == prose_b { Vec::new() } else { prose_diff(&prose_a, &prose_b) };
    (fact_changes, prose_changes)
}

/// A rendered description (one sentence per line, a blank line between paragraphs) as text again.
fn prose(rendered: &str) -> String {
    rendered.split("\n\n").map(|p| p.lines().map(str::trim).collect::<Vec<_>>().join(" ")).collect::<Vec<_>>().join("\n\n")
}

/// Word-level diff of two texts, for reading as prose. Each word keeps the space or line break
/// after it, so joining the `Same` and `Add` parts gives `after` back. Runs of the same op are
/// merged into one part.
pub fn prose_diff(before: &str, after: &str) -> Vec<(Op, String)> {
    let words = |text: &str| -> Vec<String> { text.split_inclusive(char::is_whitespace).map(String::from).collect() };
    let mut out: Vec<(Op, String)> = Vec::new();
    for (op, word) in diff(&words(before.trim()), &words(after.trim())) {
        match out.last_mut() {
            Some((last, text)) if *last == op => text.push_str(&word),
            _ => out.push((op, word)),
        }
    }
    out
}

/// Lines of context kept round each change, as in `git diff`.
const CONTEXT: usize = 3;

/// A line diff of two rendered nodes (`render_node`, `render_module`) in the shape of a git file
/// diff, so the Changes tab shows it with the code diff view: hunks of the changed lines with
/// three lines of context round them. `name` stands where the file path would.
pub fn line_diff(name: &str, before: &str, after: &str) -> FileDiff {
    let lines = |text: &str| text.lines().map(String::from).collect::<Vec<_>>();
    let (mut old_no, mut new_no) = (0u32, 0u32);
    let rows: Vec<DiffLine> = diff(&lines(before), &lines(after))
        .into_iter()
        .map(|(op, text)| {
            let (kind, old, new) = match op {
                Op::Same => {
                    old_no += 1;
                    new_no += 1;
                    (LineKind::Context, Some(old_no), Some(new_no))
                }
                Op::Del => {
                    old_no += 1;
                    (LineKind::Removed, Some(old_no), None)
                }
                Op::Add => {
                    new_no += 1;
                    (LineKind::Added, None, Some(new_no))
                }
            };
            DiffLine { kind, old_no: old, new_no: new, text }
        })
        .collect();
    let changed: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].kind != LineKind::Context).collect();
    let near_a_change = |i: usize| changed.iter().any(|&c| i + CONTEXT >= c && i <= c + CONTEXT);
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut last_kept: Option<usize> = None;
    for (i, row) in rows.into_iter().enumerate() {
        if !near_a_change(i) {
            continue;
        }
        // A row that doesn't follow the last kept one starts a new hunk.
        if last_kept.is_none_or(|k| k + 1 != i) {
            hunks.push(Hunk { header: String::new(), lines: Vec::new() });
        }
        hunks.last_mut().unwrap().lines.push(row);
        last_kept = Some(i);
    }
    for h in &mut hunks {
        let range = |nos: Vec<u32>| format!("{},{}", nos.first().copied().unwrap_or(0), nos.len());
        h.header = format!(
            "@@ -{} +{} @@",
            range(h.lines.iter().filter_map(|l| l.old_no).collect()),
            range(h.lines.iter().filter_map(|l| l.new_no).collect()),
        );
    }
    let count = |kind: LineKind| hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind == kind).count();
    FileDiff { path: name.to_string(), added: count(LineKind::Added), removed: count(LineKind::Removed), binary: false, hunks }
}

/// The non-empty lines of a rendered node. An indented line, such as a file under `Files:`,
/// gets the heading above it, so it reads `Files: src/x.rs` on its own. A heading followed by
/// indented lines is not a fact itself; its lines carry it.
fn facts(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut heading = "";
    let mut out = Vec::new();
    for (k, line) in lines.iter().enumerate() {
        if line.starts_with(' ') {
            out.push(format!("{heading} {}", line.trim()));
            continue;
        }
        heading = line.trim();
        let has_lines_below = lines.get(k + 1).is_some_and(|next| next.starts_with(' '));
        if !has_lines_below {
            out.push(heading.to_string());
        }
    }
    out
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

    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn line_diff_keeps_three_lines_of_context_round_a_change() {
        let before = numbered(10);
        let after = before.replace("line 5", "line five");
        let d = line_diff("Node", &before, &after);
        assert_eq!((d.added, d.removed, d.hunks.len()), (1, 1, 1));
        let h = &d.hunks[0];
        assert_eq!(h.header, "@@ -2,7 +2,7 @@");
        assert_eq!(h.lines.first().unwrap().text, "line 2");
        assert_eq!(h.lines.last().unwrap().text, "line 8");
    }

    #[test]
    fn an_added_link_is_one_added_line() {
        let before = links_one_per_line("Title: A\nLinks: b, c\nKind: x");
        let after = links_one_per_line("Title: A\nLinks: b, c, d\nKind: x");
        assert_eq!(before, "Title: A\nLinks:\n  b\n  c\nKind: x");
        let d = line_diff("A", &before, &after);
        assert_eq!((d.added, d.removed), (1, 0));
    }

    #[test]
    fn line_diff_of_an_added_node_is_all_added_lines() {
        let d = line_diff("Node", "", "Title: A\nKind: service");
        assert_eq!((d.added, d.removed), (2, 0));
        assert!(d.hunks[0].lines.iter().all(|l| l.kind == LineKind::Added));
        assert_eq!(d.hunks[0].lines[0].new_no, Some(1));
    }

    #[test]
    fn line_diff_splits_changes_far_apart_and_skips_equal_texts() {
        let before = numbered(20);
        let after = before.replace("line 2\n", "line two\n").replace("line 18", "line eighteen");
        assert_eq!(line_diff("Node", &before, &after).hunks.len(), 2);
        assert!(line_diff("Node", &before, &before).hunks.is_empty());
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
    fn intent_changes_lists_facts_and_diffs_the_description_as_prose() {
        let before = "Title: A\nKind: service\nLinks: b, c\n\nLoads it.\nSaves it.";
        let after = "Title: A\nKind: service\nLinks: b\n\nLoads it.\nSaves it twice.";
        let (facts, prose) = intent_changes(before, after);
        assert_eq!(facts, vec![(Op::Del, "Links: b, c".to_string()), (Op::Add, "Links: b".to_string())]);
        assert_eq!(
            prose,
            vec![(Op::Same, "Loads it. Saves ".to_string()), (Op::Del, "it.".to_string()), (Op::Add, "it twice.".to_string())]
        );
        assert_eq!(intent_changes("", "a\nb").0, vec![(Op::Add, "a".to_string()), (Op::Add, "b".to_string())]);
        assert_eq!(intent_changes("Files:\n  a.rs", "Files:\n  a.rs\n  b.rs").0, vec![(Op::Add, "Files: b.rs".to_string())]);
        assert!(intent_changes("Title: A\n\nSame.", "Title: B\n\nSame.").1.is_empty());
    }

    #[test]
    fn prose_diff_marks_words_and_keeps_paragraphs_and_punctuation() {
        let changes = prose_diff("Loads “it”. Saves it.\n\nÄr klar.", "Loads “it”. Saves it — twice.\n\nÄr klar.");
        assert_eq!(
            changes,
            vec![(Op::Same, "Loads “it”. Saves ".to_string()), (Op::Del, "it.\n".to_string()), (Op::Add, "it — twice.\n".to_string()), (Op::Same, "\nÄr klar.".to_string())]
        );
        let after: String = changes.iter().filter(|(op, _)| *op != Op::Del).map(|(_, t)| t.as_str()).collect();
        assert_eq!(after, "Loads “it”. Saves it — twice.\n\nÄr klar.");
    }
}
