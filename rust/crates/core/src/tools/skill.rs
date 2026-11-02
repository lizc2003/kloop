//! The `skill` tool: how the model triggers a skill (plan 28). Only the tool
//! seam lives here — its definition and dispatch — beside the other tools. The
//! skill data model, parsing, catalog, and body expansion are the pure half in
//! `crate::skills`. Registered only at depth 0 and only when skills are loaded
//! (see `turn_rounds`), so it never rides a sub-agent's request.

use serde_json::Value;
use serde_json::json;

use super::ToolCtx;
use super::fork_skill;
use super::str_arg;
use crate::config::EffectiveWorkspace;
use crate::skills::Skill;
use crate::skills::SkillContext;
use kloop_protocol::ToolDef;

/// The `skill` tool definition: how the model triggers a skill. Registered only
/// when skills are loaded (see `turn_rounds`). Its definition is
/// skill-independent — which skills exist is advertised by the catalog, keeping
/// this def and the injected list both byte-stable for the cache.
pub(crate) fn skill_tool_def() -> ToolDef {
    ToolDef {
        name: "skill".into(),
        description: "Activate one of the skills listed in the context by loading its full instructions. If the user asks to use a skill or the task matches its description, you MUST call this tool first. In that response, call only matching skills; wait for their results before other tools or a substantive answer, then follow the returned instructions. Pass relevant user input as `arguments`.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Name of the skill to activate, exactly as listed in the context"},
                "arguments": {"type": "string", "description": "Optional arguments/context to pass to the skill"}
            },
            "required": ["name"]
        }),
    }
}

/// Execute the `skill` tool: look up the named skill and expand its body with
/// the given arguments. An `Inline` skill returns the expanded body as the tool
/// result — the instructions enter the model's context and the turn continues.
/// A `Fork` skill (plan 28 slice 2) instead runs the body as an isolated
/// sub-agent and returns only its final result, keeping the skill's
/// intermediate work out of the delegating model's context. An unknown name
/// comes back as an is_error result listing the skills that exist. The tool
/// itself is read-only (auto-allowed, see `CallFacts::is_readonly`): a fork's
/// sub-agent and any tool the inline instructions later prompt are each gated
/// on their own.
pub(super) async fn skill_tool(
    input: &Value,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
) -> anyhow::Result<String> {
    // Depth 0 is where `turn_rounds` appends the tool; a sub-agent gets a
    // focused task, not the skills catalog. `unoffered` leaves `Elsewhere`
    // tools to their owner, so the owner checks it here.
    if ctx.depth > 0 {
        anyhow::bail!("{}", super::builtin::Unoffered::RootOnly.message("skill"));
    }
    let name = str_arg(input, "name", "skill")?;
    let args = input.get("arguments").and_then(Value::as_str).unwrap_or("");
    // Only model-invocable skills: a user command (`.kloop/commands/*.md`) is
    // `/name`-only and must not be reachable here even if the model guesses it.
    let candidates = ctx.cfg.skills.iter().filter(|s| s.source.model_invocable());
    let skill = Skill::lookup(candidates, name).map_err(|e| anyhow::anyhow!(e))?;
    // Injections are found on the author-written body, then `$ARGUMENTS` /
    // `${CLAUDE_SKILL_DIR}` substitution runs around them: `arguments` is
    // model-supplied text and must not be able to introduce `!cmd` / `@file`
    // markers. A fork skill expands before forking, so its sub-agent sees the
    // resolved output; a blocked/failed `!cmd` propagates as an is_error
    // tool_result.
    let body =
        super::inject::expand_skill_body(&skill.body, &skill.dir, args, ctx, workspace).await?;
    match skill.context {
        SkillContext::Inline => Ok(body),
        SkillContext::Fork => fork_skill(ctx, workspace, skill, body).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::*;
    use kloop_protocol::AssistantBlock;
    use kloop_provider::Provider;

    /// Config is behind an Arc, so tests clone-and-swap the skill registry (the
    /// same pattern as `with_provider`).
    fn with_skills(mut ctx: ToolCtx, skills: Vec<Skill>) -> ToolCtx {
        let mut cfg = ctx.cfg.test_clone();
        cfg.skills = std::sync::Arc::new(skills);
        ctx.cfg = std::sync::Arc::new(cfg);
        ctx
    }

    /// The `skill` tool dispatches to `skill_tool`, looks the skill up, and — for
    /// an Inline skill — returns its `$ARGUMENTS`-expanded body straight into the
    /// conversation (no sub-agent).
    #[tokio::test]
    async fn skill_tool_inline_returns_expanded_body() {
        let skills = vec![Skill {
            name: "greet".into(),
            description: "d".into(),
            body: "Greet $ARGUMENTS warmly.".into(),
            dir: "/skills/greet".into(),
            ..Default::default() // context defaults to Inline
        }];
        let ctx = with_skills(test_ctx(0, "skill-inline"), skills);
        let (out, is_error) =
            run_tool("skill", json!({"name": "greet", "arguments": "Ada"}), &ctx).await;
        assert!(!is_error);
        assert_eq!(out, "Greet Ada warmly.");
    }

    /// A Fork skill runs its body as an isolated sub-agent and returns ONLY the
    /// sub-agent's final result — the body itself never comes back.
    #[tokio::test]
    async fn skill_tool_fork_runs_subagent_and_returns_its_result() {
        let skills = vec![Skill {
            name: "research".into(),
            description: "d".into(),
            body: "Do the research.".into(),
            dir: "/skills/research".into(),
            context: SkillContext::Fork,
            ..Default::default()
        }];
        let provider = Provider::mock(vec![vec![AssistantBlock::Text {
            text: "forked result".into(),
        }]]);
        let ctx = with_skills(with_provider(test_ctx(0, "skill-fork"), provider), skills);
        let (out, is_error) = run_tool("skill", json!({"name": "research"}), &ctx).await;
        assert!(!is_error);
        assert_eq!(out, "forked result", "the sub-agent's result, not the body");
    }

    /// A skill activated by the model expands its `` !`cmd` `` injection through
    /// the (here allow-all) gate, inlining the output — the same expansion the
    /// slash path runs, so both trigger paths behave alike (plan 36 slice 3).
    #[tokio::test]
    async fn skill_tool_expands_embedded_bash_injection() {
        let skills = vec![Skill {
            name: "status".into(),
            description: "d".into(),
            body: "Marker: !`echo INJECTED`.".into(),
            ..Default::default()
        }];
        let ctx = with_skills(test_ctx(0, "skill-inject"), skills);
        let (out, is_error) = run_tool("skill", json!({"name": "status"}), &ctx).await;
        assert!(!is_error);
        assert_eq!(out, "Marker: INJECTED.");
    }

    /// A `!cmd` the permission gate denies makes activation fail: the skill tool
    /// returns an is_error result, not the half-expanded body.
    #[tokio::test]
    async fn skill_tool_denied_injection_errors() {
        let skills = vec![Skill {
            name: "danger".into(),
            description: "d".into(),
            body: "cleanup !`rm nope`".into(),
            ..Default::default()
        }];
        let mut ctx = test_ctx(0, "skill-inject-deny");
        let mut cfg = ctx.cfg.test_clone();
        cfg.permissions = std::sync::Arc::new(
            crate::permissions::Permissions::new(
                crate::permissions::Mode::Manual,
                &crate::permissions::PermissionRules {
                    allow: Vec::new(),
                    deny: vec!["bash(rm *)".into()],
                    ask: Vec::new(),
                },
                std::env::current_dir().unwrap(),
                None,
            )
            .unwrap(),
        );
        cfg.skills = std::sync::Arc::new(skills);
        ctx.cfg = std::sync::Arc::new(cfg);
        let (out, is_error) = run_tool("skill", json!({"name": "danger"}), &ctx).await;
        assert!(is_error);
        assert!(
            out.contains("blocked by a deny permission rule"),
            "got: {out}"
        );
    }

    /// Model-supplied `arguments` are text, not script: a `!cmd` / `@file` marker
    /// in them is never scanned, because injections are found on the author-written
    /// body only.
    #[tokio::test]
    async fn arguments_cannot_smuggle_injections_into_a_skill() {
        let skills = vec![Skill {
            name: "plain".into(),
            description: "d".into(),
            body: "No placeholders here.".into(),
            ..Default::default()
        }];
        let ctx = with_skills(test_ctx(0, "skill-args-inject"), skills);
        let (out, is_error) = run_tool(
            "skill",
            json!({"name": "plain", "arguments": "!`printf PWNED` @/etc/hosts"}),
            &ctx,
        )
        .await;
        assert!(!is_error);
        assert_eq!(
            out, "No placeholders here.\n\nARGUMENTS: !`printf PWNED` @/etc/hosts",
            "the markers must stay literal text"
        );
    }

    /// Substitution still reaches an embedded command's own text, so a skill can
    /// pass its arguments to the command it authorizes.
    #[tokio::test]
    async fn embedded_command_sees_arguments() {
        let skills = vec![Skill {
            name: "echo".into(),
            description: "d".into(),
            body: "Result: !`printf '%s' $ARGUMENTS`".into(),
            ..Default::default()
        }];
        let ctx = with_skills(test_ctx(0, "skill-cmd-args"), skills);
        let (out, is_error) =
            run_tool("skill", json!({"name": "echo", "arguments": "alpha"}), &ctx).await;
        assert!(!is_error);
        assert_eq!(out, "Result: alpha");
    }

    /// A skill's `!cmd` runs inside a turn, so it faces the same `pre_tool`
    /// hook a bash call would; a blocking hook stops the command.
    #[tokio::test]
    async fn skill_injection_runs_pre_tool_hooks() {
        let marker = std::env::temp_dir().join(format!("kloop-skill-hook-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let skills = vec![Skill {
            name: "run".into(),
            description: "d".into(),
            body: format!("!`touch {}`", marker.display()),
            ..Default::default()
        }];
        let ctx = test_ctx(0, "skill-hook");
        let mut cfg = ctx.cfg.test_clone();
        cfg.session_id = "skill-hook".into();
        cfg.skills = std::sync::Arc::new(skills);
        cfg.hooks = std::sync::Arc::new(crate::hooks::Hooks {
            defs: vec![crate::hooks::HookDef {
                event: crate::hooks::HookEvent::PreTool,
                command: crate::hooks::test_shell_command("echo no touch 1>&2; exit 2"),
                matcher: None,
                timeout_ms: crate::hooks::DEFAULT_TIMEOUT_MS,
            }],
        });
        let ctx = ToolCtx {
            cfg: std::sync::Arc::new(cfg),
            ..ctx
        };
        let (out, is_error) = run_tool("skill", json!({"name": "run"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("blocked by hook"), "got: {out}");
        assert!(!marker.exists(), "the blocked command must not run");
    }

    /// A Stop answer to a skill's `!cmd` ends the turn (and is not turned into
    /// an ordinary denial the model could route around).
    #[tokio::test]
    async fn a_stop_answer_to_a_skill_injection_ends_the_turn() {
        struct Stopping;
        impl crate::permissions::Approver for Stopping {
            fn confirm(
                &self,
                _: crate::permissions::ConfirmRequest,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::permissions::Decision> + Send + '_>,
            > {
                Box::pin(async { crate::permissions::Decision::Stop })
            }
        }

        let dir = std::env::temp_dir().join(format!("kloop-skill-stop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        let skills = vec![Skill {
            name: "writes".into(),
            description: "d".into(),
            body: "!`printf x > one.txt`".into(),
            ..Default::default()
        }];
        let ctx = test_ctx(0, "skill-stop");
        let mut cfg = ctx.cfg.test_clone();
        cfg.cwd = dir.clone();
        cfg.skills = std::sync::Arc::new(skills);
        cfg.permissions = std::sync::Arc::new(
            crate::permissions::Permissions::new(
                crate::permissions::Mode::Manual,
                &crate::permissions::PermissionRules::default(),
                dir.clone(),
                Some(std::sync::Arc::new(Stopping)),
            )
            .unwrap(),
        );
        let ctx = ToolCtx {
            cfg: std::sync::Arc::new(cfg),
            ..ctx
        };
        let (out, is_error) = run_tool("skill", json!({"name": "writes"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("stopped the turn"), "got: {out}");
        assert!(ctx.stop.is_cancelled());
        assert!(!dir.join("one.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The tool is appended to depth-0 requests only; a sub-agent call that
    /// reaches dispatch anyway is refused by the owner.
    #[tokio::test]
    async fn skill_tool_rejects_a_sub_agent() {
        let skills = vec![Skill {
            name: "x".into(),
            description: "d".into(),
            body: "body".into(),
            ..Default::default()
        }];
        let ctx = with_skills(test_ctx(1, "skill-depth"), skills);
        let (out, is_error) = run_tool("skill", json!({"name": "x"}), &ctx).await;
        assert!(is_error);
        assert!(
            out.contains("only available to the root agent"),
            "got: {out}"
        );
    }

    /// An unknown skill name comes back as an is_error result that lists what is
    /// available — the model-facing correction shape.
    #[tokio::test]
    async fn skill_tool_unknown_name_errors_with_available_list() {
        let skills = vec![Skill {
            name: "greet".into(),
            description: "d".into(),
            ..Default::default()
        }];
        let ctx = with_skills(test_ctx(0, "skill-unknown"), skills);
        let (out, is_error) = run_tool("skill", json!({"name": "ghost"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("unknown skill 'ghost'"), "got: {out}");
        assert!(out.contains("available: greet"), "got: {out}");
    }
}
