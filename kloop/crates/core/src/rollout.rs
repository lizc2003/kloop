//! Session persistence: one JSONL file per session. Message lines are
//! appended as the history records them; compaction — the one sanctioned
//! history rewrite — is persisted as an appended `compacted` marker carrying
//! the full replacement history, so the file itself stays append-only and
//! auditable. Replay swaps in the replacement and keeps reading.

use std::collections::HashSet;
use std::io;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::tools::interrupted;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::Role;

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RolloutLine {
    Message(Message),
    Compacted { replacement: Vec<Message> },
}

/// Append-only writer for one session file. The file (and its directory) is
/// created lazily on first append, so a session that never records anything
/// leaves nothing behind.
pub struct Rollout {
    path: PathBuf,
}

impl Rollout {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn append_message(&self, message: &Message) -> io::Result<()> {
        self.append_line(&RolloutLine::Message(message.clone()))
    }

    pub fn append_compacted(&self, replacement: &[Message]) -> io::Result<()> {
        self.append_line(&RolloutLine::Compacted {
            replacement: replacement.to_vec(),
        })
    }

    fn append_line(&self, line: &RolloutLine) -> io::Result<()> {
        let json = serde_json::to_string(line).map_err(io::Error::other)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{json}")
    }
}

/// Replay a session file into the effective history: message lines append, a
/// compacted marker replaces everything read so far. Reading stops at the
/// first malformed line — a crash mid-append can only corrupt the tail, so
/// the intact prefix is kept instead of failing the resume. The replayed
/// history then gets orphaned tool_use blocks patched so it is legal to send.
pub fn load_session(path: &Path) -> io::Result<Vec<Message>> {
    let raw = std::fs::read_to_string(path)?;
    let mut items = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(RolloutLine::Message(message)) => items.push(message),
            Ok(RolloutLine::Compacted { replacement }) => items = replacement,
            Err(_) => break,
        }
    }
    Ok(repair_orphans(items))
}

/// A session killed between recording an assistant message and its tool
/// results leaves orphaned tool_use blocks, which both provider wire formats
/// reject. Patch every unanswered id with the same is_error result that
/// dispatch_tools uses on interrupt.
fn repair_orphans(mut items: Vec<Message>) -> Vec<Message> {
    let mut i = 0;
    while i < items.len() {
        let uses: Vec<String> = if items[i].role == Role::Assistant {
            items[i]
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect()
        } else {
            Vec::new()
        };
        if !uses.is_empty() {
            let answered: HashSet<String> = items
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
                    items.insert(i + 1, Message::tool_results(missing));
                } else {
                    // A partially written results message: complete it in
                    // place rather than splitting results across two messages.
                    items[i + 1].content.extend(missing);
                }
            }
        }
        i += 1;
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    #[test]
    fn roundtrip_appended_messages() {
        let path = temp_file("roundtrip");
        let rollout = Rollout::new(path.clone());
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
    fn compacted_marker_replaces_prior_lines_on_replay() {
        let path = temp_file("compacted");
        let rollout = Rollout::new(path.clone());
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
        let rollout = Rollout::new(path.clone());
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
        let rollout = Rollout::new(path.clone());
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
        let resumed = load_session(&path).unwrap();
        assert_eq!(resumed, before_restart);
        let cfg = cfg_with(
            Provider::mock(vec![vec![ContentBlock::Text {
                text: "it was kumquat".into(),
            }]]),
            &dir,
        );
        let mut history = History::resume(dir.clone(), resumed, Rollout::new(path.clone()));
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
        let rollout = Rollout::new(path.clone());
        let good = Message::user_text("intact");
        rollout.append_message(&good).unwrap();
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"type\":\"message\",\"role\":\"us"); // crash mid-append
        std::fs::write(&path, raw).unwrap();

        assert_eq!(load_session(&path).unwrap(), vec![good]);
        cleanup(&path);
    }
}
