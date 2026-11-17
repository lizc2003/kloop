//! The accepted-key set for each built-in input, in one place.
//!
//! Hand-written parsers pass [`strict`] so their allow-list is this table, and
//! the schema/parser consistency test reads the same table. Serde parsers get
//! the equivalent guarantee from `deny_unknown_fields`; the test probes those
//! structs key by key so a field deleted from a struct but left in the schema
//! (the `max_rounds` drift) fails instead of passing.

use anyhow::Result;
use serde_json::Map;
use serde_json::Value;

use super::args;

pub(super) const CONTRACTS: &[(&str, &[&str])] = &[
    (
        "bash",
        &["command", "description", "timeout_ms", "background"],
    ),
    ("bash_output", &["bash_id", "block", "timeout_ms"]),
    ("stop_bash", &["bash_id"]),
    ("powershell", &["command", "description", "timeout_ms"]),
    ("read_file", &["path", "offset", "limit"]),
    ("write_file", &["path", "content"]),
    (
        "edit_file",
        &["path", "old_string", "new_string", "replace_all"],
    ),
    (
        "notebook_edit",
        &[
            "notebook_path",
            "cell_id",
            "new_source",
            "cell_type",
            "edit_mode",
        ],
    ),
    (
        "grep",
        &[
            "pattern",
            "path",
            "glob",
            "type",
            "output_mode",
            "-i",
            "-n",
            "-A",
            "-B",
            "-C",
            "-o",
            "head_limit",
            "offset",
            "multiline",
        ],
    ),
    ("glob", &["pattern", "path"]),
    ("todo_write", &["todos"]),
    ("send_message", &["to", "summary", "message"]),
    ("list_agents", &[]),
    (
        "run_agent",
        &[
            "description",
            "prompt",
            "agent_type",
            "model",
            "background",
            "isolation",
        ],
    ),
    ("wait_for_activity", &["timeout_ms"]),
    ("stop_agent", &["agent_id"]),
    ("tool_search", &["query", "max_results"]),
    ("call_tool", &["tool_name", "params"]),
    (
        "run_program",
        &["description", "source", "background", "resume_from_run_id"],
    ),
    ("stop_program", &["program_id"]),
    (
        "cron_create",
        &["cron", "prompt", "recurring", "durable", "check"],
    ),
    ("cron_delete", &["id"]),
    ("cron_list", &[]),
    (
        "schedule_wakeup",
        &["delay_seconds", "reason", "prompt", "stop"],
    ),
    (
        "ask_user_question",
        &["questions", "answers", "annotations", "metadata"],
    ),
    ("enter_plan_mode", &[]),
    ("exit_plan_mode", &["plan"]),
    (
        "workflow",
        &[
            "script",
            "args",
            "script_path",
            "resume_from_run_id",
            "name",
            "description",
            "title",
        ],
    ),
    ("stop_workflow", &["workflow_id"]),
    ("enter_worktree", &["name", "path"]),
    ("exit_worktree", &["action", "discard_changes"]),
    ("skill", &["name", "arguments"]),
];

pub(super) fn keys(tool: &str) -> &'static [&'static str] {
    CONTRACTS
        .iter()
        .find(|(name, _)| *name == tool)
        .map(|(_, keys)| *keys)
        .unwrap_or_else(|| panic!("no argument contract for {tool}"))
}

/// Reject anything outside the tool's contract before a parser reads fields.
pub(super) fn strict<'a>(input: &'a Value, tool: &str) -> Result<&'a Map<String, Value>> {
    args::strict_object(input, keys(tool), tool)
}

#[cfg(test)]
pub(super) fn contract_parse(input: &Value, tool: &str) -> Result<()> {
    strict(input, tool).map(|_| ())
}
