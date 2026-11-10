use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::github;
use crate::graph::{self, GRAPH_FILE, Graph, Node, RefNode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registry {
    Npm,
    Crates,
    PyPi,
    Go,
}

impl Registry {
    pub const ALL: [Registry; 4] = [Registry::Npm, Registry::Crates, Registry::PyPi, Registry::Go];

    fn prefixes(self) -> &'static [&'static str] {
        match self {
            Registry::Npm => &["npm:"],
            Registry::Crates => &["crate:", "crates:", "cargo:"],
            Registry::PyPi => &["pypi:", "pip:"],
            Registry::Go => &["go:"],
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Registry::Npm => "npm package",
            Registry::Crates => "Rust crate",
            Registry::PyPi => "Python package",
            Registry::Go => "Go module",
        }
    }

    pub fn docs_url(self, name: &str) -> String {
        match self {
            Registry::Npm => format!("https://www.npmjs.com/package/{name}"),
            Registry::Crates => format!("https://docs.rs/{name}"),
            Registry::PyPi => format!("https://pypi.org/project/{name}"),
            Registry::Go => format!("https://pkg.go.dev/{name}"),
        }
    }

    fn local_dir(self, cwd: &Path, name: &str) -> Option<PathBuf> {
        match self {
            Registry::Npm => Some(cwd.join("node_modules").join(name)).filter(|p| p.is_dir()),
            Registry::Crates => cargo_crate_dir(name),
            Registry::PyPi | Registry::Go => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Local(PathBuf),
    GitHub { repo: String, git_ref: Option<String>, path: String },
    Web(String),
    Package { registry: Registry, name: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct RefGraph {
    pub root: String,
    pub location: String,
    pub graph: Graph,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RefState {
    Loading,
    Ready(Option<RefGraph>),
    Failed(String),
}

pub type RefGraphs = HashMap<String, RefState>;

pub struct Refs<'a> {
    pub cwd: &'a Path,
    pub graphs: &'a RefGraphs,
}

impl Refs<'_> {
    pub fn graph(&self, source: &str) -> Option<&RefGraph> {
        match self.graphs.get(&key(source)) {
            Some(RefState::Ready(Some(g))) => Some(g),
            _ => None,
        }
    }
}

pub fn key(source: &str) -> String {
    source.trim().to_string()
}

fn valid_package(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['-', '/', '.'])
        && !name.contains("..")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.@/".contains(c))
}

fn safe_path(path: &str) -> bool {
    !path.split('/').any(|seg| seg == "..")
}

fn home() -> Option<PathBuf> {
    std::env::var("HOME").ok().filter(|h| !h.is_empty()).map(PathBuf::from)
}

fn expand(cwd: &Path, s: &str) -> PathBuf {
    let s = s.strip_prefix("file://").unwrap_or(s);
    if s == "~" {
        return home().unwrap_or_else(|| PathBuf::from(s));
    }
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(h) = home()
    {
        return h.join(rest);
    }
    let p = PathBuf::from(s);
    if p.is_absolute() { p } else { cwd.join(p) }
}

fn github_source(repo: &str, git_ref: Option<&str>, path: &str) -> Option<Source> {
    let repo = repo.trim_end_matches(".git");
    let path = path.trim_matches('/');
    let git_ref = git_ref.filter(|r| !r.is_empty());
    (github::valid_repo(repo) && git_ref.is_none_or(github::valid_ref) && safe_path(path)).then(|| Source::GitHub {
        repo: repo.to_string(),
        git_ref: git_ref.map(String::from),
        path: path.to_string(),
    })
}

fn parse_github_url(rest: &str) -> Option<Source> {
    let rest = rest.split(['?', '#']).next().unwrap_or("").trim_matches('/');
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() < 2 {
        return None;
    }
    let repo = format!("{}/{}", parts[0], parts[1].trim_end_matches(".git"));
    match parts.get(2) {
        Some(&"tree") | Some(&"blob") if parts.len() > 3 => github_source(&repo, Some(parts[3]), &parts[4..].join("/")),
        _ => github_source(&repo, None, ""),
    }
}

fn parse_github_short(rest: &str) -> Option<Source> {
    let parts: Vec<&str> = rest.trim().trim_matches('/').splitn(3, '/').collect();
    if parts.len() < 2 {
        return None;
    }
    let (name, git_ref) = match parts[1].split_once('@') {
        Some((n, r)) => (n, Some(r)),
        None => (parts[1], None),
    };
    github_source(&format!("{}/{name}", parts[0]), git_ref, parts.get(2).copied().unwrap_or(""))
}

pub fn parse(cwd: &Path, input: &str) -> Option<Source> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    for registry in Registry::ALL {
        for prefix in registry.prefixes() {
            if let Some(name) = s.strip_prefix(prefix) {
                let name = name.trim();
                return valid_package(name).then(|| Source::Package { registry, name: name.to_string() });
            }
        }
    }
    if let Some(rest) = s.strip_prefix("github:") {
        return parse_github_short(rest);
    }
    let lower = s.to_ascii_lowercase();
    for prefix in ["https://github.com/", "http://github.com/", "https://www.github.com/", "github.com/", "git@github.com:"] {
        if lower.starts_with(prefix) {
            return parse_github_url(&s[prefix.len()..]);
        }
    }
    if lower.starts_with("https://") || lower.starts_with("http://") {
        return Some(Source::Web(s.to_string()));
    }
    let path = expand(cwd, s);
    if !path.exists() && github::valid_repo(s) && !s.starts_with('.') {
        return github_source(s, None, "");
    }
    Some(Source::Local(path))
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Source::Local(p) if p.is_dir() => "Local folder".into(),
            Source::Local(p) if p.is_file() => "Local file".into(),
            Source::Local(_) => "Local path (not found)".into(),
            Source::GitHub { .. } => "GitHub repository".into(),
            Source::Web(_) => "Website".into(),
            Source::Package { registry, .. } => registry.label().into(),
        }
    }

    pub fn display(&self) -> String {
        match self {
            Source::Local(p) => p.display().to_string(),
            Source::GitHub { repo, git_ref, path } => {
                let at = git_ref.as_deref().map(|r| format!("@{r}")).unwrap_or_default();
                let sub = if path.is_empty() { String::new() } else { format!(" › {path}") };
                format!("{repo}{at}{sub}")
            }
            Source::Web(url) => url.clone(),
            Source::Package { name, .. } => name.clone(),
        }
    }

    pub fn short_name(&self) -> String {
        let last = |s: &str| s.trim_end_matches('/').rsplit('/').next().unwrap_or(s).to_string();
        match self {
            Source::Local(p) => p.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_else(|| p.display().to_string()),
            Source::GitHub { repo, path, .. } if path.is_empty() => last(repo),
            Source::GitHub { repo, path, .. } => format!("{} › {}", last(repo), last(path)),
            Source::Web(url) => url.split("://").nth(1).unwrap_or(url.as_str()).split('/').next().unwrap_or(url.as_str()).to_string(),
            Source::Package { name, .. } => name.clone(),
        }
    }

    pub fn url(&self) -> Option<String> {
        match self {
            Source::Local(_) => None,
            Source::GitHub { repo, git_ref, path } => Some(if path.is_empty() && git_ref.is_none() {
                format!("https://github.com/{repo}")
            } else {
                format!("https://github.com/{repo}/tree/{}/{path}", git_ref.as_deref().unwrap_or("HEAD"))
            }),
            Source::Web(url) => Some(url.clone()),
            Source::Package { registry, name } => Some(registry.docs_url(name)),
        }
    }

    pub fn how_to_read(&self, cwd: &Path) -> String {
        match self {
            Source::Local(p) if p.is_file() => format!("It is a local file: read `{}` directly when you need it.", p.display()),
            Source::Local(p) => format!(
                "It is a local folder: read files under `{}` directly with your file tools when you need them.",
                p.display()
            ),
            Source::GitHub { repo, git_ref, path } => {
                let q = git_ref.as_deref().map(|r| format!("?ref={r}")).unwrap_or_default();
                format!(
                    "It is a GitHub repository: read it with the GitHub CLI, e.g. `gh api repos/{repo}/contents/{path}{q}` to list a folder, \
                     the same with `-H 'Accept: application/vnd.github.raw+json'` for a file's contents, or `gh search code --repo {repo} QUERY`. Never push to it."
                )
            }
            Source::Web(url) => format!("It is a website: fetch `{url}` with your web tools when you need it."),
            Source::Package { registry, name } => match registry.local_dir(cwd, name) {
                Some(dir) => format!(
                    "It is the {} `{name}`. Its installed sources are at `{}`; its documentation is at {}.",
                    registry.label(),
                    dir.display(),
                    registry.docs_url(name)
                ),
                None => format!("It is the {} `{name}`. Its documentation is at {}.", registry.label(), registry.docs_url(name)),
            },
        }
    }
}

fn cargo_crate_dir(name: &str) -> Option<PathBuf> {
    let cargo = std::env::var("CARGO_HOME").ok().map(PathBuf::from).or_else(|| home().map(|h| h.join(".cargo")))?;
    let prefix = format!("{name}-");
    let mut best: Option<(Vec<u64>, PathBuf)> = None;
    for registry in std::fs::read_dir(cargo.join("registry/src")).ok()?.flatten() {
        for entry in std::fs::read_dir(registry.path()).into_iter().flatten().flatten() {
            let file_name = entry.file_name().to_string_lossy().to_string();
            let Some(version) = file_name.strip_prefix(&prefix) else { continue };
            if !version.starts_with(|c: char| c.is_ascii_digit()) {
                continue;
            }
            let v: Vec<u64> = version.split(|c: char| !c.is_ascii_digit()).filter_map(|p| p.parse().ok()).collect();
            if best.as_ref().is_none_or(|(b, _)| v > *b) {
                best = Some((v, entry.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

fn load_dir(dir: &Path) -> Result<Option<RefGraph>, String> {
    Ok(graph::load(dir, GRAPH_FILE)?.map(|graph| RefGraph {
        root: dir.display().to_string(),
        location: dir.join(GRAPH_FILE).display().to_string(),
        graph,
    }))
}

fn load_local(cwd: &Path, path: &Path) -> Result<Option<RefGraph>, String> {
    let start = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
    for dir in start.ancestors() {
        if dir == cwd {
            return Ok(None);
        }
        if dir.join(GRAPH_FILE).is_file() {
            return load_dir(dir);
        }
    }
    Ok(None)
}

fn github_candidates(path: &str) -> Vec<String> {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut out: Vec<String> = (1..=segs.len()).rev().take(3).map(|n| segs[..n].join("/")).collect();
    out.push(String::new());
    out
}

fn load_github(cwd: &Path, repo: &str, git_ref: Option<&str>, path: &str) -> Result<Option<RefGraph>, String> {
    for dir in github_candidates(path) {
        let file = if dir.is_empty() { GRAPH_FILE.to_string() } else { format!("{dir}/{GRAPH_FILE}") };
        match github::read_file_at(cwd, repo, &file, git_ref) {
            Ok(text) => {
                let graph: Graph = serde_json::from_str(&text).map_err(|e| format!("{repo}/{file}: {e}"))?;
                let at = git_ref.map(|r| format!("@{r}")).unwrap_or_default();
                let root = if dir.is_empty() { format!("{repo}{at}") } else { format!("{repo}{at}/{dir}") };
                return Ok(Some(RefGraph { location: format!("github.com/{repo}/{file}{at}"), root, graph }));
            }
            Err(e) if github::is_not_found(&e) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

pub fn load(cwd: &Path, source: &Source) -> Result<Option<RefGraph>, String> {
    match source {
        Source::Local(path) => load_local(cwd, path),
        Source::GitHub { repo, git_ref, path } => load_github(cwd, repo, git_ref.as_deref(), path),
        Source::Web(_) => Ok(None),
        Source::Package { registry, name } => match registry.local_dir(cwd, name) {
            Some(dir) => load_dir(&dir),
            None => Ok(None),
        },
    }
}

pub fn find<'a>(g: &'a Graph, r: &RefNode) -> Option<&'a Node> {
    g.net(r.module.as_deref())?.node(&r.id)
}

pub fn choices(g: &Graph) -> Vec<(RefNode, String)> {
    g.all_nodes()
        .into_iter()
        .filter(|(_, n)| n.module.is_none())
        .map(|(scope, n)| {
            let label = match scope {
                Some(m) => format!("▣ {} › {}", g.module_title(m), n.title),
                None => n.title.clone(),
            };
            (RefNode { id: n.id.clone(), module: scope.map(String::from) }, label)
        })
        .collect()
}

pub fn label(g: Option<&Graph>, r: &RefNode) -> String {
    let title = g.and_then(|g| find(g, r)).map(|n| n.title.clone()).unwrap_or_else(|| r.id.clone());
    match (&r.module, g) {
        (Some(m), Some(g)) => format!("▣ {} › {title}", g.module_title(m)),
        (Some(m), None) => format!("▣ {m} › {title}"),
        (None, _) => title,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> PathBuf {
        PathBuf::from("/tmp/wb-project-that-does-not-exist")
    }

    #[test]
    fn parses_github_sources() {
        let c = cwd();
        let gh = |repo: &str, r: Option<&str>, path: &str| Source::GitHub { repo: repo.into(), git_ref: r.map(String::from), path: path.into() };
        assert_eq!(parse(&c, "https://github.com/acme/shared"), Some(gh("acme/shared", None, "")));
        assert_eq!(parse(&c, "https://github.com/acme/shared.git"), Some(gh("acme/shared", None, "")));
        assert_eq!(parse(&c, "https://github.com/acme/shared/tree/main/crates/auth"), Some(gh("acme/shared", Some("main"), "crates/auth")));
        assert_eq!(parse(&c, "https://github.com/acme/shared/blob/v1/README.md#top"), Some(gh("acme/shared", Some("v1"), "README.md")));
        assert_eq!(parse(&c, "github:acme/shared@dev/docs"), Some(gh("acme/shared", Some("dev"), "docs")));
        assert_eq!(parse(&c, "git@github.com:acme/shared.git"), Some(gh("acme/shared", None, "")));
        assert_eq!(parse(&c, "acme/shared"), Some(gh("acme/shared", None, "")));
        assert_eq!(parse(&c, "https://github.com/acme/shared/tree/--evil/x"), None);
        assert_eq!(parse(&c, "github:acme/shared/../../x"), None);
    }

    #[test]
    fn parses_web_package_and_local_sources() {
        let c = cwd();
        assert_eq!(parse(&c, "https://docs.rs/serde"), Some(Source::Web("https://docs.rs/serde".into())));
        assert_eq!(parse(&c, "npm:@tanstack/query"), Some(Source::Package { registry: Registry::Npm, name: "@tanstack/query".into() }));
        assert_eq!(parse(&c, "crate:serde"), Some(Source::Package { registry: Registry::Crates, name: "serde".into() }));
        assert_eq!(parse(&c, "npm:../x"), None);
        assert_eq!(parse(&c, "/opt/thing"), Some(Source::Local(PathBuf::from("/opt/thing"))));
        assert_eq!(parse(&c, "./docs/spec.md"), Some(Source::Local(c.join("./docs/spec.md"))));
        assert_eq!(parse(&c, "  "), None);
        assert!(parse(&c, "https://github.com/a/b").unwrap().url().unwrap().ends_with("github.com/a/b"));
    }

    #[test]
    fn github_lookup_walks_up_a_few_levels() {
        assert_eq!(github_candidates(""), vec![String::new()]);
        assert_eq!(github_candidates("a/b"), vec!["a/b".to_string(), "a".into(), String::new()]);
        assert_eq!(github_candidates("a/b/c/d").len(), 4);
    }

    #[test]
    fn local_lookup_finds_the_nearest_workbench_graph() {
        let root = std::env::temp_dir().join(format!("wb-ref-test-{}", std::process::id()));
        let nested = root.join("other/src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join("other/.workbench")).unwrap();
        std::fs::write(root.join("other").join(GRAPH_FILE), r#"{"nodes":[{"id":"x","title":"X"}],"modules":[{"id":"m","title":"M","nodes":[{"id":"y","title":"Y"}]}]}"#).unwrap();
        let found = load(&cwd(), &Source::Local(nested.clone())).unwrap().unwrap();
        assert_eq!(found.root, root.join("other").display().to_string());
        let picks = choices(&found.graph);
        assert_eq!(picks.len(), 2);
        assert_eq!(picks[1].1, "▣ M › Y");
        assert_eq!(find(&found.graph, &picks[1].0).unwrap().title, "Y");
        assert!(load(&root.join("other"), &Source::Local(nested)).unwrap().is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
