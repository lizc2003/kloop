//! Plan 203: what the user said survives any number of compactions, and the
//! files in use are back in context right after one.
//!
//! Seen through the replacement a compaction writes: the anchors are the
//! user's own bytes whatever the summary says, and the restored files are
//! real `read_file` reads — editable at once, reported when they change, and
//! persisted so a resumed session sees exactly the same history.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use kloop_protocol::AssistantBlock;
use kloop_protocol::ContentBlock;
use kloop_protocol::Injected;
use kloop_protocol::Message;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::KEEP_RECENT_TOKENS;
use super::SUMMARY_PREFIX;
use super::plan_compaction;
use super::run_compaction;
use crate::config::Config;
use crate::history::History;
use crate::permissions::{Mode, PermissionRules, Permissions};
use crate::tools::testutil::{TestConfig, run_tool, test_ctx_with_cfg};

static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

const ANCHORS_HEADER: &str =
    "[What the user said earlier in this session, verbatim — carried across compactions]";
const FILES_HEADER: &str = "[Files that were in use before this compaction, re-read just now]";

/// A canonical workspace directory, so restored paths come out as written.
fn workspace(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kloop-plan203-{tag}-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("state")).unwrap();
    std::fs::canonicalize(dir).unwrap()
}

/// One mock summary per compaction, each a single sentence that mentions no
/// user message at all. `*.pem` reads are denied.
fn cfg(dir: &Path, tag: &str, compactions: usize) -> Arc<Config> {
    let turns = (0..compactions)
        .map(|_| {
            vec![AssistantBlock::Text {
                text: "Work continued.".into(),
            }]
        })
        .collect();
    let mut cfg = TestConfig::new(&format!("plan203-{tag}"))
        .provider(kloop_provider::Provider::mock(turns))
        .context_window(Some(200_000))
        .dirs(&dir.join("state"))
        .build()
        .test_clone();
    cfg.cwd = dir.to_path_buf();
    cfg.permissions = Arc::new(
        Permissions::new(
            Mode::Bypass,
            &PermissionRules {
                deny: vec!["read_file(**/*.pem)".into()],
                ..Default::default()
            },
            dir.to_path_buf(),
            None,
        )
        .unwrap(),
    );
    Arc::new(cfg)
}

/// Outweighs the keep budget, so everything before it is folded.
fn fat_reply() -> Message {
    Message::assistant(vec![ContentBlock::Text {
        text: "work ".repeat(KEEP_RECENT_TOKENS as usize),
    }])
}

fn reads(ids_and_paths: &[(&str, &Path)]) -> [Message; 2] {
    [
        Message::assistant(
            ids_and_paths
                .iter()
                .map(|(id, path)| ContentBlock::ToolUse {
                    id: id.to_string(),
                    name: "read_file".into(),
                    input: json!({"path": path.to_str().unwrap()}),
                })
                .collect(),
        ),
        Message::tool_results(
            ids_and_paths
                .iter()
                .map(|(id, _)| ContentBlock::ToolResult {
                    tool_use_id: id.to_string(),
                    content: "(earlier read)".into(),
                    is_error: false,
                })
                .collect(),
        ),
    ]
}

fn text_of(message: &Message) -> &str {
    match message.content.as_slice() {
        [ContentBlock::Text { text }] => text,
        other => panic!("expected one text block, got {other:?}"),
    }
}

async fn compact(cfg: &Arc<Config>, history: &mut History) {
    run_compaction(cfg, "mock", history, &CancellationToken::new())
        .await
        .expect("compaction applies");
}

/// The first compaction: the original request is the first thing the user
/// said, what they said in the folded prefix follows verbatim, and what is
/// still in the kept tail is not repeated.
#[tokio::test]
async fn the_first_compaction_carries_the_folded_user_messages_verbatim() {
    let dir = workspace("first");
    let cfg = cfg(&dir, "first", 1);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text(
        "build the parser\ndo NOT touch the lexer",
    ));
    history.record(Message::assistant(vec![ContentBlock::Text {
        text: "on it".into(),
    }]));
    history.record(Message::user_text("also keep the old API"));
    history.record(fat_reply());
    history.record(Message::user_text("still in the tail"));

    compact(&cfg, &mut history).await;

    assert_eq!(
        history.messages(),
        &[
            Message::injected(
                Injected::UserAnchors,
                format!(
                    "{ANCHORS_HEADER}\n\nOriginal request:\n  build the parser\n  do NOT touch the \
lexer\n\nLater messages (oldest first):\n- also keep the old API"
                ),
            ),
            Message::injected(
                Injected::ContextSummary,
                format!("{SUMMARY_PREFIX}Work continued."),
            ),
            Message::user_text("still in the tail"),
        ]
    );
}

/// Three compactions in a row, each summary one sentence that quotes nobody:
/// the original request is byte-for-byte what it was, every newer message is
/// there, and the oldest that no longer fit are counted, not silently lost.
#[tokio::test]
async fn three_compactions_keep_the_original_request_byte_for_byte() {
    let dir = workspace("three");
    let cfg = cfg(&dir, "three", 3);
    let mut history = History::new(cfg.offload_dir.clone());
    let original = "refactor the scheduler; never write the API key to disk";
    let mut first_anchors = String::new();
    for generation in 0..3 {
        history.record(Message::user_text(if generation == 0 {
            original.to_string()
        } else {
            format!("ask {generation}")
        }));
        if generation == 1 {
            // Enough long messages to overrun the later-messages budget.
            for index in 0..12 {
                history.record(Message::user_text(format!("long {index} ").repeat(200)));
            }
        }
        history.record(fat_reply());
        history.record(Message::user_text(format!("tail {generation}")));
        compact(&cfg, &mut history).await;
        if generation == 0 {
            first_anchors = text_of(&history.messages()[0]).to_string();
        }
    }

    let messages = history.messages();
    assert_eq!(messages[0].injected, Some(Injected::UserAnchors));
    let anchors = text_of(&messages[0]);
    assert!(
        anchors.starts_with(&format!(
            "{ANCHORS_HEADER}\n\nOriginal request:\n  {original}\n\n"
        )),
        "{anchors}"
    );
    assert_eq!(
        first_anchors,
        format!("{ANCHORS_HEADER}\n\nOriginal request:\n  {original}")
    );
    // Budgeted newest first: the latest are all there, in order, and the
    // oldest that did not fit are counted rather than silently gone.
    let long_11 = "long 11 ".repeat(200);
    assert!(
        anchors.ends_with(&format!("\n- {}\n- tail 1\n- ask 2", long_11.trim_end())),
        "{anchors}"
    );
    assert!(
        anchors.contains("\n\nLater messages (oldest first):\n(")
            && anchors.contains(" earlier message(s) omitted)\n- "),
        "{anchors}"
    );
    assert!(
        !anchors.contains("\n- tail 0\n"),
        "the oldest went first: {anchors}"
    );
    assert_eq!(messages.last(), Some(&Message::user_text("tail 2")));
}

/// Planning skips every leading product of the previous compaction, whatever
/// mix of them there is: none of it is folded again as if it were history.
#[test]
fn planning_skips_the_leading_compaction_products() {
    let messages = vec![
        Message::injected(Injected::UserAnchors, "anchors"),
        Message::injected(Injected::DroppedPrefix, "dropped"),
        Message::injected(Injected::ContextSummary, "summary"),
        Message::injected(Injected::RestoredFiles, "files"),
        Message::user_text("new ask"),
        fat_reply(),
        Message::user_text("tail"),
    ];
    let plan = plan_compaction(&messages).unwrap();
    assert_eq!(plan.folded, messages[4..6]);
    assert_eq!(plan.request, messages[..6]);
    assert_eq!(plan.tail, messages[6..]);
    assert_eq!((plan.summarized, plan.kept), (2, 1));
    assert_eq!(
        plan_compaction(&messages[..4]).unwrap_err(),
        super::NoOpReason::NoFoldableMessages
    );
}

/// Seven files read in the folded prefix; one of them read again in the tail,
/// one deleted since, one denied by a rule. The five most recent of the rest
/// come back, in that order, as the dispatched `read_file` returns them — and
/// the whole replacement persists, so a resumed session reads it back as is.
#[tokio::test]
async fn the_most_recent_readable_files_come_back_and_persist() {
    let dir = workspace("restore");
    let cfg = cfg(&dir, "restore", 1);
    let path = |name: &str| dir.join(name);
    for index in 0..7 {
        std::fs::write(path(&format!("f{index}.rs")), format!("fn f{index}() {{}}")).unwrap();
    }
    std::fs::write(path("key.pem"), "secret").unwrap();
    assert!(
        cfg.effective_workspace()
            .permissions
            .read_path_blocked(&path("key.pem")),
        "the fixture's deny rule must apply"
    );
    let session = dir.join("state").join("session.jsonl");
    let mut history = History::new(cfg.offload_dir.clone());
    history.attach_rollout(crate::rollout::Rollout::new(session.clone()));
    history.record(Message::user_text("fix the functions"));
    let (f0, f1, f2, f3, f4, f5, f6) = (
        path("f0.rs"),
        path("f1.rs"),
        path("f2.rs"),
        path("f3.rs"),
        path("f4.rs"),
        path("f5.rs"),
        path("f6.rs"),
    );
    let (pem, gone) = (path("key.pem"), path("gone.rs"));
    for message in reads(&[
        ("r0", &f0),
        ("r1", &f1),
        ("r2", &f2),
        ("r3", &f3),
        ("r4", &f4),
        ("r5", &f5),
        ("r6", &f6),
        ("r7", &pem),
        ("r8", &gone),
    ]) {
        history.record(message);
    }
    history.record(fat_reply());
    for message in reads(&[("r9", &f6)]) {
        history.record(message);
    }
    history.record(Message::user_text("continue"));

    compact(&cfg, &mut history).await;

    let section = |index: usize| {
        format!(
            "{} (lines 1-1 of 1):\n1\tfn f{index}() {{}}",
            path(&format!("f{index}.rs")).display()
        )
    };
    let expected_files = Message::injected(
        Injected::RestoredFiles,
        format!(
            "{FILES_HEADER}\n\n{}",
            [5, 4, 3, 2, 1].map(section).join("\n\n")
        ),
    );
    let messages = history.messages();
    assert_eq!(messages.len(), 6, "{messages:#?}");
    assert_eq!(messages[1].injected, Some(Injected::ContextSummary));
    assert_eq!(messages[2], expected_files);
    assert_eq!(&messages[3..5], &reads(&[("r9", &f6)]));
    assert_eq!(messages[5], Message::user_text("continue"));

    assert_eq!(
        crate::rollout::load_session(&session).unwrap(),
        messages,
        "the replacement is persisted, not re-derived on resume"
    );
}

/// A restored file is a read like any other: it can be edited at once, and a
/// later change on disk is reported at the next boundary.
#[tokio::test]
async fn a_restored_file_is_editable_and_its_changes_are_reported() {
    let dir = workspace("editable");
    let cfg = cfg(&dir, "editable", 1);
    let target = dir.join("lib.rs");
    let other = dir.join("other.rs");
    std::fs::write(&target, "fn old() {}").unwrap();
    std::fs::write(&other, "fn other() {}").unwrap();
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("rename old"));
    for message in reads(&[("r0", &target), ("r1", &other)]) {
        history.record(message);
    }
    history.record(fat_reply());
    history.record(Message::user_text("continue"));

    compact(&cfg, &mut history).await;
    assert_eq!(
        history.messages()[2].injected,
        Some(Injected::RestoredFiles)
    );

    let ctx = test_ctx_with_cfg(0, Arc::clone(&cfg));
    let (out, is_error) = run_tool(
        "edit_file",
        json!({
            "path": target.to_str().unwrap(),
            "old_string": "fn old()",
            "new_string": "fn new()",
        }),
        &ctx,
    )
    .await;
    assert!(!is_error, "a restored read grants edit authority: {out}");

    let workspace = cfg.effective_workspace();
    let reminder = || crate::tools::changed_reads_reminder(&workspace.file_state, &workspace.cwd);
    assert_eq!(reminder(), None, "the model's own edit is not news");
    std::fs::write(&other, "fn changed() {}").unwrap();
    let text = reminder().expect("an outside change to a restored file is reported");
    assert!(text.contains("- other.rs\n"), "{text}");
    assert!(!text.contains("lib.rs"), "{text}");
}
