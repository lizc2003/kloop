mod agent;
mod compact;
mod history;
mod provider;
mod sse;
mod tools;
mod types;

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio_util::sync::CancellationToken;

use crate::agent::run_turn;
use crate::agent::EndReason;
use crate::agent::Ui;
use crate::history::History;
use crate::provider::Provider;
use crate::types::ContentBlock;
use crate::types::Message;

#[derive(Clone)]
pub struct Config {
    pub provider: Arc<Provider>,
    pub model: String,
    pub system: String,
    pub max_rounds: usize,
    pub offload_dir: PathBuf,
    /// Usable context window in tokens; None disables compaction entirely.
    pub context_window: Option<u64>,
}

impl Config {
    fn from_env(mock: bool) -> Result<Self> {
        let cwd = std::env::current_dir().context("cannot determine cwd")?;
        let system = format!(
            "You are a coding agent working in a CLI. Use the provided tools to inspect and \
             modify files and run commands; keep answers short. Current working directory: {}",
            cwd.display()
        );
        let offload_dir = PathBuf::from(".kloop/offload");
        // AGENT_CONTEXT_WINDOW: token budget for compaction ("off" disables).
        let context_window = match std::env::var("AGENT_CONTEXT_WINDOW").ok().as_deref() {
            Some("off") | Some("0") => None,
            Some(raw) => Some(raw.parse::<u64>().context(
                "AGENT_CONTEXT_WINDOW must be a token count or 'off'",
            )?),
            None => Some(200_000),
        };
        let base = Self {
            provider: Arc::new(Provider::mock(vec![])),
            model: "mock".into(),
            system,
            max_rounds: 30,
            offload_dir,
            context_window,
        };
        if mock {
            return Ok(Config {
                provider: Arc::new(Provider::mock(mock_demo_turns())),
                ..base
            });
        }

        let anthropic = || -> Result<Config> {
            let key = std::env::var("ANTHROPIC_API_KEY")
                .context("ANTHROPIC_API_KEY not set")?;
            Ok(Config {
                provider: Arc::new(Provider::Anthropic {
                    key,
                    base: std::env::var("ANTHROPIC_BASE_URL")
                        .unwrap_or_else(|_| "https://api.anthropic.com".into()),
                }),
                model: std::env::var("AGENT_MODEL").unwrap_or_else(|_| "claude-sonnet-5".into()),
                ..base.clone()
            })
        };
        let openai = || -> Result<Config> {
            let key = std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set")?;
            let model = std::env::var("AGENT_MODEL")
                .context("AGENT_MODEL is required for the openai-compat provider")?;
            Ok(Config {
                provider: Arc::new(Provider::OpenAiCompat {
                    key,
                    base: std::env::var("OPENAI_BASE_URL")
                        .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
                }),
                model,
                ..base.clone()
            })
        };

        match std::env::var("AGENT_PROVIDER").ok().as_deref() {
            Some("anthropic") => anthropic(),
            Some("openai") | Some("openai-compat") => openai(),
            Some(other) => bail!("unknown AGENT_PROVIDER '{other}' (anthropic | openai)"),
            None => {
                if let Ok(cfg) = anthropic() {
                    Ok(cfg)
                } else if let Ok(cfg) = openai() {
                    Ok(cfg)
                } else {
                    bail!(
                        "no provider configured: set ANTHROPIC_API_KEY or OPENAI_API_KEY \
                         (+ AGENT_MODEL), or run with --mock"
                    )
                }
            }
        }
    }
}

struct StdoutUi;

impl Ui for StdoutUi {
    fn text_delta(&self, s: &str) {
        print!("{s}");
        let _ = std::io::stdout().flush();
    }

    fn note(&self, s: &str) {
        eprintln!("\x1b[2m[{s}]\x1b[0m");
    }
}

/// Scripted turns for `--mock`, exercising all five bets without an API key:
/// round 1 batches two read-only bash calls concurrently, round 2 runs an
/// unsafe command whose oversized output triggers offloading, round 3 reads it
/// back, round 4 spawns a sub-agent (round 5 is the sub-agent's own reply),
/// round 6 finishes with plain text.
fn mock_demo_turns() -> Vec<Vec<ContentBlock>> {
    let tool_use = |id: &str, name: &str, input: serde_json::Value| ContentBlock::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    };
    let text = |t: &str| ContentBlock::Text { text: t.into() };
    vec![
        vec![
            text("Looking around (these two run as one concurrent batch)…\n"),
            tool_use("t1", "bash", json!({"command": "pwd"})),
            tool_use("t2", "bash", json!({"command": "ls"})),
        ],
        vec![
            text("Now a non-read-only command with huge output (runs sequentially, result gets offloaded)…\n"),
            tool_use("t3", "bash", json!({"command": "yes offload-me | head -n 3000"})),
        ],
        vec![
            text("Reading the offloaded output back…\n"),
            tool_use("t4", "read_offloaded", json!({"id": "off-0001"})),
        ],
        vec![
            text("Delegating to a sub-agent…\n"),
            tool_use("t5", "task", json!({"prompt": "say hi"})),
        ],
        // consumed by the sub-agent's own run_turn
        vec![text("hi from the sub-agent")],
        vec![text("Demo complete: parallel batch, offload + read-back, and a sub-agent all worked.")],
    ]
}

#[tokio::main]
async fn main() -> Result<()> {
    let mock = std::env::args().any(|a| a == "--mock");
    let cfg = Arc::new(Config::from_env(mock)?);
    let ui: Arc<dyn Ui> = Arc::new(StdoutUi);
    let mut history = History::new(cfg.offload_dir.clone());

    if mock {
        history.record(Message::user_text("run the demo"));
        let cancel = CancellationToken::new();
        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;
        println!("\n--- mock run: {:?} after {} round(s) ---", outcome.reason, outcome.rounds);
        return Ok(());
    }

    println!("kloop — type a task, 'exit' or Ctrl+D to quit, Ctrl+C to interrupt a running turn");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        print!("> ");
        let _ = std::io::stdout().flush();
        let Some(line) = lines.next_line().await? else {
            break;
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line == "exit" {
            break;
        }

        history.record(Message::user_text(line));
        let cancel = CancellationToken::new();
        let watcher = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    cancel.cancel();
                }
            })
        };
        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;
        watcher.abort();
        println!();
        match outcome.reason {
            EndReason::Completed => {}
            EndReason::MaxRounds => println!("[stopped: hit max rounds ({})]", cfg.max_rounds),
            EndReason::Aborted => println!("[interrupted — history patched; Ctrl+D or 'exit' to quit]"),
            EndReason::Error(e) => println!("[error: {e}]"),
        }
    }
    Ok(())
}
