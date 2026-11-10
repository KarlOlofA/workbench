//! OS-specific bits in one place: where the home, config and cache folders are,
//! how to find a program on `PATH`, and how to open a link or show a file in the
//! system file manager.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The user's home folder: `HOME`, or `USERPROFILE` on Windows (where `HOME` is
/// usually unset). `None` if neither is set.
pub fn home_dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let home = var("HOME");
    if cfg!(windows) { home.or_else(|| var("USERPROFILE")) } else { home }
}

/// Folder for Workbench's own settings: `~/.config/workbench`, or
/// `%APPDATA%\workbench` on Windows.
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(windows)
        && let Some(d) = std::env::var_os("APPDATA").filter(|v| !v.is_empty())
    {
        return Some(PathBuf::from(d).join("workbench"));
    }
    home_dir().map(|h| h.join(".config").join("workbench"))
}

/// Folder for throwaway data like the updater's checkout: `$XDG_CACHE_HOME/workbench`
/// or `~/.cache/workbench`, or `%LOCALAPPDATA%\workbench` on Windows.
pub fn cache_dir() -> PathBuf {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if cfg!(windows) { var("LOCALAPPDATA") } else { var("XDG_CACHE_HOME") };
    base.or_else(|| home_dir().map(|h| h.join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("workbench")
}

/// Folders where tools like `claude`, `gh`, `git` and `cargo` are commonly
/// installed but which may be missing from `PATH` when the app is started from a
/// desktop launcher instead of a shell.
fn extra_bin_dirs() -> Vec<PathBuf> {
    let home = home_dir();
    let in_home = |rel: &str| home.as_ref().map(|h| h.join(rel));
    if cfg!(windows) {
        let env_dir = |name: &str, rel: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(|d| PathBuf::from(d).join(rel));
        [
            in_home(".local\\bin"),
            in_home(".cargo\\bin"),
            env_dir("APPDATA", "npm"),
            env_dir("ProgramFiles", "Git\\cmd"),
            env_dir("ProgramFiles", "GitHub CLI"),
            env_dir("LOCALAPPDATA", "Programs\\Git\\cmd"),
        ]
        .into_iter()
        .flatten()
        .collect()
    } else {
        let mut dirs: Vec<PathBuf> = [in_home(".local/bin"), in_home(".cargo/bin")].into_iter().flatten().collect();
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from));
        dirs
    }
}

/// `PATH` plus [`extra_bin_dirs`], for passing to child processes.
pub fn augmented_path() -> OsString {
    let mut parts: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    parts.retain(|p| !p.as_os_str().is_empty());
    for e in extra_bin_dirs() {
        if !parts.contains(&e) {
            parts.push(e);
        }
    }
    std::env::join_paths(parts).unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

/// File names a program can have: just `name` on Unix, `name.exe` / `name.cmd` /
/// `name.bat` on Windows (npm installs CLIs as `.cmd` shims).
fn program_names(name: &str) -> Vec<String> {
    if cfg!(windows) { ["exe", "cmd", "bat"].iter().map(|ext| format!("{name}.{ext}")).collect() } else { vec![name.to_string()] }
}

/// Full path of `name` in [`augmented_path`], if it's installed.
pub fn find_program(name: &str) -> Option<PathBuf> {
    let path = augmented_path();
    let names = program_names(name);
    std::env::split_paths(&path).find_map(|dir| names.iter().map(|n| dir.join(n)).find(|p| p.is_file()))
}

/// [`find_program`], falling back to the bare name so the error from spawning it
/// says what's missing.
pub fn program(name: &str) -> PathBuf {
    find_program(name).unwrap_or_else(|| PathBuf::from(name))
}

/// Opens a web link in the default browser.
pub fn open_url(url: &str) {
    let _ = opener(url.as_ref()).spawn();
}

/// Shows a file or folder in the system file manager (selected, where the OS supports that).
pub fn reveal(path: &Path) {
    let _ = if cfg!(target_os = "macos") {
        Command::new("open").arg("-R").arg(path).spawn()
    } else if cfg!(windows) {
        let mut arg = OsString::from("/select,");
        arg.push(path);
        Command::new("explorer").arg(arg).spawn()
    } else {
        // xdg-open can't select a file, so open the folder it's in.
        let dir = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
        opener(dir.as_os_str()).spawn()
    };
}

fn opener(target: &std::ffi::OsStr) -> Command {
    let mut cmd = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(windows) {
        // Not `cmd /C start`: cmd would treat `&` in a URL's query string as a command separator.
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        Command::new("xdg-open")
    };
    cmd.arg(target);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn augmented_path_keeps_path_and_adds_extras() {
        let joined = augmented_path();
        let parts: Vec<PathBuf> = std::env::split_paths(&joined).collect();
        for p in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).filter(|p| !p.as_os_str().is_empty()) {
            assert!(parts.contains(&p), "{} dropped", p.display());
        }
        for e in extra_bin_dirs() {
            assert_eq!(parts.iter().filter(|p| **p == e).count(), 1, "{} missing or duplicated", e.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn finds_sh() {
        assert!(find_program("sh").is_some());
        assert!(find_program("definitely-not-a-real-program-xyz").is_none());
    }
}
