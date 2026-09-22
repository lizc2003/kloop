use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use super::ToolCtx;
use kloop_protocol::LocalAgentId;
use kloop_protocol::ToolDef;

const MAX_TARGET_BYTES: usize = 64;
const MAX_DIAGNOSTIC_CHARS: usize = 80;

pub(super) fn send_message_def() -> ToolDef {
    ToolDef {
        name: "send_message".into(),
        description: "Queue one bounded text message for another live Agent in this local session. The runtime supplies from/message/context/status; you supply only an exact main or agent-N target, optional short UI summary, and message body. Queued means accepted by the target mailbox, not read, understood, replied to, or completed. Delivery occurs only at the recipient's next safe round boundary. Use background run_agent when main must send follow-up instructions while a child is still running; a foreground run_agent blocks main's model loop. This is local session coordination, not remote A2A, a Task API, permission transfer, broadcast, or durable messaging.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "to": {"type": "string", "description": "Exact local target: main or a live agent-N"},
                "summary": {"type": "string", "maxLength": crate::agent_mailbox::MAX_SUMMARY_CHARS, "description": "Optional single-line UI preview"},
                "message": {"type": "string", "description": "Text delivered at the recipient's next safe round boundary"}
            },
            "required": ["to", "message"],
            "additionalProperties": false
        }),
    }
}

pub(super) fn list_agents_def() -> ToolDef {
    ToolDef {
        name: "list_agents".into(),
        description: "List the other live, open Agents addressable from this Agent in the current local session. Returns exact ephemeral main/agent-N addresses plus parent/type/description metadata. The result is a snapshot, not remote Agent Card discovery or a promise that a target will remain open.".into(),
        schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    }
}

pub(super) fn send_message_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let parsed = parse_send_message(input)?;
    let id = ctx
        .cfg
        .local_agent
        .send(parsed.to.clone(), parsed.summary, parsed.message, &ctx.ui)
        .map_err(|error| anyhow!("send_message: {error}"))?;
    Ok(format!("{id} queued for {}", parsed.to))
}

pub(super) fn list_agents_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    strict_object(input, &[], "list_agents")?;
    let roster = ctx
        .cfg
        .local_agent
        .list_agents()
        .map_err(|error| anyhow!("list_agents: {error}"))?;
    let agents = roster
        .into_iter()
        .map(|entry| {
            json!({
                "id": entry.id.as_str(),
                "parent_id": entry.parent_id.as_ref().map(LocalAgentId::as_str),
                "agent_type": entry.agent_type,
                "description": entry.description,
                "state": "open",
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({"agents": agents}).to_string())
}

pub(super) fn event_input(name: &str, input: &Value) -> Value {
    if name == "list_agents" {
        return json!({});
    }
    if name != "send_message" {
        return input.clone();
    }
    let mut projected = Map::new();
    if let Some(to) = input.get("to").and_then(Value::as_str) {
        projected.insert(
            "to".into(),
            Value::String(bounded_diagnostic(to, MAX_TARGET_BYTES)),
        );
    }
    if let Some(message) = input.get("message").and_then(Value::as_str) {
        projected.insert("message_bytes".into(), json!(message.len()));
        let summary = input
            .get("summary")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| summary_from_message(message));
        projected.insert("summary".into(), Value::String(bounded_preview(&summary)));
    } else if let Some(summary) = input.get("summary").and_then(Value::as_str) {
        projected.insert("summary".into(), Value::String(bounded_preview(summary)));
    }
    Value::Object(projected)
}

struct SendMessageInput {
    to: LocalAgentId,
    summary: String,
    message: String,
}

fn parse_send_message(input: &Value) -> Result<SendMessageInput> {
    let object = strict_object(input, &["to", "summary", "message"], "send_message")?;
    let to = required_string(object, "to", "send_message")?;
    let message = required_string(object, "message", "send_message")?;
    if message.trim().is_empty() {
        bail!("send_message: message must not be empty");
    }
    if message.len() > crate::agent_mailbox::MAX_MESSAGE_BYTES {
        bail!(
            "send_message: message exceeds the {}-byte limit",
            crate::agent_mailbox::MAX_MESSAGE_BYTES
        );
    }
    if message
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        bail!("send_message: message contains an unsupported control character");
    }
    let summary = match object.get("summary") {
        None => summary_from_message(message),
        Some(Value::String(summary)) => {
            if summary.trim().is_empty() {
                bail!("send_message: summary must not be empty when provided");
            }
            if summary
                .chars()
                .any(|ch| ch.is_control() || is_unicode_line_separator(ch))
            {
                bail!("send_message: summary must be a single line without control characters");
            }
            if summary.chars().count() > crate::agent_mailbox::MAX_SUMMARY_CHARS
                || summary.len() > crate::agent_mailbox::MAX_SUMMARY_BYTES
            {
                bail!(
                    "send_message: summary exceeds {} characters or {} bytes",
                    crate::agent_mailbox::MAX_SUMMARY_CHARS,
                    crate::agent_mailbox::MAX_SUMMARY_BYTES
                );
            }
            summary.clone()
        }
        Some(_) => bail!("send_message: summary must be a string when provided"),
    };
    let to = parse_target(to)?;
    Ok(SendMessageInput {
        to,
        summary,
        message: message.to_string(),
    })
}

fn parse_target(value: &str) -> Result<LocalAgentId> {
    if value.len() > MAX_TARGET_BYTES {
        bail!("send_message: target exceeds the {MAX_TARGET_BYTES}-byte address limit");
    }
    if value.starts_with("program-") {
        bail!("send_message: {value} is a Program execution id, not an Agent address");
    }
    if value.starts_with("workflow-") {
        bail!("send_message: {value} is a Workflow execution id, not an Agent address");
    }
    if value.starts_with("bg-") {
        bail!("send_message: {value} is a background shell id, not an Agent address");
    }
    if value.starts_with("run-") {
        bail!("send_message: {value} is a durable Program run id, not an Agent address");
    }
    if value.starts_with("wf_") || value.starts_with("wf-") {
        bail!("send_message: {value} is a durable Workflow run id, not an Agent address");
    }
    value
        .parse()
        .map_err(|error: String| anyhow!("send_message: invalid target `{value}`: {error}"))
}

fn strict_object<'a>(
    input: &'a Value,
    allowed: &[&str],
    tool: &str,
) -> Result<&'a Map<String, Value>> {
    let object = input
        .as_object()
        .ok_or_else(|| anyhow!("{tool}: input must be an object"))?;
    if let Some(unexpected) = object
        .keys()
        .find(|candidate| !allowed.contains(&candidate.as_str()))
    {
        let unexpected = bounded_diagnostic(unexpected, MAX_DIAGNOSTIC_CHARS);
        bail!("{tool}: unknown field `{unexpected}`");
    }
    Ok(object)
}

fn required_string<'a>(object: &'a Map<String, Value>, key: &str, tool: &str) -> Result<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{tool}: missing required string argument '{key}'"))
}

fn summary_from_message(message: &str) -> String {
    let line = message
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("Agent message")
        .trim();
    bounded_preview(line)
}

fn is_unicode_line_separator(ch: char) -> bool {
    matches!(ch, '\u{2028}' | '\u{2029}')
}

fn bounded_diagnostic(value: &str, max_bytes: usize) -> String {
    let mut output = String::new();
    let mut truncated = false;
    for ch in value.chars() {
        let ch = if ch.is_control() || is_unicode_line_separator(ch) {
            ' '
        } else {
            ch
        };
        if output.len() + ch.len_utf8() > max_bytes {
            truncated = true;
            break;
        }
        output.push(ch);
    }
    if truncated {
        output.push('…');
    }
    output
}

fn bounded_preview(value: &str) -> String {
    let mut output = String::new();
    for ch in value.chars() {
        let ch = if ch.is_control() || is_unicode_line_separator(ch) {
            ' '
        } else {
            ch
        };
        if output.chars().count() >= crate::agent_mailbox::MAX_SUMMARY_CHARS
            || output.len() + ch.len_utf8() > crate::agent_mailbox::MAX_SUMMARY_BYTES
        {
            break;
        }
        output.push(ch);
    }
    let output = output.trim().to_string();
    if output.is_empty() {
        "Agent message".into()
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::test_ctx;
    use std::sync::Arc;

    #[test]
    fn tools_are_strict_builtins_at_main_and_child_depth() {
        for depth in [0, 1] {
            let defs = crate::tools::tool_defs(
                depth,
                &crate::shell_programs::ShellPrograms::test_fixture(),
            );
            for name in ["send_message", "list_agents"] {
                let def = defs
                    .iter()
                    .find(|definition| definition.name == name)
                    .unwrap();
                assert_eq!(def.schema["additionalProperties"], false);
            }
        }
        assert_eq!(
            send_message_def().schema["required"],
            json!(["to", "message"])
        );
        assert_eq!(list_agents_def().schema["properties"], json!({}));
    }

    #[test]
    fn parser_is_strict_and_derives_a_bounded_summary() {
        let parsed = parse_send_message(&json!({
            "to": "agent-2",
            "message": "\n  review the close race\nfull body",
        }))
        .unwrap();
        assert_eq!(parsed.to.as_str(), "agent-2");
        assert_eq!(parsed.summary, "review the close race");
        let exact = parse_send_message(&json!({
            "to":"agent-2",
            "message":"x".repeat(crate::agent_mailbox::MAX_MESSAGE_BYTES),
            "summary":"界".repeat(crate::agent_mailbox::MAX_SUMMARY_CHARS),
        }))
        .unwrap();
        assert_eq!(exact.message.len(), crate::agent_mailbox::MAX_MESSAGE_BYTES);
        assert_eq!(
            exact.summary.chars().count(),
            crate::agent_mailbox::MAX_SUMMARY_CHARS
        );
        for input in [
            json!({"to":"agent-2","message":"x","from":"main"}),
            json!({"to":"agent-2","message":null}),
            json!({"to":"agent-2","message":" "}),
            json!({"to":"agent-2","message":"x","summary":null}),
            json!({"to":"agent-2","message":"x","summary":"bad\nline"}),
            json!({"to":"agent-2","message":"x","summary":"bad\u{2028}line"}),
            json!({"to":"agent-2","message":"x\u{0000}"}),
            json!({"to":"agent-2","message":"x".repeat(crate::agent_mailbox::MAX_MESSAGE_BYTES + 1)}),
            json!({"to":"agent-2","message":"x","summary":"界".repeat(crate::agent_mailbox::MAX_SUMMARY_CHARS + 1)}),
            json!({"to":format!("agent-{}", "9".repeat(MAX_TARGET_BYTES)),"message":"x"}),
            json!({"to":"agent-02","message":"x"}),
        ] {
            assert!(parse_send_message(&input).is_err(), "{input}");
        }
    }

    #[test]
    fn resource_ids_get_directional_errors() {
        for (target, noun) in [
            ("program-1", "Program"),
            ("workflow-1", "Workflow"),
            ("bg-1", "shell"),
            ("run-1", "Program"),
            ("wf_1", "Workflow"),
        ] {
            let error = parse_target(target).unwrap_err().to_string();
            assert!(error.contains(noun), "{error}");
        }
    }

    #[test]
    fn tool_event_projection_never_copies_the_body() {
        let secret = "BODY-MUST-NOT-LEAK";
        let projected = event_input(
            "send_message",
            &json!({"to":"agent-1","message":format!("safe preview\n{secret}")}),
        );
        assert_eq!(projected["to"], "agent-1");
        assert_eq!(projected["summary"], "safe preview");
        assert_eq!(
            projected["message_bytes"],
            "safe preview\n".len() + secret.len()
        );
        assert!(projected.get("message").is_none());
        assert!(!projected.to_string().contains(secret));
    }

    #[test]
    fn invalid_event_inputs_stay_bounded_and_body_free() {
        let projected = event_input(
            "send_message",
            &json!({"to":"x".repeat(10_000),"message":"secret"}),
        );
        assert!(projected["to"].as_str().unwrap().len() <= MAX_TARGET_BYTES + '…'.len_utf8());
        assert!(projected.get("message").is_none());
        assert_eq!(
            event_input("list_agents", &json!({"huge":"x".repeat(10_000)})),
            json!({})
        );

        let error = list_agents_tool(
            &json!({"x".repeat(10_000): true}),
            &test_ctx(0, "bounded-list"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.len() < 160, "{error}");
    }

    #[tokio::test]
    async fn dispatch_lifecycle_redacts_body_but_keeps_summary() {
        struct RecordingUi(std::sync::Mutex<Vec<crate::event::Event>>);
        impl crate::agent::Ui for RecordingUi {
            fn emit(&self, event: &crate::event::Event) {
                self.0.lock().unwrap().push(event.clone());
            }
        }

        let mut ctx = test_ctx(0, "agent-message-redaction");
        let ui = Arc::new(RecordingUi(std::sync::Mutex::new(Vec::new())));
        ctx.ui = ui.clone();
        let child = ctx.cfg.local_agent.child("agent-71".parse().unwrap());
        let _lease = child
            .register_child(
                Arc::new(crate::inbox::Inbox::default()),
                None,
                "redaction target",
                ctx.ui.clone(),
            )
            .unwrap();
        let secret = "SECOND-LINE-SECRET";
        let (result, is_error) = crate::tools::testutil::run_tool(
            "send_message",
            json!({"to":"agent-71","message":format!("safe summary\n{secret}")}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{result}");
        let events = ui.0.lock().unwrap();
        let tool_inputs = events.iter().filter_map(|event| match event {
            crate::event::Event::ItemStarted {
                item: crate::event::Item::ToolCall { input, .. },
                ..
            }
            | crate::event::Event::ItemCompleted {
                item: crate::event::Item::ToolCall { input, .. },
                ..
            } => Some(input),
            _ => None,
        });
        for input in tool_inputs {
            assert_eq!(input["summary"], "safe summary");
            assert!(input.get("message").is_none());
            assert!(!input.to_string().contains(secret));
        }
    }

    #[test]
    fn send_and_list_use_runtime_identity_and_target_inbox() {
        let ctx = test_ctx(0, "agent-message-tool");
        let child_context = ctx.cfg.local_agent.child("agent-70".parse().unwrap());
        let child_inbox = Arc::new(crate::inbox::Inbox::default());
        let _lease = child_context
            .register_child(
                Arc::clone(&child_inbox),
                Some("reviewer"),
                "review races",
                ctx.ui.clone(),
            )
            .unwrap();
        let result =
            send_message_tool(&json!({"to":"agent-70","message":"check it"}), &ctx).unwrap();
        assert_eq!(result, "message-1 queued for agent-70");
        assert!(!child_inbox.is_empty());
        assert!(list_agents_tool(&Value::Null, &ctx).is_err());
        assert!(list_agents_tool(&json!({"agent_id":"agent-70"}), &ctx).is_err());
        let roster: Value =
            serde_json::from_str(&list_agents_tool(&json!({}), &ctx).unwrap()).unwrap();
        assert_eq!(roster["agents"][0]["id"], "agent-70");
        assert_eq!(roster["agents"][0]["agent_type"], "reviewer");
    }
}
