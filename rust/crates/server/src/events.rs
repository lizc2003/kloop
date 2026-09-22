use std::collections::VecDeque;
use std::sync::Mutex;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de::Error as _;
use serde_json::Value;
use serde_json::json;

use kloop_core::rollout::SessionRuntime;
use kloop_core::rollout::SessionSnapshot;
use kloop_core::rollout::SnapshotTerminal;
use kloop_protocol::ActiveProviderRoute;
use kloop_protocol::Message;

const MAX_RETAINED_EVENTS: usize = 4_096;
const MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecoverySource {
    Fresh,
    Resumed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EventCursor {
    pub thread_id: String,
    pub generation: String,
    #[serde(
        serialize_with = "serialize_sequence",
        deserialize_with = "deserialize_sequence"
    )]
    pub seq: u64,
}

impl EventCursor {
    fn validate(&self) -> Result<(), CursorError> {
        if self.thread_id.trim().is_empty() {
            return Err(CursorError::Invalid(
                "event_cursor.thread_id must not be empty",
            ));
        }
        if self.generation.trim().is_empty() {
            return Err(CursorError::Invalid(
                "event_cursor.generation must not be empty",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EventsSyncParams {
    pub thread_id: String,
    #[serde(default)]
    pub event_cursor: Option<EventCursor>,
}

impl EventsSyncParams {
    pub fn parse(params: &Value) -> Result<Self, String> {
        let parsed: Self = serde_json::from_value(params.clone())
            .map_err(|error| format!("invalid thread/events/sync params: {error}"))?;
        if parsed.thread_id.trim().is_empty() {
            return Err("threadId must not be empty".into());
        }
        if let Some(cursor) = &parsed.event_cursor {
            cursor.validate().map_err(|error| error.to_string())?;
            if cursor.thread_id != parsed.thread_id {
                return Err("event_cursor belongs to a different thread".into());
            }
        }
        Ok(parsed)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct PublicEventEnvelope {
    pub method: String,
    pub params: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SnapshotReason {
    Initial,
    GenerationChanged,
    CursorExpired,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(crate) enum ProjectionSync {
    Replay {
        generation: String,
        #[serde(serialize_with = "serialize_sequence")]
        high_water_seq: u64,
        events: Vec<PublicEventEnvelope>,
        event_cursor: EventCursor,
    },
    Snapshot {
        reason: SnapshotReason,
        generation: String,
        #[serde(serialize_with = "serialize_sequence")]
        high_water_seq: u64,
        snapshot: Box<PublicSnapshot>,
        event_cursor: EventCursor,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct PublicSnapshot {
    schema_version: u8,
    thread: SnapshotThread,
    history: SnapshotHistory,
    tail: SnapshotTail,
    recovery: SnapshotRecovery,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct SnapshotThread {
    id: String,
    cwd: String,
    route: ActiveProviderRoute,
    resumable: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct SnapshotHistory {
    messages: Vec<Message>,
    runtime: Option<SessionRuntime>,
    terminals: Vec<SnapshotTerminal>,
}

pub(crate) fn into_public_session_snapshot(mut snapshot: SessionSnapshot) -> SessionSnapshot {
    snapshot.messages = snapshot
        .messages
        .into_iter()
        .map(Message::into_public_projection)
        .collect();
    snapshot
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct SnapshotTail {
    turns: Vec<SnapshotTurn>,
    notices: Vec<SnapshotNotice>,
    background_tasks: Vec<Value>,
    agent_messages: Vec<Value>,
    scheduled_tasks: Vec<Value>,
    token_usage: Option<Value>,
    cwd: SnapshotCwd,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct SnapshotTurn {
    id: u64,
    status: String,
    input: Vec<Value>,
    items: Vec<Value>,
    error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct SnapshotNotice {
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_id: Option<u64>,
    kind: SnapshotNoticeKind,
    text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SnapshotNoticeKind {
    Note,
    System,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct SnapshotCwd {
    path: String,
    branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct SnapshotRecovery {
    source: RecoverySource,
    volatile_state: VolatileState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum VolatileState {
    Live,
    Reset,
}

#[derive(Debug)]
pub(crate) enum ProjectionError {
    SequenceExhausted,
    InvalidParams,
    Encode(serde_json::Error),
}

impl std::fmt::Display for ProjectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SequenceExhausted => formatter.write_str("public event sequence exhausted"),
            Self::InvalidParams => formatter.write_str("public event params must be an object"),
            Self::Encode(error) => write!(formatter, "cannot encode public event: {error}"),
        }
    }
}

impl std::error::Error for ProjectionError {}

impl From<serde_json::Error> for ProjectionError {
    fn from(error: serde_json::Error) -> Self {
        Self::Encode(error)
    }
}

#[derive(Debug)]
pub(crate) enum CursorError {
    Invalid(&'static str),
    ForeignThread,
    FutureSequence,
    ProjectionUnavailable,
}

impl std::fmt::Display for CursorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => formatter.write_str(message),
            Self::ForeignThread => {
                formatter.write_str("event_cursor belongs to a different thread")
            }
            Self::FutureSequence => {
                formatter.write_str("event_cursor sequence is ahead of this generation")
            }
            Self::ProjectionUnavailable => {
                formatter.write_str("public event projection is unavailable")
            }
        }
    }
}

impl std::error::Error for CursorError {}

pub(crate) struct ThreadProjection {
    thread_id: String,
    generation: String,
    max_events: usize,
    max_bytes: usize,
    state: Mutex<ProjectionState>,
}

struct ProjectionState {
    high_water_seq: u64,
    evicted_through: u64,
    retained_bytes: usize,
    ring: VecDeque<RetainedEvent>,
    snapshot: PublicSnapshot,
    pending_inputs: Vec<Value>,
    unavailable: bool,
}

struct RetainedEvent {
    seq: u64,
    encoded_bytes: usize,
    envelope: PublicEventEnvelope,
}

impl ThreadProjection {
    pub fn new(
        thread_id: String,
        cwd: String,
        route: ActiveProviderRoute,
        seed: SessionSnapshot,
        source: RecoverySource,
    ) -> Result<Self, getrandom::Error> {
        let generation = new_generation()?;
        Ok(Self::with_generation_and_limits(
            thread_id,
            generation,
            cwd,
            route,
            seed,
            source,
            MAX_RETAINED_EVENTS,
            MAX_RETAINED_BYTES,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn with_generation_and_limits(
        thread_id: String,
        generation: String,
        cwd: String,
        route: ActiveProviderRoute,
        seed: SessionSnapshot,
        source: RecoverySource,
        max_events: usize,
        max_bytes: usize,
    ) -> Self {
        let volatile_state = match source {
            RecoverySource::Fresh => VolatileState::Live,
            RecoverySource::Resumed => VolatileState::Reset,
        };
        let seed = into_public_session_snapshot(seed);
        let resumable = seed.runtime.is_some();
        let snapshot = PublicSnapshot {
            schema_version: 1,
            thread: SnapshotThread {
                id: thread_id.clone(),
                cwd: cwd.clone(),
                route,
                resumable,
            },
            history: SnapshotHistory {
                messages: seed.messages,
                runtime: seed.runtime,
                terminals: seed.terminals,
            },
            tail: SnapshotTail {
                turns: Vec::new(),
                notices: Vec::new(),
                background_tasks: Vec::new(),
                agent_messages: Vec::new(),
                scheduled_tasks: Vec::new(),
                token_usage: None,
                cwd: SnapshotCwd {
                    path: cwd,
                    branch: None,
                },
            },
            recovery: SnapshotRecovery {
                source,
                volatile_state,
            },
        };
        Self {
            thread_id,
            generation,
            max_events,
            max_bytes,
            state: Mutex::new(ProjectionState {
                high_water_seq: 0,
                evicted_through: 0,
                retained_bytes: 0,
                ring: VecDeque::new(),
                snapshot,
                pending_inputs: Vec::new(),
                unavailable: false,
            }),
        }
    }

    /// Materialize, retain, and enqueue one public notification under one lock.
    /// The enqueue callback must be non-blocking; the server passes an unbounded
    /// channel send, so sync sees either the whole event or none of it.
    pub fn publish<F>(
        &self,
        method: &'static str,
        mut params: Value,
        enqueue: F,
    ) -> Result<(), ProjectionError>
    where
        F: FnOnce(Value),
    {
        let mut state = self.state.lock().unwrap();
        if state.unavailable {
            return Err(ProjectionError::SequenceExhausted);
        }
        let Some(params_object) = params.as_object_mut() else {
            return Err(ProjectionError::InvalidParams);
        };
        let Some(seq) = state.high_water_seq.checked_add(1) else {
            state.unavailable = true;
            return Err(ProjectionError::SequenceExhausted);
        };
        params_object.insert("thread_id".into(), Value::String(self.thread_id.clone()));
        params_object.insert(
            "event_generation".into(),
            Value::String(self.generation.clone()),
        );
        params_object.insert("seq".into(), Value::String(seq.to_string()));

        apply_event(&mut state, method, &params);
        let envelope = PublicEventEnvelope {
            method: method.to_string(),
            params: params.clone(),
        };
        let encoded_bytes = serde_json::to_vec(&envelope)?.len();
        state.high_water_seq = seq;
        state.retained_bytes = state.retained_bytes.saturating_add(encoded_bytes);
        state.ring.push_back(RetainedEvent {
            seq,
            encoded_bytes,
            envelope,
        });
        while state.ring.len() > self.max_events || state.retained_bytes > self.max_bytes {
            let Some(evicted) = state.ring.pop_front() else {
                break;
            };
            state.retained_bytes = state.retained_bytes.saturating_sub(evicted.encoded_bytes);
            state.evicted_through = evicted.seq;
        }
        enqueue(params);
        Ok(())
    }

    pub fn update_provider_route(&self, route: ActiveProviderRoute) {
        self.state.lock().unwrap().snapshot.thread.route = route;
    }

    pub fn refresh_seed(&self, cwd: String, route: ActiveProviderRoute, seed: SessionSnapshot) {
        let seed = into_public_session_snapshot(seed);
        let mut state = self.state.lock().unwrap();
        state.snapshot.thread.cwd = cwd.clone();
        state.snapshot.thread.route = route;
        state.snapshot.thread.resumable = seed.runtime.is_some();
        state.snapshot.history = SnapshotHistory {
            messages: seed.messages,
            runtime: seed.runtime,
            terminals: seed.terminals,
        };
        state.snapshot.tail.cwd.path = cwd;
    }

    pub fn discard_turn(&self, turn_id: u64) {
        let mut state = self.state.lock().unwrap();
        state.snapshot.tail.turns.retain(|turn| turn.id != turn_id);
    }

    pub fn record_input(&self, turn_id: Option<u64>, input: &Value) {
        let parts = normalized_input(input);
        if parts.is_empty() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        if let Some(turn_id) = turn_id {
            let turn = ensure_turn(&mut state.snapshot.tail.turns, turn_id);
            turn.input.extend(parts);
        } else {
            state.pending_inputs.extend(parts);
        }
    }

    pub fn sync(&self, cursor: Option<&EventCursor>) -> Result<ProjectionSync, CursorError> {
        if let Some(cursor) = cursor {
            cursor.validate()?;
            if cursor.thread_id != self.thread_id {
                return Err(CursorError::ForeignThread);
            }
        }
        let state = self.state.lock().unwrap();
        if state.unavailable {
            return Err(CursorError::ProjectionUnavailable);
        }
        let event_cursor = EventCursor {
            thread_id: self.thread_id.clone(),
            generation: self.generation.clone(),
            seq: state.high_water_seq,
        };
        let Some(cursor) = cursor else {
            return Ok(ProjectionSync::Snapshot {
                reason: SnapshotReason::Initial,
                generation: self.generation.clone(),
                high_water_seq: state.high_water_seq,
                snapshot: Box::new(state.snapshot.clone()),
                event_cursor,
            });
        };
        if cursor.generation != self.generation {
            return Ok(ProjectionSync::Snapshot {
                reason: SnapshotReason::GenerationChanged,
                generation: self.generation.clone(),
                high_water_seq: state.high_water_seq,
                snapshot: Box::new(state.snapshot.clone()),
                event_cursor,
            });
        }
        if cursor.seq > state.high_water_seq {
            return Err(CursorError::FutureSequence);
        }
        if cursor.seq < state.evicted_through {
            return Ok(ProjectionSync::Snapshot {
                reason: SnapshotReason::CursorExpired,
                generation: self.generation.clone(),
                high_water_seq: state.high_water_seq,
                snapshot: Box::new(state.snapshot.clone()),
                event_cursor,
            });
        }
        let events = state
            .ring
            .iter()
            .filter(|event| event.seq > cursor.seq)
            .map(|event| event.envelope.clone())
            .collect();
        Ok(ProjectionSync::Replay {
            generation: self.generation.clone(),
            high_water_seq: state.high_water_seq,
            events,
            event_cursor,
        })
    }
}

fn new_generation() -> Result<String, getrandom::Error> {
    use std::fmt::Write as _;

    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes)?;
    let mut generation = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut generation, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(generation)
}

fn normalized_input(input: &Value) -> Vec<Value> {
    match input {
        Value::String(text) => vec![json!({"type": "text", "text": text})],
        Value::Array(parts) => parts.clone(),
        _ => Vec::new(),
    }
}

fn ensure_turn(turns: &mut Vec<SnapshotTurn>, turn_id: u64) -> &mut SnapshotTurn {
    if let Some(index) = turns.iter().position(|turn| turn.id == turn_id) {
        return &mut turns[index];
    }
    turns.push(SnapshotTurn {
        id: turn_id,
        status: "in_progress".into(),
        input: Vec::new(),
        items: Vec::new(),
        error: None,
    });
    turns.last_mut().unwrap()
}

fn apply_event(state: &mut ProjectionState, method: &str, params: &Value) {
    match method {
        "turn/started" => {
            let Some(turn_id) = params.pointer("/turn/id").and_then(Value::as_u64) else {
                return;
            };
            let pending = std::mem::take(&mut state.pending_inputs);
            let turn = ensure_turn(&mut state.snapshot.tail.turns, turn_id);
            turn.status = "in_progress".into();
            turn.error = None;
            turn.input.extend(pending);
        }
        "turn/completed" => {
            let Some(turn_id) = params.pointer("/turn/id").and_then(Value::as_u64) else {
                return;
            };
            let Some(turn) = state
                .snapshot
                .tail
                .turns
                .iter_mut()
                .find(|turn| turn.id == turn_id)
            else {
                return;
            };
            if let Some(status) = params.pointer("/turn/status").and_then(Value::as_str) {
                turn.status = status.to_string();
            }
            turn.error = params
                .pointer("/turn/error")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        "item/started" | "item/completed" => {
            let Some(turn_id) = params.get("turn_id").and_then(Value::as_u64) else {
                return;
            };
            let Some(item) = params.get("item").cloned() else {
                return;
            };
            let Some(item_id) = item.get("id").and_then(Value::as_str) else {
                return;
            };
            let turn = ensure_turn(&mut state.snapshot.tail.turns, turn_id);
            if let Some(index) = turn
                .items
                .iter()
                .position(|existing| existing.get("id").and_then(Value::as_str) == Some(item_id))
            {
                turn.items[index] = item;
            } else {
                turn.items.push(item);
            }
        }
        "item/delta" => apply_item_delta(&mut state.snapshot.tail.turns, params),
        "note" | "system" => {
            let Some(text) = params.get("text").and_then(Value::as_str) else {
                return;
            };
            state.snapshot.tail.notices.push(SnapshotNotice {
                turn_id: params.get("turn_id").and_then(Value::as_u64),
                kind: if method == "note" {
                    SnapshotNoticeKind::Note
                } else {
                    SnapshotNoticeKind::System
                },
                text: text.to_string(),
            });
        }
        "thread/background_task/updated" => {
            if let Some(task) = params.get("task") {
                upsert_value(&mut state.snapshot.tail.background_tasks, task, "id");
            }
        }
        "thread/agent_message/updated" => {
            let mut message = params.clone();
            if let Some(object) = message.as_object_mut() {
                object.remove("thread_id");
                object.remove("event_generation");
                object.remove("seq");
            }
            upsert_value(
                &mut state.snapshot.tail.agent_messages,
                &message,
                "message_id",
            );
        }
        "thread/scheduler/updated" => {
            if let Some(task) = params.get("task") {
                upsert_value(&mut state.snapshot.tail.scheduled_tasks, task, "id");
            }
        }
        "thread/token_usage/updated" => {
            state.snapshot.tail.token_usage = params.get("token_usage").cloned();
        }
        "thread/cwd/updated" => {
            if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
                state.snapshot.tail.cwd.path = cwd.to_string();
            }
            state.snapshot.tail.cwd.branch = params
                .get("branch")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        "thread/cleared" => {
            state.snapshot.history.messages.clear();
            state.snapshot.history.terminals.clear();
            state.snapshot.tail.turns.clear();
            state.snapshot.tail.notices.clear();
            state.pending_inputs.clear();
        }
        _ => {}
    }
}

fn apply_item_delta(turns: &mut [SnapshotTurn], params: &Value) {
    let Some(turn_id) = params.get("turn_id").and_then(Value::as_u64) else {
        return;
    };
    let Some(item_id) = params.get("item_id").and_then(Value::as_str) else {
        return;
    };
    let Some(text) = params.get("text").and_then(Value::as_str) else {
        return;
    };
    let field = match params.get("channel").and_then(Value::as_str) {
        Some("text" | "reasoning") => "text",
        Some("output") => "output",
        _ => return,
    };
    let Some(item) = turns
        .iter_mut()
        .find(|turn| turn.id == turn_id)
        .and_then(|turn| {
            turn.items
                .iter_mut()
                .find(|item| item.get("id").and_then(Value::as_str) == Some(item_id))
        })
    else {
        return;
    };
    let current = item.get(field).and_then(Value::as_str).unwrap_or("");
    item[field] = Value::String(format!("{current}{text}"));
}

fn upsert_value(values: &mut Vec<Value>, value: &Value, id_field: &str) {
    let Some(id) = value.get(id_field).and_then(Value::as_str) else {
        return;
    };
    if let Some(index) = values
        .iter()
        .position(|existing| existing.get(id_field).and_then(Value::as_str) == Some(id))
    {
        values[index] = value.clone();
    } else {
        values.push(value.clone());
    }
}

fn serialize_sequence<S>(seq: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&seq.to_string())
}

fn deserialize_sequence<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(D::Error::custom("sequence must be a decimal string"));
    }
    raw.parse::<u64>()
        .map_err(|_| D::Error::custom("sequence is outside the u64 range"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kloop_core::rollout::SessionRuntime;

    fn route(model: &str) -> ActiveProviderRoute {
        ActiveProviderRoute {
            revision: 1,
            provider_id: "test".into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            model: model.into(),
            continuity: kloop_protocol::ReasoningContinuity::Preserved,
            effort: None,
        }
    }

    fn seed() -> SessionSnapshot {
        SessionSnapshot {
            messages: vec![Message::user_text("persisted")],
            runtime: Some(SessionRuntime {
                cwd: "/tmp/project".into(),
            }),
            terminals: Vec::new(),
            provider_routes: Vec::new(),
        }
    }

    fn projection(max_events: usize, max_bytes: usize) -> ThreadProjection {
        ThreadProjection::with_generation_and_limits(
            "thread-a".into(),
            "generation-a".into(),
            "/tmp/project".into(),
            route("model-a"),
            seed(),
            RecoverySource::Fresh,
            max_events,
            max_bytes,
        )
    }

    fn publish(projection: &ThreadProjection, method: &'static str, params: Value) -> Value {
        let mut sent = None;
        projection
            .publish(method, params, |params| sent = Some(params))
            .unwrap();
        sent.unwrap()
    }

    #[test]
    fn snapshots_project_reasoning_without_replay_secrets() {
        let reasoning = Message::assistant_from_provider(
            vec![
                kloop_protocol::ContentBlock::Thinking {
                    thinking: "display summary".into(),
                    signature: "opaque-signature".into(),
                },
                kloop_protocol::ContentBlock::RedactedThinking {
                    data: "opaque-redacted".into(),
                },
            ],
            kloop_protocol::ProviderResponseProvenance {
                route_revision: 1,
                route_boundary: 2,
                provider_id: "anthropic".into(),
                api_family: kloop_protocol::ProviderApiFamily::AnthropicMessages,
                endpoint_fingerprint: "endpoint-sha256".into(),
                model: "wire-model".into(),
            },
        );
        let mut seeded = seed();
        seeded.messages.push(reasoning.clone());
        let projection = ThreadProjection::with_generation_and_limits(
            "thread-a".into(),
            "generation-a".into(),
            "/tmp/project".into(),
            route("model-a"),
            seeded,
            RecoverySource::Fresh,
            8,
            usize::MAX,
        );

        let value = serde_json::to_value(projection.sync(None).unwrap()).unwrap();
        let assistant = &value["snapshot"]["history"]["messages"][1];
        assert!(assistant.get("provider_provenance").is_none());
        assert_eq!(
            assistant["content"],
            json!([{"type": "thinking", "thinking": "display summary"}])
        );
        let wire = serde_json::to_string(&value).unwrap();
        assert!(!wire.contains("opaque-signature"));
        assert!(!wire.contains("opaque-redacted"));

        let mut refreshed = seed();
        refreshed.messages.push(reasoning);
        projection.refresh_seed("/tmp/project".into(), route("model-a"), refreshed);
        let refreshed = serde_json::to_string(&projection.sync(None).unwrap()).unwrap();
        assert!(!refreshed.contains("opaque-signature"));
        assert!(!refreshed.contains("opaque-redacted"));
    }

    #[test]
    fn cursor_requires_strict_string_sequence_and_matching_thread() {
        let valid = EventsSyncParams::parse(&json!({
            "thread_id": "thread-a",
            "event_cursor": {"thread_id": "thread-a", "generation": "g", "seq": "0"}
        }))
        .unwrap();
        assert_eq!(valid.event_cursor.unwrap().seq, 0);

        for invalid in [
            json!({"thread_id": "thread-a", "event_cursor": {"thread_id": "thread-a", "generation": "g", "seq": 0}}),
            json!({"thread_id": "thread-a", "event_cursor": {"thread_id": "thread-a", "generation": "g", "seq": "-1"}}),
            json!({"thread_id": "thread-a", "event_cursor": {"thread_id": "thread-a", "generation": "g", "seq": "18446744073709551616"}}),
            json!({"thread_id": "thread-a", "event_cursor": {"thread_id": "other", "generation": "g", "seq": "0"}}),
            json!({"thread_id": "thread-a", "event_cursor": {"thread_id": "thread-a", "generation": "", "seq": "0"}}),
            json!({"thread_id": "thread-a", "unknown": true}),
        ] {
            assert!(EventsSyncParams::parse(&invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn sequences_every_public_event_and_replays_exact_tail() {
        let projection = projection(8, usize::MAX);
        let first = publish(&projection, "turn/started", json!({"turn": {"id": 1}}));
        let second = publish(&projection, "unknown/future", json!({"value": 7}));
        assert_eq!(first["thread_id"], "thread-a");
        assert_eq!(first["event_generation"], "generation-a");
        assert_eq!(first["seq"], "1");
        assert_eq!(second["seq"], "2");

        let sync = projection
            .sync(Some(&EventCursor {
                thread_id: "thread-a".into(),
                generation: "generation-a".into(),
                seq: 1,
            }))
            .unwrap();
        let value = serde_json::to_value(sync).unwrap();
        assert_eq!(
            value,
            json!({
                "mode": "replay",
                "generation": "generation-a",
                "high_water_seq": "2",
                "events": [{
                    "method": "unknown/future",
                    "params": {
                        "thread_id": "thread-a",
                        "event_generation": "generation-a",
                        "seq": "2",
                        "value": 7
                    }
                }],
                "event_cursor": {"thread_id": "thread-a", "generation": "generation-a", "seq": "2"}
            })
        );
    }

    #[test]
    fn current_cursor_is_an_empty_idempotent_replay() {
        let projection = projection(8, usize::MAX);
        publish(&projection, "note", json!({"text": "one"}));
        let sync = projection
            .sync(Some(&EventCursor {
                thread_id: "thread-a".into(),
                generation: "generation-a".into(),
                seq: 1,
            }))
            .unwrap();
        let value = serde_json::to_value(sync).unwrap();
        assert_eq!(value["mode"], "replay");
        assert_eq!(value["events"], json!([]));
        assert_eq!(value["high_water_seq"], "1");
    }

    #[test]
    fn initial_changed_and_expired_cursors_choose_snapshots() {
        let projection = projection(1, usize::MAX);
        publish(&projection, "note", json!({"text": "one"}));
        publish(&projection, "note", json!({"text": "two"}));

        let initial = serde_json::to_value(projection.sync(None).unwrap()).unwrap();
        assert_eq!(initial["mode"], "snapshot");
        assert_eq!(initial["reason"], "initial");

        let changed = serde_json::to_value(
            projection
                .sync(Some(&EventCursor {
                    thread_id: "thread-a".into(),
                    generation: "old".into(),
                    seq: 99,
                }))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(changed["reason"], "generation_changed");

        let expired = serde_json::to_value(
            projection
                .sync(Some(&EventCursor {
                    thread_id: "thread-a".into(),
                    generation: "generation-a".into(),
                    seq: 0,
                }))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(expired["reason"], "cursor_expired");
    }

    #[test]
    fn same_generation_future_cursor_fails_closed() {
        let projection = projection(8, usize::MAX);
        assert!(matches!(
            projection.sync(Some(&EventCursor {
                thread_id: "thread-a".into(),
                generation: "generation-a".into(),
                seq: 1,
            })),
            Err(CursorError::FutureSequence)
        ));
    }

    #[test]
    fn reducer_materializes_items_state_and_clear_boundary() {
        let projection = projection(32, usize::MAX);
        projection.record_input(Some(3), &json!("hello"));
        publish(&projection, "turn/started", json!({"turn": {"id": 3}}));
        publish(
            &projection,
            "item/started",
            json!({"turn_id": 3, "item": {"id": "msg-0", "type": "assistant_message", "status": "in_progress", "text": ""}}),
        );
        publish(
            &projection,
            "item/delta",
            json!({"turn_id": 3, "item_id": "msg-0", "channel": "text", "text": "hi"}),
        );
        publish(
            &projection,
            "item/completed",
            json!({"turn_id": 3, "item": {"id": "msg-0", "type": "assistant_message", "status": "completed", "text": "hi"}}),
        );
        publish(
            &projection,
            "thread/background_task/updated",
            json!({"task": {"id": "bg-1", "status": "running"}}),
        );
        publish(
            &projection,
            "thread/background_task/updated",
            json!({"task": {"id": "bg-1", "status": "completed"}}),
        );
        publish(
            &projection,
            "thread/agent_message/updated",
            json!({
                "message_id": "mail-1",
                "from": "agent-a",
                "to": "agent-b",
                "summary": "ready",
                "status": "delivered",
            }),
        );
        publish(&projection, "note", json!({"turn_id": 3, "text": "n"}));
        publish(
            &projection,
            "turn/completed",
            json!({"turn": {"id": 3, "status": "completed"}}),
        );

        let snapshot = serde_json::to_value(projection.sync(None).unwrap()).unwrap();
        assert_eq!(
            snapshot["snapshot"]["tail"]["turns"][0]["input"],
            json!([{"type": "text", "text": "hello"}])
        );
        assert_eq!(
            snapshot["snapshot"]["tail"]["turns"][0]["items"][0]["text"],
            "hi"
        );
        assert_eq!(
            snapshot["snapshot"]["tail"]["turns"][0]["status"],
            "completed"
        );
        assert_eq!(
            snapshot["snapshot"]["tail"]["background_tasks"],
            json!([{"id": "bg-1", "status": "completed"}])
        );
        assert_eq!(
            snapshot["snapshot"]["tail"]["agent_messages"],
            json!([{
                "message_id": "mail-1",
                "from": "agent-a",
                "to": "agent-b",
                "summary": "ready",
                "status": "delivered",
            }])
        );
        assert_eq!(snapshot["snapshot"]["tail"]["notices"][0]["kind"], "note");

        publish(&projection, "thread/cleared", json!({}));
        let cleared = serde_json::to_value(projection.sync(None).unwrap()).unwrap();
        assert_eq!(cleared["snapshot"]["history"]["messages"], json!([]));
        assert_eq!(cleared["snapshot"]["tail"]["turns"], json!([]));
        assert_eq!(cleared["snapshot"]["tail"]["notices"], json!([]));
        assert_eq!(
            cleared["snapshot"]["tail"]["background_tasks"],
            json!([{"id": "bg-1", "status": "completed"}])
        );
    }

    #[test]
    fn byte_retention_expires_only_replay_not_snapshot_state() {
        let projection = projection(8, 1);
        publish(
            &projection,
            "note",
            json!({"text": "this event is larger than one byte"}),
        );
        let sync = serde_json::to_value(
            projection
                .sync(Some(&EventCursor {
                    thread_id: "thread-a".into(),
                    generation: "generation-a".into(),
                    seq: 0,
                }))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(sync["reason"], "cursor_expired");
        assert_eq!(
            sync["snapshot"]["tail"]["notices"][0]["text"],
            "this event is larger than one byte"
        );
    }

    #[test]
    fn sequence_overflow_terminates_the_projection_without_reuse() {
        let projection = projection(8, usize::MAX);
        projection.state.lock().unwrap().high_water_seq = u64::MAX;
        let mut enqueued = false;
        assert!(matches!(
            projection.publish("note", json!({"text": "never"}), |_| enqueued = true),
            Err(ProjectionError::SequenceExhausted)
        ));
        assert!(!enqueued);
        assert!(matches!(
            projection.sync(None),
            Err(CursorError::ProjectionUnavailable)
        ));
    }

    #[test]
    fn sync_cannot_cross_publishs_reducer_ring_enqueue_barrier() {
        use std::sync::Arc;
        use std::sync::mpsc;
        use std::time::Duration;

        let projection = Arc::new(projection(8, usize::MAX));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let publisher_projection = projection.clone();
        let publisher = std::thread::spawn(move || {
            publisher_projection
                .publish("note", json!({"text": "atomic"}), |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                })
                .unwrap();
        });
        entered_rx.recv().unwrap();

        let (sync_tx, sync_rx) = mpsc::channel();
        let sync_projection = projection.clone();
        let syncer = std::thread::spawn(move || {
            sync_tx.send(sync_projection.sync(None).unwrap()).unwrap();
        });
        assert!(sync_rx.recv_timeout(Duration::from_millis(20)).is_err());
        release_tx.send(()).unwrap();
        publisher.join().unwrap();
        syncer.join().unwrap();

        let sync = serde_json::to_value(sync_rx.recv().unwrap()).unwrap();
        assert_eq!(sync["high_water_seq"], "1");
        assert_eq!(sync["snapshot"]["tail"]["notices"][0]["text"], "atomic");
    }

    #[test]
    fn refreshed_seed_updates_thread_and_tail_runtime_projection() {
        let projection = projection(8, usize::MAX);
        let mut refreshed = seed();
        refreshed.runtime = Some(SessionRuntime {
            cwd: "/tmp/new-project".into(),
        });
        projection.refresh_seed("/tmp/new-project".into(), route("model-b"), refreshed);

        let snapshot = serde_json::to_value(projection.sync(None).unwrap()).unwrap();
        assert_eq!(
            snapshot["snapshot"]["thread"],
            json!({
                "id": "thread-a",
                "cwd": "/tmp/new-project",
                "route": {
                    "revision": 1,
                    "provider_id": "test",
                    "api_family": "mock",
                    "model": "model-b",
                    "continuity": "preserved",
                },
                "resumable": true,
            })
        );
        assert_eq!(
            snapshot["snapshot"]["tail"]["cwd"]["path"],
            "/tmp/new-project"
        );
    }

    #[test]
    fn resumed_seed_marks_volatile_state_reset() {
        let projection = ThreadProjection::with_generation_and_limits(
            "thread-a".into(),
            "generation-b".into(),
            "/tmp/project".into(),
            route("model-a"),
            seed(),
            RecoverySource::Resumed,
            8,
            usize::MAX,
        );
        let snapshot = serde_json::to_value(projection.sync(None).unwrap()).unwrap();
        assert_eq!(
            snapshot["snapshot"]["recovery"],
            json!({"source": "resumed", "volatile_state": "reset"})
        );
        assert_eq!(
            snapshot["snapshot"]["history"]["messages"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(snapshot["snapshot"]["tail"]["background_tasks"], json!([]));
    }
}
