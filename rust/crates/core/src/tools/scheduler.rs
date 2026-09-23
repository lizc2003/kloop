use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Result, bail};
use kloop_protocol::ToolDef;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::ToolCtx;
use crate::config::Config;
use crate::scheduler::{CheckOutcome, CheckRunner};

pub fn cron_create_def() -> ToolDef {
    ToolDef {
        name: "cron_create".into(),
        description: "Schedule a prompt for a future local-time cron match. recurring defaults to true; durable defaults to false. Session-only jobs die with this session. Durable jobs are stored in the user's private kloop scheduler store, partitioned by project and owner session. Recurring jobs expire after seven days, after one final due fire. With check, each fire first runs that shell command (under the same permission gate and sandbox as bash): exit 0 skips the fire without waking you; otherwise the prompt is delivered with the check's output. Use it for polling jobs where most fires find nothing new. Returns an ID for cron_delete.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "cron": {"type": "string", "description": "Five fields: minute hour day-of-month month day-of-week, in the runtime's local timezone"},
                "prompt": {"type": "string", "description": "Prompt to enqueue when the job fires"},
                "recurring": {"type": "boolean", "description": "Repeat on every match (default true); false fires once and deletes"},
                "durable": {"type": "boolean", "description": "Persist in the private per-project owner store (default false)"},
                "check": {"type": "string", "description": "Optional shell command run before each fire; exit 0 means nothing to do and the prompt is not delivered"}
            },
            "required": ["cron", "prompt"],
            "additionalProperties": false
        }),
    }
}

pub fn cron_delete_def() -> ToolDef {
    ToolDef {
        name: "cron_delete".into(),
        description: "Cancel a cron job owned by this session, whether session-only or durable."
            .into(),
        schema: json!({
            "type": "object",
            "properties": {"id": {"type": "string"}},
            "required": ["id"],
            "additionalProperties": false
        }),
    }
}

pub fn cron_list_def() -> ToolDef {
    ToolDef {
        name: "cron_list".into(),
        description: "List cron jobs visible to this owner session. The result is stably ordered and does not invent runtime status or next-run fields.".into(),
        schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    }
}

pub fn schedule_wakeup_def() -> ToolDef {
    ToolDef {
        name: "schedule_wakeup".into(),
        description: "Schedule the next dynamic /loop wakeup for this owner session. A new call atomically replaces the prior dynamic wakeup. delay_seconds is rounded, clamped to 60–3600, and mapped to the next minute boundary. Pass stop=true by itself to cancel only dynamic loop wakeups; fixed recurring cron jobs are not cancelled.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "delay_seconds": {"type": "number", "description": "Seconds from now; rounded and clamped to 60–3600"},
                "reason": {"type": "string", "description": "Short user-visible scheduling reason"},
                "prompt": {"type": "string", "description": "The /loop prompt to run on wakeup"},
                "stop": {"type": "boolean", "description": "Cancel the dynamic wakeup; when true all other fields are ignored"}
            },
            "additionalProperties": false
        }),
    }
}

pub async fn cron_create_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    require_top_level(ctx, "cron_create")?;
    strict_object(
        input,
        &["cron", "prompt", "recurring", "durable", "check"],
        "cron_create",
    )?;
    let cron = required_string(input, "cron", "cron_create")?;
    let prompt = required_string(input, "prompt", "cron_create")?;
    let recurring = optional_bool(input, "recurring", true, "cron_create")?;
    let durable = optional_bool(input, "durable", false, "cron_create")?;
    let check = match input.get("check") {
        None => None,
        Some(_) => Some(required_string(input, "check", "cron_create")?),
    };
    bind(ctx)?;
    let job = ctx
        .cfg
        .scheduler
        .create(cron, prompt, recurring, durable, check)?;
    ctx.ui.emit(&crate::event::Event::ScheduledTaskUpdated(
        crate::event::ScheduledTask {
            id: job.id.clone(),
            origin: crate::event::ScheduledTaskOrigin::Cron,
            status: crate::event::ScheduledTaskStatus::Scheduled,
            scheduled_for_ms: Some(job.next_fire_at_ms),
            reason: None,
            detail: Some(if job.durable {
                "durable".into()
            } else {
                "session-only".into()
            }),
        },
    ));
    let human = job.human_schedule(ctx.cfg.scheduler.timezone());
    let durability = if job.durable {
        "Durable (private per-project owner store)"
    } else {
        "Session-only (not written to disk; dies when this session closes)"
    };
    if job.recurring {
        Ok(format!(
            "Scheduled recurring job {} ({human}). {durability}. Auto-expires after 7 days, after one final due fire. Use cron_delete to cancel sooner.",
            job.id
        ))
    } else {
        Ok(format!(
            "Scheduled one-shot task {} ({human}). {durability}. It will fire once then auto-delete.",
            job.id
        ))
    }
}

pub async fn cron_delete_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    require_top_level(ctx, "cron_delete")?;
    strict_object(input, &["id"], "cron_delete")?;
    let id = required_string(input, "id", "cron_delete")?;
    bind(ctx)?;
    ctx.cfg.scheduler.delete(id)?;
    ctx.ui.emit(&crate::event::Event::ScheduledTaskUpdated(
        crate::event::ScheduledTask {
            id: id.into(),
            origin: crate::event::ScheduledTaskOrigin::Cron,
            status: crate::event::ScheduledTaskStatus::Cancelled,
            scheduled_for_ms: None,
            reason: None,
            detail: None,
        },
    ));
    Ok(format!("Cancelled job {id}."))
}

pub async fn cron_list_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    require_top_level(ctx, "cron_list")?;
    strict_object(input, &[], "cron_list")?;
    bind(ctx)?;
    let jobs = ctx.cfg.scheduler.list()?;
    if jobs.is_empty() {
        return Ok("No scheduled jobs.".into());
    }
    Ok(jobs
        .iter()
        .map(|job| {
            let kind = if job.recurring {
                "recurring"
            } else {
                "one-shot"
            };
            let durability = if job.durable {
                "durable"
            } else {
                "session-only"
            };
            let prompt: String = job.prompt.chars().take(80).collect();
            let check = match &job.check {
                Some(check) => format!(" (check: `{check}`)"),
                None => String::new(),
            };
            format!(
                "{} — {} ({kind}) [{durability}]{check}: {prompt}",
                job.id,
                job.human_schedule(ctx.cfg.scheduler.timezone())
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

pub async fn schedule_wakeup_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    require_top_level(ctx, "schedule_wakeup")?;
    let object = strict_object(
        input,
        &["delay_seconds", "reason", "prompt", "stop"],
        "schedule_wakeup",
    )?;
    let stop = optional_bool(input, "stop", false, "schedule_wakeup")?;
    bind(ctx)?;
    if stop {
        let result = ctx.cfg.scheduler.stop_wakeup()?;
        ctx.ui.emit(&crate::event::Event::ScheduledTaskUpdated(
            crate::event::ScheduledTask {
                id: "dynamic-loop".into(),
                origin: crate::event::ScheduledTaskOrigin::LoopWakeup,
                status: crate::event::ScheduledTaskStatus::Cancelled,
                scheduled_for_ms: None,
                reason: None,
                detail: Some(format!("{} wakeup(s)", result.cancelled_wakeups)),
            },
        ));
        return Ok(format!(
            "Dynamic loop stopped; cancelled {} pending wakeup(s). Fixed-interval cron jobs were not changed.",
            result.cancelled_wakeups
        ));
    }
    let delay = object
        .get("delay_seconds")
        .and_then(Value::as_f64)
        .ok_or_else(|| {
            anyhow::anyhow!("`delay_seconds` and `reason` are required when `stop` is not true.")
        })?;
    let reason = object
        .get("reason")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("`delay_seconds` and `reason` are required when `stop` is not true.")
        })?;
    let prompt = object
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("`prompt` is required when `stop` is not true."))?;
    let result = ctx.cfg.scheduler.schedule_wakeup(delay, reason, prompt)?;
    ctx.ui.emit(&crate::event::Event::ScheduledTaskUpdated(
        crate::event::ScheduledTask {
            id: "dynamic-loop".into(),
            origin: crate::event::ScheduledTaskOrigin::LoopWakeup,
            status: crate::event::ScheduledTaskStatus::Scheduled,
            scheduled_for_ms: Some(result.scheduled_for_ms),
            reason: Some(reason.into()),
            detail: Some(format!("delay {}s", result.clamped_delay_seconds)),
        },
    ));
    Ok(format!(
        "Scheduled dynamic loop wakeup for {} ms since epoch (delay {}s{}); replaced {} previous wakeup(s).",
        result.scheduled_for_ms,
        result.clamped_delay_seconds,
        if result.was_clamped { ", clamped" } else { "" },
        result.cancelled_wakeups
    ))
}

fn bind(ctx: &ToolCtx) -> Result<()> {
    ctx.cfg.scheduler.bind_owner(ctx.cfg.session_id.clone())?;
    ctx.cfg
        .scheduler
        .bind_check_runner(|| Arc::new(GatedCheck::new(Arc::clone(&ctx.cfg))));
    Ok(())
}

/// A scheduled `check`, run exactly as a model `bash` call without the model:
/// the same `check_call` (deny / safety / ask / approver) and then the same
/// foreground run with its sandbox and escalation — the `/name` `!cmd` path's
/// shape. Holds the session's Config; `Scheduler::shutdown` drops it.
struct GatedCheck {
    cfg: Arc<Config>,
    cancel: CancellationToken,
}

impl GatedCheck {
    fn new(cfg: Arc<Config>) -> Self {
        Self {
            cfg,
            cancel: CancellationToken::new(),
        }
    }
}

impl Drop for GatedCheck {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl CheckRunner for GatedCheck {
    fn run<'a>(
        &'a self,
        command: &'a str,
    ) -> Pin<Box<dyn Future<Output = CheckOutcome> + Send + 'a>> {
        Box::pin(async move {
            let ctx = ToolCtx::harness(Arc::clone(&self.cfg), self.cancel.clone());
            let workspace = self.cfg.effective_workspace();
            let input = json!({ "command": command });
            let sandbox_auto = super::bash::sandbox_auto_allowed("bash", &input, &workspace);
            if let Err(reason) = workspace
                .permissions
                .check_call("bash", &input, 0, sandbox_auto)
                .await
            {
                return CheckOutcome::Unavailable(format!("blocked: {reason}"));
            }
            let sandbox = workspace.sandbox.clone();
            match super::bash::run_foreground_bash(command, None, sandbox, &ctx, &workspace).await {
                Ok(run) if run.success => CheckOutcome::Passed,
                Ok(run) => CheckOutcome::Failed(run.text),
                Err(error) => CheckOutcome::Unavailable(format!("{error:#}")),
            }
        })
    }
}

/// Dispatch already refuses the depth and the surface for every gated built-in
/// ([`crate::tools::builtin::Builtin::unoffered`]); this stays as the tool's own
/// precondition, and owns the one condition the door does not know about — that
/// a depth-0 Agent can still be someone's child.
fn require_top_level(ctx: &ToolCtx, tool: &str) -> Result<()> {
    if ctx.depth != 0 || !ctx.cfg.agent_label().is_empty() {
        bail!("{tool} is available only to the top-level session owner");
    }
    if !ctx.cfg.surface.scheduler {
        bail!("{tool} is unavailable on this frontend surface");
    }
    Ok(())
}

fn strict_object<'a>(
    input: &'a Value,
    allowed: &[&str],
    tool: &str,
) -> Result<&'a Map<String, Value>> {
    let object = input
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("{tool}: input must be an object"))?;
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        bail!("{tool}: unexpected parameter '{key}'");
    }
    Ok(object)
}

fn required_string<'a>(input: &'a Value, key: &str, tool: &str) -> Result<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{tool}: missing required string argument '{key}'"))
}

fn optional_bool(input: &Value, key: &str, default: bool, tool: &str) -> Result<bool> {
    match input.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| anyhow::anyhow!("{tool}: '{key}' must be a boolean")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{Mode, PermissionRules, Permissions};

    fn check_runner(permissions: Option<Permissions>) -> GatedCheck {
        let base = crate::tools::testutil::TestConfig::new("gated-check").build();
        let mut cfg = (*base).clone();
        if let Some(permissions) = permissions {
            cfg.permissions = Arc::new(permissions);
        }
        GatedCheck::new(Arc::new(cfg))
    }

    #[tokio::test]
    async fn a_check_runs_as_bash_and_reports_by_exit_status() {
        let runner = check_runner(None);
        assert_eq!(runner.run("true").await, CheckOutcome::Passed);
        assert_eq!(
            runner.run("echo red; exit 3").await,
            CheckOutcome::Failed("red\n\n[exit status 3]".into())
        );
    }

    #[tokio::test]
    async fn a_check_faces_the_bash_permission_gate_on_every_run() {
        let denied = Permissions::new(
            Mode::Manual,
            &PermissionRules {
                allow: Vec::new(),
                deny: vec!["bash".into()],
                ask: Vec::new(),
            },
            std::env::current_dir().unwrap(),
            None,
        )
        .unwrap();
        let outcome = check_runner(Some(denied)).run("true").await;
        let CheckOutcome::Unavailable(reason) = &outcome else {
            panic!("a denied check must not report a verdict: {outcome:?}");
        };
        assert!(reason.starts_with("blocked: "), "{reason}");
        assert!(reason.contains("deny permission rule"), "{reason}");
    }

    /// End to end through the tool: `cron_create` binds the gated runner, the
    /// fire runs the check as real bash, and a failure reaches the inbox.
    #[tokio::test]
    async fn cron_create_check_fires_through_real_bash_and_lists() {
        use crate::inbox::{Inbox, InboxItem};
        use crate::scheduler::{ManualClock, Scheduler, SchedulerTimeZone};

        let clock = ManualClock::new(1_785_758_400_000); // 2026-08-03 12:00 UTC
        let inbox = Arc::new(Inbox::default());
        let scheduler = Scheduler::with_clock(
            Arc::clone(&inbox),
            None,
            clock.clone(),
            SchedulerTimeZone::named("UTC").unwrap(),
        );
        let mut ctx = crate::tools::testutil::test_ctx(0, "cron-check");
        let mut cfg = ctx.cfg.test_clone();
        cfg.inbox = Arc::clone(&inbox);
        cfg.scheduler = Arc::clone(&scheduler);
        cfg.session_id = "owner-a".into();
        cfg.surface.scheduler = true;
        ctx.cfg = Arc::new(cfg);

        let empty = cron_create_tool(
            &json!({"cron": "*/5 * * * *", "prompt": "x", "check": " "}),
            &ctx,
        )
        .await;
        assert!(empty.is_err());
        cron_create_tool(
            &json!({"cron": "*/5 * * * *", "prompt": "watch CI", "check": "echo red; exit 1"}),
            &ctx,
        )
        .await
        .unwrap();
        let listed = cron_list_tool(&json!({}), &ctx).await.unwrap();
        assert!(
            listed.contains("[session-only] (check: `echo red; exit 1`): watch CI"),
            "{listed}"
        );

        clock.set(scheduler.list().unwrap()[0].next_fire_at_ms);
        let delivered = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let items = inbox.drain();
                if !items.is_empty() {
                    return items;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a failing check delivers the prompt");
        let [InboxItem::ScheduledPrompt { prompt, .. }] = delivered.as_slice() else {
            panic!("one scheduled prompt: {delivered:?}");
        };
        assert_eq!(
            prompt,
            "watch CI\n\n[scheduled check `echo red; exit 1` failed]\nred\n\n[exit status 1]"
        );
        scheduler.shutdown().await;
    }
}
