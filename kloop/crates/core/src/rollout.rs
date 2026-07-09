//! Session persistence: one JSONL file per session. Message lines are
//! appended as the history records them; compaction — the one sanctioned
//! history rewrite — is persisted as an appended `compacted` marker carrying
//! the full replacement history, so the file itself stays append-only and
//! auditable. Replay swaps in the replacement and keeps reading.
//!
//! Every line carries an envelope (`id`, `parent`, `ts`): today replay is
//! linear and the chain is purely sequential, but the fields are the schema
//! foundation for rewind/forking later — adding them after files exist would
//! mean a format migration. Ids are `{file stem}#{seq}` — unique within the
//! file without a rand dependency. Unknown fields in a line are ignored on
//! read, so the format can grow additively.

use std::collections::HashSet;
use std::io;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;

use crate::tools::interrupted;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::Role;

#[derive(Serialize, Deserialize)]
struct LineMeta {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    /// Unix milliseconds at append time.
    ts: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RolloutLine {
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
}

impl Rollout {
    /// A fresh session: the id chain starts at `#1` with no parent.
    pub fn new(path: PathBuf) -> Self {
        let prefix = id_prefix(&path);
        Self {
            path,
            prefix,
            next_seq: 1,
            last_id: None,
        }
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

    fn next_meta(&self) -> LineMeta {
        LineMeta {
            id: format!("{}#{}", self.prefix, self.next_seq),
            parent: self.last_id.clone(),
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
        let (RolloutLine::Message { meta, .. } | RolloutLine::Compacted { meta, .. }) = line;
        self.last_id = Some(meta.id);
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
    last_id: Option<String>,
    max_seq: u64,
    /// Byte offset just past the last intact line; anything after is a
    /// malformed or unterminated tail (crash mid-append).
    intact_end: usize,
}

fn parse_session(raw: &str) -> ParsedSession {
    let mut parsed = ParsedSession {
        items: Vec::new(),
        last_id: None,
        max_seq: 0,
        intact_end: 0,
    };
    for line in raw.split_inclusive('\n') {
        // An unterminated final line is a torn write, never trustworthy.
        if !line.ends_with('\n') {
            break;
        }
        let content = line.trim();
        if content.is_empty() {
            parsed.intact_end += line.len();
            continue;
        }
        let meta = match serde_json::from_str(content) {
            Ok(RolloutLine::Message { meta, message }) => {
                parsed.items.push(message);
                meta
            }
            Ok(RolloutLine::Compacted { meta, replacement }) => {
                parsed.items = replacement;
                meta
            }
            Err(_) => break,
        };
        if let Some(seq) = meta.id.rsplit('#').next().and_then(|s| s.parse().ok()) {
            parsed.max_seq = parsed.max_seq.max(seq);
        }
        parsed.last_id = Some(meta.id);
        parsed.intact_end += line.len();
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
    };
    Ok((repair_pairing(parsed.items), rollout))
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
fn civil_from_days(days: i64) -> (i64, u32, u32) {
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
            Message::assistant(vec![tool_use("t1")]),
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
            fn text_delta(&self, _: &str) {}
            fn note(&self, _: &str) {}
        }
        let cfg_with = |provider: Provider, dir: &Path| {
            Arc::new(Config {
                provider: Arc::new(provider),
                model: "mock".into(),
                system: "test".into(),
                max_rounds: 5,
                offload_dir: dir.to_path_buf(),
                context_window: None,
                fallback_model: None,
                permissions: Arc::new(crate::permissions::Permissions::allow_all()),
                tool_sources: Vec::new(),
            })
        };
        let path = temp_file("restart");
        let dir = path.parent().unwrap().to_path_buf();
        let ui: Arc<dyn Ui> = Arc::new(NullUi);

        // First run: one completed turn, then the process "exits" (drop).
        let cfg = cfg_with(
            Provider::mock(vec![vec![ContentBlock::Text {
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
            Provider::mock(vec![vec![ContentBlock::Text {
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
