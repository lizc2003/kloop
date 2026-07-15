//! Custom agent types (plan 17 slice 2): named sub-agent definitions that
//! override the system prompt, model, and available tools. Dispatched via the
//! `task` tool's `agent_type` parameter; loaded from `.kloop/config.toml`
//! `[agents.<name>]` by the CLI. The shape follows cc's `.claude/agents`
//! frontmatter — the system prompt is **replaced**, not concatenated; an
//! omitted model or tool set inherits the parent's; an unknown type is an
//! error that lists what is available.

use std::collections::HashSet;

/// One named sub-agent definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentType {
    pub name: String,
    /// What this agent is for — shown to the model in the task tool's
    /// description so it can choose an `agent_type`.
    pub description: String,
    /// Replaces the sub-agent's system prompt entirely (cc semantics — not
    /// concatenated with the parent's). None inherits the parent's.
    pub system: Option<String>,
    /// None inherits the parent's model — the point of a cheap search agent.
    pub model: Option<String>,
    /// Exact tool-name allowlist for the sub-agent; None inherits the
    /// parent's full set. `read_offloaded` stays available regardless (see
    /// [`tool_available`]).
    pub tools: Option<Vec<String>>,
}

impl AgentType {
    /// Resolve a type by name, or an error naming the available types (cc's
    /// unknown-type behavior). The message is model-facing (is_error
    /// tool_result), so it doubles as a correction.
    pub fn lookup<'a>(types: &'a [AgentType], name: &str) -> Result<&'a AgentType, String> {
        types.iter().find(|t| t.name == name).ok_or_else(|| {
            if types.is_empty() {
                format!("unknown agent_type '{name}': no agent types are defined")
            } else {
                let available = types
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("unknown agent_type '{name}' (available: {available})")
            }
        })
    }
}

/// Whether a tool is usable under an agent type's allowlist. `read_offloaded`
/// is always available — it is how any agent reads back a truncated tool
/// result, pure infrastructure, so restricting it would strand a sub-agent
/// on its own large output rather than remove a capability.
pub fn tool_available(allowlist: Option<&HashSet<String>>, tool: &str) -> bool {
    tool == "read_offloaded" || allowlist.is_none_or(|allow| allow.contains(tool))
}

/// The block appended to the `task` tool's description listing the available
/// agent types. Session-stable (config-derived), so it stays byte-stable for
/// the prompt cache.
pub fn agent_types_hint(types: &[AgentType]) -> String {
    if types.is_empty() {
        return String::new();
    }
    let mut hint = String::from(
        "\n\nAvailable agent types for the agent_type parameter (omit it for a \
             general-purpose sub-agent that inherits this agent's model and tools):",
    );
    for t in types {
        hint.push_str(&format!("\n- {}: {}", t.name, t.description));
    }
    hint
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types() -> Vec<AgentType> {
        vec![
            AgentType {
                name: "researcher".into(),
                description: "Searches the codebase and the web.".into(),
                system: Some("You research.".into()),
                model: Some("claude-haiku-4-5".into()),
                tools: Some(vec!["grep".into(), "glob".into(), "read_file".into()]),
            },
            AgentType {
                name: "reviewer".into(),
                description: "Reviews a diff.".into(),
                system: None,
                model: None,
                tools: None,
            },
        ]
    }

    #[test]
    fn lookup_finds_by_name_and_lists_available_on_miss() {
        let types = types();
        assert_eq!(
            AgentType::lookup(&types, "reviewer").unwrap().name,
            "reviewer"
        );

        let err = AgentType::lookup(&types, "ghost").unwrap_err();
        assert_eq!(
            err,
            "unknown agent_type 'ghost' (available: researcher, reviewer)"
        );
        assert_eq!(
            AgentType::lookup(&[], "ghost").unwrap_err(),
            "unknown agent_type 'ghost': no agent types are defined"
        );
    }

    #[test]
    fn allowlist_gates_tools_but_never_read_offloaded() {
        let allow: HashSet<String> = ["grep".to_string(), "read_file".to_string()]
            .into_iter()
            .collect();
        assert!(tool_available(Some(&allow), "grep"));
        assert!(!tool_available(Some(&allow), "bash"));
        // Infrastructure exception: always available.
        assert!(tool_available(Some(&allow), "read_offloaded"));
        // No allowlist = everything.
        assert!(tool_available(None, "bash"));
    }

    #[test]
    fn hint_lists_types_and_is_empty_without_any() {
        assert_eq!(agent_types_hint(&[]), "");
        let hint = agent_types_hint(&types());
        assert!(hint.contains("\n- researcher: Searches the codebase and the web."));
        assert!(hint.contains("\n- reviewer: Reviews a diff."));
    }
}
