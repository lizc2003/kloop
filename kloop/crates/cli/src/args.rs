//! CLI argument parsing (`--mock`/`--resume`/`--fork`/…) and session picking:
//! turning argv into a [`CliArgs`], and resolving a [`SessionChoice`] into an
//! opened `History` (list / continue / resume / fork). Pure front-matter that
//! runs before any UI owns the terminal.

use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;

use kloop_core::history::History;
use kloop_core::rollout::first_user_snippet;
use kloop_core::rollout::fork_origin;
use kloop_core::rollout::fork_session;
use kloop_core::rollout::is_subagent_session;
use kloop_core::rollout::load_session;
use kloop_core::rollout::new_session_id;
use kloop_core::rollout::resume_session;
use kloop_core::rollout::session_id_of;
use kloop_core::rollout::session_origin;
use kloop_core::rollout::session_path;
use kloop_core::rollout::sessions_by_recency;
use kloop_core::rollout::Rollout;
use kloop_core::rollout::SessionOrigin;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionChoice {
    New,
    /// `--continue`: the most recently modified session.
    Continue,
    /// `--resume` with no id: pick from a numbered list.
    Pick,
    Resume(String),
    /// `--fork <id>[#<seq>]`: branch a new session off an existing one at a
    /// line boundary (no seq = at the end) and continue there. Covers rewind
    /// too: fork the current session at an earlier point and take the other
    /// road — the original file is never touched.
    Fork {
        id: String,
        cut: Option<u64>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CliArgs {
    pub(crate) mock: bool,
    pub(crate) yolo: bool,
    pub(crate) accept_edits: bool,
    pub(crate) list_sessions: bool,
    pub(crate) plain: bool,
    pub(crate) serve: bool,
    pub(crate) session: SessionChoice,
}

pub(crate) fn parse_args(args: &[String]) -> Result<CliArgs> {
    let mut parsed = CliArgs {
        mock: false,
        yolo: false,
        accept_edits: false,
        list_sessions: false,
        plain: false,
        serve: false,
        session: SessionChoice::New,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--mock" => parsed.mock = true,
            "--yolo" => parsed.yolo = true,
            "--accept-edits" => parsed.accept_edits = true,
            "--list-sessions" => parsed.list_sessions = true,
            "--plain" => parsed.plain = true,
            "--serve" => parsed.serve = true,
            "--continue" => parsed.session = SessionChoice::Continue,
            "--resume" => {
                parsed.session = match args.get(i + 1) {
                    Some(id) if !id.starts_with('-') => {
                        i += 1;
                        SessionChoice::Resume(id.clone())
                    }
                    _ => SessionChoice::Pick,
                };
            }
            "--fork" => {
                let arg = match args.get(i + 1) {
                    Some(arg) if !arg.starts_with('-') => arg,
                    _ => bail!("--fork needs a session (<id> or <id>#<seq>)"),
                };
                i += 1;
                parsed.session = match arg.split_once('#') {
                    None => SessionChoice::Fork {
                        id: arg.clone(),
                        cut: None,
                    },
                    Some((id, seq)) => SessionChoice::Fork {
                        id: id.to_string(),
                        cut: Some(seq.parse().with_context(|| {
                            format!("--fork: '{seq}' is not a line number (<id>#<seq>)")
                        })?),
                    },
                };
            }
            other => bail!(
                "unknown argument '{other}' (--mock | --yolo | --accept-edits | --plain | --serve | --continue | --resume [id] | --fork <id>[#<seq>] | --list-sessions)"
            ),
        }
        i += 1;
    }
    Ok(parsed)
}

fn session_line(path: &Path) -> String {
    let id = session_id_of(path);
    let origin = match session_origin(path) {
        Some(SessionOrigin::Fork(o)) => format!("  [forked from {o}]"),
        Some(SessionOrigin::SubAgent(o)) => format!("  [sub-agent of {o}]"),
        None => String::new(),
    };
    match load_session(path) {
        Ok(messages) => format!(
            "{id}  {} message(s)  {}{origin}",
            messages.len(),
            first_user_snippet(&messages)
        ),
        Err(e) => format!("{id}  (unreadable: {e}){origin}"),
    }
}

pub(crate) fn list_sessions(sessions_dir: &Path) {
    let sessions = sessions_by_recency(sessions_dir);
    if sessions.is_empty() {
        println!("no saved sessions in {}", sessions_dir.display());
        return;
    }
    for path in sessions {
        println!("{}", session_line(&path));
    }
}

/// Top-level sessions the resume picker offers, most recent first: every
/// session except sub-agent transcripts, which are reachable only by explicit
/// id (they still show in `--list-sessions`). Mirrors cc hiding sidechains and
/// codex filtering by source.
fn resumable_sessions(sessions_dir: &Path) -> Vec<PathBuf> {
    sessions_by_recency(sessions_dir)
        .into_iter()
        .filter(|path| !is_subagent_session(path))
        .collect()
}

/// `--resume` with no id: numbered list on stdout, one line of stdin picks.
/// Runs before any UI starts, so plain blocking stdio is fine.
fn pick_session(sessions_dir: &Path) -> Result<PathBuf> {
    let sessions = resumable_sessions(sessions_dir);
    if sessions.is_empty() {
        bail!("no saved sessions to resume");
    }
    println!("saved sessions (most recent first):");
    for (i, path) in sessions.iter().enumerate() {
        println!("{:>3}. {}", i + 1, session_line(path));
    }
    print!("resume which? [1-{}, empty = 1] > ", sessions.len());
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let index = pick_index(&line, sessions.len())?;
    Ok(sessions[index].clone())
}

/// 1-based selection, empty input = the first (most recent) entry.
fn pick_index(input: &str, len: usize) -> Result<usize> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(0);
    }
    match input.parse::<usize>() {
        Ok(n) if (1..=len).contains(&n) => Ok(n - 1),
        _ => bail!("invalid selection '{input}' (expected 1-{len})"),
    }
}

/// Build the History for this run: a fresh persisted session by default, or
/// one replayed from disk for `--resume`.
pub(crate) fn open_history(
    offload_dir: PathBuf,
    choice: &SessionChoice,
    sessions_dir: &Path,
) -> Result<(History, String)> {
    let resume_path = match choice {
        SessionChoice::New => {
            let id = new_session_id(sessions_dir);
            let mut history = History::new(offload_dir);
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
        SessionChoice::Continue => resumable_sessions(sessions_dir)
            .into_iter()
            .next()
            .context("no saved sessions to continue")?,
        SessionChoice::Pick => pick_session(sessions_dir)?,
        SessionChoice::Fork { id, cut } => {
            let src = session_path(sessions_dir, id);
            if !src.exists() {
                bail!("no session '{id}' (try --list-sessions)");
            }
            let path = fork_session(&src, *cut, sessions_dir)
                .with_context(|| format!("cannot fork session '{id}'"))?;
            println!(
                "[forked {id}#{cut} → {fork_id}]",
                cut = fork_origin(&path)
                    .and_then(|origin| origin.rsplit('#').next().map(str::to_string))
                    .unwrap_or_default(),
                fork_id = session_id_of(&path),
            );
            path
        }
    };
    let id = session_id_of(&resume_path);
    let (messages, rollout) = resume_session(&resume_path)
        .with_context(|| format!("cannot read session file {}", resume_path.display()))?;
    println!("[resumed session {id}: {} message(s)]", messages.len());
    let history = History::resume(offload_dir, messages, rollout);
    Ok((history, id))
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
                yolo: false,
                accept_edits: false,
                list_sessions: false,
                plain: false,
                serve: false,
                session: SessionChoice::New,
            }
        );
        assert_eq!(
            parse_args(&strings(&["--mock", "--resume"])).unwrap(),
            CliArgs {
                mock: true,
                yolo: false,
                accept_edits: false,
                list_sessions: false,
                plain: false,
                serve: false,
                session: SessionChoice::Pick,
            }
        );
        assert_eq!(
            parse_args(&strings(&["--continue"])).unwrap(),
            CliArgs {
                mock: false,
                yolo: false,
                accept_edits: false,
                list_sessions: false,
                plain: false,
                serve: false,
                session: SessionChoice::Continue,
            }
        );
        assert_eq!(
            parse_args(&strings(&["--serve"])).unwrap(),
            CliArgs {
                mock: false,
                yolo: false,
                accept_edits: false,
                list_sessions: false,
                plain: false,
                serve: true,
                session: SessionChoice::New,
            }
        );
        assert_eq!(
            parse_args(&strings(&["--resume", "20260709-120000", "--plain"])).unwrap(),
            CliArgs {
                mock: false,
                yolo: false,
                accept_edits: false,
                list_sessions: false,
                plain: true,
                serve: false,
                session: SessionChoice::Resume("20260709-120000".into()),
            }
        );
        assert_eq!(
            parse_args(&strings(&["--list-sessions", "--yolo", "--accept-edits"])).unwrap(),
            CliArgs {
                mock: false,
                yolo: true,
                accept_edits: true,
                list_sessions: true,
                plain: false,
                serve: false,
                session: SessionChoice::New,
            }
        );
        assert_eq!(
            parse_args(&strings(&["--fork", "20260709-120000#4"])).unwrap(),
            CliArgs {
                mock: false,
                yolo: false,
                accept_edits: false,
                list_sessions: false,
                plain: false,
                serve: false,
                session: SessionChoice::Fork {
                    id: "20260709-120000".into(),
                    cut: Some(4),
                },
            }
        );
        assert_eq!(
            parse_args(&strings(&["--fork", "20260709-120000"]))
                .unwrap()
                .session,
            SessionChoice::Fork {
                id: "20260709-120000".into(),
                cut: None,
            }
        );
        assert!(parse_args(&strings(&["--fork"])).is_err(), "id required");
        assert!(
            parse_args(&strings(&["--fork", "id#notanumber"])).is_err(),
            "seq must parse"
        );
        assert!(parse_args(&strings(&["--bogus"])).is_err());
    }

    #[test]
    fn pick_index_covers_empty_valid_and_garbage() {
        assert_eq!(pick_index("", 5).unwrap(), 0);
        assert_eq!(pick_index("  \n", 5).unwrap(), 0);
        assert_eq!(pick_index("1", 5).unwrap(), 0);
        assert_eq!(pick_index(" 5 \n", 5).unwrap(), 4);
        assert!(pick_index("0", 5).is_err());
        assert!(pick_index("6", 5).is_err());
        assert!(pick_index("abc", 5).is_err());
    }
}
