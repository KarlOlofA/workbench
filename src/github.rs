use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::claude::augmented_path;
use crate::git::{self, FileDiff};

pub const GITHUB_FILE: &str = ".workbench/github.json";

const PR_LIST_FIELDS: &str = "number,title,author,headRefName,baseRefName,state,isDraft,updatedAt,url,reviewDecision";
const PR_VIEW_FIELDS: &str = "number,title,body,author,state,isDraft,headRefName,baseRefName,mergeable,mergeStateStatus,reviewDecision,url,additions,deletions,changedFiles,comments,reviews,statusCheckRollup";
const REPO_FIELDS: &str = "nameWithOwner,description,url,defaultBranchRef";

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct Author {
    #[serde(default)]
    pub login: String,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct BranchRef {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Repo {
    pub name_with_owner: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub default_branch_ref: Option<BranchRef>,
}

impl Repo {
    pub fn default_branch(&self) -> String {
        self.default_branch_ref.as_ref().map(|b| b.name.clone()).filter(|b| !b.is_empty()).unwrap_or_else(|| "main".into())
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrSummary {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub author: Author,
    #[serde(default)]
    pub head_ref_name: String,
    #[serde(default)]
    pub base_ref_name: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub is_draft: bool,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub review_decision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    #[serde(default)]
    pub author: Author,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    #[serde(default)]
    pub author: Author,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub submitted_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: String,
    pub outcome: String,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrDetail {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub author: Author,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub is_draft: bool,
    #[serde(default)]
    pub head_ref_name: String,
    #[serde(default)]
    pub base_ref_name: String,
    #[serde(default)]
    pub mergeable: String,
    #[serde(default)]
    pub merge_state_status: String,
    #[serde(default)]
    pub review_decision: Option<String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
    #[serde(default)]
    pub changed_files: u64,
    #[serde(default)]
    pub comments: Vec<Comment>,
    #[serde(default)]
    pub reviews: Vec<Review>,
    #[serde(default)]
    status_check_rollup: Option<Vec<Value>>,
}

impl PrDetail {
    pub fn checks(&self) -> Vec<Check> {
        self.status_check_rollup
            .iter()
            .flatten()
            .map(|c| {
                let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                let name = [s("name"), s("context")].into_iter().find(|v| !v.is_empty()).unwrap_or_else(|| "check".into());
                let outcome = [s("conclusion"), s("state"), s("status")]
                    .into_iter()
                    .find(|v| !v.is_empty())
                    .unwrap_or_else(|| "PENDING".into());
                Check { name, outcome }
            })
            .collect()
    }

    pub fn is_open(&self) -> bool {
        self.state.eq_ignore_ascii_case("open")
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub kind: String,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == "dir"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Closed,
    Merged,
    All,
}

impl PrState {
    pub const ALL: [PrState; 4] = [PrState::Open, PrState::Closed, PrState::Merged, PrState::All];

    pub fn arg(self) -> &'static str {
        match self {
            PrState::Open => "open",
            PrState::Closed => "closed",
            PrState::Merged => "merged",
            PrState::All => "all",
        }
    }

    pub fn from_arg(s: &str) -> Self {
        Self::ALL.into_iter().find(|p| p.arg() == s).unwrap_or(PrState::Open)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewKind {
    Comment,
    Approve,
    RequestChanges,
}

impl ReviewKind {
    fn flag(self) -> &'static str {
        match self {
            ReviewKind::Comment => "--comment",
            ReviewKind::Approve => "--approve",
            ReviewKind::RequestChanges => "--request-changes",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMethod {
    Squash,
    Merge,
    Rebase,
}

impl MergeMethod {
    pub const ALL: [MergeMethod; 3] = [MergeMethod::Squash, MergeMethod::Merge, MergeMethod::Rebase];

    pub fn key(self) -> &'static str {
        match self {
            MergeMethod::Squash => "squash",
            MergeMethod::Merge => "merge",
            MergeMethod::Rebase => "rebase",
        }
    }

    pub fn from_key(s: &str) -> Self {
        Self::ALL.into_iter().find(|m| m.key() == s).unwrap_or(MergeMethod::Squash)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct NewPr {
    pub title: String,
    pub body: String,
    pub base: String,
    pub head: String,
    pub draft: bool,
}

fn find_gh_binary() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    [
        "/opt/homebrew/bin/gh".to_string(),
        "/usr/local/bin/gh".to_string(),
        format!("{home}/.local/bin/gh"),
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
    .unwrap_or_else(|| PathBuf::from("gh"))
}

fn run_gh(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new(find_gh_binary())
        .args(args)
        .current_dir(cwd)
        .env("PATH", augmented_path())
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("NO_COLOR", "1")
        .output()
        .map_err(|e| format!("gh: {e} (install the GitHub CLI and run `gh auth login`)"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { format!("gh exited with {}", out.status) } else { err })
    }
}

fn parse<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T, String> {
    serde_json::from_str(text).map_err(|e| format!("unexpected gh output: {e}"))
}

pub fn valid_repo(name: &str) -> bool {
    let mut parts = name.split('/');
    let ok = |p: Option<&str>| {
        p.is_some_and(|p| {
            !p.is_empty() && !p.starts_with(['-', '.']) && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

pub fn normalize_repo(input: &str) -> Option<String> {
    let s = input.trim().trim_end_matches('/').trim_end_matches(".git");
    let s = s
        .strip_prefix("https://github.com/")
        .or_else(|| s.strip_prefix("http://github.com/"))
        .or_else(|| s.strip_prefix("git@github.com:"))
        .or_else(|| s.strip_prefix("github.com/"))
        .unwrap_or(s);
    let s: String = s.split('/').take(2).collect::<Vec<_>>().join("/");
    valid_repo(&s).then_some(s)
}

fn repo_args<'a>(args: &mut Vec<&'a str>, repo: Option<&'a str>) -> Result<(), String> {
    if let Some(r) = repo {
        if !valid_repo(r) {
            return Err(format!("not a valid repository name: {r}"));
        }
        args.push("-R");
        args.push(r);
    }
    Ok(())
}

pub fn current_repo(cwd: &Path) -> Result<Repo, String> {
    parse(&run_gh(cwd, &["repo", "view", "--json", REPO_FIELDS])?)
}

pub fn view_repo(cwd: &Path, repo: &str) -> Result<Repo, String> {
    if !valid_repo(repo) {
        return Err(format!("not a valid repository name: {repo}"));
    }
    parse(&run_gh(cwd, &["repo", "view", repo, "--json", REPO_FIELDS])?)
}

pub fn list_prs(cwd: &Path, repo: Option<&str>, state: PrState) -> Result<Vec<PrSummary>, String> {
    let mut args = vec!["pr", "list", "--state", state.arg(), "--limit", "50", "--json", PR_LIST_FIELDS];
    repo_args(&mut args, repo)?;
    parse(&run_gh(cwd, &args)?)
}

pub fn view_pr(cwd: &Path, repo: Option<&str>, number: u64) -> Result<PrDetail, String> {
    let n = number.to_string();
    let mut args = vec!["pr", "view", n.as_str(), "--json", PR_VIEW_FIELDS];
    repo_args(&mut args, repo)?;
    parse(&run_gh(cwd, &args)?)
}

pub fn pr_diff(cwd: &Path, repo: Option<&str>, number: u64) -> Result<Vec<FileDiff>, String> {
    let n = number.to_string();
    let mut args = vec!["pr", "diff", n.as_str(), "--color", "never"];
    repo_args(&mut args, repo)?;
    Ok(git::parse_diff(&run_gh(cwd, &args)?))
}

pub fn create_pr(cwd: &Path, pr: &NewPr) -> Result<String, String> {
    if pr.title.trim().is_empty() {
        return Err("a pull request needs a title".into());
    }
    let mut args = vec!["pr", "create", "--title", pr.title.trim(), "--body", pr.body.as_str()];
    if !pr.base.trim().is_empty() {
        args.extend(["--base", pr.base.trim()]);
    }
    if !pr.head.trim().is_empty() {
        args.extend(["--head", pr.head.trim()]);
    }
    if pr.draft {
        args.push("--draft");
    }
    run_gh(cwd, &args).map(|s| s.trim().to_string())
}

pub fn comment(cwd: &Path, repo: Option<&str>, number: u64, body: &str) -> Result<(), String> {
    if body.trim().is_empty() {
        return Err("the comment is empty".into());
    }
    let n = number.to_string();
    let mut args = vec!["pr", "comment", n.as_str(), "--body", body];
    repo_args(&mut args, repo)?;
    run_gh(cwd, &args).map(|_| ())
}

pub fn review(cwd: &Path, repo: Option<&str>, number: u64, kind: ReviewKind, body: &str) -> Result<(), String> {
    if kind != ReviewKind::Approve && body.trim().is_empty() {
        return Err("write a review comment first".into());
    }
    let n = number.to_string();
    let mut args = vec!["pr", "review", n.as_str(), kind.flag()];
    if !body.trim().is_empty() {
        args.extend(["--body", body]);
    }
    repo_args(&mut args, repo)?;
    run_gh(cwd, &args).map(|_| ())
}

pub fn merge(cwd: &Path, repo: Option<&str>, number: u64, method: MergeMethod, delete_branch: bool) -> Result<String, String> {
    let n = number.to_string();
    let flag = format!("--{}", method.key());
    let mut args = vec!["pr", "merge", n.as_str(), flag.as_str()];
    if delete_branch {
        args.push("--delete-branch");
    }
    repo_args(&mut args, repo)?;
    run_gh(cwd, &args).map(|s| s.trim().to_string())
}

fn encode_path(path: &str) -> String {
    let mut out = String::new();
    for b in path.trim_matches('/').bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn contents_endpoint(repo: &str, path: &str) -> Result<String, String> {
    if !valid_repo(repo) {
        return Err(format!("not a valid repository name: {repo}"));
    }
    if path.split('/').any(|seg| seg == "..") {
        return Err("invalid path".into());
    }
    let p = encode_path(path);
    Ok(if p.is_empty() { format!("repos/{repo}/contents") } else { format!("repos/{repo}/contents/{p}") })
}

pub fn list_contents(cwd: &Path, repo: &str, path: &str) -> Result<Vec<Entry>, String> {
    let endpoint = contents_endpoint(repo, path)?;
    let text = run_gh(cwd, &["api", endpoint.as_str()])?;
    let mut entries: Vec<Entry> = match parse::<Value>(&text)? {
        Value::Array(_) => parse(&text)?,
        _ => return Err(format!("{path} is a file, not a directory")),
    };
    entries.sort_by(|a, b| b.is_dir().cmp(&a.is_dir()).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(entries)
}

pub fn read_file(cwd: &Path, repo: &str, path: &str) -> Result<String, String> {
    read_file_at(cwd, repo, path, None)
}

pub fn valid_ref(r: &str) -> bool {
    !r.is_empty()
        && !r.starts_with(['-', '/', '.'])
        && !r.contains("..")
        && r.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
}

pub fn read_file_at(cwd: &Path, repo: &str, path: &str, git_ref: Option<&str>) -> Result<String, String> {
    let mut endpoint = contents_endpoint(repo, path)?;
    if let Some(r) = git_ref {
        if !valid_ref(r) {
            return Err(format!("not a valid git ref: {r}"));
        }
        endpoint.push_str(&format!("?ref={}", encode_path(r)));
    }
    run_gh(cwd, &["api", "-H", "Accept: application/vnd.github.raw+json", endpoint.as_str()])
}

pub fn is_not_found(err: &str) -> bool {
    err.contains("Not Found") || err.contains("HTTP 404")
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
struct GithubFile {
    #[serde(default)]
    related: Vec<String>,
}

pub fn load_related(cwd: &Path) -> Result<Vec<String>, String> {
    match std::fs::read_to_string(cwd.join(GITHUB_FILE)) {
        Ok(text) => serde_json::from_str::<GithubFile>(&text)
            .map(|f| f.related.into_iter().filter(|r| valid_repo(r)).collect())
            .map_err(|e| format!("{GITHUB_FILE}: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{GITHUB_FILE}: {e}")),
    }
}

pub fn save_related(cwd: &Path, related: &[String]) -> Result<(), String> {
    let path = cwd.join(GITHUB_FILE);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut text = serde_json::to_string_pretty(&GithubFile { related: related.to_vec() }).map_err(|e| e.to_string())?;
    text.push('\n');
    std::fs::write(path, text).map_err(|e| format!("{GITHUB_FILE}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names_are_validated_and_normalized() {
        assert!(valid_repo("codebrain/workbench"));
        assert!(!valid_repo("-R/evil"));
        assert!(!valid_repo("a/b/c"));
        assert!(!valid_repo("../x"));
        assert_eq!(normalize_repo("https://github.com/rust-lang/rust.git").as_deref(), Some("rust-lang/rust"));
        assert_eq!(normalize_repo("git@github.com:a/b").as_deref(), Some("a/b"));
        assert_eq!(normalize_repo("https://github.com/a/b/pull/3").as_deref(), Some("a/b"));
        assert_eq!(normalize_repo("nope"), None);
    }

    #[test]
    fn contents_paths_are_encoded_and_contained() {
        assert_eq!(contents_endpoint("a/b", "").unwrap(), "repos/a/b/contents");
        assert_eq!(contents_endpoint("a/b", "/src/my file.rs").unwrap(), "repos/a/b/contents/src/my%20file.rs");
        assert!(contents_endpoint("a/b", "../../user").is_err());
        assert!(valid_ref("main"));
        assert!(valid_ref("release/1.2"));
        assert!(!valid_ref("--upload-pack"));
        assert!(!valid_ref("a/../b"));
        assert!(!valid_ref("x&y=1"));
    }

    #[test]
    fn parses_pr_detail_and_checks() {
        let pr: PrDetail = serde_json::from_str(
            r#"{"number":7,"title":"Add X","body":"b","author":{"login":"karl"},"state":"OPEN","isDraft":false,
                "headRefName":"x","baseRefName":"main","mergeable":"MERGEABLE","reviewDecision":"",
                "comments":[{"author":{"login":"a"},"body":"hi","createdAt":"t"}],"reviews":[],
                "statusCheckRollup":[{"__typename":"CheckRun","name":"ci","status":"COMPLETED","conclusion":"SUCCESS"},
                                     {"__typename":"StatusContext","context":"lint","state":"PENDING"}]}"#,
        )
        .unwrap();
        assert!(pr.is_open());
        let checks = pr.checks();
        assert_eq!(checks[0], Check { name: "ci".into(), outcome: "SUCCESS".into() });
        assert_eq!(checks[1], Check { name: "lint".into(), outcome: "PENDING".into() });
    }

    #[test]
    fn parses_pr_list() {
        let prs: Vec<PrSummary> = serde_json::from_str(
            r#"[{"number":1,"title":"T","author":{"login":"k"},"headRefName":"h","baseRefName":"main","state":"OPEN","isDraft":true,"updatedAt":"2026-01-01","url":"u","reviewDecision":null}]"#,
        )
        .unwrap();
        assert!(prs[0].is_draft);
        assert_eq!(prs[0].review_decision, None);
    }
}
