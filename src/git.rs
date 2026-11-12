use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub struct StatusEntry {
    pub code: String,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
    Meta,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileDiff {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

impl Hunk {
    pub fn to_patch(&self) -> String {
        let mut out = self.header.clone();
        for l in &self.lines {
            out.push('\n');
            out.push(match l.kind {
                LineKind::Added => '+',
                LineKind::Removed => '-',
                LineKind::Context => ' ',
                LineKind::Meta => '\\',
            });
            out.push_str(&l.text);
        }
        out
    }
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() || args.contains(&"--no-index") {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn is_repo(cwd: &Path) -> bool {
    run_git(cwd, &["rev-parse", "--is-inside-work-tree"]).is_ok()
}

/// The local user's name for attributing shared edits: `git config user.name`, else $USER.
pub fn user_name(cwd: &Path) -> Option<String> {
    run_git(cwd, &["config", "user.name"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("USER").ok().filter(|s| !s.is_empty()))
}

pub fn current_branch(cwd: &Path) -> Option<String> {
    run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
}

pub fn branches(cwd: &Path) -> Result<Vec<String>, String> {
    let current = current_branch(cwd).unwrap_or_default();
    let out = run_git(cwd, &["for-each-ref", "--sort=-committerdate", "--format=%(refname)", "refs/heads", "refs/remotes"])?;
    Ok(list_branches(&out, &current))
}

fn list_branches(refs: &str, current: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for r in refs.lines().map(str::trim) {
        let name = if let Some(local) = r.strip_prefix("refs/heads/") {
            local
        } else if let Some(remote) = r.strip_prefix("refs/remotes/") {
            match remote.split_once('/') {
                Some((_, "HEAD")) | None => continue,
                Some((_, b)) if b == current => continue,
                _ => remote,
            }
        } else {
            continue;
        };
        if name != current && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
}

#[derive(Debug, Clone, PartialEq)]
pub struct SwitchTarget {
    pub name: String,
    pub remote: Option<String>,
}

pub fn switch_targets(cwd: &Path) -> Result<Vec<SwitchTarget>, String> {
    let current = current_branch(cwd).unwrap_or_default();
    let out = run_git(cwd, &["for-each-ref", "--sort=-committerdate", "--format=%(refname)", "refs/heads", "refs/remotes"])?;
    Ok(list_switch_targets(&out, &current))
}

fn list_switch_targets(refs: &str, current: &str) -> Vec<SwitchTarget> {
    let refs: Vec<&str> = refs.lines().map(str::trim).collect();
    let locals: Vec<&str> = refs.iter().filter_map(|r| r.strip_prefix("refs/heads/")).collect();
    let mut out: Vec<SwitchTarget> = Vec::new();
    for r in &refs {
        let target = if let Some(local) = r.strip_prefix("refs/heads/") {
            SwitchTarget { name: local.to_string(), remote: None }
        } else if let Some(remote) = r.strip_prefix("refs/remotes/") {
            match remote.split_once('/') {
                Some((_, "HEAD")) | None => continue,
                Some((_, b)) if locals.contains(&b) => continue,
                Some((_, b)) => SwitchTarget { name: b.to_string(), remote: Some(remote.to_string()) },
            }
        } else {
            continue;
        };
        if target.name != current && !out.iter().any(|t| t.name == target.name) {
            out.push(target);
        }
    }
    out
}

pub fn switch_branch(cwd: &Path, target: &SwitchTarget) -> Result<(), String> {
    match &target.remote {
        Some(remote) => run_git(cwd, &["switch", "--track", remote]),
        None => run_git(cwd, &["switch", &target.name]),
    }
    .map(|_| ())
}

pub fn create_branch(cwd: &Path, name: &str) -> Result<(), String> {
    run_git(cwd, &["switch", "-c", name]).map(|_| ())
}

pub fn merge_base(cwd: &Path, a: &str, b: &str) -> Option<String> {
    run_git(cwd, &["merge-base", a, b]).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn unique_lines(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !out.iter().any(|o| o == l) {
            out.push(l.to_string());
        }
    }
    out
}

/// Who committed to `branch` since it split from HEAD, newest first. Empty when HEAD has it all.
pub fn authors_since_merge_base(cwd: &Path, branch: &str) -> Vec<String> {
    run_git(cwd, &["log", "--format=%an", &format!("HEAD..{branch}")]).map(|s| unique_lines(&s)).unwrap_or_default()
}

/// The files `branch` changed since it split from HEAD.
pub fn changed_since_merge_base(cwd: &Path, branch: &str) -> Vec<String> {
    run_git(cwd, &["diff", "--name-only", &format!("HEAD...{branch}")]).map(|s| unique_lines(&s)).unwrap_or_default()
}

/// Whether `path` has an unresolved merge conflict.
pub fn unmerged(cwd: &Path, path: &str) -> bool {
    run_git(cwd, &["ls-files", "--unmerged", "--", path]).is_ok_and(|s| !s.trim().is_empty())
}

/// Marks `paths` as resolved by staging them.
pub fn add(cwd: &Path, paths: &[&str]) -> Result<(), String> {
    let mut args = vec!["add", "--"];
    args.extend_from_slice(paths);
    run_git(cwd, &args).map(|_| ())
}

pub fn show_file(cwd: &Path, rev: &str, path: &str) -> Result<Option<String>, String> {
    let spec = format!("{rev}:{path}");
    if run_git(cwd, &["cat-file", "-e", &spec]).is_err() {
        return Ok(None);
    }
    run_git(cwd, &["show", &spec]).map(Some)
}

/// A commit that added lines to a file, as shown in the node history.
#[derive(Debug, Clone, PartialEq)]
pub struct CommitInfo {
    pub sha: String,
    pub short: String,
    pub author: String,
    pub at: u64,
    pub subject: String,
}

/// For an append-only JSON-lines file whose lines carry a `uid`, maps each uid to the
/// commit (reachable from HEAD) that first added its line. Uids missing from the map are
/// not committed on this branch yet.
pub fn line_commits(cwd: &Path, path: &str) -> Result<std::collections::HashMap<String, CommitInfo>, String> {
    if run_git(cwd, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_err() {
        return Ok(Default::default());
    }
    let out = run_git(
        cwd,
        &["log", "--reverse", "-p", "--no-color", "--no-ext-diff", "--format=%x1e%H%x1f%h%x1f%an%x1f%at%x1f%s", "--", path],
    )?;
    Ok(parse_line_commits(&out))
}

pub fn parse_line_commits(log: &str) -> std::collections::HashMap<String, CommitInfo> {
    let mut map = std::collections::HashMap::new();
    for chunk in log.split('\x1e').filter(|c| !c.trim().is_empty()) {
        let mut lines = chunk.lines();
        let Some(header) = lines.next() else { continue };
        let f: Vec<&str> = header.splitn(5, '\x1f').collect();
        let [sha, short, author, at, subject] = f[..] else { continue };
        let commit = CommitInfo {
            sha: sha.into(),
            short: short.into(),
            author: author.into(),
            at: at.trim().parse().unwrap_or(0),
            subject: subject.into(),
        };
        for l in lines {
            let Some(added) = l.strip_prefix('+').filter(|_| !l.starts_with("+++")) else { continue };
            let uid = serde_json::from_str::<serde_json::Value>(added)
                .ok()
                .and_then(|v| v.get("uid").and_then(|u| u.as_str()).map(String::from));
            if let Some(uid) = uid {
                map.entry(uid).or_insert_with(|| commit.clone());
            }
        }
    }
    map
}

pub fn status(cwd: &Path) -> Result<Vec<StatusEntry>, String> {
    let out = run_git(cwd, &["status", "--porcelain=v1", "--untracked-files=all"])?;
    Ok(out
        .lines()
        .filter(|l| l.len() > 3)
        .map(|l| {
            let path = l[3..].to_string();
            let path = path.rsplit(" -> ").next().unwrap_or(&path).trim_matches('"').to_string();
            StatusEntry { code: l[..2].to_string(), path }
        })
        .collect())
}

pub fn push_current_branch(cwd: &Path) -> Result<String, String> {
    let out = Command::new("git")
        .args(["push", "-u", "origin", "HEAD"])
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| e.to_string())?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() { Ok(text.trim().to_string()) } else { Err(text.trim().to_string()) }
}

pub fn ls_files(cwd: &Path) -> Result<Vec<String>, String> {
    let out = run_git(cwd, &["ls-files", "--cached", "--others", "--exclude-standard"])?;
    Ok(out.lines().map(String::from).collect())
}

pub fn diff(cwd: &Path, base: &str) -> Result<Vec<FileDiff>, String> {
    let base = if base.trim().is_empty() { "HEAD" } else { base.trim() };
    let mut text = run_git(cwd, &["diff", "--no-color", "--no-ext-diff", base])
        .or_else(|_| run_git(cwd, &["diff", "--no-color", "--no-ext-diff"]))?;
    let untracked: Vec<String> = status(cwd)?
        .into_iter()
        .filter(|e| e.code == "??")
        .map(|e| e.path)
        .take(100)
        .collect();
    for path in untracked {
        if let Ok(t) = run_git(cwd, &["diff", "--no-color", "--no-index", "--", "/dev/null", &path]) {
            text.push_str(&t);
        }
    }
    Ok(parse_diff(&text))
}

fn parse_range(s: &str) -> u32 {
    s.trim_start_matches(['-', '+'])
        .split(',')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

pub fn parse_diff(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut old_no = 0u32;
    let mut new_no = 0u32;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest
                .rsplit_once(" b/")
                .map(|(_, b)| b.to_string())
                .unwrap_or_else(|| rest.to_string());
            files.push(FileDiff { path, added: 0, removed: 0, binary: false, hunks: vec![] });
            continue;
        }
        let Some(file) = files.last_mut() else { continue };
        if let Some(rest) = line.strip_prefix("+++ ") {
            if let Some(p) = rest.strip_prefix("b/") {
                file.path = p.to_string();
            }
            continue;
        }
        if line.starts_with("--- ") && file.hunks.is_empty() {
            continue;
        }
        if line.starts_with("Binary files ") {
            file.binary = true;
            continue;
        }
        if line.starts_with("@@") {
            let mut parts = line.split_whitespace().skip(1);
            old_no = parts.next().map(parse_range).unwrap_or(0);
            new_no = parts.next().map(parse_range).unwrap_or(0);
            file.hunks.push(Hunk { header: line.to_string(), lines: vec![] });
            continue;
        }
        let Some(hunk) = file.hunks.last_mut() else { continue };
        let (kind, body) = match line.chars().next() {
            Some('+') => (LineKind::Added, &line[1..]),
            Some('-') => (LineKind::Removed, &line[1..]),
            Some(' ') => (LineKind::Context, &line[1..]),
            Some('\\') => (LineKind::Meta, line),
            None => (LineKind::Context, ""),
            _ => continue,
        };
        let (o, n) = match kind {
            LineKind::Added => {
                file.added += 1;
                new_no += 1;
                (None, Some(new_no - 1))
            }
            LineKind::Removed => {
                file.removed += 1;
                old_no += 1;
                (Some(old_no - 1), None)
            }
            LineKind::Context => {
                old_no += 1;
                new_no += 1;
                (Some(old_no - 1), Some(new_no - 1))
            }
            LineKind::Meta => (None, None),
        };
        hunk.lines.push(DiffLine { kind, old_no: o, new_no: n, text: body.to_string() });
    }
    files
}

/// One checkout of a repository, from `git worktree list`. The first one is the main checkout.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Worktree {
    pub path: PathBuf,
    /// The checked-out branch, without `refs/heads/`. `None` when detached or bare.
    pub branch: Option<String>,
    pub head: String,
    pub main: bool,
    pub bare: bool,
    pub locked: bool,
    /// Its folder is gone; `git worktree prune` would drop it.
    pub prunable: bool,
    /// Changed or untracked files, when the folder could be read.
    pub changes: Option<usize>,
}

impl Worktree {
    pub fn label(&self) -> String {
        match &self.branch {
            Some(b) => b.clone(),
            None if self.bare => "(bare)".into(),
            None => format!("detached {}", self.head.get(..7).unwrap_or(&self.head)),
        }
    }
}

pub fn parse_worktrees(text: &str) -> Vec<Worktree> {
    let mut out: Vec<Worktree> = Vec::new();
    for line in text.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        if key == "worktree" {
            out.push(Worktree { path: PathBuf::from(value), main: out.is_empty(), ..Default::default() });
            continue;
        }
        let Some(w) = out.last_mut() else { continue };
        match key {
            "HEAD" => w.head = value.to_string(),
            "branch" => w.branch = Some(value.strip_prefix("refs/heads/").unwrap_or(value).to_string()),
            "bare" => w.bare = true,
            "locked" => w.locked = true,
            "prunable" => w.prunable = true,
            _ => {}
        }
    }
    out
}

/// Every worktree of the repository at `repo` (which may itself be any of its worktrees),
/// with how many files each has changed.
pub fn worktrees(repo: &Path) -> Result<Vec<Worktree>, String> {
    let mut list = parse_worktrees(&run_git(repo, &["worktree", "list", "--porcelain"])?);
    for w in list.iter_mut().filter(|w| !w.bare && !w.prunable) {
        w.changes = run_git(&w.path, &["status", "--porcelain"]).ok().map(|s| s.lines().count());
    }
    Ok(list)
}

/// Where a new worktree for `branch` goes by default: next to the main checkout,
/// named `<repo>-<branch>` (slashes in the branch become dashes).
pub fn default_worktree_path(main: &Path, branch: &str) -> PathBuf {
    let name = main.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "repo".into());
    let slug: String = branch.trim().chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' }).collect();
    main.with_file_name(format!("{name}-{slug}"))
}

/// Adds a worktree at `path` on `branch`: checks out the branch if it exists, otherwise
/// creates it from the repository's current HEAD.
pub fn add_worktree(repo: &Path, path: &Path, branch: &str) -> Result<(), String> {
    let branch = branch.trim();
    if branch.is_empty() || branch.contains(char::is_whitespace) {
        return Err("Branch names can't be empty or contain spaces".into());
    }
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    let target = path.to_string_lossy().to_string();
    let exists = run_git(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok();
    if exists {
        run_git(repo, &["worktree", "add", &target, branch]).map(|_| ())
    } else {
        run_git(repo, &["worktree", "add", "-b", branch, &target]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_worktree_list() {
        let text = "worktree /code/app\nHEAD 1111111111\nbranch refs/heads/main\n\n\
                    worktree /code/app-feat\nHEAD 2222222222\nbranch refs/heads/feat/x\nlocked\n\n\
                    worktree /code/gone\nHEAD 3333333333abc\ndetached\nprunable gitdir file points to non-existent location\n";
        let w = parse_worktrees(text);
        assert_eq!(w.len(), 3);
        assert!(w[0].main && !w[1].main);
        assert_eq!(w[0].branch.as_deref(), Some("main"));
        assert_eq!(w[1].label(), "feat/x");
        assert!(w[1].locked);
        assert_eq!(w[2].label(), "detached 3333333");
        assert!(w[2].prunable);
        assert_eq!(default_worktree_path(Path::new("/code/app"), "feat/x y"), PathBuf::from("/code/app-feat-x-y"));
    }

    #[test]
    fn maps_history_lines_to_the_commit_that_added_them() {
        let log = "\x1eaaa111\x1faaa\x1fAda\x1f100\x1fInit\n\n\
            diff --git a/h.jsonl b/h.jsonl\n--- /dev/null\n+++ b/h.jsonl\n@@ -0,0 +1 @@\n+{\"uid\":\"u1\",\"id\":\"a\"}\n\
            \x1ebbb222\x1fbbb\x1fBo\x1f200\x1fMore: x\x1fy\n\n\
            diff --git a/h.jsonl b/h.jsonl\n--- a/h.jsonl\n+++ b/h.jsonl\n@@ -1 +1,2 @@\n {\"uid\":\"u1\",\"id\":\"a\"}\n+{\"uid\":\"u2\",\"id\":\"b\"}\n+not json\n\
            \x1eccc333\x1fccc\x1fAda\x1f300\x1fRe-add\n\n@@ -1,2 +1,2 @@\n-{\"uid\":\"u1\",\"id\":\"a\"}\n+{\"uid\":\"u1\",\"id\":\"a\"}\n";
        let m = parse_line_commits(log);
        assert_eq!(m.len(), 2);
        assert_eq!(m["u1"].short, "aaa");
        assert_eq!(m["u2"].short, "bbb");
        assert_eq!(m["u2"].at, 200);
        assert_eq!(m["u2"].subject, "More: x\x1fy");
        assert_eq!(m["u2"].author, "Bo");
    }

    #[test]
    fn parses_simple_diff() {
        let d = "diff --git a/x.rs b/x.rs\nindex 1..2 100644\n--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +1,2 @@\n fn a() {}\n-fn b() {}\n+fn c() {}\n";
        let files = parse_diff(d);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.path, "x.rs");
        assert_eq!((f.added, f.removed), (1, 1));
        let l = &f.hunks[0].lines;
        assert_eq!(l[0].old_no, Some(1));
        assert_eq!(l[1].old_no, Some(2));
        assert_eq!(l[2].new_no, Some(2));
    }

    #[test]
    fn lists_other_branches_without_current_or_head() {
        let refs = "refs/heads/feature/x\nrefs/heads/main\nrefs/remotes/origin/HEAD\nrefs/remotes/origin/main\nrefs/remotes/origin/spike\nrefs/heads/feature/x\n";
        assert_eq!(list_branches(refs, "main"), vec!["feature/x".to_string(), "origin/spike".to_string()]);
    }

    #[test]
    fn lists_switch_targets_preferring_local_branches() {
        let refs = "refs/heads/main\nrefs/heads/feature/x\nrefs/remotes/origin/HEAD\nrefs/remotes/origin/main\nrefs/remotes/origin/feature/x\nrefs/remotes/origin/spike\nrefs/remotes/fork/spike\n";
        let t = list_switch_targets(refs, "main");
        assert_eq!(
            t,
            vec![
                SwitchTarget { name: "feature/x".into(), remote: None },
                SwitchTarget { name: "spike".into(), remote: Some("origin/spike".into()) },
            ]
        );
    }

    #[test]
    fn parses_new_file() {
        let d = "diff --git a/dev/null b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hello\n";
        let files = parse_diff(d);
        assert_eq!(files[0].path, "new.txt");
        assert_eq!(files[0].hunks[0].lines[0].new_no, Some(1));
    }
}
