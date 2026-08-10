//! kloop's native agent protocol, wire layer: standard JSON-RPC 2.0 over
//! stdio, one JSON object per line, both ways. Every envelope carries
//! `"jsonrpc":"2.0"`; ids are integers.
//!
//! Inbound lines are distinguished structurally: a `method` field makes it a
//! request (client → server); an `id` without `method` is the client's
//! response to a server-initiated request (approvals). Server request ids live
//! in the server's own integer counter space — a response with no `method`
//! always answers one of our reverse requests, so it never collides with a
//! client-chosen request id (which arrives carrying a `method`).
//!
//! Besides the envelopes, this module owns the projection every front-end
//! shares: [`project_event`] maps a core [`Event`] onto a wire notification,
//! and [`turn_started_params`]/[`turn_completed_params`] shape the turn
//! bracket. The server and the headless `--json` stream both call these, so the
//! two front-ends speak one vocabulary by construction, not by hand-sync.

use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

use kloop_core::agent::EndReason;
use kloop_core::event::Delta;
use kloop_core::event::Event;
use kloop_core::event::Item;
use kloop_core::event::ItemStatus;

/// The protocol version kloop's engine speaks. Bumped only on a breaking wire
/// change; the handshake rejects a client asking for anything else rather than
/// silently downgrading.
pub const PROTOCOL_VERSION: &str = "1.0";

/// JSON-RPC request ids may be integers or strings; both are preserved
/// verbatim so responses match whatever the client sent. The server's own
/// reverse-request ids are always integers.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Num(i64),
    Str(String),
}

/// Any inbound line, before interpretation. The `jsonrpc` field (if present)
/// is ignored — the request/response/notification split is structural.
#[derive(Debug, Deserialize)]
pub struct Incoming {
    pub id: Option<RequestId>,
    pub method: Option<String>,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub result: Value,
}

/// Every outbound line.
#[derive(Debug)]
pub enum Outgoing {
    Response {
        id: RequestId,
        result: Value,
    },
    Error {
        id: Option<RequestId>,
        code: i64,
        message: String,
    },
    Notification {
        method: &'static str,
        params: Value,
    },
    /// Server-initiated request (approvals); the client answers with a
    /// response carrying the same id.
    ServerRequest {
        id: RequestId,
        method: &'static str,
        params: Value,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "outcome", rename_all = "camelCase", deny_unknown_fields)]
pub(super) enum QuestionResponse {
    Answered {
        #[serde(default)]
        selected: Vec<usize>,
        #[serde(default)]
        other: Option<String>,
        #[serde(default)]
        notes: Option<String>,
    },
    Cancelled,
}

pub const PARSE_ERROR: i64 = -32700;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const SERVER_ERROR: i64 = -32000;

impl Outgoing {
    pub fn to_json(&self) -> Value {
        match self {
            Outgoing::Response { id, result } => {
                json!({"jsonrpc": "2.0", "id": id, "result": result})
            }
            Outgoing::Error { id, code, message } => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
            Outgoing::Notification { method, params } => {
                json!({"jsonrpc": "2.0", "method": method, "params": params})
            }
            Outgoing::ServerRequest { id, method, params } => {
                json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
            }
        }
    }
}

/// The wire status token for an item status.
fn status_str(s: ItemStatus) -> &'static str {
    match s {
        ItemStatus::InProgress => "inProgress",
        ItemStatus::Completed => "completed",
        ItemStatus::Failed => "failed",
    }
}

/// Serialize a core [`Item`] to its wire object: a camelCase `type` tag plus
/// `id`, `status`, and the type's own fields. `fallback` is the status for the
/// items that carry none of their own (message/reasoning/todo) — derived from
/// whether the event was a start or a completion. A tool call carries the full
/// `input` both times (the wire no longer sends a summary); `output` and the
/// sub-agent `agent` label appear only when present.
fn item_json(id: &str, item: &Item, fallback: &'static str) -> Value {
    match item {
        Item::AssistantMessage { text, status } => {
            json!({"id": id, "type": "assistantMessage", "status": status_str(*status), "text": text})
        }
        Item::Reasoning { text, status } => {
            json!({"id": id, "type": "reasoning", "status": status_str(*status), "text": text})
        }
        Item::ToolCall {
            agent,
            name,
            input,
            status,
            output,
        } => {
            let mut o = json!({
                "id": id, "type": "toolCall", "name": name,
                "input": input, "status": status_str(*status),
            });
            if !agent.is_empty() {
                o["agent"] = Value::String(agent.clone());
            }
            if let Some(out) = output {
                o["output"] = Value::String(out.clone());
            }
            o
        }
        Item::SubAgent {
            label,
            task,
            status,
        } => json!({
            "id": id, "type": "subAgent", "label": label, "task": task,
            "status": status_str(*status),
        }),
        Item::Todo { agent, items } => {
            let mut o = json!({"id": id, "type": "todo", "status": fallback, "todos": items});
            if !agent.is_empty() {
                o["agent"] = Value::String(agent.clone());
            }
            o
        }
    }
}

/// Project a core [`Event`] onto a wire notification `(method, params)`, minus
/// the `threadId` each sink injects. `turn_id` tags item events with their
/// turn. Returns `None` for the turn bracket (worker-constructed, since it owns
/// the turn id) and events with no wire form.
pub fn project_event(ev: &Event, turn_id: u64) -> Option<(&'static str, Value)> {
    match ev {
        Event::ItemStarted { id, item } => Some((
            "item/started",
            json!({"turnId": turn_id, "item": item_json(id, item, "inProgress")}),
        )),
        Event::ItemDelta { id, delta } => {
            let (channel, text) = match delta {
                Delta::Text(t) => ("text", t),
                Delta::Reasoning(t) => ("reasoning", t),
                Delta::Output(t) => ("output", t),
            };
            Some((
                "item/delta",
                json!({"turnId": turn_id, "itemId": id, "channel": channel, "text": text}),
            ))
        }
        Event::ItemCompleted { id, item } => Some((
            "item/completed",
            json!({"turnId": turn_id, "item": item_json(id, item, "completed")}),
        )),
        Event::BackgroundTaskUpdated(task) => {
            let kind = match task.kind {
                kloop_core::event::BackgroundTaskKind::Shell => "shell",
                kloop_core::event::BackgroundTaskKind::Agent => "agent",
                kloop_core::event::BackgroundTaskKind::Program => "program",
                kloop_core::event::BackgroundTaskKind::Workflow => "workflow",
            };
            let status = match task.status {
                kloop_core::event::BackgroundTaskStatus::Running => "running",
                kloop_core::event::BackgroundTaskStatus::Completed => "completed",
                kloop_core::event::BackgroundTaskStatus::Failed => "failed",
                kloop_core::event::BackgroundTaskStatus::Cancelled => "cancelled",
            };
            let mut task_json = json!({
                "id": task.id,
                "kind": kind,
                "description": task.description,
                "status": status,
                "outputPath": task.output_path,
                "detail": task.detail,
            });
            if let Some(run_id) = &task.run_id {
                task_json["runId"] = Value::String(run_id.clone());
            }
            Some(("thread/backgroundTask/updated", json!({"task": task_json})))
        }
        Event::AgentMessageUpdated(message) => {
            let status = match message.status {
                kloop_core::event::AgentMessageStatus::Queued => "queued",
                kloop_core::event::AgentMessageStatus::Delivered => "delivered",
                kloop_core::event::AgentMessageStatus::Undeliverable => "undeliverable",
            };
            Some((
                "thread/agentMessage/updated",
                json!({
                    "messageId": message.id.as_str(),
                    "from": message.from.as_str(),
                    "to": message.to.as_str(),
                    "summary": message.summary,
                    "status": status,
                }),
            ))
        }
        Event::ScheduledTaskUpdated(task) => {
            let origin = match task.origin {
                kloop_core::event::ScheduledTaskOrigin::Cron => "cron",
                kloop_core::event::ScheduledTaskOrigin::LoopWakeup => "loopWakeup",
            };
            let status = match task.status {
                kloop_core::event::ScheduledTaskStatus::Scheduled => "scheduled",
                kloop_core::event::ScheduledTaskStatus::Fired => "fired",
                kloop_core::event::ScheduledTaskStatus::Cancelled => "cancelled",
                kloop_core::event::ScheduledTaskStatus::Failed => "failed",
            };
            Some((
                "thread/scheduler/updated",
                json!({"task": {
                    "id": task.id,
                    "origin": origin,
                    "status": status,
                    "scheduledForMs": task.scheduled_for_ms,
                    "reason": task.reason,
                    "detail": task.detail,
                }}),
            ))
        }
        // Token usage, cwd and scheduler updates are thread-scoped, not turn-scoped: no turnId.
        Event::Usage(n) => Some((
            "thread/tokenUsage/updated",
            json!({"tokenUsage": {"total": n}}),
        )),
        Event::CwdChanged { cwd, branch } => {
            Some(("thread/cwd/updated", json!({"cwd": cwd, "branch": branch})))
        }
        // A mode change has no dedicated wire in slice 1 (config/model is a
        // slice-4 read surface); it and plain notes surface as a `note`.
        Event::Note(_) | Event::ModeChanged(_) => {
            ev.as_note().map(|t| ("note", json!({"text": t})))
        }
        Event::TurnStarted | Event::TurnEnded(_) => None,
    }
}

/// The `turn/started` params for a turn.
pub fn turn_started_params(turn_id: u64) -> Value {
    json!({"turn": {"id": turn_id}})
}

/// The `turn/completed` params for a turn's end reason: `{turn:{id, status,
/// error?}}`. `error` is present only on the error status.
pub fn turn_completed_params(turn_id: u64, reason: &EndReason) -> Value {
    let status = match reason {
        EndReason::Completed => "completed",
        EndReason::MaxRounds => "maxRounds",
        EndReason::Aborted => "aborted",
        EndReason::Error(_) => "error",
    };
    let mut turn = json!({"id": turn_id, "status": status});
    if let EndReason::Error(e) = reason {
        turn["error"] = Value::String(e.clone());
    }
    json!({"turn": turn})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_ids_round_trip_both_shapes() {
        assert_eq!(
            serde_json::from_str::<RequestId>("7").unwrap(),
            RequestId::Num(7)
        );
        assert_eq!(
            serde_json::from_str::<RequestId>("\"c-1\"").unwrap(),
            RequestId::Str("c-1".into())
        );
        assert_eq!(serde_json::to_string(&RequestId::Num(7)).unwrap(), "7");
    }

    #[test]
    fn incoming_distinguishes_requests_from_responses() {
        let req: Incoming =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"method":"thread/start","params":{}}"#)
                .unwrap();
        assert_eq!(req.method.as_deref(), Some("thread/start"));
        assert_eq!(req.id, Some(RequestId::Num(1)));

        let resp: Incoming =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":3,"result":{"decision":"accept"}}"#)
                .unwrap();
        assert!(resp.method.is_none());
        assert_eq!(resp.id, Some(RequestId::Num(3)));
        assert_eq!(resp.result["decision"], "accept");

        // Params may be absent entirely.
        let bare: Incoming = serde_json::from_str(r#"{"id":2,"method":"thread/list"}"#).unwrap();
        assert!(bare.params.is_null());
    }

    #[test]
    fn outgoing_shapes_carry_the_jsonrpc_field() {
        let cases = [
            (
                Outgoing::Response {
                    id: RequestId::Num(1),
                    result: json!({"thread": {"id": "t"}}),
                },
                r#"{"id":1,"jsonrpc":"2.0","result":{"thread":{"id":"t"}}}"#,
            ),
            (
                Outgoing::Error {
                    id: None,
                    code: PARSE_ERROR,
                    message: "bad line".into(),
                },
                r#"{"error":{"code":-32700,"message":"bad line"},"id":null,"jsonrpc":"2.0"}"#,
            ),
            (
                Outgoing::Notification {
                    method: "item/delta",
                    params: json!({"threadId": "t", "itemId": "msg-0"}),
                },
                r#"{"jsonrpc":"2.0","method":"item/delta","params":{"itemId":"msg-0","threadId":"t"}}"#,
            ),
            (
                Outgoing::ServerRequest {
                    id: RequestId::Num(1),
                    method: "approval/request",
                    params: json!({"threadId": "t", "description": "bash: rm x"}),
                },
                r#"{"id":1,"jsonrpc":"2.0","method":"approval/request","params":{"description":"bash: rm x","threadId":"t"}}"#,
            ),
        ];
        for (outgoing, expected) in cases {
            assert_eq!(
                serde_json::to_string(&outgoing.to_json()).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn item_events_project_to_the_item_wire() {
        // A tool call carries the full input on start; completion adds output.
        let start = Event::ItemStarted {
            id: "t1".into(),
            item: Item::ToolCall {
                agent: String::new(),
                name: "bash".into(),
                input: json!({"command": "ls"}),
                status: ItemStatus::InProgress,
                output: None,
            },
        };
        assert_eq!(
            project_event(&start, 5),
            Some((
                "item/started",
                json!({
                    "turnId": 5,
                    "item": {
                        "id": "t1", "type": "toolCall", "name": "bash",
                        "input": {"command": "ls"}, "status": "inProgress",
                    },
                }),
            ))
        );

        let done = Event::ItemCompleted {
            id: "t1".into(),
            item: Item::ToolCall {
                agent: "agent-1".into(),
                name: "bash".into(),
                input: json!({"command": "ls"}),
                status: ItemStatus::Completed,
                output: Some("file.txt".into()),
            },
        };
        assert_eq!(
            project_event(&done, 5),
            Some((
                "item/completed",
                json!({
                    "turnId": 5,
                    "item": {
                        "id": "t1", "type": "toolCall", "name": "bash",
                        "input": {"command": "ls"}, "status": "completed",
                        "agent": "agent-1", "output": "file.txt",
                    },
                }),
            ))
        );
    }

    #[test]
    fn delta_channels_and_thread_scoped_events_project() {
        let delta = Event::ItemDelta {
            id: "reasoning-0".into(),
            delta: Delta::Reasoning("hmm".into()),
        };
        assert_eq!(
            project_event(&delta, 2),
            Some((
                "item/delta",
                json!({"turnId": 2, "itemId": "reasoning-0", "channel": "reasoning", "text": "hmm"}),
            ))
        );

        assert_eq!(
            project_event(&Event::Usage(1234), 2),
            Some((
                "thread/tokenUsage/updated",
                json!({"tokenUsage": {"total": 1234}})
            ))
        );

        // The turn bracket is not projected here (the worker owns the turn id).
        assert_eq!(project_event(&Event::TurnStarted, 2), None);
    }

    #[test]
    fn background_updates_are_thread_scoped() {
        let update = Event::BackgroundTaskUpdated(kloop_core::event::BackgroundTask {
            id: "bg-7".into(),
            run_id: None,
            kind: kloop_core::event::BackgroundTaskKind::Shell,
            description: "make test".into(),
            status: kloop_core::event::BackgroundTaskStatus::Failed,
            output_path: Some("/tmp/bg-7.out".into()),
            detail: Some("exit 2".into()),
        });
        assert_eq!(
            project_event(&update, 99),
            Some((
                "thread/backgroundTask/updated",
                json!({"task": {
                    "id": "bg-7",
                    "kind": "shell",
                    "description": "make test",
                    "status": "failed",
                    "outputPath": "/tmp/bg-7.out",
                    "detail": "exit 2",
                }})
            ))
        );
    }

    #[test]
    fn local_agent_message_updates_are_thread_scoped_and_body_free() {
        let update = Event::AgentMessageUpdated(kloop_core::event::AgentMessageUpdate {
            id: "message-7".parse().unwrap(),
            from: "agent-3".parse().unwrap(),
            to: "main".parse().unwrap(),
            summary: "review close race".into(),
            status: kloop_core::event::AgentMessageStatus::Delivered,
        });
        let projected = project_event(&update, 99);
        assert_eq!(
            projected,
            Some((
                "thread/agentMessage/updated",
                json!({
                    "messageId": "message-7",
                    "from": "agent-3",
                    "to": "main",
                    "summary": "review close race",
                    "status": "delivered",
                })
            ))
        );
        let (_, params) = projected.unwrap();
        assert!(params.get("turnId").is_none());
        assert!(params.get("message").is_none());
        assert!(params.get("body").is_none());
        assert!(params.get("contextId").is_none());
        assert!(params.get("taskId").is_none());
    }

    #[test]
    fn program_lifecycle_keeps_one_durable_run_id_without_turn_ownership() {
        use kloop_core::event::BackgroundTask;
        use kloop_core::event::BackgroundTaskKind;
        use kloop_core::event::BackgroundTaskStatus;

        let task = |status, output_path: Option<&str>| {
            Event::BackgroundTaskUpdated(BackgroundTask {
                id: "program-4".into(),
                run_id: Some("run-44".into()),
                kind: BackgroundTaskKind::Program,
                description: "compile assets".into(),
                status,
                output_path: output_path.map(str::to_string),
                detail: None,
            })
        };
        for (event, status, output_path) in [
            (
                task(BackgroundTaskStatus::Running, None),
                "running",
                Value::Null,
            ),
            (
                task(BackgroundTaskStatus::Completed, Some("/tmp/run-44/result")),
                "completed",
                json!("/tmp/run-44/result"),
            ),
        ] {
            assert_eq!(
                project_event(&event, 99),
                Some((
                    "thread/backgroundTask/updated",
                    json!({"task": {
                        "id": "program-4",
                        "runId": "run-44",
                        "kind": "program",
                        "description": "compile assets",
                        "status": status,
                        "outputPath": output_path,
                        "detail": null,
                    }})
                ))
            );
        }
    }

    #[test]
    fn agent_background_update_omits_durable_and_turn_ids() {
        let update = Event::BackgroundTaskUpdated(kloop_core::event::BackgroundTask {
            id: "agent-6".into(),
            run_id: None,
            kind: kloop_core::event::BackgroundTaskKind::Agent,
            description: "inspect logs".into(),
            status: kloop_core::event::BackgroundTaskStatus::Running,
            output_path: None,
            detail: None,
        });
        assert_eq!(
            project_event(&update, 123),
            Some((
                "thread/backgroundTask/updated",
                json!({"task": {
                    "id": "agent-6",
                    "kind": "agent",
                    "description": "inspect logs",
                    "status": "running",
                    "outputPath": null,
                    "detail": null,
                }})
            ))
        );
    }

    #[test]
    fn workflow_updates_include_durable_run_identity() {
        let update = Event::BackgroundTaskUpdated(kloop_core::event::BackgroundTask {
            id: "workflow-7".into(),
            run_id: Some("wf_123-7".into()),
            kind: kloop_core::event::BackgroundTaskKind::Workflow,
            description: "scan repository".into(),
            status: kloop_core::event::BackgroundTaskStatus::Running,
            output_path: None,
            detail: Some("Scan".into()),
        });
        assert_eq!(
            project_event(&update, 99),
            Some((
                "thread/backgroundTask/updated",
                json!({"task": {
                    "id": "workflow-7",
                    "kind": "workflow",
                    "description": "scan repository",
                    "status": "running",
                    "outputPath": null,
                    "detail": "Scan",
                    "runId": "wf_123-7",
                }})
            ))
        );
    }

    #[test]
    fn turn_completed_shapes_the_status_and_error() {
        assert_eq!(
            turn_completed_params(4, &EndReason::Completed),
            json!({"turn": {"id": 4, "status": "completed"}})
        );
        assert_eq!(
            turn_completed_params(4, &EndReason::Error("boom".into())),
            json!({"turn": {"id": 4, "status": "error", "error": "boom"}})
        );
    }
}
