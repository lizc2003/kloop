use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use crate::types::ContentBlock;
use crate::types::Message;

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
}

impl History {
    pub fn new(offload_dir: PathBuf) -> Self {
        Self {
            items: Vec::new(),
            offload_dir,
            cap: 8000,
            usage_anchor: None,
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
        self.items.push(msg);
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

/// ~4 chars/token heuristic over the serialized wire form, ceiling division.
pub fn estimate_message_tokens(message: &Message) -> u64 {
    let bytes = serde_json::to_string(message).map_or(0, |s| s.len());
    (bytes as u64).div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Role;

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
