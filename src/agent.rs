//! One interface over every provider, for the session loop.
//!
//! Workbench runs as one long-lived process that receives every message.
//! Every other provider runs one turn per message: Codex and Vibe start a new
//! process that resumes the previous session, and API providers make one HTTP
//! request with the conversation so far, plus one more per round of MCP tool calls. Every provider reports back with the
//! same `ClaudeEvent` values, tagged with the session loop's generation number.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::api_chat::{self, ChatMessage};
use crate::claude::{ChatMode, ClaudeEvent, ClaudeProcess, PermissionDecision};
use crate::mcp;
use crate::providers::{Access, Provider, Settings};
use crate::{codex, platform, vibe};

pub type EventSender = UnboundedSender<(u64, ClaudeEvent)>;

pub enum Agent {
    Workbench(ClaudeProcess),
    /// A single turn of a non-Workbench provider, running in a background task.
    /// Aborting the task drops its child process, which kills it.
    Turn(JoinHandle<()>),
}

impl Agent {
    pub async fn send_user(&mut self, text: &str) -> Result<()> {
        match self {
            Agent::Workbench(p) => p.send_user(text).await,
            Agent::Turn(_) => Err(anyhow!("this run already has its prompt; start a new turn instead")),
        }
    }

    /// Only Workbench can stop mid-turn and stay alive. Other runs return an error,
    /// so the session loop kills them instead.
    pub async fn interrupt(&mut self) -> Result<()> {
        match self {
            Agent::Workbench(p) => p.interrupt().await,
            Agent::Turn(_) => Err(anyhow!("one-turn runs are stopped by ending them")),
        }
    }

    /// Permission and control requests only come from Workbench, so other runs ignore the answer.
    pub async fn respond_permission(&mut self, request_id: &str, decision: &PermissionDecision) -> Result<()> {
        match self {
            Agent::Workbench(p) => p.respond_permission(request_id, decision).await,
            Agent::Turn(_) => Ok(()),
        }
    }

    pub async fn respond_error(&mut self, request_id: &str, error: &str) -> Result<()> {
        match self {
            Agent::Workbench(p) => p.respond_error(request_id, error).await,
            Agent::Turn(_) => Ok(()),
        }
    }

    pub async fn kill(self) {
        match self {
            Agent::Workbench(p) => p.kill().await,
            Agent::Turn(task) => task.abort(),
        }
    }

    pub fn exit_code(&mut self) -> Option<i32> {
        match self {
            Agent::Workbench(p) => p.exit_code(),
            Agent::Turn(_) => None,
        }
    }
}

/// Everything a non-Workbench provider needs to run one turn.
pub struct TurnOptions {
    pub cwd: PathBuf,
    pub model: String,
    /// The provider's session to continue (Codex thread id or Vibe session id).
    pub resume: Option<String>,
    pub mode: ChatMode,
    pub system_prompt: String,
    pub mcp_servers: Vec<mcp::Server>,
    pub prompt: String,
    /// API providers only: the earlier messages of this chat.
    pub history: Vec<ChatMessage>,
}

pub fn start_turn(provider: Provider, settings: &Settings, opts: TurnOptions, generation: u64, tx: EventSender) -> Result<Agent> {
    let task = match provider.access() {
        Access::Cli { .. } if provider == Provider::ClaudeCode => {
            return Err(anyhow!("Workbench runs as a long-lived process, not one turn at a time"));
        }
        Access::Cli { binary } => {
            let mut cmd = Command::new(platform::program(binary));
            let parser: Box<dyn TurnParser> = match provider {
                Provider::Vibe => {
                    vibe::configure(&mut cmd, &opts);
                    Box::new(vibe::Parser::new(&opts.model))
                }
                _ => {
                    codex::configure(&mut cmd, &opts);
                    Box::new(codex::Parser::new(&opts.model))
                }
            };
            let prompt = match provider {
                Provider::Vibe if opts.resume.is_none() => vibe::first_prompt(&opts.system_prompt, &opts.prompt),
                _ => opts.prompt.clone(),
            };
            run_cli(cmd, &opts.cwd, prompt, parser, provider.short_name(), generation, tx)?
        }
        Access::Api { base_url, style, key_env } => {
            let key = settings.api_key(provider).with_context(|| format!("no API key: set {key_env} or add one in the Providers tab"))?;
            let request = api_chat::Request { base_url, style, key, opts };
            tokio::spawn(async move {
                api_chat::run(request, |ev| {
                    let _ = tx.send((generation, ev));
                })
                .await;
                let _ = tx.send((generation, ClaudeEvent::Exited));
            })
        }
    };
    Ok(Agent::Turn(task))
}

/// Turns the output lines of a one-turn CLI into events.
pub trait TurnParser: Send {
    fn line(&mut self, line: &str) -> Vec<ClaudeEvent>;
    /// Called once the process has exited. Must end the turn with a `Done` if the output did not.
    fn finish(&mut self, success: bool) -> Vec<ClaudeEvent>;
}

fn run_cli(
    mut cmd: Command,
    cwd: &PathBuf,
    prompt: String,
    mut parser: Box<dyn TurnParser>,
    name: &'static str,
    generation: u64,
    tx: EventSender,
) -> Result<JoinHandle<()>> {
    cmd.current_dir(cwd)
        .env("PATH", platform::augmented_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().with_context(|| format!("failed to start {name}"))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;

    let err_tx = tx.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if !line.trim().is_empty() && err_tx.send((generation, ClaudeEvent::Stderr(line))).is_err() {
                return;
            }
        }
    });

    Ok(tokio::spawn(async move {
        // The prompt goes in through stdin; dropping stdin afterwards tells the CLI the prompt is complete.
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(prompt.as_bytes()).await;
        }
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            for ev in parser.line(&line) {
                let _ = tx.send((generation, ev));
            }
        }
        let success = child.wait().await.is_ok_and(|s| s.success());
        for ev in parser.finish(success) {
            let _ = tx.send((generation, ev));
        }
        let _ = tx.send((generation, ClaudeEvent::Exited));
    }))
}
