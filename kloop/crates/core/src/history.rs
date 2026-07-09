use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use crate::rollout::Rollout;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;

/// Offload ids are process-global so a sub-agent's spills never clobber the
/// parent's files in the shared offload directory.
static NEXT_OFFLOAD_ID: AtomicUsize = AtomicUsize::new(1);

const HEAD_CHARS: usize = 1500;
const TAIL_CHARS: usize = 500;

/// Append-only conversation history. Oversized tool results are offloaded to
/// disk at record time; the history keeps a preview plus a pointer the model
/// can dereference with the `read_offloaded` tool.
pub struct History {
    items: Vec<Message>,
    offload_dir: PathBuf,
    cap: usize,
    /// (items recorded at that point, total context tokens the provider
    /// reported for the request covering them). Anchors the estimate.
    usage_anchor: Option<(usize, u64)>,
    /// Session file written through on every record/replace_all; None for
    /// in-memory-only histories (sub-agents, tests).
    rollout: Option<Rollout>,
}

impl History {
    pub fn new(offload_dir: PathBuf) -> Self {
        Self {
            items: Vec::new(),
            offload_dir,
            cap: 8000,
            usage_anchor: None,
            rollout: None,
        }
    }

    pub fn attach_rollout(&mut self, rollout: Rollout) {
        self.rollout = Some(rollout);
    }

    /// Resume a persisted session: the items were already offloaded when
    /// first recorded, so they are installed verbatim — no re-spill, and no
    /// re-append to the session file. The usage anchor starts empty and
    /// re-anchors on the first sampled response.
    pub fn resume(offload_dir: PathBuf, items: Vec<Message>, rollout: Rollout) -> Self {
        sync_offload_counter(&offload_dir);
        Self {
            items,
            offload_dir,
            cap: 8000,
            usage_anchor: None,
            rollout: Some(rollout),
        }
    }

    pub fn record(&mut self, mut msg: Message) {
        for block in &mut msg.content {
            if let ContentBlock::ToolResult { content, .. } = block {
                if content.chars().count() > self.cap {
                    *content = self.spill(content);
                }
            }
        }
        self.persist(|rollout| rollout.append_message(&msg));
        self.items.push(msg);
    }

    /// Persistence must never take down the live session: a failed write
    /// drops the rollout and the session continues in memory only.
    fn persist(&mut self, write: impl FnOnce(&mut Rollout) -> std::io::Result<()>) {
        let Some(rollout) = &mut self.rollout else {
            return;
        };
        if let Err(e) = write(rollout) {
            eprintln!("[session persistence failed ({e}); continuing without it]");
            self.rollout = None;
        }
    }

    pub fn messages(&self) -> &[Message] {
        &self.items
    }

    /// Record the provider-reported total context size (input + output) for
    /// the request whose response is the most recently recorded item.
    pub fn note_usage(&mut self, total_tokens: u64) {
        self.usage_anchor = Some((self.items.len(), total_tokens));
    }

    /// Current context size: the last real usage anchor plus a ~4 chars/token
    /// estimate for everything recorded after it.
    pub fn estimated_tokens(&self) -> u64 {
        let (anchored_len, anchored_tokens) = self.usage_anchor.unwrap_or((0, 0));
        let tail: u64 = self.items[anchored_len.min(self.items.len())..]
            .iter()
            .map(estimate_message_tokens)
            .sum();
        anchored_tokens + tail
    }

    /// Compaction is the one sanctioned rewrite of the otherwise append-only
    /// history. The usage anchor no longer describes the new items, so it is
    /// dropped and the estimate runs purely on the char heuristic until the
    /// next sampled response re-anchors it.
    pub fn replace_all(&mut self, items: Vec<Message>) {
        self.persist(|rollout| rollout.append_compacted(&items));
        self.items = items;
        self.usage_anchor = None;
    }

    fn spill(&mut self, content: &str) -> String {
        let id = format!("off-{:04}", NEXT_OFFLOAD_ID.fetch_add(1, Ordering::Relaxed));
        let head: String = content.chars().take(HEAD_CHARS).collect();
        let tail_rev: Vec<char> = content.chars().rev().take(TAIL_CHARS).collect();
        let tail: String = tail_rev.into_iter().rev().collect();
        let write = std::fs::create_dir_all(&self.offload_dir)
            .and_then(|_| std::fs::write(self.offload_dir.join(format!("{id}.txt")), content));
        let pointer = match write {
            Ok(()) => {
                format!("[full output offloaded, id={id}, use the read_offloaded tool to fetch it]")
            }
            Err(e) => format!("[offload to disk failed ({e}); output truncated]"),
        };
        format!("{head}\n…[truncated]…\n{tail}\n{pointer}")
    }
}

/// A resumed session shares the offload dir with the files its earlier run
/// spilled, but the process-global counter restarts at 1 — advance it past
/// every id already on disk so new spills never clobber old files.
pub fn sync_offload_counter(offload_dir: &Path) {
    let mut max_seen = 0;
    if let Ok(entries) = std::fs::read_dir(offload_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let id = name
                .to_str()
                .and_then(|n| n.strip_prefix("off-"))
                .and_then(|n| n.strip_suffix(".txt"))
                .and_then(|n| n.parse::<usize>().ok());
            if let Some(id) = id {
                max_seen = max_seen.max(id);
            }
        }
    }
    NEXT_OFFLOAD_ID.fetch_max(max_seen + 1, Ordering::Relaxed);
}

/// ~4 chars/token heuristic over the serialized wire form, ceiling division.
pub fn estimate_message_tokens(message: &Message) -> u64 {
    let bytes = serde_json::to_string(message).map_or(0, |s| s.len());
    (bytes as u64).div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kloop_protocol::Role;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kloop-test-{}-{tag}", std::process::id()))
    }

    fn tool_result(content: String) -> Message {
        Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content,
            is_error: false,
        }])
    }

    #[test]
    fn oversized_tool_result_is_offloaded_with_pointer() {
        let dir = temp_dir("spill");
        let mut h = History::new(dir.clone());
        let big = "x".repeat(9000);
        h.record(tool_result(big.clone()));

        let ContentBlock::ToolResult { content, .. } = &h.messages()[0].content[0] else {
            panic!("expected tool result");
        };
        assert!(content.chars().count() < 9000);
        assert!(content.contains("…[truncated]…"));
        assert!(content.contains("read_offloaded"));
        let id_start = content.find("id=off-").expect("pointer has id") + 3;
        let id = &content[id_start..id_start + 8];
        let on_disk = std::fs::read_to_string(dir.join(format!("{id}.txt"))).unwrap();
        assert_eq!(on_disk, big);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn estimated_tokens_anchor_math() {
        let mut h = History::new(temp_dir("anchor"));
        h.record(Message::user_text("earlier message"));
        // Provider reports the real context size for everything so far.
        h.note_usage(1_000);
        assert_eq!(h.estimated_tokens(), 1_000);
        // Items after the anchor add their char-heuristic estimate on top.
        let tail = Message::user_text("x".repeat(400));
        let tail_estimate = estimate_message_tokens(&tail);
        h.record(tail.clone());
        assert_eq!(h.estimated_tokens(), 1_000 + tail_estimate);
        // Compaction (replace_all) invalidates the anchor: pure estimate again.
        h.replace_all(vec![tail.clone()]);
        assert_eq!(h.estimated_tokens(), tail_estimate);
    }

    #[test]
    fn offload_ids_unique_across_histories() {
        // Parent and sub-agent share the offload dir; the process-global
        // counter must keep their spill files from clobbering each other.
        let dir = temp_dir("shared");
        let mut a = History::new(dir.clone());
        let mut b = History::new(dir.clone());
        a.record(tool_result("a".repeat(9_000)));
        b.record(tool_result("b".repeat(9_000)));
        let id_of = |h: &History| {
            let ContentBlock::ToolResult { content, .. } = &h.messages()[0].content[0] else {
                panic!("expected tool result");
            };
            let start = content.find("id=off-").expect("pointer has id") + 3;
            content[start..start + 8].to_string()
        };
        assert_ne!(id_of(&a), id_of(&b));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_oversized_blocks_in_a_message_are_offloaded() {
        let mut h = History::new(temp_dir("mixed"));
        h.record(Message::tool_results(vec![
            ContentBlock::ToolResult {
                tool_use_id: "small".into(),
                content: "tiny".into(),
                is_error: false,
            },
            ContentBlock::ToolResult {
                tool_use_id: "big".into(),
                content: "z".repeat(9_000),
                is_error: false,
            },
        ]));
        let ContentBlock::ToolResult { content, .. } = &h.messages()[0].content[0] else {
            panic!()
        };
        assert_eq!(content, "tiny");
        let ContentBlock::ToolResult { content, .. } = &h.messages()[0].content[1] else {
            panic!()
        };
        assert!(content.contains("read_offloaded"));
    }

    #[test]
    fn record_and_replace_all_write_through_to_rollout() {
        let dir = temp_dir("writethrough");
        let session = dir.join("session.jsonl");
        let mut h = History::new(dir.clone());
        h.attach_rollout(Rollout::new(session.clone()));
        h.record(Message::user_text("first"));
        h.record(Message::assistant(vec![ContentBlock::Text {
            text: "reply".into(),
        }]));
        h.replace_all(vec![Message::user_text("[summary]")]);
        h.record(Message::user_text("after compaction"));

        // The file replays to exactly the in-memory history, compaction included.
        let loaded = crate::rollout::load_session(&session).unwrap();
        assert_eq!(loaded, h.messages());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn resumed_history_installs_items_without_reappending() {
        let dir = temp_dir("resume");
        let session = dir.join("session.jsonl");
        let mut rollout = Rollout::new(session.clone());
        rollout
            .append_message(&Message::user_text("earlier"))
            .unwrap();
        drop(rollout);

        let (items, resumed) = crate::rollout::resume_session(&session).unwrap();
        let mut h = History::resume(dir.clone(), items, resumed);
        assert_eq!(h.messages(), &[Message::user_text("earlier")]);
        // New records append after the resumed content, once each.
        h.record(Message::user_text("later"));
        assert_eq!(
            crate::rollout::load_session(&session).unwrap(),
            h.messages()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn synced_offload_counter_never_clobbers_existing_files() {
        let dir = temp_dir("counter");
        std::fs::create_dir_all(&dir).unwrap();
        // A file left behind by the session being resumed.
        std::fs::write(dir.join("off-0007.txt"), "old spill").unwrap();
        sync_offload_counter(&dir);

        let mut h = History::new(dir.clone());
        h.record(tool_result("n".repeat(9_000)));
        assert_eq!(
            std::fs::read_to_string(dir.join("off-0007.txt")).unwrap(),
            "old spill",
            "the resumed session's spill file must survive new spills"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn small_tool_result_kept_verbatim() {
        let mut h = History::new(temp_dir("small"));
        h.record(tool_result("hello".into()));
        assert_eq!(
            h.messages()[0],
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "hello".into(),
                    is_error: false,
                }],
            }
        );
    }
}
