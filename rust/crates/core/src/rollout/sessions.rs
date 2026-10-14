use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::inbox::STEERING_PREFIX;
use crate::request_reduction::FrozenStub;
use crate::usage::UsageLedger;
use kloop_protocol::{ContentBlock, Injected, Message, Role};

use super::line::{LineMeta, RolloutLine};
use super::validate::{intact_lines, parse_session, seq_of};
use super::{
    PairingRepairStats, Rollout, SESSION_FORMAT_VERSION, SessionSnapshot, SnapshotTerminal,
    id_prefix,
};

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PairingRepair {
    pub(super) messages: Vec<Message>,
    pub(super) terminals: Vec<SnapshotTerminal>,
    pub(super) stats: PairingRepairStats,
    changed: bool,
}

/// The result of a single, strictly read-only session inspection. The same
/// result is consumed by list/read/seed callers or by an explicit recovery.
pub struct SessionRead {
    snapshot: SessionSnapshot,
    provider_usage: UsageLedger,
    request_stubs: Vec<FrozenStub>,
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

    /// Take this session's writer lease, then perform the only on-disk recovery
    /// operation: truncate an intact prefix and, when necessary, append one
    /// canonical pairing marker.
    ///
    /// The lease is why this can fail on a transcript that reads perfectly
    /// well: one writer at a time, so a session another process currently has
    /// open comes back as an error rather than as a second writer. Read-only
    /// callers ([`load_session`], [`load_session_snapshot`], [`inspect_session`])
    /// are the ones that work on a live session.
    pub fn recover_for_writing(self) -> io::Result<ResumedSession> {
        let lease = crate::state_lock::StateLock::try_acquire(&self.path.with_extension("lock"))
            .map_err(|error| {
                io::Error::other(format!(
                    "session is already active or cannot be locked: {error}"
                ))
            })?;
        inspect_session(&self.path)?.recover_locked(lease)
    }

    fn recover_locked(self, lease: crate::state_lock::StateLock) -> io::Result<ResumedSession> {
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
            lease: Some(lease),
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
            request_stubs: self.request_stubs,
            snapshot: self.snapshot,
            repair: self.repair.stats,
            rollout,
        })
    }
}

pub fn inspect_session(path: &Path) -> io::Result<SessionRead> {
    let raw = std::fs::read(path)?;
    let parsed = parse_session(&raw)?;
    let repair = repair_pairing(parsed.items, parsed.terminals, &parsed.started);
    let snapshot = SessionSnapshot {
        messages: repair.messages.clone(),
        runtime: parsed.runtime,
        terminals: repair.terminals.clone(),
        provider_routes: parsed.route_timeline,
    };
    Ok(SessionRead {
        snapshot,
        provider_usage: parsed.provider_usage,
        request_stubs: parsed.request_stubs,
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
    /// Stubs recorded since the last compaction (plan 200).
    pub(crate) request_stubs: Vec<FrozenStub>,
    pub snapshot: SessionSnapshot,
    pub repair: PairingRepairStats,
    pub rollout: Rollout,
}

/// Open a session for continuation. Unlike the read-only load functions this
/// explicitly truncates torn tail bytes and persists canonical pairing repair —
/// and it takes the session's writer lease, so a session another process
/// currently has open cannot be resumed here.
pub fn resume_session(path: &Path) -> io::Result<ResumedSession> {
    inspect_session(path)?.recover_for_writing()
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

    let claim = claim_new_session(sessions_dir)?;
    let written =
        fork_lines(lines, cut, src, claim.path()).and_then(|out| std::fs::write(claim.path(), out));
    if let Err(error) = written {
        // The claimed file is this call's own and holds nothing usable.
        let _ = std::fs::remove_file(claim.path());
        return Err(error);
    }
    Ok(claim.path)
}

/// The kept prefix, re-enveloped under the fork's own stem.
fn fork_lines(lines: Vec<RolloutLine>, cut: u64, src: &Path, path: &Path) -> io::Result<String> {
    let prefix = id_prefix(path);
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
                // The copied prefix is renumbered, so the version rides
                // whichever line became this file's first one.
                format_version: (n == 0).then_some(SESSION_FORMAT_VERSION),
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
            RolloutLine::RequestStub { meta, stub } => RolloutLine::RequestStub {
                meta: remeta(meta),
                stub,
            },
            RolloutLine::ToolStarted { meta, tool_use_id } => RolloutLine::ToolStarted {
                meta: remeta(meta),
                tool_use_id,
            },
        };
        out.push_str(&serde_json::to_string(&line).map_err(io::Error::other)?);
        out.push('\n');
    }
    Ok(out)
}

/// A line that begins a fresh user turn: a plain user message (a tool_result
/// carrier is the tail of the previous turn, not a new one). This is the cut
/// boundary both `legal_cut_seqs` and `fork_points` key off, so a fork point
/// the picker offers is always one `fork_session` will accept.
///
/// A rewind's branch summary is not one: it opens the branch, and the user's
/// next message is the turn. Rewinding to that turn keeps the summary, and
/// the picker previews what the user said rather than the summary's framing.
fn opens_user_turn(line: &RolloutLine) -> bool {
    match line {
        RolloutLine::Message { message, .. } => {
            message.role == Role::User
                && message.injected != Some(Injected::BranchSummary)
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
        | RolloutLine::Repaired { .. }
        | RolloutLine::RequestStub { .. }
        | RolloutLine::ToolStarted { .. } => false,
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
/// entirely) — and every tool_use left unanswered gets an is_error result
/// saying which way the session died around it: `started` holds the calls
/// that got past their gates (plan 204), the only ones that may have acted.
fn repair_pairing(
    items: Vec<Message>,
    terminals: Vec<SnapshotTerminal>,
    started: &HashSet<String>,
) -> PairingRepair {
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
            let missing: Vec<ContentBlock> = repaired[i]
                .message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, .. } if !answered.contains(id) => {
                        Some(unfinished_result(id, started.contains(id)))
                    }
                    _ => None,
                })
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

/// What a crash left a call that never reported back. The two texts differ
/// in what they tell the model to do next: a call that never started can be
/// repeated as is; one that started may have acted, fully or partly, and the
/// state has to be checked before repeating it. Both halves of that second
/// instruction were measured against a real model (plan 204 §九): without
/// "check" it reran blind, without "finish only what remains" it checked,
/// saw the half-done work, and reran the whole call anyway. Only calls that may have
/// side effects are ever recorded as started — a read that died midway is
/// reported as not run, which is just as safe to act on.
fn unfinished_result(tool_use_id: &str, started: bool) -> ContentBlock {
    let content = if started {
        "interrupted: the session ended while this call was running. It may have taken \
         effect, fully or partly. Check the current state before repeating it. If it took \
         effect partly, finish only what remains instead of repeating it."
    } else {
        "not run: the session ended before this call started. Nothing happened; \
         calling it again is safe."
    };
    ContentBlock::ToolResult {
        tool_use_id: tool_use_id.into(),
        content: content.into(),
        is_error: true,
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

/// A new session's id, held by having created its file (empty) under that id.
#[derive(Debug)]
pub struct ClaimedSession {
    id: String,
    pub(super) path: PathBuf,
}

impl ClaimedSession {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Session ids are UTC wall-clock timestamps — readable, sortable, and free
/// of a rand dependency. A collision within one second gets a numeric suffix.
///
/// Picking the id and creating its file are one step (`create_new`), because
/// as two they raced: two processes starting in the same second both saw the
/// id free, and then a fork failed on `File exists` while two fresh sessions
/// silently appended into one file — and when both writers had checked
/// before either wrote, each thought the file its own, so the one that quit
/// without a word could delete the other's conversation. Now the loser of
/// the creation moves on to the next suffix.
pub fn claim_new_session(sessions_dir: &Path) -> io::Result<ClaimedSession> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    claim_session_from(sessions_dir, &super::timestamp_id(secs))
}

fn claim_session_from(sessions_dir: &Path, base: &str) -> io::Result<ClaimedSession> {
    std::fs::create_dir_all(sessions_dir)?;
    let mut id = base.to_string();
    let mut n = 2;
    loop {
        let path = session_path(sessions_dir, &id);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(ClaimedSession { id, path }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                id = format!("{base}-{n}");
                n += 1;
            }
            Err(error) => return Err(error),
        }
    }
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
/// `-agent-*` — so this rejects nothing kloop produces.
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
    use super::super::testutil::*;
    use super::super::{SessionRuntime, TurnError, TurnTerminal};
    use super::*;
    use kloop_protocol::{AssistantOutcome, IncompleteReason};
    use kloop_provider::ProviderFailure;
    use serde_json::{Value, json};

    #[test]
    fn a_live_session_can_be_inspected_but_cannot_get_a_second_writer() {
        let dir =
            std::env::temp_dir().join(crate::resource_id::fresh("kloop-session-lease-").unwrap());
        let path = dir.join("session.jsonl");
        let mut writer = Rollout::new(path.clone());
        let message = Message::user_text("owned by the first writer");
        writer.append_message(&message).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert_eq!(
            inspect_session(&path)
                .unwrap()
                .snapshot()
                .messages
                .as_slice(),
            std::slice::from_ref(&message)
        );
        assert!(resume_session(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        drop(writer);
        let resumed = resume_session(&path).unwrap();
        assert_eq!(resumed.messages, [message]);
        drop(resumed);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn timestamp_ids_match_utc_civil_time() {
        assert_eq!(timestamp_id(0), "19700101-000000");
        // date -u -r 1783958400 → 2026-07-13 16:00:00 UTC
        assert_eq!(timestamp_id(1_783_958_400), "20260713-160000");
        // leap-year day: 2024-02-29 12:34:56 UTC
        assert_eq!(timestamp_id(1_709_209_496), "20240229-122456");
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

    /// Plan 200: stubs ride beside the conversation, never in it; a compacted
    /// marker voids those before it, and a fork keeps those it copies.
    #[test]
    fn request_stubs_replay_beside_the_messages_until_a_compaction() {
        let path = temp_file("request-stubs");
        let stub = |id: &str| FrozenStub {
            tool_use_id: id.into(),
            stub: format!("[stub of {id}]"),
            saved_tokens: 7,
            recall: None,
        };
        let mut rollout = Rollout::new(path.clone());
        let conversation = vec![
            Message::user_text("go"),
            Message::assistant(vec![tool_use("t1")]),
            Message::tool_results(vec![tool_result("t1")]),
        ];
        for message in &conversation {
            rollout.append_message(message).unwrap();
        }
        rollout.append_request_stub(&stub("t1")).unwrap();
        rollout.append_message(&Message::user_text("next")).unwrap();
        let stub_seq = raw_lines(&path)
            .iter()
            .find(|line| line["type"] == "request_stub")
            .and_then(|line| line["id"].as_str()?.rsplit_once('#')?.1.parse().ok())
            .unwrap();

        drop(rollout);
        let resumed = resume_session(&path).unwrap();
        assert_eq!(resumed.request_stubs, vec![stub("t1")]);
        let mut with_next = conversation.clone();
        with_next.push(Message::user_text("next"));
        assert_eq!(resumed.messages, with_next);
        drop(resumed);

        let sessions = path.with_extension("forks");
        let fork = fork_session(&path, Some(stub_seq), &sessions).unwrap();
        let forked = resume_session(&fork).unwrap();
        assert_eq!(forked.request_stubs, vec![stub("t1")]);
        assert_eq!(forked.messages, conversation);
        drop(forked);

        let mut rollout = resume_session(&path).unwrap().rollout;
        rollout
            .append_compacted(&[Message::user_text("[summary]")])
            .unwrap();
        rollout.append_request_stub(&stub("t2")).unwrap();
        drop(rollout);
        assert_eq!(
            resume_session(&path).unwrap().request_stubs,
            vec![stub("t2")]
        );
        cleanup(&path);
        let _ = std::fs::remove_dir_all(sessions);
    }

    #[test]
    fn orphaned_tool_use_that_never_started_gets_not_run() {
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
            Message::tool_results(vec![unfinished_result("t1", false)])
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
            Message::tool_results(vec![tool_result("t1"), unfinished_result("t2", false),])
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
            Message::tool_results(vec![tool_result("t1"), unfinished_result("t2", false),]),
            "ghost dropped, t2 patched, t1 kept"
        );
        cleanup(&path);
    }

    /// Plan 204: a round of three side-effecting calls, killed while the
    /// second ran. Results are recorded for the round as a whole, so the
    /// first — which finished — has none either; what tells the three apart
    /// is only which ones got as far as starting.
    #[test]
    fn a_crash_mid_round_tells_started_calls_from_ones_that_never_ran() {
        let dir = temp_file("started").parent().unwrap().to_path_buf();
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("session.jsonl");
        let mut rollout = Rollout::new(path.clone());
        rollout
            .append_message(&Message::user_text("do three things"))
            .unwrap();
        let assistant = Message::assistant(vec![tool_use("w1"), tool_use("w2"), tool_use("w3")]);
        rollout.append_message(&assistant).unwrap();
        rollout.append_tool_started("w1").unwrap();
        rollout.append_tool_started("w2").unwrap();
        drop(rollout);

        let expected = vec![
            Message::user_text("do three things"),
            assistant,
            Message::tool_results(vec![
                unfinished_result("w1", true),
                unfinished_result("w2", true),
                unfinished_result("w3", false),
            ]),
        ];
        assert_eq!(
            expected[2].content[0],
            ContentBlock::ToolResult {
                tool_use_id: "w1".into(),
                content: "interrupted: the session ended while this call was running. It may \
                          have taken effect, fully or partly. Check the current state before \
                          repeating it. If it took effect partly, finish only what remains \
                          instead of repeating it."
                    .into(),
                is_error: true,
            }
        );
        assert_eq!(
            expected[2].content[2],
            ContentBlock::ToolResult {
                tool_use_id: "w3".into(),
                content: "not run: the session ended before this call started. Nothing \
                          happened; calling it again is safe."
                    .into(),
                is_error: true,
            }
        );

        // A fork copies the started lines with the prefix, so its own resume
        // repairs the same way.
        let fork = fork_session(&path, None, &dir).unwrap();
        assert_eq!(resume_session(&fork).unwrap().messages, expected, "fork");

        assert_eq!(resume_session(&path).unwrap().messages, expected, "resume");
        // The repair marker persisted by that resume carries the texts; a
        // second resume reads them back instead of repairing again.
        assert_eq!(resume_session(&path).unwrap().messages, expected, "again");
        let kinds: Vec<Value> = raw_lines(&path)
            .into_iter()
            .map(|line| line["type"].clone())
            .collect();
        assert_eq!(
            kinds,
            vec![
                json!("provider_route_initial"),
                json!("message"),
                json!("message"),
                json!("tool_started"),
                json!("tool_started"),
                json!("repaired"),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
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
        // The renumbered prefix carries the version on the fork's own first
        // line, not on whichever copied line happened to hold it.
        assert_eq!(lines[0]["format_version"], SESSION_FORMAT_VERSION);
        for line in &lines[1..] {
            assert_eq!(line.get("format_version"), None, "{line}");
        }
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
        drop(rollout);
        let forked = resume_session(&fork_path).unwrap();
        assert_eq!(
            forked.provider_usage.records(),
            &[usage_record("primary", 10)]
        );
        assert_eq!(
            resume_session(&path).unwrap().provider_usage.records(),
            &[usage_record("primary", 10), usage_record("fallback", 20),]
        );
        drop(forked);
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

    /// A rewind's summary opens its branch but is not a turn: the user's next
    /// message is, so rewinding there keeps the summary, and the picker shows
    /// what the user said rather than the summary's framing.
    #[test]
    fn a_branch_summary_is_not_a_turn_the_picker_offers() {
        let path = temp_file("forkpointsbranch");
        let mut rollout = Rollout::new(path.clone());
        for m in [
            Message::user_text("start"),
            Message::assistant(vec![ContentBlock::Text { text: "ok".into() }]),
            Message::injected(Injected::BranchSummary, "[rewound] what was learned"),
            Message::user_text("a different way"),
            Message::assistant(vec![ContentBlock::Text {
                text: "done".into(),
            }]),
        ] {
            rollout.append_message(&m).unwrap();
        }
        assert_eq!(
            fork_points(&path).unwrap(),
            vec![ForkPoint {
                seq: 4,
                preview: "a different way".into(),
            }]
        );
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
        // each: exclusive creation must preserve both branches’ output.
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
            content[start..].split_once(".txt").unwrap().0.to_string()
        };
        let main_id = spill_from(&path);
        let fork_id = spill_from(&fork_path);
        assert_ne!(main_id, fork_id);
        assert!(dir.join(format!("{main_id}.txt")).exists());
        assert!(dir.join(format!("{fork_id}.txt")).exists());
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

        drop(rollout);
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

        drop(rollout);
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

        drop(rollout);
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

        drop(rollout);
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

        drop(rollout);
        let resumed = inspected.recover_for_writing().unwrap();
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
        let resumed = inspected.recover_for_writing().unwrap();
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

    /// Processes that start in the same second race for one id. Creating the
    /// file is the claim, so every racer ends up with its own id and its own
    /// file — under the old "is it free?" check they all got the same one.
    #[test]
    fn racing_claims_in_one_second_never_share_an_id() {
        let path = temp_file("claim-race");
        let dir = path.parent().unwrap().to_path_buf();
        let base = "20260930-120000";
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(session_path(&dir, base), b"").unwrap();
        let racers = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(racers));
        let handles: Vec<_> = (0..racers)
            .map(|_| {
                let (dir, barrier) = (dir.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    claim_session_from(&dir, base).unwrap().id().to_string()
                })
            })
            .collect();
        let mut ids: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        ids.sort_by_key(|id| id.rsplit('-').next().unwrap().parse::<u32>().unwrap());

        let expected: Vec<String> = (2..=racers + 1).map(|n| format!("{base}-{n}")).collect();
        assert_eq!(
            ids, expected,
            "the taken base id is skipped, the rest are shared out"
        );
        assert!(ids.iter().all(|id| session_path(&dir, id).exists()));
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
        drop(rollout);
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
        drop(rollout);
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
            claim_new_session(dir).unwrap().id(),
            "20260831-083106",
            "20260831-083106-2",
            "20260831-083106-agent-1",
        ] {
            assert!(checked_session_path(dir, id).is_ok(), "rejected {id:?}");
        }
        #[cfg(unix)]
        {
            let target = dir.join("outside.jsonl");
            std::fs::write(&target, b"not a session").unwrap();
            let leaf = dir.join("link.jsonl");
            std::os::unix::fs::symlink(&target, &leaf).unwrap();
            assert!(checked_session_path(dir, "link").is_err());
        }
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
