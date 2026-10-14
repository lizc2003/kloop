use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::request_reduction::FrozenStub;
use crate::usage::ProviderUsageRecord;
use kloop_protocol::{AssistantOutcome, IncompleteReason, Message, ProviderRouteReceipt};
use kloop_provider::{ProviderFailure, ProviderFailureKind, TimeoutStage};

/// The session file format this kloop writes, and the only one it reads. A
/// file opens with its version, so a reader meets an unknown format at the
/// first line instead of misreading a field whose meaning moved under it.
/// Bump this when a change would make an older file parse into the wrong
/// meaning; purely additive fields do not need it, because unknown fields are
/// ignored on read.
pub const SESSION_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRuntime {
    pub cwd: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnError {
    Core(String),
    ProviderOutcome(AssistantOutcome),
    ProviderFailure(ProviderFailure),
    /// A round hit its output cap with nothing visible to continue from: all
    /// of it went to reasoning. Carries the cap the round asked for.
    OutputSpentOnReasoning {
        max_output_tokens: u64,
    },
}

mod provider_failure_serde {
    use super::*;

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum RecordedKind {
        ContextOverflow,
        Http { status: u16 },
        Transport,
        Timeout { stage: RecordedTimeoutStage },
        Protocol,
        ResponseTooLarge,
        Cancelled,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum RecordedTimeoutStage {
        Open,
        Idle,
        Wall,
    }

    #[derive(Serialize, Deserialize)]
    struct RecordedFailure {
        kind: RecordedKind,
        message: String,
        retryable: bool,
        retry_after: Option<Duration>,
        semantic_output: bool,
    }

    impl RecordedKind {
        fn capture(kind: &ProviderFailureKind) -> Self {
            match kind {
                ProviderFailureKind::ContextOverflow => Self::ContextOverflow,
                ProviderFailureKind::Http { status } => Self::Http { status: *status },
                ProviderFailureKind::Transport => Self::Transport,
                ProviderFailureKind::Timeout { stage } => Self::Timeout {
                    stage: match stage {
                        TimeoutStage::Open => RecordedTimeoutStage::Open,
                        TimeoutStage::Idle => RecordedTimeoutStage::Idle,
                        TimeoutStage::Wall => RecordedTimeoutStage::Wall,
                    },
                },
                ProviderFailureKind::Protocol => Self::Protocol,
                ProviderFailureKind::ResponseTooLarge => Self::ResponseTooLarge,
                ProviderFailureKind::Cancelled => Self::Cancelled,
            }
        }

        fn restore(self) -> ProviderFailureKind {
            match self {
                Self::ContextOverflow => ProviderFailureKind::ContextOverflow,
                Self::Http { status } => ProviderFailureKind::Http { status },
                Self::Transport => ProviderFailureKind::Transport,
                Self::Timeout { stage } => ProviderFailureKind::Timeout {
                    stage: match stage {
                        RecordedTimeoutStage::Open => TimeoutStage::Open,
                        RecordedTimeoutStage::Idle => TimeoutStage::Idle,
                        RecordedTimeoutStage::Wall => TimeoutStage::Wall,
                    },
                },
                Self::Protocol => ProviderFailureKind::Protocol,
                Self::ResponseTooLarge => ProviderFailureKind::ResponseTooLarge,
                Self::Cancelled => ProviderFailureKind::Cancelled,
            }
        }
    }

    pub fn serialize<S>(failure: &ProviderFailure, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        RecordedFailure {
            kind: RecordedKind::capture(failure.kind()),
            message: failure.message().to_string(),
            retryable: failure.is_retryable(),
            retry_after: failure.retry_after(),
            semantic_output: failure.after_semantic_output(),
        }
        .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<ProviderFailure, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let failure = RecordedFailure::deserialize(deserializer)?;
        Ok(ProviderFailure::from_recorded_terminal(
            failure.kind.restore(),
            failure.message,
            failure.retryable,
            failure.retry_after,
            failure.semantic_output,
        ))
    }
}

#[derive(Serialize, Deserialize)]
struct RecordedProviderFailure(#[serde(with = "provider_failure_serde")] ProviderFailure);

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum RecordedTurnError {
    Core(String),
    ProviderOutcome(AssistantOutcome),
    ProviderFailure(RecordedProviderFailure),
    OutputSpentOnReasoning { max_output_tokens: u64 },
}

impl Serialize for TurnError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Core(error) => RecordedTurnError::Core(error.clone()),
            Self::ProviderOutcome(outcome) => RecordedTurnError::ProviderOutcome(outcome.clone()),
            Self::ProviderFailure(failure) => {
                RecordedTurnError::ProviderFailure(RecordedProviderFailure(failure.clone()))
            }
            Self::OutputSpentOnReasoning { max_output_tokens } => {
                RecordedTurnError::OutputSpentOnReasoning {
                    max_output_tokens: *max_output_tokens,
                }
            }
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TurnError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match RecordedTurnError::deserialize(deserializer)? {
            RecordedTurnError::Core(error) => Self::Core(error),
            RecordedTurnError::ProviderOutcome(outcome) => Self::ProviderOutcome(outcome),
            RecordedTurnError::ProviderFailure(RecordedProviderFailure(failure)) => {
                Self::ProviderFailure(failure)
            }
            RecordedTurnError::OutputSpentOnReasoning { max_output_tokens } => {
                Self::OutputSpentOnReasoning { max_output_tokens }
            }
        })
    }
}

impl TurnError {
    pub fn contains(&self, needle: &str) -> bool {
        self.to_string().contains(needle)
    }
}

impl From<String> for TurnError {
    fn from(error: String) -> Self {
        Self::Core(error)
    }
}

impl From<&str> for TurnError {
    fn from(error: &str) -> Self {
        Self::Core(error.to_string())
    }
}

impl std::fmt::Display for TurnError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Core(error) => formatter.write_str(error),
            Self::ProviderFailure(error) => write!(formatter, "{error}"),
            Self::ProviderOutcome(AssistantOutcome::Refused) => {
                formatter.write_str("model refused the request")
            }
            Self::ProviderOutcome(AssistantOutcome::Filtered) => {
                formatter.write_str("provider filtered the response")
            }
            Self::ProviderOutcome(AssistantOutcome::Incomplete(reason)) => {
                let reason = match reason {
                    IncompleteReason::PauseTurn => "pause_turn",
                    IncompleteReason::Provider(reason) => reason,
                };
                write!(
                    formatter,
                    "provider returned an incomplete response: {reason}"
                )
            }
            Self::ProviderOutcome(AssistantOutcome::OutputLimit(_)) => {
                formatter.write_str("response remained truncated after 3 continuation attempts")
            }
            Self::ProviderOutcome(AssistantOutcome::EndTurn) => {
                formatter.write_str("provider end_turn was misclassified as an error")
            }
            Self::ProviderOutcome(AssistantOutcome::ToolUse) => {
                formatter.write_str("provider tool_use was misclassified as an error")
            }
            // No advice about effort: lowering it was measured not to help.
            Self::OutputSpentOnReasoning { max_output_tokens } => write!(
                formatter,
                "the model spent the entire {max_output_tokens}-token output limit on \
                 reasoning and produced no answer; split the request into smaller steps \
                 or switch models"
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnTerminal {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typed_error: Option<TurnError>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotTerminal {
    pub after_message: usize,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionSnapshot {
    pub messages: Vec<Message>,
    pub runtime: Option<SessionRuntime>,
    pub terminals: Vec<SnapshotTerminal>,
    #[serde(skip_serializing)]
    pub provider_routes: Vec<ProviderRouteReceipt>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingRepairStats {
    pub messages_before: usize,
    pub messages_after: usize,
    pub dropped_tool_results: usize,
    pub duplicate_tool_results: usize,
    pub dropped_messages: usize,
    pub inserted_tool_results: usize,
    pub changed_messages: usize,
}

#[derive(Serialize, Deserialize)]
pub(super) struct LineMeta {
    pub(super) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) parent: Option<String>,
    /// Set only on a sub-agent session's FIRST line: `{parent stem}#{seq}` of
    /// the parent turn's assistant line that carried the spawning task
    /// tool_use. Lineage metadata only — unlike a fork, a sub-agent history is
    /// wholly independent (no prefix copied), so replay ignores it. It drives
    /// the `[sub-agent of …]` label and keeps sub-agent files out of the
    /// default resume picker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) subagent_of: Option<String>,
    /// Set only on a file's FIRST line: which format the whole file is written
    /// in. A reader meets an unknown version at the first line it parses,
    /// instead of reading later fields under a meaning that has moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) format_version: Option<u32>,
    /// Unix milliseconds at append time.
    pub(super) ts: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum RolloutLine {
    Session {
        #[serde(flatten)]
        meta: LineMeta,
        runtime: SessionRuntime,
    },
    ProviderRouteInitial {
        #[serde(flatten)]
        meta: LineMeta,
        #[serde(flatten)]
        receipt: ProviderRouteReceipt,
    },
    ProviderRouteChanged {
        #[serde(flatten)]
        meta: LineMeta,
        #[serde(flatten)]
        receipt: ProviderRouteReceipt,
    },
    Message {
        #[serde(flatten)]
        meta: LineMeta,
        #[serde(flatten)]
        message: Message,
    },
    ProviderUsage {
        #[serde(flatten)]
        meta: LineMeta,
        #[serde(flatten)]
        record: ProviderUsageRecord,
    },
    Compacted {
        #[serde(flatten)]
        meta: LineMeta,
        replacement: Vec<Message>,
    },
    TurnTerminal {
        #[serde(flatten)]
        meta: LineMeta,
        #[serde(flatten)]
        terminal: TurnTerminal,
    },
    Repaired {
        #[serde(flatten)]
        meta: LineMeta,
        kind: String,
        replacement: Vec<Message>,
        terminals: Vec<SnapshotTerminal>,
        stats: PairingRepairStats,
    },
    /// A result requests carry as a stub from here on (plan 200). Not part of
    /// the conversation — replay leaves `items` alone — but part of what was
    /// sent, so a resumed session sends the same bytes. A compacted marker
    /// voids every stub before it.
    RequestStub {
        #[serde(flatten)]
        meta: LineMeta,
        #[serde(flatten)]
        stub: FrozenStub,
    },
    /// A call that may have side effects got past every gate and is about to
    /// run (plan 204). Not part of the conversation; only pairing repair reads
    /// it, to tell a call that never started from one that may have acted.
    ToolStarted {
        #[serde(flatten)]
        meta: LineMeta,
        tool_use_id: String,
    },
}

impl RolloutLine {
    /// Whether the line is opening preamble — the runtime and the provider
    /// route a session records before it has anything to say. A file holding
    /// only these replays as an empty conversation.
    pub(super) fn is_preamble(&self) -> bool {
        matches!(
            self,
            RolloutLine::Session { .. }
                | RolloutLine::ProviderRouteInitial { .. }
                | RolloutLine::ProviderRouteChanged { .. }
        )
    }

    /// Whether the line must reach the disk, not just the page cache, before
    /// the append returns. Only a lost `ToolStarted` errs toward danger — it
    /// would tell the model a call that may have acted never ran — so only it
    /// pays for a sync. Every other line lost to a power cut costs at most
    /// the tail of a conversation.
    pub(super) fn needs_sync(&self) -> bool {
        match self {
            RolloutLine::ToolStarted { .. } => true,
            RolloutLine::Session { .. }
            | RolloutLine::ProviderRouteInitial { .. }
            | RolloutLine::ProviderRouteChanged { .. }
            | RolloutLine::Message { .. }
            | RolloutLine::ProviderUsage { .. }
            | RolloutLine::Compacted { .. }
            | RolloutLine::TurnTerminal { .. }
            | RolloutLine::Repaired { .. }
            | RolloutLine::RequestStub { .. } => false,
        }
    }

    /// The line's metadata, common to every variant.
    pub(super) fn meta(&self) -> &LineMeta {
        match self {
            RolloutLine::Session { meta, .. }
            | RolloutLine::ProviderRouteInitial { meta, .. }
            | RolloutLine::ProviderRouteChanged { meta, .. }
            | RolloutLine::Message { meta, .. }
            | RolloutLine::ProviderUsage { meta, .. }
            | RolloutLine::Compacted { meta, .. }
            | RolloutLine::TurnTerminal { meta, .. }
            | RolloutLine::Repaired { meta, .. }
            | RolloutLine::RequestStub { meta, .. }
            | RolloutLine::ToolStarted { meta, .. } => meta,
        }
    }

    /// Consume the line for its metadata, dropping the payload.
    pub(super) fn into_meta(self) -> LineMeta {
        match self {
            RolloutLine::Session { meta, .. }
            | RolloutLine::ProviderRouteInitial { meta, .. }
            | RolloutLine::ProviderRouteChanged { meta, .. }
            | RolloutLine::Message { meta, .. }
            | RolloutLine::ProviderUsage { meta, .. }
            | RolloutLine::Compacted { meta, .. }
            | RolloutLine::TurnTerminal { meta, .. }
            | RolloutLine::Repaired { meta, .. }
            | RolloutLine::RequestStub { meta, .. }
            | RolloutLine::ToolStarted { meta, .. } => meta,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_started_line_is_synced_to_disk() {
        let meta = || LineMeta {
            id: "s#1".into(),
            parent: None,
            subagent_of: None,
            format_version: None,
            ts: 0,
        };
        let started = RolloutLine::ToolStarted {
            meta: meta(),
            tool_use_id: "w1".into(),
        };
        let message = RolloutLine::Message {
            meta: meta(),
            message: Message::user_text("hi"),
        };
        assert_eq!((started.needs_sync(), message.needs_sync()), (true, false));
    }

    /// The reasoning-only stop keeps its cap through the session file, and the
    /// line a reader sees says what happened and what to do — not "lower the
    /// effort", which was measured not to help.
    #[test]
    fn output_spent_on_reasoning_round_trips_with_its_cap() {
        let error = TurnError::OutputSpentOnReasoning {
            max_output_tokens: 65_536,
        };
        let encoded = serde_json::to_value(&error).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({
                "kind": "output_spent_on_reasoning",
                "value": {"max_output_tokens": 65_536},
            })
        );
        assert_eq!(serde_json::from_value::<TurnError>(encoded).unwrap(), error);
        assert_eq!(
            error.to_string(),
            "the model spent the entire 65536-token output limit on reasoning and produced \
             no answer; split the request into smaller steps or switch models"
        );
    }
}
