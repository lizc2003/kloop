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

use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::provider_route::FrozenProviderRoute;
use crate::request_reduction::FrozenStub;
use crate::usage::ProviderUsageRecord;
use kloop_protocol::{Message, ProviderRouteReceipt, ProviderRouteSource, ReasoningContinuity};

mod line;
mod sessions;
#[cfg(test)]
mod testutil;
mod validate;

pub use line::{
    PairingRepairStats, SESSION_FORMAT_VERSION, SessionRuntime, SessionSnapshot, SnapshotTerminal,
    TurnError, TurnTerminal,
};
pub use sessions::{
    ClaimedSession, ForkPoint, ResumedSession, SessionDigest, SessionOrigin, SessionRead,
    checked_session_path, claim_new_session, first_user_snippet, fork_origin, fork_points,
    fork_session, inspect_session, is_subagent_session, load_session, load_session_snapshot,
    resume_session, session_digest, session_id_of, session_origin, session_path,
    sessions_by_recency,
};
pub(crate) use sessions::{civil_from_days, timestamp_id};

use line::{LineMeta, RolloutLine};
use sessions::PairingRepair;

/// Append-only writer for one session file. A new session's file is created
/// by [`claim_new_session`], which is what reserves its id against a
/// concurrent process; appending only ever adds to it. A writer that owns its
/// file and never gets past the opening preamble removes the file again when
/// it is dropped, so starting kloop and quitting without a word leaves nothing
/// behind. Tracks the id chain: each line's `parent` is the previous line's id.
pub struct Rollout {
    lease: Option<crate::state_lock::StateLock>,
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
    /// session owners use `claimed_with_initial_route` once their catalog route
    /// is resolved.
    pub fn new(path: PathBuf) -> Self {
        let mut rollout = Self::with_origin(path, None);
        rollout
            .append_fixture_initial_route()
            .expect("fixture rollout initial route must be writable");
        rollout
    }

    /// A new session on the file [`claim_new_session`] just created. The file
    /// exists but is this writer's own, so it goes again if nothing past the
    /// preamble is ever written — including when writing the preamble fails.
    pub fn claimed_with_initial_route(
        claim: ClaimedSession,
        route: &FrozenProviderRoute,
    ) -> io::Result<Self> {
        let mut rollout = Self::with_origin(claim.path, None);
        rollout.preexisting = false;
        rollout.append_initial_route(route)?;
        Ok(rollout)
    }

    /// Test fixtures at a path of their choosing. Nothing claims that path, so
    /// production code must not start a session this way.
    #[cfg(test)]
    pub(crate) fn new_with_initial_route(
        path: PathBuf,
        route: &FrozenProviderRoute,
    ) -> io::Result<Self> {
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
        let lease = crate::state_lock::StateLock::try_acquire(&path.with_extension("lock"))?;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let mut rollout = Self::with_origin(path, Some(subagent_of));
        rollout.lease = Some(lease);
        rollout.preexisting = false;
        rollout.append_initial_route(route)?;
        Ok(rollout)
    }

    fn with_origin(path: PathBuf, subagent_of: Option<String>) -> Self {
        let prefix = id_prefix(&path);
        Self {
            lease: None,
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

    pub(crate) fn append_request_stub(&mut self, stub: &FrozenStub) -> io::Result<()> {
        self.append_line(RolloutLine::RequestStub {
            meta: self.next_meta(),
            stub: stub.clone(),
        })
    }

    pub fn append_turn_terminal(&mut self, terminal: &TurnTerminal) -> io::Result<()> {
        self.append_line(RolloutLine::TurnTerminal {
            meta: self.next_meta(),
            terminal: terminal.clone(),
        })
    }

    pub(crate) fn append_tool_started(&mut self, tool_use_id: &str) -> io::Result<()> {
        self.append_line(RolloutLine::ToolStarted {
            meta: self.next_meta(),
            tool_use_id: tool_use_id.to_string(),
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
            // Likewise the format version: stamping it here rather than in
            // each constructor is what makes it impossible for a new
            // construction path to forget it.
            format_version: (self.next_seq == 1).then_some(SESSION_FORMAT_VERSION),
            ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }
    }

    fn ensure_lease(&mut self) -> io::Result<()> {
        if self.lease.is_none() {
            self.lease = Some(
                crate::state_lock::StateLock::try_acquire(&self.path.with_extension("lock"))
                    .map_err(|error| {
                        io::Error::other(format!(
                            "session is already active or cannot be locked: {error}"
                        ))
                    })?,
            );
        }
        Ok(())
    }

    fn append_line(&mut self, line: RolloutLine) -> io::Result<()> {
        self.ensure_lease()?;
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
        if line.needs_sync() {
            file.sync_data()?;
        }
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

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use kloop_protocol::ContentBlock;
    use serde_json::json;

    #[test]
    fn a_subagent_cannot_append_a_new_history_into_an_existing_transcript() {
        let dir =
            std::env::temp_dir().join(crate::resource_id::fresh("kloop-child-claim-").unwrap());
        let path = dir.join("child.jsonl");
        let cfg = crate::tools::testutil::TestConfig::new("child-claim").build();
        let mut writer =
            Rollout::new_subagent_with_route(path.clone(), "parent#1".into(), &cfg.provider_route)
                .unwrap();
        writer
            .append_message(&Message::user_text("original child"))
            .unwrap();
        drop(writer);
        let before = std::fs::read(&path).unwrap();
        assert!(
            Rollout::new_subagent_with_route(path.clone(), "parent#2".into(), &cfg.provider_route)
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::remove_dir_all(dir).unwrap();
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
        drop(rollout);
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
    fn a_file_states_its_format_version_on_its_first_line_only() {
        let path = temp_file("format-version");
        let mut rollout = Rollout::new(path.clone());
        rollout.append_message(&Message::user_text("one")).unwrap();
        rollout.append_message(&Message::user_text("two")).unwrap();

        let lines = raw_lines(&path);
        assert_eq!(lines[0]["format_version"], SESSION_FORMAT_VERSION);
        for line in &lines[1..] {
            assert_eq!(line.get("format_version"), None, "{line}");
        }
        // A resumed writer appends to a file that already opened with its
        // version, and must not stamp a second one.
        drop(rollout);
        let mut resumed = resume_session(&path).unwrap().rollout;
        resumed
            .append_message(&Message::user_text("three"))
            .unwrap();
        let lines = raw_lines(&path);
        assert_eq!(lines[0]["format_version"], SESSION_FORMAT_VERSION);
        for line in &lines[1..] {
            assert_eq!(line.get("format_version"), None, "{line}");
        }
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

    /// A claimed file is the new session's own: saying nothing still leaves
    /// nothing behind, and saying something keeps it.
    #[test]
    fn a_claimed_session_is_removed_only_while_it_holds_nothing() {
        let path = temp_file("claimed-owner");
        let dir = path.parent().unwrap().to_path_buf();
        let (_, route) = crate::provider_route::ProviderCatalog::from_provider(
            "mock",
            kloop_provider::Provider::mock(Vec::new()),
            "mock",
            vec!["mock".into()],
        )
        .unwrap();

        let claim = claim_new_session(&dir).unwrap();
        let quiet = claim.path().to_path_buf();
        drop(Rollout::claimed_with_initial_route(claim, &route).unwrap());
        assert!(!quiet.exists());

        let claim = claim_new_session(&dir).unwrap();
        let kept = claim.path().to_path_buf();
        let mut rollout = Rollout::claimed_with_initial_route(claim, &route).unwrap();
        rollout.append_message(&Message::user_text("hi")).unwrap();
        drop(rollout);
        assert_eq!(load_session_snapshot(&kept).unwrap().messages.len(), 1);
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
}
