//! `/clear` — end this session and start a new one: a new id, a new rollout,
//! and session state that is empty because it is new, not because someone
//! remembered to empty each piece of it. The old session file is left exactly
//! as it was, so `--resume` still returns to it.
//!
//! The command itself only asks for the switch ([`SlashResult::new_session`]):
//! the front-end owns the Config and History being replaced, so it calls
//! [`start_fresh_session`] and swaps them in.

use std::sync::Arc;

use super::SlashResult;
use crate::agent::Ui;
use crate::config::Config;
use crate::history::History;
use crate::rollout::Rollout;

pub const SUMMARY: &str = "start a new session with an empty conversation";

pub fn run() -> SlashResult {
    SlashResult::new_session()
}

/// A session ready to replace the one `/clear` ended.
pub struct FreshSession {
    pub cfg: Config,
    pub history: History,
    /// What retiring the old session did, one line each (see [`replace_session`]).
    pub report: Vec<String>,
}

/// Build the session `/clear` switches to — a new rollout whose first line
/// is the route in effect right now — then retire the old one.
pub async fn start_fresh_session(old: &Config, ui: &Arc<dyn Ui>) -> anyhow::Result<FreshSession> {
    let mut history = History::new(old.offload_dir.clone());
    // No sessions directory means an ephemeral session (tests): nothing is
    // saved, so the new one gets no file and no id either.
    let session_id = if old.sessions_dir.as_os_str().is_empty() {
        String::new()
    } else {
        let id = crate::rollout::new_session_id(&old.sessions_dir);
        history.attach_rollout(Rollout::new_with_initial_route(
            crate::rollout::session_path(&old.sessions_dir, &id),
            &old.provider_route,
        )?);
        id
    };
    let (cfg, report) = replace_session(old, session_id, ui).await?;
    Ok(FreshSession {
        cfg,
        history,
        report,
    })
}

/// Derive the Config for `session_id` from `old`, then stop everything `old`
/// still runs: its background executions and shells, its scheduler, and its
/// worktree (kept on disk, left behind). Nothing is stopped if the new Config
/// cannot be built, so a failure leaves the old session running as it was.
///
/// Returns the new Config and one line per thing the user should hear about.
pub async fn replace_session(
    old: &Config,
    session_id: String,
    ui: &Arc<dyn Ui>,
) -> anyhow::Result<(Config, Vec<String>)> {
    let fresh = old.fresh_session(session_id)?;
    let running = old.background_executions.running_count() + old.background_shells.running_count();
    let missed = old.shutdown_background_work(ui).await;
    let mut report = Vec::new();
    if running > 0 {
        report.push(format!("stopped {running} background task(s)"));
    }
    if missed > 0 {
        report.push(format!(
            "{missed} background task(s) missed the shutdown deadline"
        ));
    }
    if let Some(note) = crate::worktree::finish_active(old).await {
        report.push(note);
    }
    Ok((fresh, report))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::path::PathBuf;

    use kloop_protocol::Message;
    use serde_json::json;

    use super::*;
    use crate::rollout::session_path;
    use crate::tools::testutil::SilentUi;
    use crate::tools::testutil::TestConfig;
    use crate::tools::testutil::run_tool;
    use crate::tools::testutil::test_ctx_with_cfg;

    const OLD_ID: &str = "20260101-000000";

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kloop-clear-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A saved session with one message in it.
    fn old_session(tag: &str, dir: &Path) -> (Config, History) {
        let mut cfg = TestConfig::new(tag).dirs(dir).build().test_clone();
        cfg.bind_session(OLD_ID.into()).unwrap();
        let mut history = History::new(dir.to_path_buf());
        history.attach_rollout(
            Rollout::new_with_initial_route(session_path(dir, OLD_ID), &cfg.provider_route)
                .unwrap(),
        );
        history.record(Message::user_text("before the clear"));
        (cfg, history)
    }

    fn ui() -> Arc<dyn Ui> {
        Arc::new(SilentUi)
    }

    /// The old file gets no new line — no empty `compacted` marker — so
    /// resuming it lands exactly where the conversation was; the new file is
    /// its own session, opened on the route in effect at the clear.
    #[tokio::test]
    async fn the_old_file_is_left_as_it_was_and_the_new_one_starts_empty() {
        let dir = temp_dir("files");
        let (cfg, history) = old_session("clear-files", &dir);
        let old_path = history.rollout_path().unwrap().to_path_buf();
        let before = std::fs::read_to_string(&old_path).unwrap();

        let fresh = start_fresh_session(&cfg, &ui()).await.unwrap();

        assert_eq!(std::fs::read_to_string(&old_path).unwrap(), before);
        assert_eq!(
            crate::rollout::resume_session(&old_path).unwrap().messages,
            [Message::user_text("before the clear")]
        );
        let new_path = fresh.history.rollout_path().unwrap().to_path_buf();
        assert_ne!(new_path, old_path);
        assert_eq!(
            crate::rollout::session_id_of(&new_path),
            fresh.cfg.session_id
        );
        assert!(fresh.history.messages().is_empty());
        let routes = fresh.history.provider_routes();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].model, cfg.provider_route.model());
        assert_eq!(routes[0].route_revision, cfg.provider_route.revision());
        assert_eq!(fresh.report, Vec::<String>::new());

        // Clearing again before saying anything leaves no empty session behind.
        drop(fresh);
        assert!(!new_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file read before the clear is one the new conversation has never
    /// seen, so it cannot be edited without reading it again.
    #[tokio::test]
    async fn a_file_read_before_the_clear_must_be_read_again() {
        let dir = temp_dir("file-state");
        let (cfg, _history) = old_session("clear-file-state", &dir);
        let file = dir.join("notes.txt");
        std::fs::write(&file, "alpha\n").unwrap();
        let path = file.to_str().unwrap();
        let old_ctx = test_ctx_with_cfg(0, Arc::new(cfg));
        let (output, is_error) = run_tool("read_file", json!({"path": path}), &old_ctx).await;
        assert!(!is_error, "{output}");

        let fresh = start_fresh_session(&old_ctx.cfg, &ui()).await.unwrap();
        let new_ctx = test_ctx_with_cfg(0, Arc::new(fresh.cfg));
        let edit = json!({"path": path, "old_string": "alpha", "new_string": "beta"});
        let (output, is_error) = run_tool("edit_file", edit.clone(), &new_ctx).await;
        assert!(is_error, "{output}");
        assert!(output.contains("must read"), "{output}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n");

        let (output, is_error) = run_tool("read_file", json!({"path": path}), &new_ctx).await;
        assert!(!is_error, "{output}");
        let (output, is_error) = run_tool("edit_file", edit, &new_ctx).await;
        assert!(!is_error, "{output}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Background work of the old session is stopped, said so, and delivers
    /// nothing into the new session.
    #[cfg(unix)]
    #[tokio::test]
    async fn background_work_is_stopped_and_never_reaches_the_new_session() {
        let dir = temp_dir("background");
        let (cfg, _history) = old_session("clear-background", &dir);
        let old_ctx = test_ctx_with_cfg(0, Arc::new(cfg));
        let (output, is_error) = run_tool(
            "bash",
            json!({"command": "sleep 30", "background": true}),
            &old_ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        assert_eq!(old_ctx.cfg.background_shells.running_count(), 1);

        let fresh = start_fresh_session(&old_ctx.cfg, &ui()).await.unwrap();

        assert_eq!(fresh.report, ["stopped 1 background task(s)"]);
        assert_eq!(old_ctx.cfg.background_shells.running_count(), 0);
        assert!(fresh.cfg.inbox.is_empty());
        assert_eq!(fresh.cfg.background_shells.running_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A rewind hands `replace_session` the branch's id. Everything that reads
    /// the session id must then name the branch: the cache key, the compaction
    /// summary's transcript pointer, and the scheduler's owner.
    #[tokio::test]
    async fn a_replaced_session_answers_to_its_new_id_everywhere() {
        let dir = temp_dir("replace");
        let (cfg, _history) = old_session("clear-replace", &dir);
        let branch = "20260101-000001";

        let (fresh, report) = replace_session(&cfg, branch.into(), &ui()).await.unwrap();

        assert_eq!(report, Vec::<String>::new());
        assert_eq!(fresh.cache_key(), Some(branch));
        let pointer = crate::compact::transcript_pointer(&fresh).unwrap();
        assert!(
            pointer.ends_with(&session_path(&dir, branch).display().to_string()),
            "{pointer}"
        );
        // The new scheduler is bound (to the branch); the old one is closed.
        assert_eq!(fresh.scheduler.list().unwrap(), Vec::new());
        assert!(cfg.scheduler.list().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without a sessions directory nothing was being saved, and the new
    /// session saves nothing either.
    #[tokio::test]
    async fn an_ephemeral_session_clears_into_another_ephemeral_one() {
        let mut cfg = TestConfig::new("clear-ephemeral").build().test_clone();
        cfg.sessions_dir = PathBuf::new();
        let fresh = start_fresh_session(&cfg, &ui()).await.unwrap();
        assert_eq!(fresh.cfg.session_id, "");
        assert_eq!(fresh.history.rollout_path(), None);
    }
}
