use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::github;
use crate::graph::{self, GRAPH_FILE, slugify};
use crate::screens;

const PROJECTS_FILE: &str = "projects.json";

pub const REPO_W: f64 = 190.0;
pub const REPO_H: f64 = 68.0;
pub const NOTE_GAP: f64 = 14.0;
pub const NOTE_W: f64 = 230.0;
const CELL_W: f64 = REPO_W + NOTE_GAP + NOTE_W + 70.0;
const CELL_H: f64 = 150.0;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ProjectRepo {
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pos: Option<[f64; 2]>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default)]
    pub repos: Vec<ProjectRepo>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RepoInfo {
    pub exists: bool,
    pub nodes: usize,
    pub modules: usize,
    pub has_graph: bool,
}

impl ProjectRepo {
    pub fn new(path: &Path) -> Self {
        Self { path: path.display().to_string(), github: github_remote(path), ..Default::default() }
    }

    pub fn dir(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }

    pub fn label(&self) -> String {
        if !self.title.trim().is_empty() {
            return self.title.clone();
        }
        self.dir().file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_else(|| self.path.clone())
    }

    pub fn is_at(&self, dir: &Path) -> bool {
        same_dir(&self.dir(), dir)
    }
}

impl Project {
    pub fn repo(&self, path: &str) -> Option<&ProjectRepo> {
        self.repos.iter().find(|r| r.path == path)
    }

    pub fn repo_mut(&mut self, path: &str) -> Option<&mut ProjectRepo> {
        self.repos.iter_mut().find(|r| r.path == path)
    }

    pub fn contains(&self, dir: &Path) -> bool {
        self.repos.iter().any(|r| r.is_at(dir))
    }

    pub fn add_repo(&mut self, dir: &Path) -> Result<String, String> {
        if self.contains(dir) {
            return Err(format!("{} is already in {}", dir.display(), self.name));
        }
        let repo = ProjectRepo::new(dir);
        let path = repo.path.clone();
        self.repos.push(repo);
        Ok(path)
    }

    pub fn positions(&self) -> HashMap<String, [f64; 2]> {
        let mut placed: HashMap<String, [f64; 2]> =
            self.repos.iter().filter_map(|r| r.pos.map(|p| (r.path.clone(), p))).collect();
        let columns = if self.repos.len() <= 4 { 1 } else { 2 };
        let mut slot = 0usize;
        for r in self.repos.iter().filter(|r| r.pos.is_none()) {
            loop {
                let want = [(slot % columns) as f64 * CELL_W, (slot / columns) as f64 * CELL_H];
                slot += 1;
                let taken = placed.values().any(|p| (p[0] - want[0]).abs() < CELL_W - 40.0 && (p[1] - want[1]).abs() < CELL_H - 40.0);
                if !taken {
                    placed.insert(r.path.clone(), want);
                    break;
                }
            }
        }
        placed
    }

    pub fn pin_all(&mut self) {
        let pos = self.positions();
        for r in self.repos.iter_mut() {
            r.pos = r.pos.or_else(|| pos.get(&r.path).copied());
        }
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    a == b || canon(a) == canon(b)
}

pub fn unique_id(projects: &[Project], name: &str) -> String {
    let base = if name.chars().any(|c| c.is_ascii_alphanumeric()) { slugify(name) } else { "project".to_string() };
    let mut id = base.clone();
    let mut n = 2;
    while projects.iter().any(|p| p.id == id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

pub fn containing<'a>(projects: &'a [Project], dir: &Path) -> Option<&'a Project> {
    projects.iter().find(|p| p.contains(dir))
}

pub fn load() -> Vec<Project> {
    screens::load_json(PROJECTS_FILE)
}

pub fn save(projects: &[Project]) -> Result<(), String> {
    screens::save_json(PROJECTS_FILE, &projects)
}

pub fn github_remote(dir: &Path) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(["remote", "get-url", "origin"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_remote(String::from_utf8_lossy(&out.stdout).trim())
}

fn parse_remote(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("ssh://git@github.com/")
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    github::normalize_repo(rest)
}

pub fn info(repo: &ProjectRepo) -> RepoInfo {
    let dir = repo.dir();
    if !dir.is_dir() {
        return RepoInfo::default();
    }
    match graph::load(&dir, GRAPH_FILE) {
        Ok(Some(g)) => RepoInfo {
            exists: true,
            nodes: g.nodes.len() + g.modules.iter().map(|m| m.net.nodes.len()).sum::<usize>(),
            modules: g.modules.len(),
            has_graph: true,
        },
        _ => RepoInfo { exists: true, ..Default::default() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(path: &str, pos: Option<[f64; 2]>) -> ProjectRepo {
        ProjectRepo { path: path.into(), pos, ..Default::default() }
    }

    #[test]
    fn parses_github_remotes() {
        assert_eq!(parse_remote("git@github.com:codebrain/workbench.git").as_deref(), Some("codebrain/workbench"));
        assert_eq!(parse_remote("https://github.com/codebrain/workbench").as_deref(), Some("codebrain/workbench"));
        assert_eq!(parse_remote("ssh://git@github.com/codebrain/host.git").as_deref(), Some("codebrain/host"));
        assert_eq!(parse_remote("git@gitlab.com:codebrain/workbench.git"), None);
    }

    #[test]
    fn unpinned_repos_fill_free_grid_slots() {
        let p = Project {
            id: "wb".into(),
            name: "Workbench".into(),
            repos: vec![repo("/a", Some([0.0, 0.0])), repo("/b", None), repo("/c", None)],
            ..Default::default()
        };
        let pos = p.positions();
        assert_eq!(pos["/a"], [0.0, 0.0]);
        assert_eq!(pos["/b"], [0.0, CELL_H]);
        assert_eq!(pos["/c"], [0.0, 2.0 * CELL_H]);
        let mut wide = p.clone();
        wide.repos.extend(["/d", "/e"].map(|x| repo(x, None)));
        let pos = wide.positions();
        assert_eq!(pos["/b"], [CELL_W, 0.0]);
        assert_eq!(pos["/c"], [0.0, CELL_H]);
    }

    #[test]
    fn labels_fall_back_to_folder_name_and_ids_are_unique() {
        assert_eq!(repo("/code/frontend", None).label(), "frontend");
        let existing = vec![Project { id: "workbench".into(), ..Default::default() }];
        assert_eq!(unique_id(&existing, "Workbench"), "workbench-2");
        assert_eq!(unique_id(&[], "  "), "project");
    }

    #[test]
    fn projects_round_trip_through_json() {
        let p = Project {
            id: "wb".into(),
            name: "Workbench".into(),
            description: String::new(),
            repos: vec![ProjectRepo { path: "/x".into(), github: Some("a/b".into()), ..Default::default() }],
        };
        let text = serde_json::to_string(&vec![p.clone()]).unwrap();
        let back: Vec<Project> = serde_json::from_str(&text).unwrap();
        assert_eq!(back, vec![p]);
    }
}
