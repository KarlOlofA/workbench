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
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::api_chat::{self, ChatMessage};
use crate::claude::{ChatMode, ClaudeEvent, PermissionDecision};
use crate::mcp;
use crate::providers::{Access, Provider, Settings};
use crate::{codex, platform, vibe};

pub type EventSender = UnboundedSender<(u64, ClaudeEvent)>;

/// The contract between the session loop and an agent backend. Each backend has its own
/// constructor (`ClaudeProcess::spawn` for the long-lived Workbench process, `start_turn` for
/// one-turn providers) that takes an `EventSender`. Everything the agent does comes back on it
/// as `ClaudeEvent`s, including permission requests, which are answered with `respond_permission`.
#[async_trait]
pub trait AgentAdapter: Send {
    async fn send_user(&mut self, text: &str) -> Result<()>;
    /// Stops the current turn and keeps the agent alive. Agents that can't return an error,
    /// so the session loop kills them instead.
    async fn interrupt(&mut self) -> Result<()>;
    async fn respond_permission(&mut self, request_id: &str, decision: &PermissionDecision) -> Result<()>;
    async fn respond_error(&mut self, request_id: &str, error: &str) -> Result<()>;
    async fn kill(self: Box<Self>);
    fn exit_code(&mut self) -> Option<i32>;
}

/// A single turn of a non-Workbench provider, running in a background task.
/// Aborting the task drops its child process, which kills it.
pub struct TurnRun(JoinHandle<()>);

#[async_trait]
impl AgentAdapter for TurnRun {
    async fn send_user(&mut self, _text: &str) -> Result<()> {
        Err(anyhow!("this run already has its prompt; start a new turn instead"))
    }

    async fn interrupt(&mut self) -> Result<()> {
        Err(anyhow!("one-turn runs are stopped by ending them"))
    }

    /// Permission and control requests only come from Workbench, so one-turn runs ignore the answer.
    async fn respond_permission(&mut self, _request_id: &str, _decision: &PermissionDecision) -> Result<()> {
        Ok(())
    }

    async fn respond_error(&mut self, _request_id: &str, _error: &str) -> Result<()> {
        Ok(())
    }

    async fn kill(self: Box<Self>) {
        self.0.abort();
    }

    fn exit_code(&mut self) -> Option<i32> {
        None
    }
}

/// Everything a non-Workbench provider needs to run one turn.
pub struct TurnOptions {
    pub cwd: PathBuf,
    pub model: String,
    /// The provider's session to continue (Codex thread id or Vibe session id).
    pub resume: Option<String>,
    pub mode: ChatMode,
    /// The project's other repositories and worktrees, which the agent may also read.
    pub extra_dirs: Vec<PathBuf>,
    pub system_prompt: String,
    pub mcp_servers: Vec<mcp::Server>,
    pub prompt: String,
    /// API providers only: the earlier messages of this chat.
    pub history: Vec<ChatMessage>,
}

pub fn start_turn(provider: Provider, settings: &Settings, opts: TurnOptions, generation: u64, tx: EventSender) -> Result<Box<dyn AgentAdapter>> {
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
    Ok(Box::new(TurnRun(task)))
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
