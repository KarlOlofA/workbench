//! Blast radius enforcement. Selecting nodes scopes an agent run's result, not only its
//! prompt: after the run, the files it changed are compared with the files its nodes own.
//! Changes outside are reported, and in strict mode undone where that is safe.

use std::path::Path;

use crate::claims::NodeKey;
use crate::git::{self, StatusEntry};
use crate::graph::Graph;

/// Workbench's own files, which every run may change.
pub const ALWAYS_ALLOWED: &str = ".workbench/";

/// What an agent run was scoped to, captured when it starts.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunScope {
    /// The nodes the run works on, including the callers its prompt names.
    pub nodes: Vec<NodeKey>,
    /// The files those nodes owned when the run started.
    pub files: Vec<String>,
    /// Every node that existed when the run started. Nodes the run adds own their files too.
    pub existing: Vec<NodeKey>,
    /// Files already changed when the run started; the run isn't blamed for them.
    pub dirty_before: Vec<String>,
}

/// A file the run changed outside its scope.
#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    pub path: String,
    /// A new file, which strict mode leaves for a person to delete.
    pub untracked: bool,
}

fn key(scope: Option<&str>, id: &str) -> NodeKey {
    (scope.map(String::from), id.to_string())
}

/// Snapshots the scope of a run on `nodes`. Blocking: runs git.
pub fn start(cwd: &Path, g: &Graph, nodes: Vec<NodeKey>) -> RunScope {
    let mut files: Vec<String> = Vec::new();
    for (scope, n) in g.all_nodes() {
        if !nodes.contains(&key(scope, &n.id)) {
            continue;
        }
        for f in &n.files {
            if !files.contains(f) {
                files.push(f.clone());
            }
        }
    }
    let existing = g.all_nodes().into_iter().map(|(scope, n)| key(scope, &n.id)).collect();
    let dirty_before = git::status(cwd).unwrap_or_default().into_iter().map(|e| e.path).collect();
    RunScope { nodes, files, existing, dirty_before }
}

impl RunScope {
    /// The files the run may have changed, given the graph after the run: the scoped nodes'
    /// files before and after (a re-slicing shows up in the graph diff), and the files of
    /// nodes the run added.
    pub fn allowed(&self, g: &Graph) -> Vec<String> {
        let mut out = self.files.clone();
        for (scope, n) in g.all_nodes() {
            let k = key(scope, &n.id);
            if self.nodes.contains(&k) || !self.existing.contains(&k) {
                out.extend(n.files.iter().cloned());
            }
        }
        out
    }

    /// The changed files in `status` that the scope doesn't cover.
    pub fn violations(&self, g: &Graph, status: Vec<StatusEntry>) -> Vec<Violation> {
        let allowed = self.allowed(g);
        status
            .into_iter()
            .filter(|e| !self.dirty_before.contains(&e.path) && !covers(&allowed, &e.path))
            .map(|e| Violation { untracked: e.code == "??", path: e.path })
            .collect()
    }
}

/// Whether `path` is one of `allowed` or inside an allowed folder.
pub fn covers(allowed: &[String], path: &str) -> bool {
    path.starts_with(ALWAYS_ALLOWED)
        || allowed.iter().any(|a| {
            let a = a.trim_end_matches('/');
            path == a || path.starts_with(&format!("{a}/"))
        })
}

/// The run's out-of-scope changes. In strict mode the tracked ones are restored from HEAD:
/// they were clean when the run started, so nothing of the person's is lost. Blocking: runs git.
pub fn enforce(cwd: &Path, scope: &RunScope, g: &Graph, strict: bool) -> Result<Vec<Violation>, String> {
    let violations = scope.violations(g, git::status(cwd)?);
    let tracked: Vec<&str> = violations.iter().filter(|v| !v.untracked).map(|v| v.path.as_str()).collect();
    if strict && !tracked.is_empty() {
        git::restore(cwd, &tracked)?;
    }
    Ok(violations)
}

/// The files changed since `dirty_before` was taken that no node in `g` owns (PRO-233).
/// Agents should find code through the graph; these show where the graph has a gap.
pub fn unowned(g: &Graph, status: Vec<StatusEntry>, dirty_before: &[String]) -> Vec<String> {
    let owned: Vec<String> = g.all_nodes().into_iter().flat_map(|(_, n)| n.files.iter().cloned()).collect();
    status.into_iter().map(|e| e.path).filter(|p| !dirty_before.contains(p) && !covers(&owned, p)).collect()
}

/// The chat notice for files no node owns, and how often the agent searched the file system.
pub fn report_unowned(paths: &[String], searches: usize) -> Option<String> {
    if paths.is_empty() {
        return None;
    }
    let list = paths.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", ");
    let searched = match searches {
        0 => String::new(),
        n => format!(" The agent searched the file system {n} time(s) this turn."),
    };
    Some(format!("Not in the graph: {list}. No node owns these files, so agents can't find them through the graph. Add them to a node's files.{searched}"))
}

/// The chat message for a run's violations.
pub fn report(violations: &[Violation], strict: bool) -> Option<String> {
    if violations.is_empty() {
        return None;
    }
    let list = violations.iter().map(|v| format!("`{}`{}", v.path, if v.untracked { " (new)" } else { "" })).collect::<Vec<_>>().join(", ");
    Some(if strict {
        format!("Strict scope: the agent changed files outside the selected nodes. Changes to existing files were undone; new files were kept for you to review: {list}")
    } else {
        format!("The agent changed files outside the selected nodes' files: {list}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Node;
    use std::process::Command;

    fn node(id: &str, files: &[&str]) -> Node {
        Node { id: id.into(), title: id.into(), files: files.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn changed_files_no_node_owns_are_flagged() {
        let g = Graph { nodes: vec![node("api", &["src/api"])], ..Default::default() };
        let entry = |p: &str| StatusEntry { code: " M".into(), path: p.into() };
        let status = vec![entry("src/api/x.rs"), entry("src/new.rs"), entry("old.rs"), entry(".workbench/graph.json")];
        assert_eq!(unowned(&g, status, &["old.rs".to_string()]), vec!["src/new.rs".to_string()]);
        assert!(report_unowned(&[], 3).is_none());
        assert!(report_unowned(&["src/new.rs".into()], 2).unwrap().contains("2 time(s)"));
    }

    #[test]
    fn files_and_folders_cover_their_paths() {
        let allowed = vec!["src/api.rs".to_string(), "web/".to_string()];
        assert!(covers(&allowed, "src/api.rs"));
        assert!(covers(&allowed, "web/app/main.ts"));
        assert!(covers(&allowed, ".workbench/graph.json"));
        assert!(!covers(&allowed, "src/api.rs.bak"));
        assert!(!covers(&allowed, "src/db.rs"));
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").args(args).current_dir(dir).output().unwrap().status.success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn out_of_scope_changes_are_reported_and_undone_in_strict_mode() {
        let dir = std::env::temp_dir().join(format!("wb-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        for f in ["src/api.rs", "src/db.rs", "src/ui.rs"] {
            std::fs::write(dir.join(f), "old\n").unwrap();
        }
        git(&dir, &["init", "-q"]);
        git(&dir, &["add", "."]);
        git(&dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"]);
        std::fs::write(dir.join("src/ui.rs"), "already being edited\n").unwrap();

        let g = Graph { nodes: vec![node("api", &["src/api.rs"]), node("db", &["src/db.rs"])], ..Default::default() };
        let scope = start(&dir, &g, vec![(None, "api".into())]);

        // The run edits its own file, a file it wasn't scoped to, a file that was dirty
        // already, and adds a node with a new file plus a stray new file.
        std::fs::write(dir.join("src/api.rs"), "new\n").unwrap();
        std::fs::write(dir.join("src/db.rs"), "new\n").unwrap();
        std::fs::write(dir.join("src/ui.rs"), "still being edited\n").unwrap();
        std::fs::write(dir.join("src/cache.rs"), "new\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "stray\n").unwrap();
        let mut after = g.clone();
        after.nodes.push(node("cache", &["src/cache.rs"]));

        let v = enforce(&dir, &scope, &after, true).unwrap();
        let paths: Vec<&str> = v.iter().map(|v| v.path.as_str()).collect();
        assert_eq!(paths, vec!["src/db.rs", "notes.txt"]);
        assert_eq!(std::fs::read_to_string(dir.join("src/db.rs")).unwrap(), "old\n");
        assert_eq!(std::fs::read_to_string(dir.join("src/api.rs")).unwrap(), "new\n");
        assert!(dir.join("notes.txt").exists());
        assert!(report(&v, true).unwrap().contains("`notes.txt` (new)"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One agent turn on a selected node, end to end without the app or a model: its intent
    /// rewrite becomes a proposal, its out-of-scope edit is undone, its work order declares
    /// the blast radius and the caller of the edited node is flagged as possibly stale.
    #[test]
    fn an_agent_turn_end_to_end() {
        use crate::{claims, graph, ledger, work_order};
        let dir = std::env::temp_dir().join(format!("wb-turn-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        for f in ["src/api.rs", "src/db.rs", "src/ui.rs"] {
            std::fs::write(dir.join(f), "old\n").unwrap();
        }
        git(&dir, &["init", "-q"]);
        git(&dir, &["add", "."]);
        git(&dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"]);
        let mut api = node("api", &["src/api.rs"]);
        api.links = vec!["db".into()];
        let g = Graph { nodes: vec![api, node("db", &["src/db.rs"]), node("ui", &["src/ui.rs"])], ..Default::default() };
        let known = ledger::record(&dir, &[], &g, ledger::Source::Person, Some("Anna"), 0).unwrap();

        // The person selects `db` and asks the agent to work on it.
        let scope = start(&dir, &g, vec![(None, "db".into())]);
        let mut after = g.clone();
        after.node_mut("db").unwrap().description = "the agent's own idea of db".into();
        std::fs::write(dir.join("src/db.rs"), "new\n").unwrap();
        std::fs::write(dir.join("src/ui.rs"), "new\n").unwrap();

        let entries = ledger::record(&dir, &known, &after, ledger::Source::Agent, Some("Anna"), 0).unwrap();
        assert_eq!(ledger::open_proposals(&entries).len(), 1);
        assert_eq!(ledger::materialize(&entries, &after).node("db").unwrap().description, g.node("db").unwrap().description);

        let v = enforce(&dir, &scope, &after, true).unwrap();
        assert_eq!(v, vec![Violation { path: "src/ui.rs".into(), untracked: false }]);
        assert_eq!(std::fs::read_to_string(dir.join("src/ui.rs")).unwrap(), "old\n");
        assert_eq!(std::fs::read_to_string(dir.join("src/db.rs")).unwrap(), "new\n");

        let changes = graph::diff(&g, &after);
        let orders = work_order::from_changes(&g, &after, &changes, &[]);
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].kind, work_order::Kind::Description);
        assert_eq!(orders[0].blast_radius.files, vec!["src/db.rs".to_string()]);
        assert_eq!(orders[0].blast_radius.dependents, vec![(None, "api".to_string())]);
        let stale = claims::stale(&after, &changes);
        assert_eq!(stale.iter().map(|s| s.node.1.as_str()).collect::<Vec<_>>(), vec!["api"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
