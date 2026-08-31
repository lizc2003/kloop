//! `enter_worktree` / `exit_worktree` (plan 35 slice 2): let the SESSION move
//! into an isolated git worktree at runtime and back out again — cc's
//! EnterWorktree/ExitWorktree and codex's enter_worktree/exit_worktree, "one
//! tree per session". Distinct from `run_agent {isolation:worktree}` (slice 1),
//! which isolates a throwaway sub-agent. The switch flips the session's active
//! worktree slot; the `effective_*` accessors make it take effect immediately.

use std::path::PathBuf;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use super::ToolCtx;
use crate::worktree;
use crate::worktree::ExitAction;
use kloop_protocol::ToolDef;

pub(super) fn enter_worktree_def() -> ToolDef {
    ToolDef {
        name: "enter_worktree".into(),
        description: "Create a managed git worktree and switch this session into it, or pass path to enter an existing registered worktree. name and path are optional but mutually exclusive; omitting both generates a name. Existing paths require approval unless they are managed by the current worktree session."
            .into(),
        schema: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Optional name. Each '/'-separated segment may contain letters, digits, dots, underscores, and dashes; max 64 chars total."
                },
                "path": {
                    "type": "string",
                    "description": "Optional path to an existing registered worktree. Mutually exclusive with name."
                }
            },
            "additionalProperties": false
        }),
    }
}

pub(super) fn exit_worktree_def() -> ToolDef {
    ToolDef {
        name: "exit_worktree".into(),
        description: "Leave the current worktree and return to the original working directory. action is required: keep preserves the tree and branch; remove deletes a current-session managed tree. remove refuses content changes or commits unless discard_changes is true."
            .into(),
        schema: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["keep", "remove"],
                    "description": "keep preserves the worktree; remove deletes a current-session managed worktree"
                },
                "discard_changes": {
                    "type": "boolean",
                    "description": "Allow remove to discard observed content changes or commits"
                }
            },
            "required": ["action"],
            "additionalProperties": false
        }),
    }
}

pub(super) async fn enter_worktree_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    guard(ctx, "enter_worktree")?;
    let parsed = parse_enter(input)?;
    let message = match parsed {
        EnterInput::Create(name) => worktree::enter(&ctx.cfg, &name).await,
        EnterInput::Existing(path) => worktree::enter_existing(&ctx.cfg, &path).await,
    }
    .map_err(|error| anyhow!("enter_worktree: {error:#}"))?;
    if let Some(active) = ctx.cfg.active_worktree.read().unwrap().as_ref() {
        ctx.ui.emit(&crate::event::Event::CwdChanged {
            cwd: active.cwd.display().to_string(),
            branch: Some(active.branch.clone()),
        });
    }
    Ok(message)
}

pub(super) async fn exit_worktree_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    guard(ctx, "exit_worktree")?;
    let (action, discard_changes) = parse_exit(input)?;
    let message = worktree::exit(&ctx.cfg, action, discard_changes)
        .await
        .map_err(|error| anyhow!("exit_worktree: {error:#}"))?;
    ctx.ui.emit(&crate::event::Event::CwdChanged {
        cwd: ctx.cfg.cwd.display().to_string(),
        branch: None,
    });
    Ok(message)
}

fn guard(ctx: &ToolCtx, tool: &str) -> Result<()> {
    if ctx.depth >= 1 {
        bail!("{tool}: only the top-level agent manages the session worktree");
    }
    if !ctx.cfg.surface.worktree {
        bail!("{tool}: worktree mode is not enabled for this session");
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum EnterInput {
    Create(String),
    Existing(PathBuf),
}

fn strict_object<'a>(input: &'a Value, tool: &str) -> Result<&'a Map<String, Value>> {
    input
        .as_object()
        .ok_or_else(|| anyhow!("{tool}: input must be an object"))
}

fn reject_unknown(object: &Map<String, Value>, allowed: &[&str], tool: &str) -> Result<()> {
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        bail!("{tool}: unexpected parameter `{key}`");
    }
    Ok(())
}

fn optional_string(object: &Map<String, Value>, key: &str, tool: &str) -> Result<Option<String>> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => bail!("{tool}: `{key}` must be a string"),
    }
}

fn parse_enter(input: &Value) -> Result<EnterInput> {
    let object = strict_object(input, "enter_worktree")?;
    reject_unknown(object, &["name", "path"], "enter_worktree")?;
    let name = optional_string(object, "name", "enter_worktree")?;
    let path = optional_string(object, "path", "enter_worktree")?;
    match (name, path) {
        (Some(_), Some(_)) => bail!("Provide at most one of `name` or `path`, not both."),
        (Some(name), None) => {
            worktree::validate_name(&name)?;
            Ok(EnterInput::Create(name))
        }
        (None, Some(path)) => Ok(EnterInput::Existing(PathBuf::from(path))),
        (None, None) => Ok(EnterInput::Create(worktree::generated_name())),
    }
}

fn parse_exit(input: &Value) -> Result<(ExitAction, bool)> {
    let object = strict_object(input, "exit_worktree")?;
    reject_unknown(object, &["action", "discard_changes"], "exit_worktree")?;
    let action = match object.get("action") {
        Some(Value::String(action)) if action == "keep" => ExitAction::Keep,
        Some(Value::String(action)) if action == "remove" => ExitAction::Remove,
        Some(Value::String(_)) | None => {
            bail!("exit_worktree: `action` must be one of \"keep\" or \"remove\"")
        }
        Some(_) => bail!("exit_worktree: `action` must be a string"),
    };
    let discard_changes = match object.get("discard_changes") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => bail!("exit_worktree: `discard_changes` must be a boolean"),
    };
    Ok((action, discard_changes))
}

#[cfg(test)]
mod tests {
    use crate::tools::testutil::*;
    use kloop_protocol::AssistantBlock;
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
        assert!(out.contains("Created worktree"), "{out}");
        // The session's effective cwd is now the worktree.
        let wt = repo.join(".kloop/worktrees/feat");
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

        let (out, is_error) = run_tool("exit_worktree", json!({"action": "keep"}), &base).await;
        assert!(!is_error, "{out}");
        assert!(out.contains("back in"), "{out}");
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
        let (out, is_error) = run_tool("exit_worktree", json!({"action": "remove"}), &ctx).await;
        assert!(!is_error, "{out}");
        assert!(out.contains("removed worktree"), "{out}");
        assert!(!repo.join(".kloop/worktrees/look").exists(), "tree removed");
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
        assert!(out.contains("Already in a worktree session"), "{out}");
        let _ = run_tool(
            "exit_worktree",
            json!({"action": "remove", "discard_changes": true}),
            &ctx,
        )
        .await;

        let (out, is_error) = run_tool("enter_worktree", json!({"name": "../escape"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("Invalid worktree name"), "{out}");

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
            &crate::shell_programs::ShellPrograms::native_posix(),
        );
        assert!(on.iter().any(|d| d.name == "enter_worktree"));
        assert!(on.iter().any(|d| d.name == "exit_worktree"));
        let off = all_tool_defs(
            0,
            &[],
            30,
            Default::default(),
            &crate::shell_programs::ShellPrograms::native_posix(),
        );
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
        let (out, is_error) = run_tool(
            "exit_worktree",
            json!({"action": "remove", "discard_changes": true}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(out.contains("Discarded"), "{out}");
        assert!(!repo.join(".kloop/worktrees/d").exists(), "tree discarded");
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// A sub-agent inherits the parent's ACTIVE worktree as its base cwd — a
    /// task spawned while the session is in a worktree works inside it.
    #[tokio::test]
    async fn subagent_inherits_active_worktree() {
        let repo = temp_git_repo("inherit");
        let provider = Provider::mock(vec![
            vec![AssistantBlock::ToolUse {
                id: "w".into(),
                name: "write_file".into(),
                input: json!({"path": "sub.txt", "content": "s"}),
            }],
            vec![AssistantBlock::Text { text: "ok".into() }],
        ]);
        let ctx = git_ctx(with_provider(test_ctx(0, "inherit"), provider), &repo, true);
        run_tool("enter_worktree", json!({"name": "wt"}), &ctx).await;

        let (out, is_error) = run_tool("run_agent", json!({"prompt": "go"}), &ctx).await;
        assert!(!is_error, "{out}");
        // The sub-agent's relative write landed in the session's worktree.
        assert!(repo.join(".kloop/worktrees/wt/sub.txt").exists());
        assert!(!repo.join("sub.txt").exists());
        let _ = run_tool(
            "exit_worktree",
            json!({"action": "remove", "discard_changes": true}),
            &ctx,
        )
        .await;
        let _ = std::fs::remove_dir_all(&repo);
    }
}
