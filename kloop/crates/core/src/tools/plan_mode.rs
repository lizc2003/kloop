//! `exit_plan_mode` (plan 37): the model's escape from plan mode. It presents
//! the plan it built during read-only exploration for the user's approval; on
//! approval the session leaves plan mode (restoring the pre-plan mode) and the
//! model may act, on denial it stays in plan mode to keep planning — cc's
//! ExitPlanMode. Session-control like the worktree tools: depth-0 only, and the
//! approval itself runs inside `Permissions::confirm_exit_plan` so the plan text
//! rides the same y/n popup a file-change diff does (its scrollable `preview`).

use anyhow::Result;
use anyhow::bail;
use serde_json::Value;
use serde_json::json;

use super::ToolCtx;
use super::str_arg;
use crate::config::EffectiveWorkspace;
use crate::permissions::Mode;
use crate::permissions::PlanExitOutcome;
use kloop_protocol::ToolDef;

pub(super) fn enter_plan_mode_def() -> ToolDef {
    ToolDef {
        name: "enter_plan_mode".into(),
        description: "Enter plan mode before a non-trivial implementation. This changes the current session to read-only exploration: mutating tools stay blocked until exit_plan_mode presents the finished plan for approval. The call is idempotent. Only the top-level agent may use it.".into(),
        schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    }
}

pub(super) async fn enter_plan_mode_tool(
    _input: &Value,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
) -> Result<String> {
    if ctx.depth >= 1 {
        bail!("enter_plan_mode: only the top-level agent can enter plan mode");
    }
    let perms = &workspace.permissions;
    if perms.enter_plan() {
        ctx.ui.emit(&crate::event::Event::ModeChanged(Mode::Plan));
        Ok("Entered plan mode. Continue with read-only exploration, then call exit_plan_mode with the complete implementation plan for approval.".into())
    } else {
        Ok("Already in plan mode. Continue with read-only exploration and do not call mutating tools until the plan is approved.".into())
    }
}

pub(super) fn exit_plan_mode_def() -> ToolDef {
    ToolDef {
        name: "exit_plan_mode".into(),
        description: "Present your implementation plan for the user's approval and leave plan \
            mode. Use this ONLY when the session is in plan mode and you have finished exploring \
            (read-only) and have a concrete, step-by-step plan. Pass the full plan text; the user \
            sees it and approves or rejects. On approval, plan mode turns off and you may make \
            changes; on rejection, you stay in plan mode — refine the plan and call this again. Do \
            not use it to ask general questions or when not in plan mode."
            .into(),
        schema: json!({
            "type": "object",
            "properties": {
                "plan": {"type": "string", "description": "The full implementation plan to show the user for approval (markdown)"}
            },
            "required": ["plan"]
        }),
    }
}

pub(super) async fn exit_plan_mode_tool(
    input: &Value,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
) -> Result<String> {
    // Session-scoped like the worktree tools: a sub-agent is read-only in plan
    // mode but does not manage the session's mode.
    if ctx.depth >= 1 {
        bail!("exit_plan_mode: only the top-level agent can leave plan mode");
    }
    let perms = &workspace.permissions;
    if perms.mode() != Mode::Plan {
        bail!("exit_plan_mode: the session is not in plan mode, so there is nothing to exit");
    }
    let plan = str_arg(input, "plan", "exit_plan_mode")?;
    match perms.confirm_exit_plan(plan, ctx.depth).await {
        PlanExitOutcome::Approved(mode) => {
            // Keep a cwd/mode-tracking client (the TUI status bar) honest.
            ctx.ui.emit(&crate::event::Event::ModeChanged(mode));
            Ok(format!(
                "The user approved the plan. Plan mode is off (now {}); go ahead and implement it.",
                mode.label()
            ))
        }
        // Not an is_error: the model asked and got a considered "not yet".
        PlanExitOutcome::Declined => Ok("The user did not approve the plan and wants to keep \
            planning. Stay in plan mode: keep exploring read-only, refine the plan, and call \
            exit_plan_mode again when it is ready."
            .into()),
        PlanExitOutcome::NoApprover => {
            bail!("exit_plan_mode: no one is available to approve the plan in this mode")
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::permissions::Approver;
    use crate::permissions::ConfirmRequest;
    use crate::permissions::Decision;
    use crate::permissions::Mode;
    use crate::permissions::PermissionRules;
    use crate::permissions::Permissions;
    use crate::tools::ToolCtx;
    use crate::tools::testutil::*;
    use serde_json::json;
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::Mutex;

    /// Answers each confirm with the next scripted decision and records the plan
    /// text it was shown.
    struct PlanApprover {
        script: Mutex<Vec<Decision>>,
        previews: Mutex<Vec<Option<String>>>,
    }

    impl PlanApprover {
        fn new(script: Vec<Decision>) -> Arc<Self> {
            Arc::new(PlanApprover {
                script: Mutex::new(script),
                previews: Mutex::new(Vec::new()),
            })
        }
    }

    impl Approver for PlanApprover {
        fn confirm(
            &self,
            req: ConfirmRequest,
        ) -> Pin<Box<dyn Future<Output = Decision> + Send + '_>> {
            self.previews.lock().unwrap().push(req.preview.clone());
            let mut script = self.script.lock().unwrap();
            let decision = if script.is_empty() {
                Decision::Deny
            } else {
                script.remove(0)
            };
            Box::pin(async move { decision })
        }
    }

    /// Swap the ctx's gate for a plan-mode one with a scripted approver.
    fn plan_ctx(ctx: ToolCtx, approver: Arc<PlanApprover>) -> ToolCtx {
        let perms = Permissions::new(
            Mode::Plan,
            &PermissionRules::default(),
            PathBuf::from("/work/proj"),
            Some(approver),
        )
        .unwrap();
        let mut cfg = ctx.cfg.test_clone();
        cfg.permissions = Arc::new(perms);
        ToolCtx {
            cfg: Arc::new(cfg),
            ..ctx
        }
    }

    fn mode_ctx(ctx: ToolCtx, mode: Mode, approver: Arc<PlanApprover>) -> ToolCtx {
        let perms = Permissions::new(
            mode,
            &PermissionRules::default(),
            PathBuf::from("/work/proj"),
            Some(approver),
        )
        .unwrap();
        let mut cfg = ctx.cfg.test_clone();
        cfg.permissions = Arc::new(perms);
        ToolCtx {
            cfg: Arc::new(cfg),
            ..ctx
        }
    }

    /// Enter is a session transition: the first call changes mode, a repeated
    /// call is idempotent, and Exit restores the mode from before the first call.
    #[tokio::test]
    async fn enter_is_idempotent_and_exit_restores_original_mode() {
        let approver = PlanApprover::new(vec![Decision::Allow(
            crate::permissions::ApprovalScope::Once,
        )]);
        let ctx = mode_ctx(test_ctx(0, "planenter"), Mode::AcceptEdits, approver);

        let (first, first_error) = run_tool("enter_plan_mode", json!({}), &ctx).await;
        assert!(!first_error, "{first}");
        assert!(first.contains("Entered plan mode"), "{first}");
        assert_eq!(ctx.cfg.permissions.mode(), Mode::Plan);

        let (second, second_error) = run_tool("enter_plan_mode", json!({}), &ctx).await;
        assert!(!second_error, "{second}");
        assert!(second.contains("Already in plan mode"), "{second}");

        let (exit, exit_error) =
            run_tool("exit_plan_mode", json!({"plan": "implement it"}), &ctx).await;
        assert!(!exit_error, "{exit}");
        assert_eq!(ctx.cfg.permissions.mode(), Mode::AcceptEdits);
    }

    #[tokio::test]
    async fn subagent_cannot_enter_plan_mode() {
        let ctx = test_ctx(1, "planentersub");
        let (out, is_error) = run_tool("enter_plan_mode", json!({}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("top-level"), "{out}");
    }

    /// Approve: plan mode turns off (restores manual), the result confirms it,
    /// and the approver was shown the plan text as the popup preview.
    #[tokio::test]
    async fn approve_exits_plan_mode_and_reports_it() {
        let approver = PlanApprover::new(vec![Decision::Allow(
            crate::permissions::ApprovalScope::Once,
        )]);
        let ctx = plan_ctx(test_ctx(0, "planexit"), approver.clone());
        let (out, is_error) =
            run_tool("exit_plan_mode", json!({"plan": "1. do X\n2. do Y"}), &ctx).await;
        assert!(!is_error, "{out}");
        assert!(out.contains("approved") && out.contains("manual"), "{out}");
        assert_eq!(ctx.cfg.permissions.mode(), Mode::Manual, "left plan mode");
        assert_eq!(
            approver.previews.lock().unwrap().as_slice(),
            &[Some("1. do X\n2. do Y".to_string())]
        );

        // The loop closes: a write that was hard-blocked in plan mode now
        // reaches the ordinary ask path (manual mode) — the block message is
        // gone, so the switch actually took effect. (The approver's script is
        // spent, so the ask resolves to deny, but with the "declined" wording.)
        let err = ctx
            .cfg
            .permissions
            .check(
                "write_file",
                &json!({"path": "src/x.rs", "content": "y"}),
                0,
            )
            .await
            .unwrap_err();
        assert!(!err.contains("plan mode"), "no longer plan-blocked: {err}");
        assert!(err.contains("declined"), "{err}");
    }

    /// Reject: stays in plan mode, result guides continued planning (not error).
    #[tokio::test]
    async fn reject_keeps_plan_mode_and_guides() {
        let approver = PlanApprover::new(vec![Decision::Deny]);
        let ctx = plan_ctx(test_ctx(0, "planstay"), approver);
        let (out, is_error) = run_tool("exit_plan_mode", json!({"plan": "draft"}), &ctx).await;
        assert!(!is_error, "{out}");
        assert!(
            out.contains("keep planning") || out.contains("Stay in plan mode"),
            "{out}"
        );
        assert_eq!(ctx.cfg.permissions.mode(), Mode::Plan, "still in plan mode");
    }

    /// Called outside plan mode (allow_all is bypass), it refuses rather than
    /// silently doing nothing.
    #[tokio::test]
    async fn errors_when_not_in_plan_mode() {
        let ctx = test_ctx(0, "planoff");
        let (out, is_error) = run_tool("exit_plan_mode", json!({"plan": "x"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("not in plan mode"), "{out}");
    }

    /// A sub-agent cannot leave plan mode (session-scoped, top-level only).
    #[tokio::test]
    async fn subagent_cannot_exit_plan_mode() {
        let approver = PlanApprover::new(vec![Decision::Allow(
            crate::permissions::ApprovalScope::Once,
        )]);
        // depth-1 ctx, still swap in a plan-mode gate to reach the depth guard.
        let ctx = plan_ctx(test_ctx(1, "plansub"), approver);
        let (out, is_error) = run_tool("exit_plan_mode", json!({"plan": "x"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("top-level"), "{out}");
    }
}
