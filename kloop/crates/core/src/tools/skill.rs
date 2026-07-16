//! The `skill` tool: how the model triggers a skill (plan 28). Only the tool
//! seam lives here — its definition and dispatch — beside the other tools. The
//! skill data model, parsing, catalog, and body expansion are the pure half in
//! `crate::skills`. Registered only at depth 0 and only when skills are loaded
//! (see `turn_rounds`), so it never rides a sub-agent's request.

use serde_json::json;
use serde_json::Value;

use super::fork_skill;
use super::str_arg;
use super::ToolCtx;
use crate::skills::expand_body;
use crate::skills::Skill;
use crate::skills::SkillContext;
use crate::skills::SkillSource;
use kloop_protocol::ToolDef;

/// The `skill` tool definition: how the model triggers a skill. Registered only
/// when skills are loaded (see `turn_rounds`). Its definition is
/// skill-independent — which skills exist is advertised by the catalog, keeping
/// this def and the injected list both byte-stable for the cache.
pub(crate) fn skill_tool_def() -> ToolDef {
    ToolDef {
        name: "skill".into(),
        description: "Activate one of the available skills, loading its full instructions into the conversation so you can carry them out. The available skills are listed by name and description in the context; pick the one whose description matches the task. Pass any relevant user input as `arguments`.".into(),
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
pub(super) async fn skill_tool(input: &Value, ctx: &ToolCtx) -> anyhow::Result<String> {
    let name = str_arg(input, "name", "skill")?;
    let args = input.get("arguments").and_then(Value::as_str).unwrap_or("");
    // Only model-invocable skills: a user command (`.kloop/commands/*.md`) is
    // `/name`-only and must not be reachable here even if the model guesses it.
    let candidates = ctx
        .cfg
        .skills
        .iter()
        .filter(|s| s.source == SkillSource::Skill);
    let skill = Skill::lookup(candidates, name).map_err(|e| anyhow::anyhow!(e))?;
    let body = expand_body(&skill.body, &skill.dir, args);
    // Then run any `!cmd` / `@file` injections (plan 36 slice 3), gated exactly
    // like a bash/read call. A `fork` skill expands before forking, so its
    // sub-agent sees the resolved output; a blocked/failed `!cmd` propagates as
    // an is_error tool_result. This is the same expansion the slash path runs.
    let body = super::inject::expand(&body, ctx).await?;
    match skill.context {
        SkillContext::Inline => Ok(body),
        SkillContext::Fork => fork_skill(ctx, skill, body).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::*;
    use kloop_protocol::ContentBlock;
    use kloop_provider::Provider;

    /// Config is behind an Arc, so tests clone-and-swap the skill registry (the
    /// same pattern as `with_provider`).
    fn with_skills(mut ctx: ToolCtx, skills: Vec<Skill>) -> ToolCtx {
        let mut cfg = (*ctx.cfg).clone();
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
        let provider = Provider::mock(vec![vec![ContentBlock::Text {
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
        let mut cfg = (*ctx.cfg).clone();
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
