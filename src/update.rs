//! Self-update: checks GitHub for newer commits on `main` than the one this
//! binary was built from, and rebuilds/reinstalls Workbench from source.
//!
//! The update builds in a private checkout under the user cache dir, so the
//! user's own working copy of the repo is never touched.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::claude::augmented_path;

pub const REPO: &str = "KarlOlofA/workbench";
pub const BRANCH: &str = "main";
/// Commit this binary was built from (set by build.rs; empty if unknown).
pub const BUILD_COMMIT: &str = env!("WORKBENCH_COMMIT");

#[derive(Debug, Clone, PartialEq)]
pub struct CommitInfo {
    pub sha: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Available {
    /// How many commits `main` has that this build doesn't.
    pub behind_by: u64,
    /// New commits, newest first.
    pub commits: Vec<CommitInfo>,
}

pub fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

pub fn enabled() -> bool {
    !BUILD_COMMIT.is_empty() && std::env::var_os("WORKBENCH_NO_UPDATE_CHECK").is_none()
}

/// Asks GitHub how this build compares to `main`. `Ok(None)` means up to date,
/// or this build is ahead of `main` / from a commit GitHub doesn't know (a local
/// dev build), in which case there's nothing sensible to offer.
pub fn check() -> Result<Option<Available>, String> {
    if !enabled() {
        return Ok(None);
    }
    let url = format!("https://api.github.com/repos/{REPO}/compare/{BUILD_COMMIT}...{BRANCH}");
    let out = std::process::Command::new("curl")
        .args(["-sSL", "--max-time", "10", "-w", "\n%{http_code}"])
        .args(["-H", "Accept: application/vnd.github+json", "-H", "User-Agent: workbench-updater"])
        .arg(&url)
        .env("PATH", augmented_path())
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').unwrap_or((&text, ""));
    match code.trim() {
        "200" => {}
        "404" => return Ok(None),
        c => return Err(format!("GitHub returned HTTP {c}")),
    }
    let v: Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    parse_compare(&v)
}

fn parse_compare(v: &Value) -> Result<Option<Available>, String> {
    let status = v["status"].as_str().ok_or("unexpected response from GitHub")?;
    // base = this build, head = main: "ahead"/"diverged" mean main has commits we lack.
    if !matches!(status, "ahead" | "diverged") {
        return Ok(None);
    }
    let behind_by = v["ahead_by"].as_u64().unwrap_or(0);
    if behind_by == 0 {
        return Ok(None);
    }
    let mut commits: Vec<CommitInfo> = v["commits"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|c| CommitInfo {
                    sha: c["sha"].as_str().unwrap_or_default().to_string(),
                    message: c["commit"]["message"].as_str().unwrap_or_default().lines().next().unwrap_or_default().to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    commits.reverse();
    Ok(Some(Available { behind_by, commits }))
}

fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache"));
    base.join("workbench")
}

pub fn source_dir() -> PathBuf {
    cache_dir().join("source")
}

pub fn log_path() -> PathBuf {
    cache_dir().join("update.log")
}

fn log_line(log: &mut Option<File>, line: &str) {
    if let Some(f) = log {
        let _ = writeln!(f, "{line}");
    }
}

async fn run(log: &mut Option<File>, cwd: &Path, program: &str, args: &[&str]) -> Result<(), String> {
    log_line(log, &format!("$ {program} {}", args.join(" ")));
    let out = tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("PATH", augmented_path())
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("{program}: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    log_line(log, String::from_utf8_lossy(&out.stdout).trim_end());
    log_line(log, stderr.trim_end());
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{program} {} failed:\n{}", args.join(" "), stderr.trim()))
    }
}

/// Fetches the latest `main` into the private checkout and builds a release
/// binary there. `progress` gets short status lines while it runs. Returns the
/// checkout dir, ready for [`install_and_relaunch`].
pub async fn build_latest(mut progress: impl FnMut(String)) -> Result<PathBuf, String> {
    let path = augmented_path();
    let missing: Vec<&str> = ["git", "cargo"].into_iter().filter(|t| !path.split(':').any(|d| Path::new(d).join(t).is_file())).collect();
    if !missing.is_empty() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh");
        return Err(format!(
            "Updating needs {}, which isn't installed.\nRun {} in a terminal: it offers to install what's missing.",
            missing.join(" and "),
            script.display()
        ));
    }
    let dir = source_dir();
    std::fs::create_dir_all(cache_dir()).map_err(|e| e.to_string())?;
    let mut log = OpenOptions::new().create(true).write(true).truncate(true).open(log_path()).ok();

    if dir.join(".git").is_dir() {
        progress(format!("Fetching {BRANCH}…"));
        run(&mut log, &dir, "git", &["fetch", "--depth", "1", "origin", BRANCH]).await?;
        run(&mut log, &dir, "git", &["checkout", "-f", "-B", BRANCH, "FETCH_HEAD"]).await?;
    } else {
        progress(format!("Downloading {REPO}…"));
        let _ = std::fs::remove_dir_all(&dir);
        let url = format!("https://github.com/{REPO}.git");
        let dest = dir.to_string_lossy().to_string();
        run(&mut log, &cache_dir(), "git", &["clone", "--depth", "1", "--branch", BRANCH, &url, &dest]).await?;
    }

    progress("Building release binary…".into());
    log_line(&mut log, "$ cargo build --release");
    let mut child = tokio::process::Command::new("cargo")
        .args(["build", "--release", "--color", "never"])
        .current_dir(&dir)
        .env("PATH", augmented_path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cargo: {e} (is Rust installed?)"))?;
    let mut tail: Vec<String> = Vec::new();
    if let Some(stderr) = child.stderr.take() {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            log_line(&mut log, &line);
            let t = line.trim();
            if !t.is_empty() {
                progress(t.to_string());
            }
            tail.push(line);
            if tail.len() > 20 {
                tail.remove(0);
            }
        }
    }
    let status = child.wait().await.map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("cargo build failed:\n{}", tail.join("\n")));
    }
    Ok(dir)
}

/// Runs the checkout's install script in a detached process, then reopens
/// Workbench on `cwd`. The caller should exit right after this returns Ok, so
/// the installer can replace the running binary.
pub fn install_and_relaunch(dir: &Path, cwd: &Path) -> Result<(), String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let bin_dir = std::env::var("BIN_DIR").unwrap_or_else(|_| format!("{home}/.local/bin"));
    let launcher = format!("{bin_dir}/wbrs");
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let log = OpenOptions::new().create(true).append(true).open(log_path()).map_err(|e| e.to_string())?;
    let log_err = log.try_clone().map_err(|e| e.to_string())?;
    // $1 checkout, $2 launcher, $3 folder to reopen, $4 fallback binary.
    let script = r#"sleep 1
"$1/scripts/install.sh"
if [ -x "$2" ]; then "$2" "$3"; else nohup "$4" "$3" >/dev/null 2>&1 & fi"#;
    std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .arg("workbench-update")
        .arg(dir)
        .arg(&launcher)
        .arg(cwd)
        .arg(&exe)
        .current_dir(dir)
        .env("PATH", augmented_path())
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .process_group(0)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identical_and_behind_are_up_to_date() {
        assert_eq!(parse_compare(&json!({"status": "identical", "ahead_by": 0})).unwrap(), None);
        assert_eq!(parse_compare(&json!({"status": "behind", "ahead_by": 0, "behind_by": 3})).unwrap(), None);
    }

    #[test]
    fn ahead_lists_commits_newest_first() {
        let v = json!({
            "status": "ahead",
            "ahead_by": 2,
            "commits": [
                {"sha": "aaaaaaa1", "commit": {"message": "First\n\nbody"}},
                {"sha": "bbbbbbb2", "commit": {"message": "Second"}}
            ]
        });
        let a = parse_compare(&v).unwrap().unwrap();
        assert_eq!(a.behind_by, 2);
        assert_eq!(a.commits[0].message, "Second");
        assert_eq!(a.commits[1].message, "First");
    }
}
