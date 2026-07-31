//! `enter_worktree` / `exit_worktree` (plan 35 slice 2): let the SESSION move
//! into an isolated git worktree at runtime and back out again — cc's
//! EnterWorktree/ExitWorktree and codex's enter_worktree/exit_worktree, "one
//! tree per session". Distinct from `task {isolation:worktree}` (slice 1),
//! which isolates a throwaway sub-agent. The switch flips the session's active
//! worktree slot; the `effective_*` accessors make it take effect immediately.

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Result;
use serde_json::json;
use serde_json::Value;

use super::str_arg;
use super::ToolCtx;
use crate::worktree;
use kloop_protocol::ToolDef;

pub(super) fn enter_worktree_def() -> ToolDef {
    ToolDef {
        name: "enter_worktree".into(),
        description:
            "Move your session into a fresh isolated git worktree — a separate checkout \
            on its own branch, created from HEAD — so you can make and test changes without \
            touching the main working tree. Your working directory switches to the worktree \
            immediately (relative paths, bash, and file edits all resolve there). Use it before a \
            risky or exploratory change; call exit_worktree when done. Only one worktree at a time."
                .into(),
        schema: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Short task name; becomes the worktree dir and branch (letters, digits, '.', '_', '-')"}
            },
            "required": ["name"]
        }),
    }
}

pub(super) fn exit_worktree_def() -> ToolDef {
    ToolDef {
        name: "exit_worktree".into(),
        description: "Leave the current worktree and return to the main working tree. By default \
            an unchanged worktree is removed and one with changes is kept on its branch for you or \
            the user to merge; pass discard_changes:true to throw the worktree away even if it has \
            changes."
            .into(),
        schema: json!({
            "type": "object",
            "properties": {
                "discard_changes": {"type": "boolean", "description": "Force-remove the worktree even if it has uncommitted changes or commits (default false)"}
            }
        }),
    }
}

pub(super) async fn enter_worktree_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    guard(ctx, "enter_worktree")?;
    let name = str_arg(input, "name", "enter_worktree")?;
    validate_name(name)?;
    let msg = worktree::enter(&ctx.cfg, name)
        .await
        .map_err(|e| anyhow!("enter_worktree: {e:#}"))?;
    // Tell a cwd-tracking client (the server) the session moved into the tree.
    if let Some(a) = ctx.cfg.active_worktree.read().unwrap().as_ref() {
        ctx.ui.emit(&crate::event::Event::CwdChanged {
            cwd: a.cwd.display().to_string(),
            branch: Some(a.branch.clone()),
        });
    }
    Ok(msg)
}

pub(super) async fn exit_worktree_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    guard(ctx, "exit_worktree")?;
    let discard = input["discard_changes"].as_bool().unwrap_or(false);
    let msg = worktree::exit(&ctx.cfg, discard)
        .await
        .map_err(|e| anyhow!("exit_worktree: {e:#}"))?;
    // Back in the main checkout (slot cleared) — announce the cwd is `cfg.cwd`.
    ctx.ui.emit(&crate::event::Event::CwdChanged {
        cwd: ctx.cfg.cwd.display().to_string(),
        branch: None,
    });
    Ok(msg)
}

/// Both tools are top-level and session-scoped: a sub-agent gets isolation via
/// `task {isolation:worktree}` instead, and a session without worktree mode
/// (server threads, --mock) never advertises these — so a stray call is
/// rejected rather than silently mutating state.
fn guard(ctx: &ToolCtx, tool: &str) -> Result<()> {
    if ctx.depth >= 1 {
        bail!("{tool}: only the top-level agent manages the session worktree");
    }
    if !ctx.cfg.surface.worktree {
        bail!("{tool}: worktree mode is not enabled for this session");
    }
    Ok(())
}

/// A worktree name becomes a directory (`.kloop-worktrees/<name>`) and a branch
/// (`kloop/worktree/<name>`), so restrict it to a safe leaf.
fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        bail!(
            "enter_worktree: invalid name '{name}' (use letters, digits, '.', '_', '-'; no '..')"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tools::testutil::*;
    use kloop_protocol::ContentBlock;
    use kloop_provider::Provider;
    use serde_json::json;

    /// enter_worktree moves the session in (cwd + effective_* switch), a
    /// subsequent relative write lands in the tree not the main repo, and
    /// exit_worktree keeps the dirty tree on its branch and returns to the repo.
    #[tokio::test]
    async fn enter_confines_writes_then_exit_keeps_dirty_tree() {
        let repo = temp_git_repo("enter");
        let base = git_ctx(test_ctx(0, "enter"), &repo, true);

        let (out, is_error) = run_tool("enter_worktree", json!({"name": "feat"}), &base).await;
        assert!(!is_error, "{out}");
        assert!(out.contains("Entered worktree"), "{out}");
        // The session's effective cwd is now the worktree.
        let wt = repo.join(".kloop-worktrees/feat");
        assert_eq!(base.cfg.effective_cwd(), wt);

        // A relative write resolves against the worktree, not the main repo.
        let (_, err) = run_tool(
            "write_file",
            json!({"path": "note.txt", "content": "hi"}),
            &base,
        )
        .await;
        assert!(!err);
        assert!(wt.join("note.txt").exists(), "write landed in the worktree");
        assert!(!repo.join("note.txt").exists(), "main repo untouched");

        let (out, is_error) = run_tool("exit_worktree", json!({}), &base).await;
        assert!(!is_error, "{out}");
        assert!(out.contains("Back in"), "{out}");
        assert!(
            out.contains("kloop/worktree/feat"),
            "kept tree named: {out}"
        );
        assert!(
            base.cfg.active_worktree.read().unwrap().is_none(),
            "slot cleared"
        );
        assert!(wt.join("note.txt").exists(), "dirty tree preserved");
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// A clean worktree is torn down on exit; effective cwd is back to the repo.
    #[tokio::test]
    async fn exit_removes_a_clean_tree() {
        let repo = temp_git_repo("clean");
        let ctx = git_ctx(test_ctx(0, "clean"), &repo, true);
        run_tool("enter_worktree", json!({"name": "look"}), &ctx).await;
        let (out, is_error) = run_tool("exit_worktree", json!({}), &ctx).await;
        assert!(!is_error, "{out}");
        assert!(out.contains("no changes"), "{out}");
        assert!(!repo.join(".kloop-worktrees/look").exists(), "tree removed");
        assert_eq!(ctx.cfg.effective_cwd(), repo, "back to the main repo");
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Entering while already inside a worktree is an error (one tree per
    /// session); an invalid name and a non-git repo are errors too.
    #[tokio::test]
    async fn enter_guards_double_entry_bad_name_and_non_git() {
        let repo = temp_git_repo("guard");
        let ctx = git_ctx(test_ctx(0, "guard"), &repo, true);
        run_tool("enter_worktree", json!({"name": "a"}), &ctx).await;
        let (out, is_error) = run_tool("enter_worktree", json!({"name": "b"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("already inside"), "{out}");
        let _ = run_tool("exit_worktree", json!({}), &ctx).await;

        let (out, is_error) = run_tool("enter_worktree", json!({"name": "../escape"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("invalid name"), "{out}");

        let nogit =
            std::env::temp_dir().join(format!("kloop-wt-enter-nogit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&nogit);
        std::fs::create_dir_all(&nogit).unwrap();
        let ctx = git_ctx(test_ctx(0, "nogit"), &nogit, true);
        let (out, is_error) = run_tool("enter_worktree", json!({"name": "x"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("git repository"), "{out}");
        let _ = std::fs::remove_dir_all(&nogit);
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// The enter/exit tools are advertised only when worktree mode is on, and a
    /// call in a session without it is rejected (server threads / --mock).
    #[tokio::test]
    async fn worktree_tools_gated_by_mode() {
        use crate::tools::all_tool_defs;
        let on = all_tool_defs(
            0,
            &[],
            30,
            crate::config::SurfaceCapabilities {
                worktree: true,
                ..Default::default()
            },
        );
        assert!(on.iter().any(|d| d.name == "enter_worktree"));
        assert!(on.iter().any(|d| d.name == "exit_worktree"));
        let off = all_tool_defs(0, &[], 30, Default::default());
        assert!(!off.iter().any(|d| d.name == "enter_worktree"));

        // Even if a model somehow calls it, a mode-off session refuses.
        let ctx = test_ctx(0, "gated"); // testutil default: worktree_enabled = false
        let (out, is_error) = run_tool("enter_worktree", json!({"name": "x"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("not enabled"), "{out}");
    }

    /// exit_worktree with discard_changes throws away even a dirty tree.
    #[tokio::test]
    async fn exit_discard_removes_dirty_tree() {
        let repo = temp_git_repo("discard");
        let ctx = git_ctx(test_ctx(0, "discard"), &repo, true);
        run_tool("enter_worktree", json!({"name": "d"}), &ctx).await;
        run_tool("write_file", json!({"path": "x.txt", "content": "y"}), &ctx).await;
        let (out, is_error) =
            run_tool("exit_worktree", json!({"discard_changes": true}), &ctx).await;
        assert!(!is_error, "{out}");
        assert!(out.contains("discarded"), "{out}");
        assert!(!repo.join(".kloop-worktrees/d").exists(), "tree discarded");
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// A sub-agent inherits the parent's ACTIVE worktree as its base cwd — a
    /// task spawned while the session is in a worktree works inside it.
    #[tokio::test]
    async fn subagent_inherits_active_worktree() {
        let repo = temp_git_repo("inherit");
        let provider = Provider::mock(vec![
            vec![ContentBlock::ToolUse {
                id: "w".into(),
                name: "write_file".into(),
                input: json!({"path": "sub.txt", "content": "s"}),
            }],
            vec![ContentBlock::Text { text: "ok".into() }],
        ]);
        let ctx = git_ctx(with_provider(test_ctx(0, "inherit"), provider), &repo, true);
        run_tool("enter_worktree", json!({"name": "wt"}), &ctx).await;

        let (out, is_error) = run_tool("task", json!({"prompt": "go"}), &ctx).await;
        assert!(!is_error, "{out}");
        // The sub-agent's relative write landed in the session's worktree.
        assert!(repo.join(".kloop-worktrees/wt/sub.txt").exists());
        assert!(!repo.join("sub.txt").exists());
        let _ = run_tool("exit_worktree", json!({"discard_changes": true}), &ctx).await;
        let _ = std::fs::remove_dir_all(&repo);
    }
}
