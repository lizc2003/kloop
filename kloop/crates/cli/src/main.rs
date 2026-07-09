//! kloop CLI: environment-driven configuration, a line-based REPL with
//! Ctrl+C interruption, session persistence (`--resume`, `--list-sessions`),
//! and the keyless `--mock` demo.

use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio_util::sync::CancellationToken;

use kloop_core::agent::run_turn;
use kloop_core::agent::EndReason;
use kloop_core::agent::Ui;
use kloop_core::history::History;
use kloop_core::rollout::load_session;
use kloop_core::rollout::resume_session;
use kloop_core::rollout::Rollout;
use kloop_core::Config;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::Role;
use kloop_provider::Provider;

#[derive(Debug, PartialEq, Eq)]
enum SessionChoice {
    New,
    ResumeLatest,
    Resume(String),
}

#[derive(Debug, PartialEq, Eq)]
struct CliArgs {
    mock: bool,
    list_sessions: bool,
    session: SessionChoice,
}

fn parse_args(args: &[String]) -> Result<CliArgs> {
    let mut parsed = CliArgs {
        mock: false,
        list_sessions: false,
        session: SessionChoice::New,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--mock" => parsed.mock = true,
            "--list-sessions" => parsed.list_sessions = true,
            "--resume" => {
                parsed.session = match args.get(i + 1) {
                    Some(id) if !id.starts_with('-') => {
                        i += 1;
                        SessionChoice::Resume(id.clone())
                    }
                    _ => SessionChoice::ResumeLatest,
                };
            }
            other => bail!("unknown argument '{other}' (--mock | --resume [id] | --list-sessions)"),
        }
        i += 1;
    }
    Ok(parsed)
}

/// Session ids are UTC wall-clock timestamps — readable, sortable, and free
/// of a rand dependency. A collision within one second gets a numeric suffix.
fn new_session_id(sessions_dir: &Path) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let base = timestamp_id(secs);
    let mut id = base.clone();
    let mut n = 2;
    while session_path(sessions_dir, &id).exists() {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

fn timestamp_id(unix_secs: u64) -> String {
    let (y, m, d) = civil_from_days((unix_secs / 86_400) as i64);
    let rem = unix_secs % 86_400;
    format!(
        "{y:04}{m:02}{d:02}-{h:02}{min:02}{s:02}",
        h = rem / 3600,
        min = rem % 3600 / 60,
        s = rem % 60
    )
}

/// Days since 1970-01-01 to a UTC civil date (Howard Hinnant's
/// civil_from_days), so session ids don't need a chrono dependency.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn session_path(sessions_dir: &Path, id: &str) -> PathBuf {
    sessions_dir.join(format!("{id}.jsonl"))
}

/// All session files, most recently modified first (modified = last active,
/// which is what `--resume` without an id should pick up).
fn sessions_by_recency(sessions_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(sessions_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    files.into_iter().map(|(_, path)| path).collect()
}

fn session_id_of(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session")
        .to_string()
}

fn list_sessions(sessions_dir: &Path) {
    let sessions = sessions_by_recency(sessions_dir);
    if sessions.is_empty() {
        println!("no saved sessions in {}", sessions_dir.display());
        return;
    }
    for path in sessions {
        let id = session_id_of(&path);
        match load_session(&path) {
            Ok(messages) => println!(
                "{id}  {} message(s)  {}",
                messages.len(),
                first_user_snippet(&messages)
            ),
            Err(e) => println!("{id}  (unreadable: {e})"),
        }
    }
}

fn first_user_snippet(messages: &[Message]) -> String {
    for message in messages {
        if message.role != Role::User {
            continue;
        }
        for block in &message.content {
            if let ContentBlock::Text { text } = block {
                let mut snippet: String = text
                    .chars()
                    .take(60)
                    .map(|c| if c == '\n' { ' ' } else { c })
                    .collect();
                if text.chars().count() > 60 {
                    snippet.push('…');
                }
                return snippet;
            }
        }
    }
    String::new()
}

/// Build the History for this run: a fresh persisted session by default, or
/// one replayed from disk for `--resume`.
fn open_history(
    cfg: &Config,
    choice: &SessionChoice,
    sessions_dir: &Path,
) -> Result<(History, String)> {
    let resume_path = match choice {
        SessionChoice::New => {
            let id = new_session_id(sessions_dir);
            let mut history = History::new(cfg.offload_dir.clone());
            history.attach_rollout(Rollout::new(session_path(sessions_dir, &id)));
            return Ok((history, id));
        }
        SessionChoice::Resume(id) => {
            let path = session_path(sessions_dir, id);
            if !path.exists() {
                bail!("no session '{id}' (try --list-sessions)");
            }
            path
        }
        SessionChoice::ResumeLatest => sessions_by_recency(sessions_dir)
            .into_iter()
            .next()
            .context("no saved sessions to resume")?,
    };
    let id = session_id_of(&resume_path);
    let (messages, rollout) = resume_session(&resume_path)
        .with_context(|| format!("cannot read session file {}", resume_path.display()))?;
    println!("[resumed session {id}: {} message(s)]", messages.len());
    let history = History::resume(cfg.offload_dir.clone(), messages, rollout);
    Ok((history, id))
}

fn config_from_env(mock: bool) -> Result<Config> {
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
        Some(raw) => Some(
            raw.parse::<u64>()
                .context("AGENT_CONTEXT_WINDOW must be a token count or 'off'")?,
        ),
        None => Some(200_000),
    };
    let base = Config {
        provider: Arc::new(Provider::mock(vec![])),
        model: "mock".into(),
        system,
        max_rounds: 30,
        offload_dir,
        context_window,
        fallback_model: std::env::var("AGENT_FALLBACK_MODEL").ok(),
    };
    if mock {
        return Ok(Config {
            provider: Arc::new(Provider::mock(mock_demo_turns())),
            ..base
        });
    }

    let anthropic = || -> Result<Config> {
        let key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY not set")?;
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&args)?;
    let sessions_dir = PathBuf::from(".kloop/sessions");
    if args.list_sessions {
        list_sessions(&sessions_dir);
        return Ok(());
    }

    let cfg = Arc::new(config_from_env(args.mock)?);
    let ui: Arc<dyn Ui> = Arc::new(StdoutUi);
    let (mut history, session_id) = open_history(&cfg, &args.session, &sessions_dir)?;

    if args.mock {
        history.record(Message::user_text("run the demo"));
        let cancel = CancellationToken::new();
        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;
        println!(
            "\n--- mock run: {:?} after {} round(s) ---",
            outcome.reason, outcome.rounds
        );
        return Ok(());
    }

    println!(
        "kloop — session {session_id}; type a task, 'exit' or Ctrl+D to quit, \
         Ctrl+C to interrupt a running turn"
    );
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
            EndReason::Aborted => {
                println!("[interrupted — history patched; Ctrl+D or 'exit' to quit]")
            }
            EndReason::Error(e) => println!("[error: {e}]"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_args_covers_all_flags() {
        assert_eq!(
            parse_args(&[]).unwrap(),
            CliArgs {
                mock: false,
                list_sessions: false,
                session: SessionChoice::New,
            }
        );
        assert_eq!(
            parse_args(&strings(&["--mock", "--resume"])).unwrap(),
            CliArgs {
                mock: true,
                list_sessions: false,
                session: SessionChoice::ResumeLatest,
            }
        );
        assert_eq!(
            parse_args(&strings(&["--resume", "20260709-120000"])).unwrap(),
            CliArgs {
                mock: false,
                list_sessions: false,
                session: SessionChoice::Resume("20260709-120000".into()),
            }
        );
        assert_eq!(
            parse_args(&strings(&["--list-sessions"])).unwrap(),
            CliArgs {
                mock: false,
                list_sessions: true,
                session: SessionChoice::New,
            }
        );
        assert!(parse_args(&strings(&["--bogus"])).is_err());
    }

    #[test]
    fn timestamp_ids_match_utc_civil_time() {
        assert_eq!(timestamp_id(0), "19700101-000000");
        // date -u -r 1783958400 → 2026-07-13 16:00:00 UTC
        assert_eq!(timestamp_id(1_783_958_400), "20260713-160000");
        // leap-year day: 2024-02-29 12:34:56 UTC
        assert_eq!(timestamp_id(1_709_209_496), "20240229-122456");
    }
}
