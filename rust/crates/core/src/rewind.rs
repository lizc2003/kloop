//! Rewind: continue the live session from an earlier turn, as a new session
//! (plans 18, 205, 210). The front-end owns the Config and History being
//! replaced, so this builds the branch's Config, rebases History onto the
//! branch, and hands the Config back for the front-end to swap in — the split
//! `/clear` has with [`crate::commands::start_fresh_session`].
//!
//! A rewind does not undo anything on disk: the files the abandoned turns
//! changed stay changed. A summarizing rewind is the one that tells the branch.

use std::sync::Arc;

use anyhow::Context as _;
use tokio_util::sync::CancellationToken;

use crate::agent::Ui;
use crate::compact::summarize_abandoned_branch;
use crate::config::Config;
use crate::history::History;
use crate::rollout::ResumedSession;
use crate::rollout::fork_session;
use crate::rollout::inspect_session;
use crate::rollout::session_id_of;

/// What a rewind does with the turns it drops. Either way they stay in the old
/// session file; only a summary puts anything onto the branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbandonedBranch {
    /// Rewind straight away: free, and what a rewind always did.
    Drop,
    /// Summarize them onto the branch first: one full-price request, and the
    /// branch learns what they found and which files they changed.
    Summarize,
}

/// The session a rewind landed on.
pub struct Rewound {
    pub cfg: Config,
    /// What retiring the old session did (see
    /// [`crate::commands::replace_session`]), one line each.
    pub report: Vec<String>,
    /// Anything else about the rewind the user should hear, one line each.
    pub notes: Vec<String>,
}

pub enum RewindOutcome {
    Rewound(Box<Rewound>),
    /// Cancelled while summarizing: the session, History and the files on disk
    /// are as they were.
    Cancelled,
}

/// Rewind onto the fork cut at `seq`. An error leaves History and the session
/// untouched.
pub async fn rewind(
    cfg: &Config,
    history: &mut History,
    seq: u64,
    abandoned: AbandonedBranch,
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
) -> anyhow::Result<RewindOutcome> {
    let (session_id, resumed) = fork_here(history, seq)?;
    // Summarized from the live session, before anything is swapped: the
    // abandoned turns are what it has past the branch, and the transcript
    // pointer has to name this session's file, not the branch's.
    let mut notes = Vec::new();
    let summary = match abandoned {
        AbandonedBranch::Drop => None,
        AbandonedBranch::Summarize => {
            match summarize_abandoned_branch(cfg, history, &resumed.messages, cancel).await {
                Ok(summary) => summary,
                // Cancelling takes the whole rewind back, not just the summary:
                // rewinding without one is a different key. The branch file is
                // this rewind's own and nothing points at it yet.
                Err(_) if cancel.is_cancelled() => {
                    let branch = resumed.rollout.path().to_path_buf();
                    drop(resumed);
                    let _ = std::fs::remove_file(branch);
                    return Ok(RewindOutcome::Cancelled);
                }
                // The rewind is what the user asked for; the summary rode
                // along, and its failure does not get to stop the rewind.
                Err(error) => {
                    let old = history
                        .rollout_path()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default();
                    notes.push(format!(
                        "rewind: the rewound-away turns could not be summarized ({error:#}); \
                         they are still in {old}"
                    ));
                    None
                }
            }
        }
    };
    let (fresh, report) = crate::commands::replace_session(cfg, session_id, ui).await?;
    history.rebase(resumed);
    // A rewind does not change who is using the session, so the route it is
    // running on right now — `/provider` included — carries onto the branch,
    // and the cut's own route is not restored. Adopting writes to the forked
    // rollout, so it runs after the rebase.
    let cfg = match history.adopt_provider_route(&cfg.provider_route) {
        Ok((route, reopened)) => {
            if let Some(reopened) = reopened {
                notes.push(format!("rewind: {reopened}"));
            }
            fresh.clone_with_provider_route(route)
        }
        Err(error) => {
            notes.push(format!(
                "rewind landed on the new branch but its provider route \
                 could not be adopted: {error}"
            ));
            fresh
        }
    };
    if let Some(summary) = summary {
        summary.record(history);
    }
    Ok(RewindOutcome::Rewound(Box::new(Rewound {
        cfg,
        report,
        notes,
    })))
}

/// Fork the live session at `seq` and load the branch: the new id, its messages,
/// and a rollout writer pointed at the fork file. The session's own directory is
/// the sessions dir (branches are siblings). Errors if the history isn't backed
/// by a file or the fork/reload fails.
fn fork_here(history: &History, seq: u64) -> anyhow::Result<(String, ResumedSession)> {
    let src = history
        .rollout_path()
        .context("this session is not being saved, so it cannot be rewound")?;
    let sessions_dir = src
        .parent()
        .context("session file has no parent directory")?;
    let fork_path = fork_session(src, Some(seq), sessions_dir)?;
    let resumed = inspect_session(&fork_path)?.recover()?;
    Ok((session_id_of(&fork_path), resumed))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::path::PathBuf;

    use kloop_protocol::AssistantBlock;
    use kloop_protocol::ContentBlock;
    use kloop_protocol::Injected;
    use kloop_protocol::Message;
    use kloop_provider::MockRequest;
    use kloop_provider::MockTurn;
    use kloop_provider::Provider;
    use kloop_provider::ProviderFailure;
    use serde_json::json;

    use super::*;
    use crate::compact::BRANCH_SUMMARY_PREFIX;
    use crate::rollout::Rollout;
    use crate::rollout::fork_points;
    use crate::rollout::session_path;
    use crate::tools::testutil::SilentUi;
    use crate::tools::testutil::TestConfig;

    const LIVE_ID: &str = "20260101-000000";

    type Seen = Arc<std::sync::Mutex<Vec<MockRequest>>>;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kloop-rewind-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn text(text: &str) -> Message {
        Message::assistant(vec![ContentBlock::Text { text: text.into() }])
    }

    fn first_turn() -> Vec<Message> {
        vec![
            Message::user_text("read the parser"),
            text("it is recursive"),
        ]
    }

    /// A saved session of two turns — the second edits a file — whose summary
    /// requests `turns` answers, and the cut that keeps only the first turn.
    fn live_session(tag: &str, dir: &Path, turns: Vec<MockTurn>) -> (Config, History, u64, Seen) {
        let (provider, seen) = Provider::mock_recording(turns);
        let mut cfg = TestConfig::new(&format!("rewind-{tag}"))
            .dirs(dir)
            .provider(provider)
            .build()
            .test_clone();
        cfg.bind_session(LIVE_ID.into()).unwrap();
        let mut history = History::new(dir.to_path_buf());
        history.attach_rollout(
            Rollout::new_with_initial_route(session_path(dir, LIVE_ID), &cfg.provider_route)
                .unwrap(),
        );
        for message in first_turn() {
            history.record(message);
        }
        history.record(Message::user_text("make it iterative"));
        history.record(Message::assistant(vec![ContentBlock::ToolUse {
            id: "e1".into(),
            name: "edit_file".into(),
            input: json!({"path": "src/parser.rs"}),
        }]));
        history.record(Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "e1".into(),
            content: kloop_protocol::ToolResultContent::Text("ok".into()),
            is_error: false,
        }]));
        history.record(text("done"));
        let cut = fork_points(history.rollout_path().unwrap()).unwrap()[0].seq;
        (cfg, history, cut, seen)
    }

    fn ui() -> Arc<dyn Ui> {
        Arc::new(SilentUi)
    }

    fn session_files(dir: &Path) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        files.sort();
        files
    }

    fn rewound(outcome: RewindOutcome) -> Rewound {
        match outcome {
            RewindOutcome::Rewound(rewound) => *rewound,
            RewindOutcome::Cancelled => panic!("the rewind was not cancelled"),
        }
    }

    /// The branch opens with the summary, on disk as in memory, under a new
    /// id; the session it left is untouched.
    #[tokio::test]
    async fn a_summarizing_rewind_opens_the_branch_with_the_summary() {
        let dir = temp_dir("summarize");
        let (cfg, mut history, cut, _seen) = live_session(
            "summarize",
            &dir,
            vec![MockTurn::Blocks(vec![AssistantBlock::Text {
                text: "<summary>went iterative</summary>".into(),
            }])],
        );
        let live_path = history.rollout_path().unwrap().to_path_buf();
        let live_before = std::fs::read_to_string(&live_path).unwrap();

        let rewound = rewound(
            rewind(
                &cfg,
                &mut history,
                cut,
                AbandonedBranch::Summarize,
                &ui(),
                &CancellationToken::new(),
            )
            .await
            .unwrap(),
        );

        assert_ne!(rewound.cfg.session_id, cfg.session_id);
        assert_eq!(rewound.notes, Vec::<String>::new());
        let [first, answer, summary] = history.messages() else {
            panic!("the first turn and the summary: {:?}", history.messages())
        };
        assert_eq!([first.clone(), answer.clone()], first_turn().as_slice());
        assert_eq!(summary.injected, Some(Injected::BranchSummary));
        let ContentBlock::Text { text } = &summary.content[0] else {
            panic!("the summary is text")
        };
        assert!(
            text.starts_with(&format!("{BRANCH_SUMMARY_PREFIX}went iterative\n\n")),
            "{text}"
        );
        assert!(text.contains("\n- src/parser.rs"), "{text}");
        let branch_path = history.rollout_path().unwrap();
        assert_eq!(
            branch_path,
            session_path(&dir, &rewound.cfg.session_id).as_path()
        );
        assert_eq!(
            crate::rollout::load_session_snapshot(branch_path)
                .unwrap()
                .messages,
            history.messages()
        );
        assert_eq!(std::fs::read_to_string(&live_path).unwrap(), live_before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The rewind is what the user asked for: a summary that fails does not
    /// stop it, and the note says where the dropped turns still are.
    #[tokio::test]
    async fn a_failed_summary_still_rewinds() {
        let dir = temp_dir("failed");
        let (cfg, mut history, cut, _seen) = live_session(
            "failed",
            &dir,
            vec![MockTurn::Failure(ProviderFailure::protocol("refused"))],
        );
        let live_path = history.rollout_path().unwrap().to_path_buf();

        let rewound = rewound(
            rewind(
                &cfg,
                &mut history,
                cut,
                AbandonedBranch::Summarize,
                &ui(),
                &CancellationToken::new(),
            )
            .await
            .unwrap(),
        );

        assert_eq!(history.messages(), first_turn());
        assert_eq!(
            rewound.notes,
            [format!(
                "rewind: the rewound-away turns could not be summarized (compaction request \
                 failed: provider protocol error: refused); they are still in {}",
                live_path.display()
            )]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cancelling mid-summary takes the whole rewind back: same session, same
    /// History, and no branch file left behind.
    #[tokio::test]
    async fn cancelling_the_summary_cancels_the_rewind() {
        let dir = temp_dir("cancel");
        let (started, started_rx) = tokio::sync::oneshot::channel();
        let (_release, release) = tokio::sync::oneshot::channel();
        let (cfg, mut history, cut, _seen) = live_session(
            "cancel",
            &dir,
            vec![MockTurn::Gate {
                started,
                release,
                blocks: Vec::new(),
            }],
        );
        let messages = history.messages().to_vec();
        let live_path = history.rollout_path().unwrap().to_path_buf();
        let files = session_files(&dir);
        let cancel = CancellationToken::new();
        let ui = ui();

        let (outcome, ()) = tokio::join!(
            rewind(
                &cfg,
                &mut history,
                cut,
                AbandonedBranch::Summarize,
                &ui,
                &cancel,
            ),
            async {
                started_rx.await.unwrap();
                cancel.cancel();
            }
        );

        assert!(matches!(outcome.unwrap(), RewindOutcome::Cancelled));
        assert_eq!(history.messages(), messages);
        assert_eq!(history.rollout_path(), Some(live_path.as_path()));
        assert_eq!(session_files(&dir), files);
        assert!(cfg.scheduler.list().is_ok(), "the live session still runs");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Rewinding without a summary sends nothing and adds nothing.
    #[tokio::test]
    async fn a_plain_rewind_sends_no_request() {
        let dir = temp_dir("drop");
        let (cfg, mut history, cut, seen) = live_session("drop", &dir, Vec::new());

        let rewound = rewound(
            rewind(
                &cfg,
                &mut history,
                cut,
                AbandonedBranch::Drop,
                &ui(),
                &CancellationToken::new(),
            )
            .await
            .unwrap(),
        );

        assert_eq!(history.messages(), first_turn());
        assert_eq!(rewound.notes, Vec::<String>::new());
        assert!(seen.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fork the live session's file at a cut, derive the sessions dir from the
    /// rollout path, and hand back the branch's id and truncated messages
    /// (which `rebase` then installs).
    #[test]
    fn fork_here_branches_the_live_session_at_a_cut() {
        let dir = temp_dir("forkhere");
        let session = dir.join("session.jsonl");
        let mut history = History::new(dir.clone());
        history.attach_rollout(Rollout::new(session.clone()));
        history.record(Message::user_text("one"));
        history.record(text("done"));
        history.record(Message::user_text("two"));
        history.record(text("bye"));

        // Route receipt + two messages keeps the first turn only.
        let (id, resumed) = fork_here(&history, 3).unwrap();
        assert_ne!(id, "session");
        assert_eq!(
            resumed.messages,
            vec![Message::user_text("one"), text("done")]
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
