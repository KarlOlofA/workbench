//! Finds the coding agents (Codex, Vibe, Workbench) running on this machine, which folder
//! each one runs in, and whether it is working right now.
//!
//! Agents started outside Workbench (in a terminal, say) don't report their state, so
//! "working" is measured: an agent is working while it, or the commands it started, use CPU.
//! An agent waiting for your input uses next to none.
//!
//! Only Linux is supported (it reads `/proc`); elsewhere the scan finds nothing.
//! Workbench's own session is skipped here: the app knows its state directly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::logos::Logo;
use crate::providers::Provider;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentKind {
    Workbench,
    Codex,
    Vibe,
    /// Workbench's own session on an API provider: chat only, no process.
    Api(Provider),
}

impl AgentKind {
    pub fn label(self) -> &'static str {
        match self {
            AgentKind::Workbench => "Workbench",
            AgentKind::Codex => "Codex",
            AgentKind::Vibe => "Vibe",
            AgentKind::Api(provider) => provider.label(),
        }
    }

    /// The logo of the company whose models the agent runs.
    pub fn logo(self) -> Logo {
        match self {
            AgentKind::Workbench | AgentKind::Api(Provider::AnthropicApi) => Logo::Claude,
            AgentKind::Codex | AgentKind::Api(Provider::OpenAiApi) => Logo::OpenAi,
            AgentKind::Vibe | AgentKind::Api(Provider::MistralApi) => Logo::Mistral,
            AgentKind::Api(Provider::OpenRouterApi) => Logo::OpenRouter,
            // Workbench never reports a CLI provider as an API session.
            AgentKind::Api(Provider::ClaudeCode | Provider::Codex | Provider::Vibe) => Logo::Claude,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentProc {
    /// 0 for Workbench's own session.
    pub pid: u32,
    pub kind: AgentKind,
    pub cwd: PathBuf,
    pub working: bool,
    /// Workbench's own session rather than one found on the machine.
    pub in_app: bool,
}

/// CPU use (agent plus its commands) above this, in percent of one core, counts as working.
const WORKING_CPU_PCT: f64 = 2.0;

/// Remembers CPU time between scans, so each scan can tell which agents are busy.
#[derive(Default)]
pub struct Scanner {
    last: HashMap<u32, u64>,
    at: Option<Instant>,
}

/// Which agent a command line belongs to, if any. Handles the native binaries as well as
/// the `node …/cli.js` and `python …/vibe` forms.
pub fn kind_of(argv: &[String]) -> Option<AgentKind> {
    let base = |s: &str| Path::new(s).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
    let head: Vec<String> = argv.iter().take(2).map(|a| base(a)).collect();
    let script = argv.get(1).map(String::as_str).unwrap_or("");
    let first = head.first().map(String::as_str).unwrap_or("");
    let launcher = matches!(first, "node" | "bun" | "deno") || first.starts_with("python");
    let named = |name: &str| first == name || (launcher && head.get(1).map(String::as_str) == Some(name));
    if named("claude") || (launcher && script.contains("claude-code")) {
        Some(AgentKind::Workbench)
    } else if named("codex") || (launcher && script.contains("@openai/codex")) {
        Some(AgentKind::Codex)
    } else if named("vibe") || (launcher && script.contains("mistral-vibe")) {
        Some(AgentKind::Vibe)
    } else {
        None
    }
}

impl Scanner {
    pub fn scan(&mut self) -> Vec<AgentProc> {
        let procs = read_procs();
        let now = Instant::now();
        let elapsed = self.at.map(|t| now.duration_since(t).as_secs_f64()).filter(|s| *s > 0.0);
        self.at = Some(now);
        let found = find_agents(&procs, std::process::id(), clock_ticks());

        let mut next = HashMap::new();
        let out = found
            .into_iter()
            .map(|(pid, kind, cwd, ticks, tps)| {
                let working = match (self.last.get(&pid), elapsed) {
                    (Some(prev), Some(secs)) => ticks.saturating_sub(*prev) as f64 / tps / secs * 100.0 > WORKING_CPU_PCT,
                    _ => false,
                };
                next.insert(pid, ticks);
                AgentProc { pid, kind, cwd, working, in_app: false }
            })
            .collect();
        self.last = next;
        out
    }
}

/// What the scan needs from one process.
#[derive(Debug, Clone, Default)]
pub struct Proc {
    pub ppid: u32,
    /// User plus system CPU time, in clock ticks.
    pub ticks: u64,
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
}

/// The agents in a process table: (pid, kind, cwd, CPU ticks of it and everything it
/// started, ticks per second). Skips processes started by `own_pid` (Workbench's session)
/// and wrappers whose parent is the same agent (e.g. the node shim that starts codex).
pub fn find_agents(procs: &HashMap<u32, Proc>, own_pid: u32, tps: f64) -> Vec<(u32, AgentKind, PathBuf, u64, f64)> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, p) in procs {
        children.entry(p.ppid).or_default().push(*pid);
    }
    let descends_from = |mut pid: u32, root: u32| {
        for _ in 0..64 {
            let Some(p) = procs.get(&pid) else { return false };
            if p.ppid == root {
                return true;
            }
            if p.ppid <= 1 {
                return false;
            }
            pid = p.ppid;
        }
        false
    };
    let tree_ticks = |root: u32| {
        let mut total = 0;
        let mut stack = vec![root];
        while let Some(pid) = stack.pop() {
            total += procs.get(&pid).map(|p| p.ticks).unwrap_or(0);
            stack.extend(children.get(&pid).into_iter().flatten().copied());
        }
        total
    };

    let mut out = Vec::new();
    for (pid, p) in procs {
        let Some(kind) = kind_of(&p.argv) else { continue };
        let Some(cwd) = p.cwd.clone() else { continue };
        if descends_from(*pid, own_pid) {
            continue;
        }
        if procs.get(&p.ppid).is_some_and(|parent| kind_of(&parent.argv) == Some(kind)) {
            continue;
        }
        out.push((*pid, kind, cwd, tree_ticks(*pid), tps));
    }
    out.sort_by_key(|a| a.0);
    out
}

/// USER_HZ, the unit of `/proc` CPU times. It is 100 on every mainstream Linux build;
/// asking with `sysconf(_SC_CLK_TCK)` would need libc.
fn clock_ticks() -> f64 {
    100.0
}

#[cfg(target_os = "linux")]
fn read_procs() -> HashMap<u32, Proc> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else { return out };
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let dir = e.path();
        let Ok(stat) = std::fs::read_to_string(dir.join("stat")) else { continue };
        let Some((ppid, ticks)) = parse_stat(&stat) else { continue };
        let argv: Vec<String> = std::fs::read(dir.join("cmdline"))
            .map(|b| b.split(|c| *c == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect())
            .unwrap_or_default();
        // Only agents need a cwd; reading it for every process is wasted work.
        let cwd = if kind_of(&argv).is_some() { std::fs::read_link(dir.join("cwd")).ok() } else { None };
        out.insert(pid, Proc { ppid, ticks, argv, cwd });
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn read_procs() -> HashMap<u32, Proc> {
    HashMap::new()
}

/// (ppid, utime + stime) from `/proc/<pid>/stat`. The command name in parentheses may
/// contain spaces, so fields are counted from the last `)`.
pub fn parse_stat(stat: &str) -> Option<(u32, u64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // After the name: state(0) ppid(1) … utime(11) stime(12).
    let ppid = f.get(1)?.parse().ok()?;
    let utime: u64 = f.get(11)?.parse().ok()?;
    let stime: u64 = f.get(12)?.parse().ok()?;
    Some((ppid, utime + stime))
}

/// Gives each agent to the folder it runs in: the deepest of `dirs` that contains its cwd.
/// Agents outside every folder are left out. Paths are compared after resolving symlinks.
pub fn assign(agents: &[AgentProc], dirs: &[PathBuf]) -> HashMap<PathBuf, Vec<AgentProc>> {
    let canon: Vec<(PathBuf, PathBuf)> = dirs.iter().map(|d| (d.clone(), d.canonicalize().unwrap_or_else(|_| d.clone()))).collect();
    let mut out: HashMap<PathBuf, Vec<AgentProc>> = HashMap::new();
    for a in agents {
        let cwd = a.cwd.canonicalize().unwrap_or_else(|_| a.cwd.clone());
        let owner = canon.iter().filter(|(_, c)| cwd.starts_with(c)).max_by_key(|(_, c)| c.components().count());
        if let Some((dir, _)) = owner {
            out.entry(dir.clone()).or_default().push(a.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }

    #[test]
    fn recognises_agent_command_lines() {
        assert_eq!(kind_of(&argv("claude")), Some(AgentKind::Workbench));
        assert_eq!(kind_of(&argv("/home/u/.local/bin/claude --resume x")), Some(AgentKind::Workbench));
        assert_eq!(kind_of(&argv("node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js")), Some(AgentKind::Workbench));
        assert_eq!(kind_of(&argv("node /usr/lib/node_modules/@openai/codex/bin/codex.js")), Some(AgentKind::Codex));
        assert_eq!(kind_of(&argv("/usr/bin/codex exec")), Some(AgentKind::Codex));
        assert_eq!(kind_of(&argv("/usr/bin/python3 /home/u/.local/bin/vibe")), Some(AgentKind::Vibe));
        assert_eq!(kind_of(&argv("vim claude.md")), None);
        assert_eq!(kind_of(&argv("node server.js")), None);
    }

    #[test]
    fn parses_stat_with_spaces_in_the_name() {
        let stat = "4242 (my (odd) name) S 17 4242 4242 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 1 0 1 1 1";
        assert_eq!(parse_stat(stat), Some((17, 300)));
    }

    #[test]
    fn finds_agents_with_their_commands_cpu_and_skips_our_own() {
        let p = |ppid, ticks, cmd: &str, cwd: Option<&str>| Proc { ppid, ticks, argv: argv(cmd), cwd: cwd.map(PathBuf::from) };
        let procs: HashMap<u32, Proc> = [
            (10, p(1, 5, "bash", None)),
            (11, p(10, 100, "claude", Some("/code/app"))),
            (12, p(11, 40, "cargo build", None)),
            (20, p(1, 1, "node /x/@openai/codex/bin/codex.js", Some("/code/app-feat"))),
            (21, p(20, 7, "codex", Some("/code/app-feat"))),
            (30, p(1, 9, "workbench", None)),
            (31, p(30, 9, "claude --print", Some("/code/app"))),
        ]
        .into_iter()
        .collect();
        let found = find_agents(&procs, 30, 100.0);
        let pids: Vec<(u32, AgentKind, u64)> = found.iter().map(|f| (f.0, f.1, f.3)).collect();
        assert_eq!(pids, vec![(11, AgentKind::Workbench, 140), (20, AgentKind::Codex, 8)]);
    }

    #[test]
    fn assigns_agents_to_the_deepest_folder() {
        let a = |cwd: &str| AgentProc { pid: 1, kind: AgentKind::Workbench, cwd: PathBuf::from(cwd), working: true, in_app: false };
        let dirs = vec![PathBuf::from("/nope/app"), PathBuf::from("/nope/app/.worktrees/x")];
        let got = assign(&[a("/nope/app/src"), a("/nope/app/.worktrees/x/lib"), a("/elsewhere")], &dirs);
        assert_eq!(got[&dirs[0]].len(), 1);
        assert_eq!(got[&dirs[1]].len(), 1);
        assert_eq!(got.len(), 2);
    }
}
