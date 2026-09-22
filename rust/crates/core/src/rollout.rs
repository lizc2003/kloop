//! Session persistence: one JSONL file per session. Message lines are
//! appended as the history records them; compaction — the one sanctioned
//! history rewrite — is persisted as an appended `compacted` marker carrying
//! the full replacement history, so the file itself stays append-only and
//! auditable. Replay swaps in the replacement and keeps reading.
//!
//! Every line carries an envelope (`id`, `parent`, `ts`): replay is linear
//! and within one file the chain is purely sequential, but the fields make
//! forking expressible — a forked file's FIRST line carries a cross-file
//! parent (`{source stem}#{cut seq}`) recording where it branched off. That
//! pointer is lineage metadata only: a fork physically copies the kept
//! prefix (what cc's /branch and codex's thread/fork both do — neither
//! replays across files), so replay never follows it. Ids are
//! `{file stem}#{seq}` — unique within the file without a rand dependency.
//! Unknown fields in a line are ignored on read, so the format can grow
//! additively.

use std::collections::HashSet;
use std::io;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;

use crate::inbox::STEERING_PREFIX;
use crate::provider_route::FrozenProviderRoute;
use crate::provider_route::ProvenanceMismatch;
use crate::provider_route::ReasoningShape;
use crate::provider_route::validate_provenance;
use crate::provider_route::validate_timeline;
use crate::tools::interrupted;
use crate::usage::{ProviderUsageRecord, UsageLedger};
use kloop_protocol::AssistantOutcome;
use kloop_protocol::ContentBlock;
use kloop_protocol::IncompleteReason;
use kloop_protocol::Message;
use kloop_protocol::ProviderRouteReceipt;
use kloop_protocol::ProviderRouteSource;
use kloop_protocol::ReasoningContinuity;
use kloop_protocol::Role;
use kloop_provider::ProviderFailure;
use kloop_provider::ProviderFailureKind;
use kloop_provider::TimeoutStage;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRuntime {
    pub cwd: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnError {
    Core(String),
    ProviderOutcome(AssistantOutcome),
    ProviderFailure(ProviderFailure),
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

#[derive(Clone, Debug, PartialEq)]
struct PairingRepair {
    messages: Vec<Message>,
    terminals: Vec<SnapshotTerminal>,
    stats: PairingRepairStats,
    changed: bool,
}

#[derive(Serialize, Deserialize)]
struct LineMeta {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    /// Set only on a sub-agent session's FIRST line: `{parent stem}#{seq}` of
    /// the parent turn's assistant line that carried the spawning task
    /// tool_use. Lineage metadata only — unlike a fork, a sub-agent history is
    /// wholly independent (no prefix copied), so replay ignores it. It drives
    /// the `[sub-agent of …]` label and keeps sub-agent files out of the
    /// default resume picker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subagent_of: Option<String>,
    /// Unix milliseconds at append time.
    ts: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RolloutLine {
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
}

impl RolloutLine {
    /// Whether the line is opening preamble — the runtime and the provider
    /// route a session records before it has anything to say. A file holding
    /// only these replays as an empty conversation.
    fn is_preamble(&self) -> bool {
        matches!(
            self,
            RolloutLine::Session { .. }
                | RolloutLine::ProviderRouteInitial { .. }
                | RolloutLine::ProviderRouteChanged { .. }
        )
    }

    /// The line's metadata, common to every variant.
    fn meta(&self) -> &LineMeta {
        match self {
            RolloutLine::Session { meta, .. }
            | RolloutLine::ProviderRouteInitial { meta, .. }
            | RolloutLine::ProviderRouteChanged { meta, .. }
            | RolloutLine::Message { meta, .. }
            | RolloutLine::ProviderUsage { meta, .. }
            | RolloutLine::Compacted { meta, .. }
            | RolloutLine::TurnTerminal { meta, .. }
            | RolloutLine::Repaired { meta, .. } => meta,
        }
    }

    /// Consume the line for its metadata, dropping the payload.
    fn into_meta(self) -> LineMeta {
        match self {
            RolloutLine::Session { meta, .. }
            | RolloutLine::ProviderRouteInitial { meta, .. }
            | RolloutLine::ProviderRouteChanged { meta, .. }
            | RolloutLine::Message { meta, .. }
            | RolloutLine::ProviderUsage { meta, .. }
            | RolloutLine::Compacted { meta, .. }
            | RolloutLine::TurnTerminal { meta, .. }
            | RolloutLine::Repaired { meta, .. } => meta,
        }
    }
}

/// Append-only writer for one session file. The file (and its directory) are
/// created on first append — which is the opening preamble, so the file exists
/// from the start and its name reserves the session id against a concurrent
/// process picking the same one. A writer that never gets past that preamble
/// removes the file again when it is dropped, so starting kloop and quitting
/// without a word leaves nothing behind. Tracks the id chain: each line's
/// `parent` is the previous line's id.
pub struct Rollout {
    path: PathBuf,
    prefix: String,
    next_seq: u64,
    last_id: Option<String>,
    /// Set only for a sub-agent's rollout: stamped onto the FIRST appended
    /// line's envelope (`subagent_of`) and ignored thereafter.
    subagent_of: Option<String>,
    route_timeline: Vec<ProviderRouteReceipt>,
    /// The file was already on disk when this writer opened it, so the writer
    /// does not own it and never removes it.
    preexisting: bool,
    /// Something past the opening preamble has been appended, i.e. the file now
    /// holds a session worth resuming.
    wrote_content: bool,
}

impl Drop for Rollout {
    fn drop(&mut self) {
        if self.preexisting || self.wrote_content {
            return;
        }
        // Nothing but the preamble was ever written: launching kloop and
        // quitting without a word must not leave a session behind, or
        // `--continue` picks that empty shell over the last real conversation.
        // Only a file this writer created can be removed here.
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Rollout {
    /// A fresh fixture session with a complete mock route timeline. Production
    /// session owners use `new_with_initial_route` once their catalog route is resolved.
    pub fn new(path: PathBuf) -> Self {
        let mut rollout = Self::with_origin(path, None);
        rollout
            .append_fixture_initial_route()
            .expect("fixture rollout initial route must be writable");
        rollout
    }

    pub fn new_with_initial_route(path: PathBuf, route: &FrozenProviderRoute) -> io::Result<Self> {
        let mut rollout = Self::with_origin(path, None);
        rollout.append_initial_route(route)?;
        Ok(rollout)
    }

    pub fn new_with_runtime_pending_route(
        path: PathBuf,
        runtime: SessionRuntime,
    ) -> io::Result<Self> {
        let mut rollout = Self::with_origin(path, None);
        rollout.append_runtime(&runtime)?;
        Ok(rollout)
    }

    /// A fresh fixture session whose runtime choices must survive process restarts.
    pub fn new_with_runtime(path: PathBuf, runtime: SessionRuntime) -> io::Result<Self> {
        let mut rollout = Self::with_origin(path, None);
        rollout.append_runtime(&runtime)?;
        rollout.append_fixture_initial_route()?;
        Ok(rollout)
    }

    pub fn new_with_runtime_and_route(
        path: PathBuf,
        runtime: SessionRuntime,
        route: &FrozenProviderRoute,
    ) -> io::Result<Self> {
        let mut rollout = Self::with_origin(path, None);
        rollout.append_runtime(&runtime)?;
        rollout.append_initial_route(route)?;
        Ok(rollout)
    }

    /// A sub-agent's rollout: like [`Rollout::new`], but the first appended
    /// line records the parent turn (`{parent stem}#{seq}`) that spawned it.
    pub fn new_subagent(path: PathBuf, subagent_of: String) -> Self {
        let mut rollout = Self::with_origin(path, Some(subagent_of));
        rollout
            .append_fixture_initial_route()
            .expect("fixture sub-agent initial route must be writable");
        rollout
    }

    pub fn new_subagent_with_route(
        path: PathBuf,
        subagent_of: String,
        route: &FrozenProviderRoute,
    ) -> io::Result<Self> {
        let mut rollout = Self::with_origin(path, Some(subagent_of));
        rollout.append_initial_route(route)?;
        Ok(rollout)
    }

    fn with_origin(path: PathBuf, subagent_of: Option<String>) -> Self {
        let prefix = id_prefix(&path);
        Self {
            // A file already on disk belongs to a resumed or forked session:
            // this writer did not create it and must never remove it.
            preexisting: path.exists(),
            path,
            prefix,
            next_seq: 1,
            last_id: None,
            subagent_of,
            route_timeline: Vec::new(),
            wrote_content: false,
        }
    }

    /// The id of the most recently appended line (`{stem}#{seq}`), or None if
    /// nothing has been written yet. A spawning parent uses this as the
    /// `subagent_of` back-pointer for the sub-agent it launches.
    pub fn last_id(&self) -> Option<&str> {
        self.last_id.as_deref()
    }

    pub fn next_boundary(&self) -> u64 {
        self.next_seq
    }

    /// The session file this rollout writes to — surfaced in the subagent_stop
    /// hook so an audit hook can point at the sub-agent's transcript.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append_runtime(&mut self, runtime: &SessionRuntime) -> io::Result<()> {
        self.append_line(RolloutLine::Session {
            meta: self.next_meta(),
            runtime: runtime.clone(),
        })
    }

    fn append_fixture_initial_route(&mut self) -> io::Result<ProviderRouteReceipt> {
        let receipt = ProviderRouteReceipt {
            route_revision: 1,
            route_boundary: self.next_seq,
            source: ProviderRouteSource::Initial,
            provider_id: "test".into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            endpoint_fingerprint: kloop_provider::Provider::endpoint_fingerprint_for(
                kloop_protocol::ProviderApiFamily::Mock,
                "mock",
            ),
            model: "mock".into(),
            effort: None,
            continuity: ReasoningContinuity::Preserved,
        };
        self.append_line(RolloutLine::ProviderRouteInitial {
            meta: self.next_meta(),
            receipt: receipt.clone(),
        })?;
        self.route_timeline.push(receipt.clone());
        Ok(receipt)
    }

    pub fn append_initial_route(
        &mut self,
        route: &FrozenProviderRoute,
    ) -> io::Result<ProviderRouteReceipt> {
        if !self.route_timeline.is_empty() || route.revision() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "initial provider route must be the unique revision 1 receipt",
            ));
        }
        let receipt = route.receipt(
            self.next_seq,
            ProviderRouteSource::Initial,
            ReasoningContinuity::Preserved,
        );
        self.append_line(RolloutLine::ProviderRouteInitial {
            meta: self.next_meta(),
            receipt: receipt.clone(),
        })?;
        self.route_timeline.push(receipt.clone());
        Ok(receipt)
    }

    pub fn append_provider_route_changed(
        &mut self,
        route: &FrozenProviderRoute,
        source: ProviderRouteSource,
        continuity: ReasoningContinuity,
    ) -> io::Result<ProviderRouteReceipt> {
        if source == ProviderRouteSource::Initial {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a route change cannot claim to be the initial route",
            ));
        }
        let previous = self.route_timeline.last().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "provider route timeline is missing",
            )
        })?;
        if route.revision() != previous.route_revision.saturating_add(1) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "provider route revision must advance by exactly one",
            ));
        }
        let receipt = route.receipt(self.next_seq, source, continuity);
        self.append_line(RolloutLine::ProviderRouteChanged {
            meta: self.next_meta(),
            receipt: receipt.clone(),
        })?;
        self.route_timeline.push(receipt.clone());
        Ok(receipt)
    }

    pub fn route_timeline(&self) -> &[ProviderRouteReceipt] {
        &self.route_timeline
    }

    pub fn append_message(&mut self, message: &Message) -> io::Result<()> {
        self.append_line(RolloutLine::Message {
            meta: self.next_meta(),
            message: message.clone(),
        })
    }

    pub fn append_provider_usage(&mut self, record: &ProviderUsageRecord) -> io::Result<()> {
        self.append_line(RolloutLine::ProviderUsage {
            meta: self.next_meta(),
            record: record.clone(),
        })
    }

    pub fn append_compacted(&mut self, replacement: &[Message]) -> io::Result<()> {
        self.append_line(RolloutLine::Compacted {
            meta: self.next_meta(),
            replacement: replacement.to_vec(),
        })
    }

    pub fn append_turn_terminal(&mut self, terminal: &TurnTerminal) -> io::Result<()> {
        self.append_line(RolloutLine::TurnTerminal {
            meta: self.next_meta(),
            terminal: terminal.clone(),
        })
    }

    fn append_repaired(&mut self, repair: &PairingRepair) -> io::Result<()> {
        self.append_line(RolloutLine::Repaired {
            meta: self.next_meta(),
            kind: "pairing".into(),
            replacement: repair.messages.clone(),
            terminals: repair.terminals.clone(),
            stats: repair.stats.clone(),
        })
    }

    fn next_meta(&self) -> LineMeta {
        LineMeta {
            id: format!("{}#{}", self.prefix, self.next_seq),
            parent: self.last_id.clone(),
            // Only the first line carries the sub-agent back-pointer.
            subagent_of: if self.next_seq == 1 {
                self.subagent_of.clone()
            } else {
                None
            },
            ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }
    }

    fn append_line(&mut self, line: RolloutLine) -> io::Result<()> {
        if self.next_seq == u64::MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session sequence exhausted",
            ));
        }
        let json = serde_json::to_string(&line).map_err(io::Error::other)?;
        let content = !line.is_preamble();
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{json}")?;
        // Only advance the chain once the line is durably in the file.
        self.wrote_content |= content;
        self.last_id = Some(line.into_meta().id);
        self.next_seq = self.next_seq.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "session sequence exhausted")
        })?;
        Ok(())
    }
}

fn id_prefix(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session")
        .to_string()
}

/// One parse of a session file: the effective replayed history plus the
/// chain state needed to keep appending, and where the intact content ends.
struct ParsedSession {
    items: Vec<Message>,
    provider_usage: UsageLedger,
    runtime: Option<SessionRuntime>,
    terminals: Vec<SnapshotTerminal>,
    route_timeline: Vec<ProviderRouteReceipt>,
    last_id: Option<String>,
    max_seq: u64,
    /// Byte offset just past the last intact line; anything after is a
    /// malformed or unterminated tail (crash mid-append).
    intact_end: usize,
}

/// Every intact line in file order, plus the byte offset just past the last
/// one; anything after that offset is a malformed or unterminated tail.
fn intact_lines(raw: &[u8]) -> io::Result<(Vec<RolloutLine>, usize)> {
    let mut lines = Vec::new();
    let mut intact_end = 0;
    for line in raw.split_inclusive(|byte| *byte == b'\n') {
        if !line.ends_with(b"\n") {
            break;
        }
        let Ok(line) = std::str::from_utf8(line) else {
            break;
        };
        let content = line.trim();
        if !content.is_empty() {
            let value: serde_json::Value = match serde_json::from_str(content) {
                Ok(value) => value,
                Err(_) => break,
            };
            let parsed = serde_json::from_value(value).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid complete session line: {error}"),
                )
            })?;
            lines.push(parsed);
        }
        intact_end += line.len();
    }
    Ok((lines, intact_end))
}

fn checked_seq_of(meta: &LineMeta) -> io::Result<u64> {
    meta.id
        .rsplit_once('#')
        .and_then(|(_, seq)| seq.parse().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid session line id '{}'", meta.id),
            )
        })
}

fn seq_of(meta: &LineMeta) -> u64 {
    checked_seq_of(meta).unwrap_or(0)
}

fn validate_envelope(lines: &[RolloutLine]) -> io::Result<()> {
    let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidData, message);
    let mut previous: Option<(&str, u64)> = None;
    for line in lines {
        let meta = line.meta();
        let seq = checked_seq_of(meta)?;
        if let Some((previous_id, previous_seq)) = previous {
            if previous_seq
                .checked_add(1)
                .is_none_or(|expected| seq != expected)
            {
                return Err(invalid(
                    "session line id sequences must strictly increase by one".into(),
                ));
            }
            if meta.parent.as_deref() != Some(previous_id) {
                return Err(invalid(
                    "session line parent must reference the preceding line".into(),
                ));
            }
        } else if let Some(parent) = meta.parent.as_deref() {
            let current_prefix = meta.id.rsplit_once('#').map(|(prefix, _)| prefix);
            let valid_fork_parent = parent.rsplit_once('#').is_some_and(|(prefix, seq)| {
                !prefix.is_empty()
                    && prefix != current_prefix.unwrap_or_default()
                    && seq.parse::<u64>().is_ok()
            });
            if meta.subagent_of.is_some() || !valid_fork_parent {
                return Err(invalid(
                    "the first session line parent must be a valid cross-file fork origin".into(),
                ));
            }
        }
        previous = Some((&meta.id, seq));
    }
    Ok(())
}

fn validate_provider_routes(lines: &[RolloutLine]) -> io::Result<()> {
    let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidData, message);
    let mut routes: Vec<ProviderRouteReceipt> = Vec::new();
    for line in lines {
        let (meta, receipt, source_matches_line) = match line {
            RolloutLine::ProviderRouteInitial { meta, receipt } => (
                meta,
                receipt,
                receipt.source == ProviderRouteSource::Initial,
            ),
            // Both change kinds ride the same line; which one it was lives in
            // the receipt's own `source`, and `validate_timeline` decides
            // whether that value is legal at this revision.
            RolloutLine::ProviderRouteChanged { meta, receipt } => (
                meta,
                receipt,
                receipt.source != ProviderRouteSource::Initial,
            ),
            _ => continue,
        };
        let boundary = checked_seq_of(meta)?;
        if receipt.route_boundary != boundary || !source_matches_line {
            return Err(invalid(
                "provider route receipt boundary/source does not match its line".into(),
            ));
        }
        routes.push(receipt.clone());
    }
    validate_timeline(&routes).map_err(|_| invalid("provider route timeline is invalid".into()))?;

    let first_message = lines.iter().find_map(|line| match line {
        RolloutLine::Message { meta, .. } => Some(seq_of(meta)),
        _ => None,
    });
    if first_message.is_some_and(|boundary| routes[0].route_boundary >= boundary) {
        return Err(invalid(
            "initial provider route must precede the first history message".into(),
        ));
    }

    for line in lines {
        if let RolloutLine::ProviderUsage { meta, record } = line {
            let boundary = checked_seq_of(meta)?;
            let route = routes
                .iter()
                .rev()
                .find(|route| route.route_boundary < boundary)
                .ok_or_else(|| invalid("provider usage precedes the initial route".into()))?;
            let model_matches = record.model == route.model;
            if record.route_revision != route.route_revision
                || record.provider_id != route.provider_id
                || record.api_family != route.api_family
                || !model_matches
            {
                return Err(invalid(
                    "provider usage identity does not match the active route".into(),
                ));
            }
        }
    }

    let mut original_origins = HashSet::new();
    for line in lines {
        if let RolloutLine::Message { meta, message } = line {
            let boundary = checked_seq_of(meta)?;
            validate_provider_message(message, boundary, &routes, true)?;
            if let Some(source) = &message.provider_provenance {
                original_origins.insert((source.route_boundary, source.route_revision));
            }
        }
    }
    for line in lines {
        let replacement = match line {
            RolloutLine::Compacted { replacement, .. }
            | RolloutLine::Repaired { replacement, .. } => replacement,
            _ => continue,
        };
        for message in replacement {
            validate_provider_message(message, 0, &routes, false)?;
            if let Some(source) = &message.provider_provenance
                && !original_origins.contains(&(source.route_boundary, source.route_revision))
            {
                return Err(invalid(
                    "replacement history contains provider provenance with no original message"
                        .into(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_provider_message(
    message: &Message,
    line_boundary: u64,
    routes: &[ProviderRouteReceipt],
    original_line: bool,
) -> io::Result<()> {
    let invalid = |message: &'static str| io::Error::new(io::ErrorKind::InvalidData, message);
    let Some(source) = message.provider_provenance.as_ref() else {
        return if message.has_reasoning() {
            Err(invalid(
                "reasoning history is missing provider route provenance",
            ))
        } else {
            Ok(())
        };
    };
    if message.role != Role::Assistant {
        return Err(invalid(
            "only provider assistant messages may carry provider provenance",
        ));
    }
    validate_provenance(
        source,
        routes,
        ReasoningShape::of(message),
        original_line.then_some(line_boundary),
    )
    .map(|_| ())
    .map_err(|mismatch| {
        invalid(match mismatch {
            ProvenanceMismatch::UnknownRevision => {
                "provider provenance references an unknown route revision"
            }
            ProvenanceMismatch::OriginOutsideInterval => {
                "provider provenance origin lies outside its route interval"
            }
            ProvenanceMismatch::OriginNotOnItsLine => {
                "provider provenance origin does not match its message line boundary"
            }
            ProvenanceMismatch::IdentityMismatch => {
                "provider provenance identity does not match its producing route"
            }
            ProvenanceMismatch::BlockShapeMismatch => {
                "provider reasoning block shape does not match its producing API family"
            }
        })
    })
}

fn parse_session(raw: &[u8]) -> io::Result<ParsedSession> {
    let (lines, intact_end) = intact_lines(raw)?;
    validate_provider_routes(&lines)?;
    validate_envelope(&lines)?;
    let mut parsed = ParsedSession {
        items: Vec::new(),
        provider_usage: UsageLedger::default(),
        runtime: None,
        terminals: Vec::new(),
        route_timeline: Vec::new(),
        last_id: None,
        max_seq: 0,
        intact_end,
    };
    for line in lines {
        let meta = match line {
            RolloutLine::Session { meta, runtime } => {
                parsed.runtime = Some(runtime);
                meta
            }
            RolloutLine::ProviderRouteInitial { meta, receipt } => {
                parsed.route_timeline.push(receipt);
                meta
            }
            RolloutLine::ProviderRouteChanged { meta, receipt } => {
                parsed.route_timeline.push(receipt);
                meta
            }
            RolloutLine::Message { meta, message } => {
                parsed.items.push(message);
                meta
            }
            RolloutLine::ProviderUsage { meta, record } => {
                parsed.provider_usage.push(record);
                meta
            }
            RolloutLine::Compacted { meta, replacement } => {
                parsed.items = replacement;
                parsed.terminals.clear();
                meta
            }
            RolloutLine::TurnTerminal { meta, terminal } => {
                parsed.terminals.push(SnapshotTerminal {
                    after_message: parsed.items.len(),
                    status: terminal.status,
                    error: terminal.error,
                });
                meta
            }
            RolloutLine::Repaired {
                meta,
                kind: _,
                replacement,
                terminals,
                stats: _,
            } => {
                parsed.items = replacement;
                parsed.terminals = terminals;
                meta
            }
        };
        let seq = checked_seq_of(&meta)?;
        parsed.max_seq = parsed.max_seq.max(seq);
        parsed.last_id = Some(meta.id);
    }
    Ok(parsed)
}

/// The result of a single, strictly read-only session inspection. The same
/// result is consumed by list/read/seed callers or by an explicit recovery.
pub struct SessionRead {
    snapshot: SessionSnapshot,
    provider_usage: UsageLedger,
    repair: PairingRepair,
    path: PathBuf,
    raw_len: usize,
    intact_end: usize,
    last_id: Option<String>,
    max_seq: u64,
}

impl SessionRead {
    pub fn snapshot(&self) -> SessionSnapshot {
        self.snapshot.clone()
    }

    pub fn provider_usage(&self) -> &UsageLedger {
        &self.provider_usage
    }

    pub fn repair_stats(&self) -> &PairingRepairStats {
        &self.repair.stats
    }

    /// Perform the only on-disk recovery operation: truncate an intact prefix
    /// and, when necessary, append one canonical pairing marker.
    pub fn recover(self) -> io::Result<ResumedSession> {
        let next_seq = self.max_seq.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "session sequence exhausted")
        })?;
        if next_seq == u64::MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session sequence exhausted",
            ));
        }
        let mut rollout = Rollout {
            path: self.path.clone(),
            prefix: id_prefix(&self.path),
            next_seq,
            last_id: self.last_id,
            subagent_of: None,
            route_timeline: self.snapshot.provider_routes.clone(),
            // A resumed session's file is on disk and belongs to whoever wrote
            // it; this writer only continues it.
            preexisting: true,
            wrote_content: true,
        };
        if self.intact_end < self.raw_len {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&self.path)?
                .set_len(self.intact_end as u64)?;
        }
        if self.repair.changed {
            rollout.append_repaired(&self.repair)?;
        }
        Ok(ResumedSession {
            messages: self.snapshot.messages.clone(),
            provider_usage: self.provider_usage,
            snapshot: self.snapshot,
            repair: self.repair.stats,
            rollout,
        })
    }
}

pub fn inspect_session(path: &Path) -> io::Result<SessionRead> {
    let raw = std::fs::read(path)?;
    let parsed = parse_session(&raw)?;
    let repair = repair_pairing(parsed.items, parsed.terminals);
    let snapshot = SessionSnapshot {
        messages: repair.messages.clone(),
        runtime: parsed.runtime,
        terminals: repair.terminals.clone(),
        provider_routes: parsed.route_timeline,
    };
    Ok(SessionRead {
        snapshot,
        provider_usage: parsed.provider_usage,
        repair,
        path: path.to_path_buf(),
        raw_len: raw.len(),
        intact_end: parsed.intact_end,
        last_id: parsed.last_id,
        max_seq: parsed.max_seq,
    })
}

/// Read-only replay (used by `--list-sessions`): a malformed tail is ignored,
/// and orphaned tool pairing is repaired only in the returned history.
pub fn load_session(path: &Path) -> io::Result<Vec<Message>> {
    Ok(inspect_session(path)?.snapshot.messages)
}

/// Read the persisted session for a history client. Runtime and terminal
/// records are display/recovery metadata only.
pub fn load_session_snapshot(path: &Path) -> io::Result<SessionSnapshot> {
    Ok(inspect_session(path)?.snapshot)
}

pub struct ResumedSession {
    pub messages: Vec<Message>,
    pub provider_usage: UsageLedger,
    pub snapshot: SessionSnapshot,
    pub repair: PairingRepairStats,
    pub rollout: Rollout,
}

/// Open a session for continuation. Unlike the read-only load functions this
/// explicitly truncates torn tail bytes and persists canonical pairing repair.
pub fn resume_session(path: &Path) -> io::Result<ResumedSession> {
    inspect_session(path)?.recover()
}

/// Fork a session: copy lines `#1..=#{cut}` of `src` into a brand-new
/// session file whose first line's `parent` points across files at
/// `{src stem}#{cut}`. The pointer is lineage metadata only — the prefix is
/// physically copied (re-enveloped under the new stem, timestamps
/// preserved), so replay stays single-file and [`resume_session`] works on
/// a fork unchanged. The source file is never touched.
///
/// A cut is legal when the kept prefix ends a complete exchange: the next
/// line — if any — must open a fresh user turn (a user message with no
/// tool_result blocks). This whitelist (cc's /rewind rule) makes splitting
/// a tool_use/tool_result pair impossible by construction. A cut just
/// before a compacted marker is therefore illegal, but a cut at any legal
/// point BEFORE one forks the raw pre-compaction history — the lines are
/// still in the file. `None` forks at the end.
pub fn fork_session(src: &Path, cut: Option<u64>, sessions_dir: &Path) -> io::Result<PathBuf> {
    let illegal = |msg: String| io::Error::new(io::ErrorKind::InvalidInput, msg);
    let raw = std::fs::read(src)?;
    let (lines, _) = intact_lines(&raw)?;
    let legal = legal_cut_seqs(&lines);
    let Some(&last) = legal.last() else {
        return Err(illegal("session has no lines to fork".into()));
    };
    let cut = cut.unwrap_or(last);
    if !legal.contains(&cut) {
        let mut near = legal;
        near.sort_by_key(|s| s.abs_diff(cut));
        near.truncate(8);
        near.sort_unstable();
        let near: Vec<String> = near.iter().map(|s| format!("#{s}")).collect();
        return Err(illegal(format!(
            "cannot fork at #{cut}: the kept prefix must end a complete exchange \
             (the next line must start a user turn); legal points near it: {} (end = #{last})",
            near.join(", ")
        )));
    }

    let id = new_session_id(sessions_dir);
    let path = session_path(sessions_dir, &id);
    let prefix = id_prefix(&path);
    let mut parent = Some(format!("{}#{cut}", id_prefix(src)));
    let mut out = String::new();
    for (n, line) in lines
        .into_iter()
        .take_while(|line| seq_of(line.meta()) <= cut)
        .enumerate()
    {
        let mut remeta = |meta: LineMeta| {
            let new_id = format!("{prefix}#{}", n as u64 + 1);
            LineMeta {
                id: new_id.clone(),
                parent: parent.replace(new_id),
                // A fork's lineage is its cross-file `parent`; it is an
                // independent branch, never a sub-agent, so drop any
                // subagent_of the copied source line may have carried.
                subagent_of: None,
                ts: meta.ts,
            }
        };
        let line = match line {
            RolloutLine::Session { meta, runtime } => RolloutLine::Session {
                meta: remeta(meta),
                runtime,
            },
            RolloutLine::ProviderRouteInitial { meta, receipt } => {
                RolloutLine::ProviderRouteInitial {
                    meta: remeta(meta),
                    receipt,
                }
            }
            RolloutLine::ProviderRouteChanged { meta, receipt } => {
                RolloutLine::ProviderRouteChanged {
                    meta: remeta(meta),
                    receipt,
                }
            }
            RolloutLine::Message { meta, message } => RolloutLine::Message {
                meta: remeta(meta),
                message,
            },
            RolloutLine::ProviderUsage { meta, record } => RolloutLine::ProviderUsage {
                meta: remeta(meta),
                record,
            },
            RolloutLine::Compacted { meta, replacement } => RolloutLine::Compacted {
                meta: remeta(meta),
                replacement,
            },
            RolloutLine::TurnTerminal { meta, terminal } => RolloutLine::TurnTerminal {
                meta: remeta(meta),
                terminal,
            },
            RolloutLine::Repaired {
                meta,
                kind,
                replacement,
                terminals,
                stats,
            } => RolloutLine::Repaired {
                meta: remeta(meta),
                kind,
                replacement,
                terminals,
                stats,
            },
        };
        out.push_str(&serde_json::to_string(&line).map_err(io::Error::other)?);
        out.push('\n');
    }
    std::fs::create_dir_all(sessions_dir)?;
    // create_new: new_session_id picked an unused id; clobbering an existing
    // session here would destroy history, so a collision must be an error.
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)?;
    file.write_all(out.as_bytes())?;
    Ok(path)
}

/// A line that begins a fresh user turn: a plain user message (a tool_result
/// carrier is the tail of the previous turn, not a new one). This is the cut
/// boundary both `legal_cut_seqs` and `fork_points` key off, so a fork point
/// the picker offers is always one `fork_session` will accept.
fn opens_user_turn(line: &RolloutLine) -> bool {
    match line {
        RolloutLine::Message { message, .. } => {
            message.role == Role::User
                && !message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        }
        RolloutLine::Session { .. }
        | RolloutLine::ProviderRouteInitial { .. }
        | RolloutLine::ProviderRouteChanged { .. }
        | RolloutLine::ProviderUsage { .. }
        | RolloutLine::Compacted { .. }
        | RolloutLine::TurnTerminal { .. }
        | RolloutLine::Repaired { .. } => false,
    }
}

/// Every legal cut in file order as `(index, seq)`: a line whose successor
/// opens a fresh user turn, plus the tip. The one place the boundary rule
/// lives — `legal_cut_seqs` keeps just the seqs, `fork_points` also needs the
/// index so it can read the turn that cutting there would drop.
fn cut_points(lines: &[RolloutLine]) -> Vec<(usize, u64)> {
    let mut seen_user_turn = false;
    let mut cuts = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if opens_user_turn(line) {
            seen_user_turn = true;
        }
        let boundary = lines.get(index + 1).is_none_or(opens_user_turn);
        if seen_user_turn && boundary {
            cuts.push((index, seq_of(line.meta())));
        }
    }
    cuts
}

/// Seqs after which the file may be cut: every line whose successor starts
/// a fresh user turn, plus the last line.
fn legal_cut_seqs(lines: &[RolloutLine]) -> Vec<u64> {
    cut_points(lines).into_iter().map(|(_, seq)| seq).collect()
}

/// A turn boundary a live session can rewind to: the cut `seq` plus a preview
/// of the user message that opens the turn dropped by cutting there — what the
/// TUI shows as "rewind to before this".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkPoint {
    pub seq: u64,
    pub preview: String,
}

/// The rewind targets of a live session, oldest first. Each is a legal cut whose
/// next line starts a user turn, paired with that turn's opening message — so the
/// caller picks by content, never by raw seq. Excludes the tip (rewinding to the
/// current end is a no-op) and the first turn (nothing precedes it). The seqs are
/// exactly the non-tip entries of `legal_cut_seqs`, so `fork_session` accepts any
/// of them. Reads the whole file.
pub fn fork_points(path: &Path) -> io::Result<Vec<ForkPoint>> {
    let raw = std::fs::read(path)?;
    let (lines, _) = intact_lines(&raw)?;
    Ok(cut_points(&lines)
        .into_iter()
        .filter_map(|(index, seq)| {
            // The tip has no successor: skip it (a no-op rewind). Anything left
            // opens a user turn, so the message pattern always matches.
            let RolloutLine::Message { message, .. } = lines.get(index + 1)? else {
                return None;
            };
            Some(ForkPoint {
                seq,
                preview: user_turn_preview(message),
            })
        })
        .collect())
}

/// A one-line gist of a user turn's opening message for the rewind picker: the
/// first text block, with steering framing stripped (a steer is `PREFIX\ntext`)
/// and whitespace collapsed. Empty if the message carries no text.
fn user_turn_preview(message: &Message) -> String {
    let text = message
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or("");
    let text = text
        .strip_prefix(STEERING_PREFIX)
        .map(str::trim_start)
        .unwrap_or(text);
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Where a session was forked from: the first line's cross-file parent
/// (`{src stem}#{seq}`), or None for a session started fresh (its first
/// line has no parent). Reads only the first line.
pub fn fork_origin(path: &Path) -> Option<String> {
    read_first_meta(path).and_then(|meta| meta.parent)
}

/// How a session file relates to another, read from its first line: forked
/// from a cut point of another session, or spawned as a sub-agent of a parent
/// turn. None for a session started fresh at the top level.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionOrigin {
    /// A fork's cross-file `parent` (`{src stem}#{cut}`).
    Fork(String),
    /// A sub-agent's `subagent_of` (`{parent stem}#{spawning line seq}`).
    SubAgent(String),
}

/// The first line's lineage. `subagent_of` wins over `parent`: a sub-agent's
/// first line never has a cross-file `parent` (its chain starts fresh), so the
/// two are mutually exclusive in practice, but the precedence keeps the label
/// unambiguous. Reads only the first line.
pub fn session_origin(path: &Path) -> Option<SessionOrigin> {
    origin_of(&read_first_meta(path)?)
}

fn origin_of(meta: &LineMeta) -> Option<SessionOrigin> {
    match (&meta.subagent_of, &meta.parent) {
        (Some(sa), _) => Some(SessionOrigin::SubAgent(sa.clone())),
        (None, Some(parent)) => Some(SessionOrigin::Fork(parent.clone())),
        (None, None) => None,
    }
}

/// A sub-agent's session is kept out of the default resume picker (like cc's
/// sidechain files and codex's subagent-source filter) — it is reachable
/// only by explicit id. `--list-sessions` still shows it, labelled.
pub fn is_subagent_session(path: &Path) -> bool {
    matches!(session_origin(path), Some(SessionOrigin::SubAgent(_)))
}

fn read_first_meta(path: &Path) -> Option<LineMeta> {
    use std::io::BufRead as _;
    let file = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).ok()?;
    let line: RolloutLine = serde_json::from_str(first.trim()).ok()?;
    Some(line.into_meta())
}

/// Everything a session list shows for one file, read without replaying it.
/// `inspect_session` reads and JSON-parses every byte; a picker facing dozens
/// of megabyte-sized transcripts would pay that for all of them before drawing
/// its first frame, so this stops at the first user text instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDigest {
    pub id: String,
    /// First user text, truncated. Empty when the session has none yet.
    pub title: String,
    pub origin: Option<SessionOrigin>,
    /// Last write = when the session was last active, the order a list wants.
    pub modified: SystemTime,
    pub bytes: u64,
    /// The file holds a conversation, not just the opening preamble.
    pub has_content: bool,
}

pub fn session_digest(path: &Path) -> io::Result<SessionDigest> {
    use std::io::BufRead as _;
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    let mut digest = SessionDigest {
        id: session_id_of(path),
        title: String::new(),
        origin: None,
        modified: metadata.modified()?,
        bytes: metadata.len(),
        has_content: false,
    };
    let mut reader = std::io::BufReader::new(file);
    let mut raw = String::new();
    let mut at_first_line = true;
    loop {
        raw.clear();
        if reader.read_line(&mut raw)? == 0 {
            break;
        }
        // Lineage rides the first line only, and an unparseable line is skipped
        // rather than fatal — `parse_session` tolerates a torn tail the same
        // way, and a damaged session must still be listable.
        let first = std::mem::take(&mut at_first_line);
        let Ok(line) = serde_json::from_str::<RolloutLine>(raw.trim()) else {
            continue;
        };
        if first {
            digest.origin = origin_of(line.meta());
        }
        match &line {
            RolloutLine::Message { message, .. } => {
                digest.has_content = true;
                if digest.title.is_empty() {
                    // Wide enough for any terminal the picker will draw in; it
                    // truncates to the real width itself.
                    digest.title = user_snippet(std::slice::from_ref(message), 200);
                }
            }
            RolloutLine::Compacted { .. } | RolloutLine::Repaired { .. } => {
                digest.has_content = true;
            }
            _ => {}
        }
        if digest.has_content && !digest.title.is_empty() {
            break;
        }
    }
    Ok(digest)
}

/// Make the replayed history legal to send. Both directions, mirroring what
/// the live loop guarantees (cc's ensureToolResultPairing is also two-way):
/// a tool_result must answer a tool_use in the immediately preceding
/// assistant message — strays are dropped (a message stripped empty goes
/// entirely) — and every tool_use left unanswered gets the same is_error
/// result the interrupt path uses.
fn repair_pairing(items: Vec<Message>, terminals: Vec<SnapshotTerminal>) -> PairingRepair {
    #[derive(Debug)]
    struct Entry {
        message: Message,
        origin: Option<usize>,
        inserted_after: Option<usize>,
    }

    let messages_before = items.len();
    // Reverse: drop tool_results that answer nothing while retaining the
    // original message index for terminal boundary remapping.
    let mut repaired: Vec<Entry> = Vec::with_capacity(items.len());
    let mut dropped_tool_results = 0;
    let mut duplicate_tool_results = 0;
    let mut dropped_messages = 0;
    for (index, mut msg) in items.into_iter().enumerate() {
        let prev_uses = repaired
            .last()
            .map(|entry| tool_use_ids(&entry.message))
            .unwrap_or_default();
        let mut answered_ids = HashSet::new();
        let before_blocks = msg.content.len();
        msg.content.retain(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => {
                if prev_uses.contains(tool_use_id) {
                    if answered_ids.insert(tool_use_id.clone()) {
                        true
                    } else {
                        duplicate_tool_results += 1;
                        false
                    }
                } else {
                    false
                }
            }
            _ => true,
        });
        dropped_tool_results += before_blocks - msg.content.len();
        if !msg.content.is_empty() {
            repaired.push(Entry {
                message: msg,
                origin: Some(index),
                inserted_after: None,
            });
        } else {
            dropped_messages += 1;
        }
    }

    // Forward: patch tool_uses left unanswered.
    let mut inserted_tool_results = 0;
    let mut i = 0;
    while i < repaired.len() {
        let uses = tool_use_ids(&repaired[i].message);
        if !uses.is_empty() {
            let answered: HashSet<String> = repaired
                .get(i + 1)
                .map(|next| {
                    next.message
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::ToolResult { tool_use_id, .. } => {
                                Some(tool_use_id.clone())
                            }
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            let missing: Vec<ContentBlock> = uses
                .iter()
                .filter(|id| !answered.contains(*id))
                .map(|id| interrupted(id))
                .collect();
            inserted_tool_results += missing.len();
            if !missing.is_empty() {
                let origin = repaired[i].origin;
                if answered.is_empty() {
                    repaired.insert(
                        i + 1,
                        Entry {
                            message: Message::tool_results(missing),
                            origin: None,
                            inserted_after: origin,
                        },
                    );
                } else {
                    // A partially written results message: complete it in
                    // place rather than splitting results across two messages.
                    repaired[i + 1].message.content.extend(missing);
                }
            }
        }
        i += 1;
    }

    let messages: Vec<Message> = repaired.iter().map(|entry| entry.message.clone()).collect();
    let original_terminals = terminals.clone();
    let mut canonical_terminals = Vec::with_capacity(terminals.len());
    for terminal in terminals {
        let boundary = terminal.after_message;
        let after_message = repaired
            .iter()
            .filter(|entry| {
                entry.origin.is_some_and(|origin| origin < boundary)
                    || entry.inserted_after.is_some_and(|origin| origin < boundary)
            })
            .count();
        canonical_terminals.push(SnapshotTerminal {
            after_message,
            ..terminal
        });
    }
    let changed_messages = repaired
        .iter()
        .filter(|entry| entry.origin.is_none())
        .count()
        + dropped_messages;
    let stats = PairingRepairStats {
        messages_before,
        messages_after: messages.len(),
        dropped_tool_results,
        duplicate_tool_results,
        dropped_messages,
        inserted_tool_results,
        changed_messages,
    };
    let changed = dropped_tool_results != 0
        || dropped_messages != 0
        || inserted_tool_results != 0
        || canonical_terminals != original_terminals;
    PairingRepair {
        messages,
        terminals: canonical_terminals,
        stats,
        changed,
    }
}

fn tool_use_ids(message: &Message) -> HashSet<String> {
    if message.role != Role::Assistant {
        return HashSet::new();
    }
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

// ── sessions directory helpers ──────────────────────────────────────────
// Shared by every frontend that manages sessions (cli picker, server
// thread/start|resume|list), so they live next to the file format.

/// Session ids are UTC wall-clock timestamps — readable, sortable, and free
/// of a rand dependency. A collision within one second gets a numeric suffix.
pub fn new_session_id(sessions_dir: &Path) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let base = timestamp_id(secs);
    let mut id = base.clone();
    let mut n = 2;
    while session_path(sessions_dir, &id).exists() {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

pub(crate) fn timestamp_id(unix_secs: u64) -> String {
    let (y, m, d) = civil_from_days((unix_secs / 86_400) as i64);
    let rem = unix_secs % 86_400;
    format!(
        "{y:04}{m:02}{d:02}-{h:02}{min:02}{s:02}",
        h = rem / 3600,
        min = rem % 3600 / 60,
        s = rem % 60
    )
}

/// Days since 1970-01-01 to a UTC civil date (Howard Hinnant's
/// civil_from_days), so session ids don't need a chrono dependency.
/// Also feeds the environment block's date (context::utc_date).
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

pub fn session_path(sessions_dir: &Path, id: &str) -> PathBuf {
    sessions_dir.join(format!("{id}.jsonl"))
}

/// The character set a session id may use. Every id kloop mints already fits —
/// CLI and server stems are `YYYYMMDD-HHMMSS[-N]`, sub-agent transcripts append
/// `-agent-N` — so this rejects nothing kloop produces.
///
/// The rule it replaced asked only about path traversal (no `/`, no `..`, no
/// control characters), which left non-ASCII ids accepted. That was never
/// reachable into a wrong session — the ids kloop mints are all ASCII, and a
/// pure-ASCII name has no non-ASCII Unicode-canonical equivalent, so a
/// non-ASCII id could not collide with one on a normalizing filesystem — but
/// "no caller happens to produce one" is a weaker guarantee than "none is
/// accepted", and it was the only guarantee downstream had. Consumers now get
/// an id that is safe as a filename *and* as an HTTP header value, a log field,
/// and a wire token, instead of each re-deriving that for itself (the gateway
/// session header in `provider/src/anthropic.rs` had to).
///
/// The leading-dot rule subsumes `.` and `..`; the charset subsumes separators
/// and control characters, so the old path-component check is implied.
fn session_id_is_safe(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

pub fn checked_session_path(sessions_dir: &Path, id: &str) -> io::Result<PathBuf> {
    if !session_id_is_safe(id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session id must be ASCII letters, digits, '.', '-' or '_', and cannot start with '.'",
        ));
    }
    let path = session_path(sessions_dir, id);
    if let Ok(metadata) = std::fs::symlink_metadata(&path)
        && (!metadata.file_type().is_file() || metadata.file_type().is_symlink())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session path is not a regular file",
        ));
    }
    Ok(path)
}

/// All session files, most recently modified first (modified = last active,
/// which is what "continue the latest" should pick up).
pub fn sessions_by_recency(sessions_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(sessions_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|file_type| file_type.is_file()))
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    files.into_iter().map(|(_, path)| path).collect()
}

pub fn session_id_of(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session")
        .to_string()
}

/// First user text in a session, truncated — the human-readable label for
/// session lists.
pub fn first_user_snippet(messages: &[Message]) -> String {
    user_snippet(messages, 60)
}

/// The cap is the caller's: a one-line stdout listing wants a short label,
/// while the picker truncates to the terminal's width and would rather have
/// the room.
fn user_snippet(messages: &[Message], max_chars: usize) -> String {
    for message in messages {
        if message.role != Role::User {
            continue;
        }
        for block in &message.content {
            if let ContentBlock::Text { text } = block {
                let mut snippet: String = text
                    .chars()
                    .take(max_chars)
                    .map(|c| if c == '\n' { ' ' } else { c })
                    .collect();
                if text.chars().count() > max_chars {
                    snippet.push('…');
                }
                return snippet;
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn timestamp_ids_match_utc_civil_time() {
        assert_eq!(timestamp_id(0), "19700101-000000");
        // date -u -r 1783958400 → 2026-07-13 16:00:00 UTC
        assert_eq!(timestamp_id(1_783_958_400), "20260713-160000");
        // leap-year day: 2024-02-29 12:34:56 UTC
        assert_eq!(timestamp_id(1_709_209_496), "20240229-122456");
    }
    use serde_json::Value;

    fn temp_file(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("kloop-rollout-{}-{tag}", std::process::id()))
            .join("session.jsonl")
    }

    fn tool_use(id: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.into(),
            name: "bash".into(),
            input: json!({"command": "ls"}),
        }
    }

    fn tool_result(id: &str) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: "ok".into(),
            is_error: false,
        }
    }

    fn usage_record(_model: &str, input_tokens: u64) -> ProviderUsageRecord {
        ProviderUsageRecord {
            provider_id: "test".into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            route_revision: 1,
            model: "mock".into(),
            operation: crate::usage::UsageOperation::Sampling,
            usage: kloop_protocol::Usage {
                input_tokens,
                output_tokens: 2,
                cache_read_input_tokens: 3,
                cache_creation_input_tokens: 4,
            },
        }
    }

    fn fixture_route(route_boundary: u64) -> ProviderRouteReceipt {
        ProviderRouteReceipt {
            route_revision: 1,
            route_boundary,
            source: ProviderRouteSource::Initial,
            provider_id: "test".into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            endpoint_fingerprint: kloop_provider::Provider::endpoint_fingerprint_for(
                kloop_protocol::ProviderApiFamily::Mock,
                "mock",
            ),
            model: "mock".into(),
            effort: None,
            continuity: ReasoningContinuity::Preserved,
        }
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    fn raw_lines(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn roundtrip_appended_messages() {
        let path = temp_file("roundtrip");
        let mut rollout = Rollout::new(path.clone());
        let messages = vec![
            Message::user_text("hello"),
            // Thinking must round-trip byte-exact (the signature is a replay
            // credential), empty text included.
            Message::assistant_from_provider(
                vec![
                    ContentBlock::Thinking {
                        thinking: String::new(),
                        signature: "sig".into(),
                    },
                    ContentBlock::RedactedThinking { data: "d".into() },
                    tool_use("t1"),
                ],
                kloop_protocol::ProviderResponseProvenance {
                    route_revision: 1,
                    route_boundary: 3,
                    provider_id: "test".into(),
                    api_family: kloop_protocol::ProviderApiFamily::Mock,
                    endpoint_fingerprint: kloop_provider::Provider::endpoint_fingerprint_for(
                        kloop_protocol::ProviderApiFamily::Mock,
                        "mock",
                    ),
                    model: "mock".into(),
                },
            ),
            Message::tool_results(vec![tool_result("t1")]),
            Message::assistant(vec![ContentBlock::Text {
                text: "done".into(),
            }]),
        ];
        for m in &messages {
            rollout.append_message(m).unwrap();
        }
        assert_eq!(load_session(&path).unwrap(), messages);
        cleanup(&path);
    }

    #[test]
    fn provider_usage_roundtrips_without_entering_message_snapshot() {
        let path = temp_file("provider-usage");
        let mut rollout = Rollout::new(path.clone());
        rollout.append_message(&Message::user_text("one")).unwrap();
        rollout
            .append_provider_usage(&usage_record("actual-model", 120))
            .unwrap();
        rollout
            .append_message(&Message::assistant(vec![ContentBlock::Text {
                text: "done".into(),
            }]))
            .unwrap();

        let lines = raw_lines(&path);
        assert_eq!(
            lines[2],
            json!({
                "type": "provider_usage",
                "id": "session#3",
                "parent": "session#2",
                "ts": lines[2]["ts"],
                "provider_id": "test",
                "api_family": "mock",
                "route_revision": 1,
                "model": "mock",
                "operation": "sampling",
                "usage": {
                    "input_tokens": 120,
                    "output_tokens": 2,
                    "cache_read_input_tokens": 3,
                    "cache_creation_input_tokens": 4,
                },
            })
        );
        let resumed = resume_session(&path).unwrap();
        assert_eq!(
            resumed.provider_usage.records(),
            &[usage_record("actual-model", 120)]
        );
        assert_eq!(
            resumed.messages,
            vec![
                Message::user_text("one"),
                Message::assistant(vec![ContentBlock::Text {
                    text: "done".into(),
                }]),
            ]
        );
        let snapshot = load_session_snapshot(&path).unwrap();
        assert_eq!(snapshot.messages, resumed.messages);
        assert_eq!(
            serde_json::to_value(snapshot).unwrap(),
            json!({
                "messages": resumed.messages,
                "runtime": null,
                "terminals": [],
            })
        );
        cleanup(&path);
    }

    #[test]
    fn envelope_forms_a_sequential_chain() {
        let path = temp_file("envelope");
        let mut rollout = Rollout::new(path.clone());
        rollout.append_message(&Message::user_text("one")).unwrap();
        rollout.append_message(&Message::user_text("two")).unwrap();
        rollout
            .append_compacted(&[Message::user_text("[summary]")])
            .unwrap();

        let lines = raw_lines(&path);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0]["id"], "session#1");
        assert_eq!(lines[0].get("parent"), None, "first line has no parent");
        assert_eq!(lines[0]["type"], "provider_route_initial");
        assert_eq!(lines[1]["id"], "session#2");
        assert_eq!(lines[1]["parent"], "session#1");
        assert_eq!(lines[2]["id"], "session#3");
        assert_eq!(lines[2]["parent"], "session#2");
        assert_eq!(lines[3]["id"], "session#4");
        assert_eq!(lines[3]["parent"], "session#3");
        assert_eq!(lines[3]["type"], "compacted");
        for line in &lines {
            assert!(line["ts"].as_u64().unwrap() > 0, "ts must be stamped");
        }
        cleanup(&path);
    }

    #[test]
    fn resume_continues_the_id_chain() {
        let path = temp_file("chain");
        let mut rollout = Rollout::new(path.clone());
        rollout.append_message(&Message::user_text("one")).unwrap();
        rollout.append_message(&Message::user_text("two")).unwrap();
        drop(rollout);

        let mut resumed = resume_session(&path).unwrap();
        assert_eq!(resumed.messages.len(), 2);
        resumed
            .rollout
            .append_message(&Message::user_text("three"))
            .unwrap();

        let lines = raw_lines(&path);
        assert_eq!(lines[2]["id"], "session#3", "seq continues, no collision");
        assert_eq!(lines[2]["parent"], "session#2", "chain links across runs");
        cleanup(&path);
    }

    #[test]
    fn unknown_fields_are_ignored_forward_compat() {
        let path = temp_file("compat");
        let mut rollout = Rollout::new(path.clone());
        rollout.append_message(&Message::user_text("old")).unwrap();
        // A line written by a future version with an extra field.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            r#"{{"type":"message","id":"session#3","parent":"session#2","ts":1,"future_field":42,"role":"user","content":[{{"type":"text","text":"new"}}]}}"#
        )
        .unwrap();

        assert_eq!(
            load_session(&path).unwrap(),
            vec![Message::user_text("old"), Message::user_text("new")]
        );
        cleanup(&path);
    }

    #[test]
    fn compacted_marker_preserves_provider_usage_ledger() {
        let path = temp_file("compacted-usage");
        let mut rollout = Rollout::new(path.clone());
        rollout
            .append_provider_usage(&usage_record("before", 10))
            .unwrap();
        rollout
            .append_compacted(&[Message::user_text("[summary]")])
            .unwrap();
        rollout
            .append_provider_usage(&usage_record("after", 20))
            .unwrap();

        let resumed = resume_session(&path).unwrap();
        assert_eq!(resumed.messages, vec![Message::user_text("[summary]")]);
        assert_eq!(
            resumed.provider_usage.records(),
            &[usage_record("before", 10), usage_record("after", 20)]
        );
        cleanup(&path);
    }

    #[test]
    fn compacted_marker_replaces_prior_lines_on_replay() {
        let path = temp_file("compacted");
        let mut rollout = Rollout::new(path.clone());
        rollout
            .append_message(&Message::user_text("old 1"))
            .unwrap();
        rollout
            .append_message(&Message::user_text("old 2"))
            .unwrap();
        let replacement = vec![
            Message::user_text("[summary]"),
            Message::user_text("kept tail"),
        ];
        rollout.append_compacted(&replacement).unwrap();
        let after = Message::user_text("after compaction");
        rollout.append_message(&after).unwrap();

        let mut expected = replacement;
        expected.push(after);
        assert_eq!(load_session(&path).unwrap(), expected);
        cleanup(&path);
    }

    #[test]
    fn orphaned_tool_use_gets_interrupted_result() {
        let path = temp_file("orphan");
        let mut rollout = Rollout::new(path.clone());
        rollout
            .append_message(&Message::user_text("do it"))
            .unwrap();
        // Killed after the assistant message, before any results landed.
        rollout
            .append_message(&Message::assistant(vec![tool_use("t1")]))
            .unwrap();

        let loaded = load_session(&path).unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(
            loaded[2],
            Message::tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "interrupted".into(),
                is_error: true,
            }])
        );
        cleanup(&path);
    }

    #[test]
    fn partially_answered_tool_uses_are_completed_in_place() {
        let path = temp_file("partial");
        let mut rollout = Rollout::new(path.clone());
        rollout
            .append_message(&Message::assistant(vec![tool_use("t1"), tool_use("t2")]))
            .unwrap();
        rollout
            .append_message(&Message::tool_results(vec![tool_result("t1")]))
            .unwrap();

        let loaded = load_session(&path).unwrap();
        assert_eq!(loaded.len(), 2, "no extra message inserted");
        assert_eq!(
            loaded[1],
            Message::tool_results(vec![
                tool_result("t1"),
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "interrupted".into(),
                    is_error: true,
                },
            ])
        );
        cleanup(&path);
    }

    #[test]
    fn stray_tool_result_is_stripped() {
        let path = temp_file("stray");
        let mut rollout = Rollout::new(path.clone());
        rollout
            .append_message(&Message::user_text("start"))
            .unwrap();
        // A results message answering nothing (e.g. its tool_use line was
        // lost to a torn write): the whole message must go.
        rollout
            .append_message(&Message::tool_results(vec![tool_result("ghost")]))
            .unwrap();
        rollout
            .append_message(&Message::assistant(vec![ContentBlock::Text {
                text: "hi".into(),
            }]))
            .unwrap();

        assert_eq!(
            load_session(&path).unwrap(),
            vec![
                Message::user_text("start"),
                Message::assistant(vec![ContentBlock::Text { text: "hi".into() }]),
            ]
        );
        cleanup(&path);
    }

    #[test]
    fn mixed_stray_and_valid_results_repair_both_ways() {
        let path = temp_file("mixed");
        let mut rollout = Rollout::new(path.clone());
        rollout
            .append_message(&Message::assistant(vec![tool_use("t1"), tool_use("t2")]))
            .unwrap();
        // t1 answered, "ghost" answers nothing, t2 left unanswered.
        rollout
            .append_message(&Message::tool_results(vec![
                tool_result("t1"),
                tool_result("ghost"),
            ]))
            .unwrap();

        let loaded = load_session(&path).unwrap();
        assert_eq!(
            loaded[1],
            Message::tool_results(vec![
                tool_result("t1"),
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "interrupted".into(),
                    is_error: true,
                },
            ]),
            "ghost dropped, t2 patched, t1 kept"
        );
        cleanup(&path);
    }

    /// The full resume story over the agent loop: a persisted turn, a process
    /// restart (drop + reload), and a second turn appending to the same file.
    #[tokio::test]
    async fn agent_turn_resumes_across_a_restart() {
        use crate::agent::{EndReason, Ui, run_turn};
        use crate::history::History;
        use kloop_provider::Provider;
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;

        struct NullUi;
        impl Ui for NullUi {
            fn emit(&self, _: &crate::event::Event) {}
        }
        let cfg_with = |provider: Provider, dir: &Path| {
            crate::tools::testutil::TestConfig::new("rollout-restart")
                .provider(provider)
                .dirs(dir)
                .build()
        };
        let path = temp_file("restart");
        let dir = path.parent().unwrap().to_path_buf();
        let ui: Arc<dyn Ui> = Arc::new(NullUi);

        // First run: one completed turn, then the process "exits" (drop).
        let cfg = cfg_with(
            Provider::mock(vec![vec![kloop_protocol::AssistantBlock::Text {
                text: "noted: the magic word is kumquat".into(),
            }]]),
            &dir,
        );
        let mut history = History::new(dir.clone());
        history.attach_rollout(Rollout::new(path.clone()));
        history.record(Message::user_text("remember the magic word: kumquat"));
        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
        assert_eq!(outcome.reason, EndReason::Completed);
        let before_restart = history.messages().to_vec();
        drop(history);

        // Second run: resume from disk and continue the conversation.
        let resumed = resume_session(&path).unwrap();
        assert_eq!(resumed.messages, before_restart);
        let cfg = cfg_with(
            Provider::mock(vec![vec![kloop_protocol::AssistantBlock::Text {
                text: "it was kumquat".into(),
            }]]),
            &dir,
        );
        let mut history = History::resume(dir.clone(), resumed);
        history.record(Message::user_text("what was the magic word?"));
        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(outcome.final_text, "it was kumquat");
        // The file now replays to the full two-run conversation.
        assert_eq!(load_session(&path).unwrap(), history.messages());
        assert_eq!(history.messages().len(), 4);
        cleanup(&path);
    }

    /// Six lines with one tool exchange: legal cuts are #4 (next line opens
    /// a user turn) and #6 (end).
    fn seed_forkable(path: &Path) -> Vec<Message> {
        let messages = vec![
            Message::user_text("one"),
            Message::assistant(vec![tool_use("t1")]),
            Message::tool_results(vec![tool_result("t1")]),
            Message::assistant(vec![ContentBlock::Text {
                text: "done".into(),
            }]),
            Message::user_text("two"),
            Message::assistant(vec![ContentBlock::Text { text: "bye".into() }]),
        ];
        let mut rollout = Rollout::new(path.to_path_buf());
        for m in &messages {
            rollout.append_message(m).unwrap();
        }
        messages
    }

    #[test]
    fn fork_copies_prefix_and_branches_diverge_independently() {
        let path = temp_file("fork");
        let dir = path.parent().unwrap().to_path_buf();
        let messages = seed_forkable(&path);

        let fork_path = fork_session(&path, Some(5), &dir).unwrap();
        let fork_stem = session_id_of(&fork_path);
        assert_ne!(fork_stem, "session");
        assert_eq!(load_session(&fork_path).unwrap(), messages[..4].to_vec());
        assert_eq!(fork_origin(&fork_path).unwrap(), "session#5");
        assert_eq!(fork_origin(&path), None, "fresh session has no origin");

        // Re-enveloped chain: new stem, seq from 1, first parent crosses
        // files, timestamps preserved from the source lines.
        let src_lines = raw_lines(&path);
        let lines = raw_lines(&fork_path);
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0]["id"], format!("{fork_stem}#1"));
        assert_eq!(lines[0]["parent"], "session#5");
        assert_eq!(lines[1]["id"], format!("{fork_stem}#2"));
        assert_eq!(lines[1]["parent"], format!("{fork_stem}#1"));
        for (line, src) in lines.iter().zip(&src_lines) {
            assert_eq!(line["ts"], src["ts"], "history keeps its original time");
        }

        // Both branches keep appending without seeing each other.
        let mut fork_rollout = resume_session(&fork_path).unwrap().rollout;
        fork_rollout
            .append_message(&Message::user_text("fork branch"))
            .unwrap();
        let mut src_rollout = resume_session(&path).unwrap().rollout;
        src_rollout
            .append_message(&Message::user_text("main branch"))
            .unwrap();
        let mut fork_expected = messages[..4].to_vec();
        fork_expected.push(Message::user_text("fork branch"));
        assert_eq!(load_session(&fork_path).unwrap(), fork_expected);
        let mut src_expected = messages;
        src_expected.push(Message::user_text("main branch"));
        assert_eq!(load_session(&path).unwrap(), src_expected);
        let appended = raw_lines(&fork_path);
        assert_eq!(appended[5]["id"], format!("{fork_stem}#6"));
        assert_eq!(appended[5]["parent"], format!("{fork_stem}#5"));
        cleanup(&path);
    }

    #[test]
    fn fork_inherits_only_usage_lines_before_the_cut() {
        let path = temp_file("fork-usage");
        let dir = path.parent().unwrap().to_path_buf();
        let mut rollout = Rollout::new(path.clone());
        rollout.append_message(&Message::user_text("one")).unwrap();
        rollout
            .append_provider_usage(&usage_record("primary", 10))
            .unwrap();
        rollout
            .append_message(&Message::assistant(vec![ContentBlock::Text {
                text: "done".into(),
            }]))
            .unwrap();
        rollout.append_message(&Message::user_text("two")).unwrap();
        rollout
            .append_provider_usage(&usage_record("fallback", 20))
            .unwrap();
        rollout
            .append_message(&Message::assistant(vec![ContentBlock::Text {
                text: "bye".into(),
            }]))
            .unwrap();

        let fork_path = fork_session(&path, Some(4), &dir).unwrap();
        let forked = resume_session(&fork_path).unwrap();
        assert_eq!(
            forked.provider_usage.records(),
            &[usage_record("primary", 10)]
        );
        assert_eq!(
            resume_session(&path).unwrap().provider_usage.records(),
            &[usage_record("primary", 10), usage_record("fallback", 20),]
        );
        let fork_of_fork = fork_session(&fork_path, None, &dir).unwrap();
        assert_eq!(
            resume_session(&fork_of_fork)
                .unwrap()
                .provider_usage
                .records(),
            &[usage_record("primary", 10)]
        );
        cleanup(&path);
    }

    #[test]
    fn fork_without_cut_copies_the_whole_session() {
        let path = temp_file("forkend");
        let dir = path.parent().unwrap().to_path_buf();
        let messages = seed_forkable(&path);
        let fork_path = fork_session(&path, None, &dir).unwrap();
        assert_eq!(load_session(&fork_path).unwrap(), messages);
        assert_eq!(fork_origin(&fork_path).unwrap(), "session#7");
        cleanup(&path);
    }

    #[test]
    fn illegal_cuts_are_rejected_with_nearby_legal_points() {
        let path = temp_file("forkbad");
        let dir = path.parent().unwrap().to_path_buf();
        seed_forkable(&path);
        // #2 would split the t1 tool exchange.
        let err = fork_session(&path, Some(3), &dir).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let msg = err.to_string();
        assert!(msg.contains("#5") && msg.contains("end = #7"), "{msg}");
        // Past the end of the file.
        assert!(fork_session(&path, Some(99), &dir).is_err());
        // Nothing to fork at all.
        let empty = dir.join("empty.jsonl");
        std::fs::write(&empty, "").unwrap();
        assert!(fork_session(&empty, None, &dir).is_err());
        cleanup(&path);
    }

    #[test]
    fn fork_across_a_compacted_marker_replays_each_side() {
        let path = temp_file("forkcompact");
        let dir = path.parent().unwrap().to_path_buf();
        let mut rollout = Rollout::new(path.clone());
        let assistant_text =
            |t: &str| Message::assistant(vec![ContentBlock::Text { text: t.into() }]);
        let pre = vec![Message::user_text("a"), assistant_text("b")];
        for m in &pre {
            rollout.append_message(m).unwrap();
        }
        rollout.append_message(&Message::user_text("c")).unwrap();
        rollout.append_message(&assistant_text("d")).unwrap();
        let replacement = vec![Message::user_text("[summary]")];
        rollout.append_compacted(&replacement).unwrap(); // #5
        rollout.append_message(&Message::user_text("e")).unwrap();

        // Cutting at the marker keeps it: the fork replays the replacement.
        let at_marker = fork_session(&path, Some(6), &dir).unwrap();
        assert_eq!(load_session(&at_marker).unwrap(), replacement);
        // Cutting before compaction forks the raw history the marker later
        // superseded — those lines never left the file.
        let before = fork_session(&path, Some(3), &dir).unwrap();
        assert_eq!(load_session(&before).unwrap(), pre);
        // The line just before the marker is not a legal cut (its successor
        // is the marker, not a user turn).
        assert!(fork_session(&path, Some(5), &dir).is_err());
        cleanup(&path);
    }

    #[test]
    fn fork_of_a_fork_points_at_the_middle_file() {
        let path = temp_file("forkfork");
        let dir = path.parent().unwrap().to_path_buf();
        seed_forkable(&path);
        let first = fork_session(&path, Some(5), &dir).unwrap();
        let second = fork_session(&first, None, &dir).unwrap();
        let first_stem = session_id_of(&first);
        assert_eq!(fork_origin(&second).unwrap(), format!("{first_stem}#5"));
        assert_eq!(
            load_session(&second).unwrap(),
            load_session(&first).unwrap()
        );
        cleanup(&path);
    }

    #[test]
    fn fork_points_list_turn_boundaries_with_previews() {
        let path = temp_file("forkpoints");
        seed_forkable(&path);
        // seed_forkable's legal cuts are {4, 6}; #6 is the tip (a no-op rewind)
        // so only #4 remains, previewed by the turn it drops (user "two").
        assert_eq!(
            fork_points(&path).unwrap(),
            vec![ForkPoint {
                seq: 5,
                preview: "two".into(),
            }]
        );
        // Every offered seq is one fork_session accepts.
        for point in fork_points(&path).unwrap() {
            let dir = path.parent().unwrap();
            assert!(fork_session(&path, Some(point.seq), dir).is_ok());
        }
        cleanup(&path);
    }

    #[test]
    fn fork_points_strip_steering_framing_and_skip_the_tip() {
        let path = temp_file("forkpointsteer");
        let mut rollout = Rollout::new(path.clone());
        for m in [
            Message::user_text("start"),
            Message::assistant(vec![tool_use("t1")]),
            Message::tool_results(vec![tool_result("t1")]),
            // A mid-turn steer is a user message with no tool_result block, so
            // it opens a turn the picker can rewind to — shown by its own words.
            Message::user_text(format!("{STEERING_PREFIX}\nmid-turn nudge")),
            Message::assistant(vec![ContentBlock::Text { text: "ok".into() }]),
        ] {
            rollout.append_message(&m).unwrap();
        }
        assert_eq!(
            fork_points(&path).unwrap(),
            vec![ForkPoint {
                seq: 4,
                preview: "mid-turn nudge".into(),
            }]
        );
        cleanup(&path);
    }

    #[test]
    fn fork_points_of_a_single_turn_session_is_empty() {
        let path = temp_file("forkpointsone");
        let mut rollout = Rollout::new(path.clone());
        rollout.append_message(&Message::user_text("only")).unwrap();
        rollout
            .append_message(&Message::assistant(vec![ContentBlock::Text {
                text: "hi".into(),
            }]))
            .unwrap();
        assert!(fork_points(&path).unwrap().is_empty());
        cleanup(&path);
    }

    #[test]
    fn forked_branches_share_the_offload_dir_without_clobbering() {
        use crate::history::History;
        let path = temp_file("forkoffload");
        let dir = path.parent().unwrap().to_path_buf();
        seed_forkable(&path);
        let fork_path = fork_session(&path, Some(5), &dir).unwrap();

        // Resume both branches against the shared offload dir and spill from
        // each: the ids must never collide (counter is dir-global).
        let spill_from = |session: &Path| {
            let resumed = resume_session(session).unwrap();
            let mut history = History::resume(dir.clone(), resumed);
            history.record(Message::tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: "big".into(),
                content: "x".repeat(crate::history::OFFLOAD_CAP_CHARS + 1_000).into(),
                is_error: false,
            }]));
            let ContentBlock::ToolResult { content, .. } =
                &history.messages().last().unwrap().content[0]
            else {
                panic!("expected tool result");
            };
            let content = content.as_text();
            // The id lives only in the path the pointer names now.
            let start = content.find("off-").expect("pointer names the file");
            content[start..start + 8].to_string()
        };
        let main_id = spill_from(&path);
        let fork_id = spill_from(&fork_path);
        assert_ne!(main_id, fork_id);
        assert!(dir.join(format!("{main_id}.txt")).exists());
        assert!(dir.join(format!("{fork_id}.txt")).exists());
        cleanup(&path);
    }

    #[test]
    fn subagent_rollout_stamps_origin_on_the_first_line_only() {
        let path = temp_file("subagent");
        let mut rollout = Rollout::new_subagent(path.clone(), "parent#5".into());
        rollout
            .append_message(&Message::user_text("do the sub task"))
            .unwrap();
        rollout
            .append_message(&Message::assistant(vec![ContentBlock::Text {
                text: "done".into(),
            }]))
            .unwrap();

        let lines = raw_lines(&path);
        assert_eq!(
            lines[0]["subagent_of"], "parent#5",
            "first line records the spawning turn"
        );
        assert_eq!(
            lines[0].get("parent"),
            None,
            "a sub-agent history starts a fresh chain, no cross-file parent"
        );
        assert_eq!(
            lines[1].get("subagent_of"),
            None,
            "only the first line carries the back-pointer"
        );
        assert_eq!(lines[1]["parent"], "session#1", "chain is sequential after");

        assert_eq!(
            session_origin(&path),
            Some(SessionOrigin::SubAgent("parent#5".into()))
        );
        assert!(is_subagent_session(&path));

        // Lineage metadata is inert on replay: the history is just the two messages.
        assert_eq!(load_session(&path).unwrap().len(), 2);
        cleanup(&path);
    }

    /// The digest is what a session list reads instead of replaying the file:
    /// the first user line, and nothing after it. A torn tail past that point
    /// must not change the answer — the picker has to list a damaged session,
    /// not fail on it.
    #[test]
    fn session_digest_stops_at_the_first_user_line() {
        use std::io::Write as _;

        let path = temp_file("digest");
        let mut rollout = Rollout::new(path.clone());
        rollout
            .append_message(&Message::user_text("review the picker"))
            .unwrap();
        rollout
            .append_message(&Message::assistant(vec![ContentBlock::Text {
                text: "on it".into(),
            }]))
            .unwrap();
        rollout
            .append_message(&Message::user_text("a later prompt"))
            .unwrap();
        drop(rollout);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"type\": \"mess").unwrap();
        drop(file);

        let digest = session_digest(&path).unwrap();
        assert_eq!(digest.id, "session");
        assert_eq!(digest.title, "review the picker");
        assert_eq!(digest.origin, None);
        assert!(digest.has_content);
        assert_eq!(digest.bytes, std::fs::metadata(&path).unwrap().len());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A file holding only its preamble replays as an empty conversation, and
    /// the digest says so without a title to show for it.
    #[test]
    fn session_digest_marks_a_preamble_only_file_as_empty() {
        let path = temp_file("digest-shell");
        // A writer that never gets past the preamble removes the file on drop;
        // a hard kill does not, and that is the file this covers.
        std::mem::forget(Rollout::new(path.clone()));

        let digest = session_digest(&path).unwrap();
        assert!(!digest.has_content);
        assert_eq!(digest.title, "");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Lineage rides the first line, so the digest reads it for free — that is
    /// what keeps sub-agent transcripts out of the picker without a second pass
    /// over every file.
    #[test]
    fn session_digest_carries_lineage() {
        let path = temp_file("digest-fork");
        let dir = path.parent().unwrap().to_path_buf();
        seed_forkable(&path);
        let fork = fork_session(&path, Some(5), &dir).unwrap();

        let digest = session_digest(&fork).unwrap();
        assert!(matches!(digest.origin, Some(SessionOrigin::Fork(_))));
        assert_eq!(digest.title, "one");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_origin_distinguishes_fork_subagent_and_fresh() {
        let path = temp_file("origin");
        let dir = path.parent().unwrap().to_path_buf();
        seed_forkable(&path);

        // A fresh top-level session has no origin and is not a sub-agent.
        assert_eq!(session_origin(&path), None);
        assert!(!is_subagent_session(&path));

        // A fork: cross-file parent, no subagent_of → Fork.
        let fork_path = fork_session(&path, Some(5), &dir).unwrap();
        assert_eq!(
            session_origin(&fork_path),
            Some(SessionOrigin::Fork("session#5".into()))
        );
        assert!(!is_subagent_session(&fork_path));
        cleanup(&path);
    }

    #[test]
    fn forking_a_subagent_session_becomes_a_fork_not_a_subagent() {
        let path = temp_file("subfork");
        let dir = path.parent().unwrap().to_path_buf();
        let mut rollout = Rollout::new_subagent(path.clone(), "parent#5".into());
        rollout
            .append_message(&Message::user_text("sub work"))
            .unwrap();
        rollout
            .append_message(&Message::assistant(vec![ContentBlock::Text {
                text: "done".into(),
            }]))
            .unwrap();

        // Forking it yields an independent branch: the copied subagent_of is
        // dropped and the new first line is a fork pointer.
        let fork = fork_session(&path, None, &dir).unwrap();
        assert!(matches!(
            session_origin(&fork),
            Some(SessionOrigin::Fork(_))
        ));
        assert!(!is_subagent_session(&fork));
        cleanup(&path);
    }

    #[test]
    fn malformed_tail_line_truncates_instead_of_failing() {
        let path = temp_file("corrupt");
        let mut rollout = Rollout::new(path.clone());
        let good = Message::user_text("intact");
        rollout.append_message(&good).unwrap();
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"type\":\"message\",\"role\":\"us"); // crash mid-append
        std::fs::write(&path, &raw).unwrap();

        // Read-only load ignores the tail but leaves the file alone.
        assert_eq!(load_session(&path).unwrap(), vec![good.clone()]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        cleanup(&path);
    }

    #[test]
    fn runtime_and_terminals_are_snapshot_only() {
        let path = temp_file("snapshot");
        let runtime = SessionRuntime {
            cwd: "/tmp/project".into(),
        };
        let mut rollout = Rollout::new_with_runtime(path.clone(), runtime.clone()).unwrap();
        rollout
            .append_message(&Message::user_text("hello"))
            .unwrap();
        rollout
            .append_message(&Message::assistant(vec![ContentBlock::Text {
                text: "half".into(),
            }]))
            .unwrap();
        rollout
            .append_turn_terminal(&TurnTerminal {
                status: "error".into(),
                error: Some("stream dropped".into()),
                typed_error: None,
            })
            .unwrap();

        assert_eq!(
            load_session(&path).unwrap(),
            vec![
                Message::user_text("hello"),
                Message::assistant(vec![ContentBlock::Text {
                    text: "half".into(),
                }]),
            ],
            "runtime and terminal records must never enter provider history"
        );
        assert_eq!(
            load_session_snapshot(&path).unwrap(),
            SessionSnapshot {
                messages: vec![
                    Message::user_text("hello"),
                    Message::assistant(vec![ContentBlock::Text {
                        text: "half".into(),
                    }]),
                ],
                runtime: Some(runtime),
                terminals: vec![SnapshotTerminal {
                    after_message: 2,
                    status: "error".into(),
                    error: Some("stream dropped".into()),
                }],
                provider_routes: vec![fixture_route(2)],
            }
        );
        cleanup(&path);
    }

    #[test]
    fn fork_copies_runtime_and_terminal_records() {
        let path = temp_file("snapshot-fork");
        let dir = path.parent().unwrap().to_path_buf();
        let runtime = SessionRuntime {
            cwd: "/tmp/fork-project".into(),
        };
        let mut rollout = Rollout::new_with_runtime(path.clone(), runtime.clone()).unwrap();
        rollout.append_message(&Message::user_text("q")).unwrap();
        rollout
            .append_message(&Message::assistant(vec![ContentBlock::Text {
                text: "a".into(),
            }]))
            .unwrap();
        rollout
            .append_turn_terminal(&TurnTerminal {
                status: "completed".into(),
                error: None,
                typed_error: None,
            })
            .unwrap();

        let fork = fork_session(&path, None, &dir).unwrap();
        let snapshot = load_session_snapshot(&fork).unwrap();
        assert_eq!(snapshot.runtime, Some(runtime));
        assert_eq!(
            snapshot.terminals,
            vec![SnapshotTerminal {
                after_message: 2,
                status: "completed".into(),
                error: None,
            }]
        );
        assert_eq!(fork_origin(&fork), Some("session#5".into()));
        cleanup(&path);
    }

    #[test]
    fn chat_text_provenance_survives_read_without_reasoning() {
        let path = temp_file("chat-text-provenance");
        let provider = kloop_provider::Provider::OpenAiCompat {
            cred: kloop_provider::Credential::bearer("test-key"),
            base: "https://chat.invalid".into(),
        };
        let (catalog, _) = crate::provider_route::ProviderCatalog::from_provider(
            "chat",
            provider,
            "chat-model",
            vec!["chat-model".into()],
        )
        .unwrap();
        let route = catalog.initial_route("chat", Some("chat-model")).unwrap();
        let mut rollout = Rollout::new_with_initial_route(path.clone(), &route).unwrap();
        let assistant = Message::assistant_from_provider(
            vec![ContentBlock::Text {
                text: "chat answer".into(),
            }],
            route.primary_attempt().provenance(2),
        );
        rollout.append_message(&assistant).unwrap();
        assert_eq!(
            load_session_snapshot(&path).unwrap().messages,
            vec![assistant]
        );
        cleanup(&path);
    }

    /// And the same route with reasoning in it: a chat model that streams
    /// `reasoning_content` writes signature-less thinking under chat
    /// provenance, so refusing that shape on read would make the session it
    /// wrote unopenable. Stripping reasoning is the request projection's job,
    /// not the validator's.
    #[test]
    fn chat_reasoning_provenance_survives_read() {
        let path = temp_file("chat-reasoning-provenance");
        let provider = kloop_provider::Provider::OpenAiCompat {
            cred: kloop_provider::Credential::bearer("test-key"),
            base: "https://chat.invalid".into(),
        };
        let (catalog, _) = crate::provider_route::ProviderCatalog::from_provider(
            "chat",
            provider,
            "chat-model",
            vec!["chat-model".into()],
        )
        .unwrap();
        let route = catalog.initial_route("chat", Some("chat-model")).unwrap();
        let mut rollout = Rollout::new_with_initial_route(path.clone(), &route).unwrap();
        let assistant = Message::assistant_from_provider(
            vec![
                ContentBlock::Thinking {
                    thinking: "streamed as reasoning_content".into(),
                    signature: String::new(),
                },
                ContentBlock::Text {
                    text: "chat answer".into(),
                },
            ],
            route.primary_attempt().provenance(2),
        );
        rollout.append_message(&assistant).unwrap();
        assert_eq!(
            load_session_snapshot(&path).unwrap().messages,
            vec![assistant]
        );
        cleanup(&path);
    }
    #[test]
    fn reasoning_provenance_and_typed_terminal_survive_resume_and_fork() {
        let path = temp_file("reasoning-continuity");
        let dir = path.parent().unwrap().to_path_buf();
        let provenance = kloop_protocol::ProviderResponseProvenance {
            route_revision: 1,
            route_boundary: 3,
            provider_id: "test".into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            endpoint_fingerprint: kloop_provider::Provider::endpoint_fingerprint_for(
                kloop_protocol::ProviderApiFamily::Mock,
                "mock",
            ),
            model: "mock".into(),
        };
        let assistant = Message::assistant_from_provider(
            vec![ContentBlock::Thinking {
                thinking: "summary".into(),
                signature: "encrypted".into(),
            }],
            provenance.clone(),
        );
        let typed_error = TurnError::ProviderOutcome(AssistantOutcome::Incomplete(
            IncompleteReason::Provider("provider_status".into()),
        ));
        let mut rollout = Rollout::new(path.clone());
        rollout.append_message(&Message::user_text("q")).unwrap();
        rollout.append_message(&assistant).unwrap();
        rollout
            .append_turn_terminal(&TurnTerminal {
                status: "error".into(),
                error: Some(typed_error.to_string()),
                typed_error: Some(typed_error.clone()),
            })
            .unwrap();

        let snapshot = load_session_snapshot(&path).unwrap();
        assert_eq!(snapshot.messages[1], assistant);
        assert_eq!(
            snapshot.messages[1].provider_provenance,
            Some(provenance.clone())
        );
        let raw = raw_lines(&path);
        assert_eq!(raw[3]["typed_error"]["kind"], "provider_outcome");
        assert_eq!(raw[3]["typed_error"]["value"]["type"], "incomplete");
        let failure = ProviderFailure::transport("stream dropped").with_semantic_output(true);
        let encoded = serde_json::to_value(TurnError::ProviderFailure(failure.clone())).unwrap();
        assert_eq!(
            serde_json::from_value::<TurnError>(encoded).unwrap(),
            TurnError::ProviderFailure(failure)
        );

        let resumed = resume_session(&path).unwrap();
        assert_eq!(resumed.messages[1].provider_provenance, Some(provenance));
        let fork = fork_session(&path, None, &dir).unwrap();
        assert_eq!(load_session_snapshot(&fork).unwrap().messages[1], assistant);
        assert_eq!(raw_lines(&fork)[3]["typed_error"], raw[3]["typed_error"]);
        cleanup(&path);
    }

    #[test]
    fn complete_usage_before_torn_tail_is_recovered() {
        let path = temp_file("usage-before-torn");
        let mut rollout = Rollout::new(path.clone());
        rollout
            .append_provider_usage(&usage_record("model", 10))
            .unwrap();
        let good_len = std::fs::metadata(&path).unwrap().len();
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"type\":\"message");
        std::fs::write(&path, raw).unwrap();

        let resumed = resume_session(&path).unwrap();
        assert_eq!(
            resumed.provider_usage.records(),
            &[usage_record("model", 10)]
        );
        assert_eq!(std::fs::metadata(&path).unwrap().len(), good_len);
        cleanup(&path);
    }

    #[test]
    fn torn_usage_line_is_ignored_and_truncated() {
        let path = temp_file("torn-usage");
        let mut rollout = Rollout::new(path.clone());
        rollout
            .append_message(&Message::user_text("intact"))
            .unwrap();
        let good_len = std::fs::metadata(&path).unwrap().len();
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"type\":\"provider_usage\",\"id\":\"session#2\"");
        std::fs::write(&path, raw).unwrap();

        let resumed = resume_session(&path).unwrap();
        assert!(resumed.provider_usage.records().is_empty());
        assert_eq!(resumed.messages, vec![Message::user_text("intact")]);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), good_len);
        cleanup(&path);
    }

    /// The plan-7 latent bug: appending after a torn tail merges into the
    /// partial line and everything after becomes unreadable. Resume must
    /// physically truncate the tail before the chain continues.
    #[test]
    fn resume_truncates_torn_tail_so_appends_survive() {
        let path = temp_file("torn");
        let mut rollout = Rollout::new(path.clone());
        rollout
            .append_message(&Message::user_text("intact"))
            .unwrap();
        let good_len = std::fs::metadata(&path).unwrap().len();
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"type\":\"mess"); // torn write, no newline
        std::fs::write(&path, &raw).unwrap();

        let mut resumed = resume_session(&path).unwrap();
        assert_eq!(resumed.messages, vec![Message::user_text("intact")]);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            good_len,
            "torn tail physically removed"
        );
        resumed
            .rollout
            .append_message(&Message::user_text("after crash"))
            .unwrap();
        assert_eq!(
            load_session(&path).unwrap(),
            vec![
                Message::user_text("intact"),
                Message::user_text("after crash"),
            ]
        );
        cleanup(&path);
    }

    #[test]
    fn read_is_side_effect_free_but_recovery_persists_pairing_marker() {
        let path = temp_file("repair-marker");
        let mut rollout = Rollout::new(path.clone());
        rollout
            .append_message(&Message::assistant(vec![tool_use("missing")]))
            .unwrap();
        let before = std::fs::read(&path).unwrap();

        let inspected = inspect_session(&path).unwrap();
        assert_eq!(inspected.repair_stats().inserted_tool_results, 1);
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let resumed = inspected.recover().unwrap();
        assert_eq!(resumed.repair.inserted_tool_results, 1);
        let lines = raw_lines(&path);
        assert_eq!(lines.last().unwrap()["type"], "repaired");
        let after_marker = lines.len();
        drop(resumed);

        let inspected = inspect_session(&path).unwrap();
        assert_eq!(
            inspected.repair_stats(),
            &PairingRepairStats {
                messages_before: 2,
                messages_after: 2,
                ..PairingRepairStats::default()
            }
        );
        let resumed = inspected.recover().unwrap();
        assert_eq!(
            resumed.repair,
            PairingRepairStats {
                messages_before: 2,
                messages_after: 2,
                ..PairingRepairStats::default()
            }
        );
        assert_eq!(raw_lines(&path).len(), after_marker);
        cleanup(&path);
    }

    /// Launching kloop and quitting without saying anything leaves no session:
    /// the file exists while the writer lives (its name reserves the id), and
    /// goes away with it. Otherwise `--continue` picks the empty shell over the
    /// last real conversation.
    #[test]
    fn a_session_with_only_preamble_removes_itself() {
        let path = temp_file("preamble-only");
        let rollout = Rollout::new(path.clone());
        assert!(path.exists(), "the id is reserved while the writer lives");
        drop(rollout);
        assert!(!path.exists());
        cleanup(&path);
    }

    /// One recorded message is enough to keep it: the preamble it was holding
    /// is already in the file, so replay is unaffected.
    #[test]
    fn one_message_keeps_the_session_file() {
        let path = temp_file("kept-after-message");
        let mut rollout = Rollout::new(path.clone());
        rollout.append_message(&Message::user_text("hi")).unwrap();
        drop(rollout);
        assert!(path.exists());
        assert_eq!(load_session_snapshot(&path).unwrap().messages.len(), 1);
        cleanup(&path);
    }

    /// A resumed session's file belongs to whoever wrote it. Resuming one and
    /// quitting without a word must not delete the conversation.
    #[test]
    fn resuming_and_saying_nothing_never_deletes_the_file() {
        let path = temp_file("resume-then-quit");
        let mut rollout = Rollout::new(path.clone());
        rollout.append_message(&Message::user_text("hi")).unwrap();
        drop(rollout);

        let resumed = resume_session(&path).unwrap();
        drop(resumed.rollout);
        assert!(path.exists());
        assert_eq!(load_session_snapshot(&path).unwrap().messages.len(), 1);
        cleanup(&path);
    }

    #[test]
    fn repair_remaps_snapshot_terminal_boundaries() {
        let path = temp_file("repair-terminal-boundary");
        let mut rollout = Rollout::new(path.clone());
        rollout
            .append_message(&Message::assistant(vec![tool_use("missing")]))
            .unwrap();
        rollout
            .append_turn_terminal(&TurnTerminal {
                status: "error".into(),
                error: None,
                typed_error: None,
            })
            .unwrap();

        let snapshot = load_session_snapshot(&path).unwrap();
        assert_eq!(snapshot.messages.len(), 2);
        assert_eq!(snapshot.terminals[0].after_message, 2);
        let resumed = resume_session(&path).unwrap();
        assert_eq!(resumed.snapshot.terminals[0].after_message, 2);
        assert_eq!(load_session_snapshot(&path).unwrap(), resumed.snapshot);
        cleanup(&path);
    }

    #[test]
    fn invalid_utf8_tail_is_read_only_until_explicit_recovery() {
        let path = temp_file("invalid-utf8-tail");
        let mut rollout = Rollout::new(path.clone());
        rollout.append_message(&Message::user_text("good")).unwrap();
        let good_len = std::fs::metadata(&path).unwrap().len();
        let mut raw = std::fs::read(&path).unwrap();
        raw.extend_from_slice(b"{\"type\":\"message\",\"content\":\xff");
        std::fs::write(&path, &raw).unwrap();

        assert_eq!(
            load_session(&path).unwrap(),
            vec![Message::user_text("good")]
        );
        assert_eq!(std::fs::read(&path).unwrap(), raw);
        resume_session(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), good_len);
        cleanup(&path);
    }

    #[test]
    fn checked_session_paths_reject_traversal_and_symlink_leaves() {
        let path = temp_file("checked-path");
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        for id in [
            "",
            ".",
            "..",
            "../outside",
            "a/b",
            "a\\\\b",
            "/tmp/out",
            // Beyond traversal: an id is a token every downstream consumer can
            // hand to a filesystem, an HTTP header, a log, and the wire.
            "线程-1",    // non-ASCII
            "a b",       // space
            ".hidden",   // leading dot
            "a\\u{7f}b", // control character
            "sess:1",    // punctuation outside the set
        ] {
            assert!(checked_session_path(dir, id).is_err(), "accepted {id:?}");
        }
        // Everything kloop mints must still pass: CLI/server stems and the
        // sub-agent transcripts derived from them.
        for id in [
            new_session_id(dir).as_str(),
            "20260831-083106",
            "20260831-083106-2",
            "20260831-083106-agent-1",
        ] {
            assert!(checked_session_path(dir, id).is_ok(), "rejected {id:?}");
        }
        let target = dir.join("outside.jsonl");
        std::fs::write(&target, b"not a session").unwrap();
        let leaf = dir.join("link.jsonl");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &leaf).unwrap();
        #[cfg(unix)]
        assert!(checked_session_path(dir, "link").is_err());
        cleanup(&path);
    }

    #[test]
    fn duplicate_tool_results_are_dropped_and_counted() {
        let path = temp_file("duplicate-results");
        let mut rollout = Rollout::new(path.clone());
        rollout
            .append_message(&Message::assistant(vec![tool_use("same")]))
            .unwrap();
        rollout
            .append_message(&Message::tool_results(vec![
                tool_result("same"),
                tool_result("same"),
            ]))
            .unwrap();
        let inspected = inspect_session(&path).unwrap();
        assert_eq!(inspected.repair_stats().duplicate_tool_results, 1);
        assert_eq!(inspected.snapshot().messages[1].content.len(), 1);
        cleanup(&path);
    }

    #[test]
    fn sequence_exhaustion_at_next_line_fails_before_recovery_truncates() {
        let path = temp_file("sequence-next-overflow");
        let raw = format!(
            "{{\"type\":\"message\",\"id\":\"session#{}\",\"ts\":1,\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"x\"}}]}}\n{{\"type\":\"message\"",
            u64::MAX - 1
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, raw.as_bytes()).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(resume_session(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        cleanup(&path);
    }

    #[test]
    fn legacy_and_semantically_invalid_route_lines_fail_without_repair() {
        let legacy_path = temp_file("legacy-no-route");
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::write(
            &legacy_path,
            b"{\"type\":\"message\",\"id\":\"session#1\",\"ts\":1,\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"old\"}]}\n",
        )
        .unwrap();
        let legacy_before = std::fs::read(&legacy_path).unwrap();
        assert!(inspect_session(&legacy_path).is_err());
        assert_eq!(std::fs::read(&legacy_path).unwrap(), legacy_before);

        let invalid_path = temp_file("invalid-route-line");
        let mut rollout = Rollout::new(invalid_path.clone());
        rollout.append_message(&Message::user_text("new")).unwrap();
        drop(rollout);
        let mut lines = raw_lines(&invalid_path);
        let mut duplicate = lines[0].clone();
        duplicate["type"] = json!("provider_route_changed");
        duplicate["id"] = json!("session#2");
        duplicate["parent"] = json!("session#1");
        duplicate["route_boundary"] = json!(2);
        duplicate["source"] = json!("explicit_switch");
        lines.insert(1, duplicate);
        let raw = lines
            .iter()
            .map(|line| format!("{}\n", serde_json::to_string(line).unwrap()))
            .collect::<String>();
        std::fs::write(&invalid_path, raw.as_bytes()).unwrap();
        let before = std::fs::read(&invalid_path).unwrap();
        assert!(inspect_session(&invalid_path).is_err());
        assert_eq!(std::fs::read(&invalid_path).unwrap(), before);
        cleanup(&legacy_path);
        cleanup(&invalid_path);
    }

    #[test]
    fn malformed_envelopes_and_route_timelines_fail_closed_without_repair() {
        let path = temp_file("malformed-envelope");
        let mut rollout = Rollout::new(path.clone());
        rollout.append_message(&Message::user_text("one")).unwrap();
        rollout.append_message(&Message::user_text("two")).unwrap();
        drop(rollout);

        let original = raw_lines(&path);
        let cases = [
            ("duplicate-id", "id", json!("session#2")),
            ("regressed-id", "id", json!("session#1")),
            ("bad-parent", "parent", json!("other#1")),
        ];
        for (tag, field, value) in cases {
            let case_path = temp_file(tag);
            let mut lines = original.clone();
            lines[2][field] = value;
            let raw = lines
                .iter()
                .map(|line| format!("{}\n", serde_json::to_string(line).unwrap()))
                .collect::<String>();
            std::fs::create_dir_all(case_path.parent().unwrap()).unwrap();
            std::fs::write(&case_path, raw).unwrap();
            let before = std::fs::read(&case_path).unwrap();
            assert!(inspect_session(&case_path).is_err(), "{tag}");
            assert_eq!(std::fs::read(&case_path).unwrap(), before, "{tag}");
            cleanup(&case_path);
        }
        cleanup(&path);
    }

    #[test]
    fn duplicate_or_regressed_boundary_and_revision_gap_fail_closed_without_repair() {
        let path = temp_file("malformed-route-timeline");
        let mut rollout = Rollout::new(path.clone());
        rollout.append_message(&Message::user_text("one")).unwrap();
        let mut first_switch = fixture_route(3);
        first_switch.route_revision = 2;
        first_switch.source = ProviderRouteSource::ExplicitSwitch;
        rollout
            .append_line(RolloutLine::ProviderRouteChanged {
                meta: rollout.next_meta(),
                receipt: first_switch,
            })
            .unwrap();
        rollout.append_message(&Message::user_text("two")).unwrap();
        let mut second_switch = fixture_route(5);
        second_switch.route_revision = 3;
        second_switch.source = ProviderRouteSource::ExplicitSwitch;
        rollout
            .append_line(RolloutLine::ProviderRouteChanged {
                meta: rollout.next_meta(),
                receipt: second_switch,
            })
            .unwrap();
        rollout
            .append_message(&Message::user_text("three"))
            .unwrap();
        drop(rollout);

        let original = raw_lines(&path);
        let cases = [
            ("duplicate-boundary", "route_boundary", json!(3)),
            ("regressed-boundary", "route_boundary", json!(2)),
            ("revision-gap", "route_revision", json!(4)),
        ];
        for (tag, field, value) in cases {
            let case_path = temp_file(tag);
            let mut lines = original.clone();
            lines[4][field] = value;
            let raw = lines
                .iter()
                .map(|line| format!("{}\n", serde_json::to_string(line).unwrap()))
                .collect::<String>();
            std::fs::create_dir_all(case_path.parent().unwrap()).unwrap();
            std::fs::write(&case_path, raw).unwrap();
            let before = std::fs::read(&case_path).unwrap();
            assert!(inspect_session(&case_path).is_err(), "{tag}");
            assert_eq!(std::fs::read(&case_path).unwrap(), before, "{tag}");
            cleanup(&case_path);
        }
        cleanup(&path);
    }

    #[test]
    fn future_provider_revision_in_message_provenance_fails_closed() {
        let path = temp_file("future-route-provenance");
        let mut rollout = Rollout::new(path.clone());
        rollout
            .append_message(&Message::user_text("question"))
            .unwrap();
        let mut provenance = kloop_protocol::ProviderResponseProvenance {
            route_revision: 2,
            route_boundary: 3,
            provider_id: "test".into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            endpoint_fingerprint: kloop_provider::Provider::endpoint_fingerprint_for(
                kloop_protocol::ProviderApiFamily::Mock,
                "mock",
            ),
            model: "mock".into(),
        };
        rollout
            .append_message(&Message::assistant_from_provider(
                vec![ContentBlock::Thinking {
                    thinking: "summary".into(),
                    signature: "opaque".into(),
                }],
                provenance.clone(),
            ))
            .unwrap();
        assert!(inspect_session(&path).is_err());
        provenance.route_revision = 1;
        provenance.route_boundary = 99;
        let raw = std::fs::read_to_string(&path).unwrap().replace(
            "\"routeRevision\":2,\"originBoundary\":3",
            "\"routeRevision\":1,\"originBoundary\":99",
        );
        std::fs::write(&path, raw).unwrap();
        assert!(inspect_session(&path).is_err());
        cleanup(&path);
    }

    #[test]
    fn sequence_exhaustion_fails_before_recovery_truncates() {
        let path = temp_file("sequence-overflow");
        let raw = format!(
            "{{\"type\":\"message\",\"id\":\"session#{}\",\"ts\":1,\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"x\"}}]}}\n{{\"type\":\"message\"",
            u64::MAX
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, raw.as_bytes()).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(resume_session(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        cleanup(&path);
    }
}
