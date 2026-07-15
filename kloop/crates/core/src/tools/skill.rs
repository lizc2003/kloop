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
    let skill = Skill::lookup(&ctx.cfg.skills, name).map_err(|e| anyhow::anyhow!(e))?;
    let body = expand_body(&skill.body, &skill.dir, args);
    match skill.context {
        SkillContext::Inline => Ok(body),
        SkillContext::Fork => fork_skill(ctx, skill, body).await,
    }
}
