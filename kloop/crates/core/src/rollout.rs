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
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;

use crate::inbox::STEERING_PREFIX;
use crate::tools::interrupted;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::Role;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntime {
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnTerminal {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotTerminal {
    pub after_message: usize,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot {
    pub messages: Vec<Message>,
    pub runtime: Option<SessionRuntime>,
    pub terminals: Vec<SnapshotTerminal>,
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
    Message {
        #[serde(flatten)]
        meta: LineMeta,
        #[serde(flatten)]
        message: Message,
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
}

impl RolloutLine {
    /// The line's metadata, common to every variant.
    fn meta(&self) -> &LineMeta {
        match self {
            RolloutLine::Session { meta, .. }
            | RolloutLine::Message { meta, .. }
            | RolloutLine::Compacted { meta, .. }
            | RolloutLine::TurnTerminal { meta, .. } => meta,
        }
    }

    /// Consume the line for its metadata, dropping the payload.
    fn into_meta(self) -> LineMeta {
        match self {
            RolloutLine::Session { meta, .. }
            | RolloutLine::Message { meta, .. }
            | RolloutLine::Compacted { meta, .. }
            | RolloutLine::TurnTerminal { meta, .. } => meta,
        }
    }
}

/// Append-only writer for one session file. The file (and its directory) is
/// created lazily on first append, so a session that never records anything
/// leaves nothing behind. Tracks the id chain: each line's `parent` is the
/// previous line's id.
pub struct Rollout {
    path: PathBuf,
    prefix: String,
    next_seq: u64,
    last_id: Option<String>,
    /// Set only for a sub-agent's rollout: stamped onto the FIRST appended
    /// line's envelope (`subagent_of`) and ignored thereafter.
    subagent_of: Option<String>,
}

impl Rollout {
    /// A fresh session: the id chain starts at `#1` with no parent.
    pub fn new(path: PathBuf) -> Self {
        Self::with_origin(path, None)
    }

    /// A fresh session whose runtime choices must survive process restarts.
    /// The metadata is written before the thread id is returned to a client.
    pub fn new_with_runtime(path: PathBuf, runtime: SessionRuntime) -> io::Result<Self> {
        let mut rollout = Self::new(path);
        rollout.append_runtime(&runtime)?;
        Ok(rollout)
    }

    /// A sub-agent's rollout: like [`Rollout::new`], but the first appended
    /// line records the parent turn (`{parent stem}#{seq}`) that spawned it.
    pub fn new_subagent(path: PathBuf, subagent_of: String) -> Self {
        Self::with_origin(path, Some(subagent_of))
    }

    fn with_origin(path: PathBuf, subagent_of: Option<String>) -> Self {
        let prefix = id_prefix(&path);
        Self {
            path,
            prefix,
            next_seq: 1,
            last_id: None,
            subagent_of,
        }
    }

    /// The id of the most recently appended line (`{stem}#{seq}`), or None if
    /// nothing has been written yet. A spawning parent uses this as the
    /// `subagent_of` back-pointer for the sub-agent it launches.
    pub fn last_id(&self) -> Option<&str> {
        self.last_id.as_deref()
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

    pub fn append_message(&mut self, message: &Message) -> io::Result<()> {
        self.append_line(RolloutLine::Message {
            meta: self.next_meta(),
            message: message.clone(),
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
        let json = serde_json::to_string(&line).map_err(io::Error::other)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{json}")?;
        // Only advance the chain once the line is durably in the file.
        self.last_id = Some(line.into_meta().id);
        self.next_seq += 1;
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
    runtime: Option<SessionRuntime>,
    terminals: Vec<SnapshotTerminal>,
    last_id: Option<String>,
    max_seq: u64,
    /// Byte offset just past the last intact line; anything after is a
    /// malformed or unterminated tail (crash mid-append).
    intact_end: usize,
}

/// Every intact line in file order, plus the byte offset just past the last
/// one; anything after that offset is a malformed or unterminated tail
/// (crash mid-append).
fn intact_lines(raw: &str) -> (Vec<RolloutLine>, usize) {
    let mut lines = Vec::new();
    let mut intact_end = 0;
    for line in raw.split_inclusive('\n') {
        // An unterminated final line is a torn write, never trustworthy.
        if !line.ends_with('\n') {
            break;
        }
        let content = line.trim();
        if !content.is_empty() {
            match serde_json::from_str(content) {
                Ok(parsed) => lines.push(parsed),
                Err(_) => break,
            }
        }
        intact_end += line.len();
    }
    (lines, intact_end)
}

fn seq_of(meta: &LineMeta) -> u64 {
    meta.id
        .rsplit('#')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn parse_session(raw: &str) -> ParsedSession {
    let (lines, intact_end) = intact_lines(raw);
    let mut parsed = ParsedSession {
        items: Vec::new(),
        runtime: None,
        terminals: Vec::new(),
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
            RolloutLine::Message { meta, message } => {
                parsed.items.push(message);
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
        };
        parsed.max_seq = parsed.max_seq.max(seq_of(&meta));
        parsed.last_id = Some(meta.id);
    }
    parsed
}

/// Read-only replay (used by `--list-sessions`): message lines append, a
/// compacted marker replaces everything read so far, a malformed tail is
/// ignored, and orphaned tool pairing is repaired in the returned history.
pub fn load_session(path: &Path) -> io::Result<Vec<Message>> {
    let raw = std::fs::read_to_string(path)?;
    Ok(repair_pairing(parse_session(&raw).items))
}

/// Read the persisted session for a history client. Runtime and terminal records
/// are display/recovery metadata only; provider replay still consumes only
/// `messages` through [`load_session`] / [`resume_session`].
pub fn load_session_snapshot(path: &Path) -> io::Result<SessionSnapshot> {
    let raw = std::fs::read_to_string(path)?;
    let parsed = parse_session(&raw);
    Ok(SessionSnapshot {
        messages: repair_pairing(parsed.items),
        runtime: parsed.runtime,
        terminals: parsed.terminals,
    })
}

/// Open a session for continuation: replay like [`load_session`], but also
/// TRUNCATE any torn tail off the file — appending after leftover partial
/// bytes would merge into them and make every later line unreadable — and
/// return a [`Rollout`] whose id chain continues where the file left off.
pub fn resume_session(path: &Path) -> io::Result<(Vec<Message>, Rollout)> {
    let raw = std::fs::read_to_string(path)?;
    let parsed = parse_session(&raw);
    if parsed.intact_end < raw.len() {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)?
            .set_len(parsed.intact_end as u64)?;
    }
    let rollout = Rollout {
        path: path.to_path_buf(),
        prefix: id_prefix(path),
        next_seq: parsed.max_seq + 1,
        last_id: parsed.last_id,
        // The first line (with any subagent_of) is already on disk; resumed
        // appends never sit at seq 1, so this is never consulted.
        subagent_of: None,
    };
    Ok((repair_pairing(parsed.items), rollout))
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
    let raw = std::fs::read_to_string(src)?;
    let (lines, _) = intact_lines(&raw);
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
            RolloutLine::Message { meta, message } => RolloutLine::Message {
                meta: remeta(meta),
                message,
            },
            RolloutLine::Compacted { meta, replacement } => RolloutLine::Compacted {
                meta: remeta(meta),
                replacement,
            },
            RolloutLine::TurnTerminal { meta, terminal } => RolloutLine::TurnTerminal {
                meta: remeta(meta),
                terminal,
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
        | RolloutLine::Compacted { .. }
        | RolloutLine::TurnTerminal { .. } => false,
    }
}

/// Seqs after which the file may be cut: every line whose successor starts
/// a fresh user turn, plus the last line.
fn legal_cut_seqs(lines: &[RolloutLine]) -> Vec<u64> {
    let mut seen_user_turn = false;
    let mut legal = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if opens_user_turn(line) {
            seen_user_turn = true;
        }
        let boundary = match lines.get(i + 1) {
            Some(next) => opens_user_turn(next),
            None => true,
        };
        if seen_user_turn && boundary {
            legal.push(seq_of(line.meta()));
        }
    }
    legal
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
    let raw = std::fs::read_to_string(path)?;
    let (lines, _) = intact_lines(&raw);
    let mut points = Vec::new();
    let mut seen_user_turn = false;
    for (i, line) in lines.iter().enumerate() {
        if opens_user_turn(line) {
            seen_user_turn = true;
        }
        // The tip has no successor: skip it (a no-op rewind).
        let Some(next) = lines.get(i + 1) else {
            continue;
        };
        if !seen_user_turn || !opens_user_turn(next) {
            continue;
        }
        let RolloutLine::Message { message, .. } = next else {
            continue;
        };
        points.push(ForkPoint {
            seq: seq_of(line.meta()),
            preview: user_turn_preview(message),
        });
    }
    Ok(points)
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
#[derive(Debug, PartialEq, Eq)]
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
    let meta = read_first_meta(path)?;
    match (meta.subagent_of, meta.parent) {
        (Some(sa), _) => Some(SessionOrigin::SubAgent(sa)),
        (None, Some(parent)) => Some(SessionOrigin::Fork(parent)),
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

/// Make the replayed history legal to send. Both directions, mirroring what
/// the live loop guarantees (cc's ensureToolResultPairing is also two-way):
/// a tool_result must answer a tool_use in the immediately preceding
/// assistant message — strays are dropped (a message stripped empty goes
/// entirely) — and every tool_use left unanswered gets the same is_error
/// result the interrupt path uses.
fn repair_pairing(items: Vec<Message>) -> Vec<Message> {
    // Reverse: drop tool_results that answer nothing.
    let mut repaired: Vec<Message> = Vec::with_capacity(items.len());
    for mut msg in items {
        let prev_uses = repaired.last().map(tool_use_ids).unwrap_or_default();
        msg.content.retain(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => prev_uses.contains(tool_use_id),
            _ => true,
        });
        if !msg.content.is_empty() {
            repaired.push(msg);
        }
    }

    // Forward: patch tool_uses left unanswered.
    let mut i = 0;
    while i < repaired.len() {
        let uses = tool_use_ids(&repaired[i]);
        if !uses.is_empty() {
            let answered: HashSet<String> = repaired
                .get(i + 1)
                .map(|next| {
                    next.content
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
            if !missing.is_empty() {
                if answered.is_empty() {
                    repaired.insert(i + 1, Message::tool_results(missing));
                } else {
                    // A partially written results message: complete it in
                    // place rather than splitting results across two messages.
                    repaired[i + 1].content.extend(missing);
                }
            }
        }
        i += 1;
    }
    repaired
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

/// All session files, most recently modified first (modified = last active,
/// which is what "continue the latest" should pick up).
pub fn sessions_by_recency(sessions_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(sessions_dir)
        .into_iter()
        .flatten()
        .flatten()
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
    for message in messages {
        if message.role != Role::User {
            continue;
        }
        for block in &message.content {
            if let ContentBlock::Text { text } = block {
                let mut snippet: String = text
                    .chars()
                    .take(60)
                    .map(|c| if c == '\n' { ' ' } else { c })
                    .collect();
                if text.chars().count() > 60 {
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
            Message::assistant(vec![
                ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: "sig".into(),
                },
                ContentBlock::RedactedThinking { data: "d".into() },
                tool_use("t1"),
            ]),
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
    fn envelope_forms_a_sequential_chain() {
        let path = temp_file("envelope");
        let mut rollout = Rollout::new(path.clone());
        rollout.append_message(&Message::user_text("one")).unwrap();
        rollout.append_message(&Message::user_text("two")).unwrap();
        rollout
            .append_compacted(&[Message::user_text("[summary]")])
            .unwrap();

        let lines = raw_lines(&path);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["id"], "session#1");
        assert_eq!(lines[0].get("parent"), None, "first line has no parent");
        assert_eq!(lines[1]["id"], "session#2");
        assert_eq!(lines[1]["parent"], "session#1");
        // The compacted marker participates in the same chain.
        assert_eq!(lines[2]["id"], "session#3");
        assert_eq!(lines[2]["parent"], "session#2");
        assert_eq!(lines[2]["type"], "compacted");
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

        let (messages, mut resumed) = resume_session(&path).unwrap();
        assert_eq!(messages.len(), 2);
        resumed
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
            r#"{{"type":"message","id":"session#2","parent":"session#1","ts":1,"future_field":42,"role":"user","content":[{{"type":"text","text":"new"}}]}}"#
        )
        .unwrap();

        assert_eq!(
            load_session(&path).unwrap(),
            vec![Message::user_text("old"), Message::user_text("new")]
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
        use crate::agent::{run_turn, EndReason, Ui};
        use crate::history::History;
        use crate::Config;
        use kloop_provider::Provider;
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;

        struct NullUi;
        impl Ui for NullUi {
            fn emit(&self, _: &crate::event::Event) {}
        }
        let cfg_with = |provider: Provider, dir: &Path| {
            Arc::new(Config {
                provider: Arc::new(provider),
                model: "mock".into(),
                system: "test".into(),
                project_instructions: None,
                max_rounds: Some(5),
                cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                offload_dir: dir.to_path_buf(),
                sessions_dir: dir.to_path_buf(),
                context_window: None,
                fallback_model: None,
                permissions: Arc::new(crate::permissions::Permissions::allow_all()),
                questioner: None,
                file_state: Default::default(),
                tool_sources: Vec::new(),
                session_id: String::new(),
                agent_label: String::new(),
                hooks: std::sync::Arc::new(crate::hooks::Hooks::none()),
                background_shells: crate::tools::BackgroundShells::new(),
                shell_programs: std::sync::Arc::new(
                    crate::shell_programs::ShellPrograms::test_fixture(),
                ),
                powershell_execution_gate: Default::default(),
                sandbox: None,
                agent_types: Arc::new(Vec::new()),
                tool_allowlist: None,
                defer_threshold: 30,
                unlocked_tools: Default::default(),
                todos: Default::default(),
                inbox: Default::default(),
                scheduler: crate::scheduler::Scheduler::in_memory(Default::default()),
                background_tasks: Default::default(),
                program_limits: Default::default(),
                skills: Default::default(),
                active_worktree: std::sync::Arc::new(
                    crate::worktree::ActiveWorktreeState::default(),
                ),
                surface: Default::default(),
            })
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
        let (resumed, rollout) = resume_session(&path).unwrap();
        assert_eq!(resumed, before_restart);
        let cfg = cfg_with(
            Provider::mock(vec![vec![kloop_protocol::AssistantBlock::Text {
                text: "it was kumquat".into(),
            }]]),
            &dir,
        );
        let mut history = History::resume(dir.clone(), resumed, rollout);
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

        let fork_path = fork_session(&path, Some(4), &dir).unwrap();
        let fork_stem = session_id_of(&fork_path);
        assert_ne!(fork_stem, "session");
        assert_eq!(load_session(&fork_path).unwrap(), messages[..4].to_vec());
        assert_eq!(fork_origin(&fork_path).unwrap(), "session#4");
        assert_eq!(fork_origin(&path), None, "fresh session has no origin");

        // Re-enveloped chain: new stem, seq from 1, first parent crosses
        // files, timestamps preserved from the source lines.
        let src_lines = raw_lines(&path);
        let lines = raw_lines(&fork_path);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0]["id"], format!("{fork_stem}#1"));
        assert_eq!(lines[0]["parent"], "session#4");
        assert_eq!(lines[1]["id"], format!("{fork_stem}#2"));
        assert_eq!(lines[1]["parent"], format!("{fork_stem}#1"));
        for (line, src) in lines.iter().zip(&src_lines) {
            assert_eq!(line["ts"], src["ts"], "history keeps its original time");
        }

        // Both branches keep appending without seeing each other.
        let (_, mut fork_rollout) = resume_session(&fork_path).unwrap();
        fork_rollout
            .append_message(&Message::user_text("fork branch"))
            .unwrap();
        let (_, mut src_rollout) = resume_session(&path).unwrap();
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
        assert_eq!(appended[4]["id"], format!("{fork_stem}#5"));
        assert_eq!(appended[4]["parent"], format!("{fork_stem}#4"));
        cleanup(&path);
    }

    #[test]
    fn fork_without_cut_copies_the_whole_session() {
        let path = temp_file("forkend");
        let dir = path.parent().unwrap().to_path_buf();
        let messages = seed_forkable(&path);
        let fork_path = fork_session(&path, None, &dir).unwrap();
        assert_eq!(load_session(&fork_path).unwrap(), messages);
        assert_eq!(fork_origin(&fork_path).unwrap(), "session#6");
        cleanup(&path);
    }

    #[test]
    fn illegal_cuts_are_rejected_with_nearby_legal_points() {
        let path = temp_file("forkbad");
        let dir = path.parent().unwrap().to_path_buf();
        seed_forkable(&path);
        // #2 would split the t1 tool exchange.
        let err = fork_session(&path, Some(2), &dir).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let msg = err.to_string();
        assert!(msg.contains("#4") && msg.contains("end = #6"), "{msg}");
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
        let at_marker = fork_session(&path, Some(5), &dir).unwrap();
        assert_eq!(load_session(&at_marker).unwrap(), replacement);
        // Cutting before compaction forks the raw history the marker later
        // superseded — those lines never left the file.
        let before = fork_session(&path, Some(2), &dir).unwrap();
        assert_eq!(load_session(&before).unwrap(), pre);
        // The line just before the marker is not a legal cut (its successor
        // is the marker, not a user turn).
        assert!(fork_session(&path, Some(4), &dir).is_err());
        cleanup(&path);
    }

    #[test]
    fn fork_of_a_fork_points_at_the_middle_file() {
        let path = temp_file("forkfork");
        let dir = path.parent().unwrap().to_path_buf();
        seed_forkable(&path);
        let first = fork_session(&path, Some(4), &dir).unwrap();
        let second = fork_session(&first, None, &dir).unwrap();
        let first_stem = session_id_of(&first);
        assert_eq!(fork_origin(&second).unwrap(), format!("{first_stem}#4"));
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
                seq: 4,
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
                seq: 3,
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
        let fork_path = fork_session(&path, Some(4), &dir).unwrap();

        // Resume both branches against the shared offload dir and spill from
        // each: the ids must never collide (counter is dir-global).
        let spill_from = |session: &Path| {
            let (messages, rollout) = resume_session(session).unwrap();
            let mut history = History::resume(dir.clone(), messages, rollout);
            history.record(Message::tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: "big".into(),
                content: "x".repeat(9_000).into(),
                is_error: false,
            }]));
            let ContentBlock::ToolResult { content, .. } =
                &history.messages().last().unwrap().content[0]
            else {
                panic!("expected tool result");
            };
            let content = content.as_text();
            let start = content.find("id=off-").expect("pointer has id") + 3;
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

    #[test]
    fn session_origin_distinguishes_fork_subagent_and_fresh() {
        let path = temp_file("origin");
        let dir = path.parent().unwrap().to_path_buf();
        seed_forkable(&path);

        // A fresh top-level session has no origin and is not a sub-agent.
        assert_eq!(session_origin(&path), None);
        assert!(!is_subagent_session(&path));

        // A fork: cross-file parent, no subagent_of → Fork.
        let fork_path = fork_session(&path, Some(4), &dir).unwrap();
        assert_eq!(
            session_origin(&fork_path),
            Some(SessionOrigin::Fork("session#4".into()))
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
            model: Some("test-model".into()),
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
            model: None,
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
        assert_eq!(fork_origin(&fork), Some("session#4".into()));
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

        let (messages, mut resumed) = resume_session(&path).unwrap();
        assert_eq!(messages, vec![Message::user_text("intact")]);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            good_len,
            "torn tail physically removed"
        );
        resumed
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
}
