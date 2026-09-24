//! Files that were in use before a compaction, read again and placed after the
//! summary.
//!
//! The fold takes the tool results with it, so the files the work is about are
//! gone from context too, and the first thing a compacted agent did was read
//! them all again. The reads here go through the real `read_file` dispatch on
//! a harness context: the permission gate, deny rules and sensitive paths all
//! apply, and the read leaves the same file-state observation a model read
//! does — so the file can be edited straight away, and a later external change
//! is reported the usual way.
//!
//! The result is written into the replacement and persisted with it rather
//! than injected fresh on every request: the per-request injection sits at the
//! head of the prompt, where content re-read from disk would break the prompt
//! cache from the first byte whenever a file changed.

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use kloop_protocol::ContentBlock;
use kloop_protocol::Injected;
use kloop_protocol::Message;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::file_state::normalize_absolute_path;
use crate::history::estimate_text_tokens;
use crate::tools::ToolCtx;

const HEADER: &str = "[Files that were in use before this compaction, re-read just now]";
const MAX_FILES: usize = 5;
const FILE_CHARS: usize = 12_000;
const TOTAL_CHARS: usize = 40_000;

/// Re-read the files most recently read in `folded` that `tail` does not
/// already carry. `token_budget` is the caller's share of the window left
/// after compaction. `None` when nothing qualifies.
pub(super) async fn restore_files(
    cfg: &Arc<Config>,
    folded: &[Message],
    tail: &[Message],
    token_budget: u64,
    cancel: &CancellationToken,
) -> Option<Message> {
    let workspace = cfg.effective_workspace();
    let in_tail: HashSet<PathBuf> = read_paths(tail, &workspace.cwd).into_iter().collect();
    let ctx = ToolCtx::harness(Arc::clone(cfg), cancel.clone());
    let mut sections = Vec::new();
    let mut chars = HEADER.len();
    let mut tokens = estimate_text_tokens(HEADER);
    for path in read_paths(folded, &workspace.cwd) {
        if sections.len() == MAX_FILES || cancel.is_cancelled() {
            break;
        }
        if in_tail.contains(&path)
            || path
                .extension()
                .is_some_and(|extension| extension == "ipynb")
            || workspace.permissions.read_path_blocked(&path)
        {
            continue;
        }
        let Some(page) = first_page(&path) else {
            continue;
        };
        // Sized before the read, not after: a read leaves the lines it covered
        // registered as in context, and lines that are then left out would
        // make the reread advisory tell the model it has what it does not.
        let predicted = section(&path, &page, &page.text);
        let predicted_tokens = estimate_text_tokens(&predicted);
        if chars + predicted.len() > TOTAL_CHARS || tokens + predicted_tokens > token_budget {
            break;
        }
        let Some(text) = read_through_dispatch(&ctx, &path, page.lines).await else {
            continue;
        };
        let rendered = section(&path, &page, &text);
        chars += rendered.len();
        tokens += estimate_text_tokens(&rendered);
        sections.push(rendered);
    }
    if sections.is_empty() {
        return None;
    }
    Some(Message::injected(
        Injected::RestoredFiles,
        format!("{HEADER}\n\n{}", sections.join("\n\n")),
    ))
}

/// Paths passed to `read_file`, newest first, each once.
fn read_paths(messages: &[Message], cwd: &Path) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    messages
        .iter()
        .rev()
        .flat_map(|message| message.content.iter().rev())
        .filter_map(|block| match block {
            ContentBlock::ToolUse { name, input, .. } if name == "read_file" => {
                input["path"].as_str()
            }
            _ => None,
        })
        .map(|raw| normalize_absolute_path(cwd, Path::new(raw)))
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

/// How much of a file's head fits the per-file cap, as `read_file` will
/// render it.
struct Page {
    lines: usize,
    total_lines: usize,
    text: String,
}

/// Only sizes the read — the content that reaches the model is what the
/// dispatched `read_file` returns. A path that is gone, is not a regular file,
/// is not text, or is empty has nothing to restore.
fn first_page(path: &Path) -> Option<Page> {
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    if content.is_empty() {
        return None;
    }
    let total_lines = content.split('\n').count();
    let mut text = String::new();
    let mut chars = 0usize;
    let mut lines = 0usize;
    for line in content.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let rendered = format!("{}\t{line}", lines + 1);
        let separator = usize::from(lines > 0);
        let rendered_chars = rendered.chars().count();
        if chars + separator + rendered_chars > FILE_CHARS {
            break;
        }
        if separator == 1 {
            text.push('\n');
        }
        text.push_str(&rendered);
        chars += separator + rendered_chars;
        lines += 1;
    }
    if lines == 0 {
        return None;
    }
    if lines < total_lines {
        text.push_str(&format!(
            "\n\n[showing lines 1-{lines} of {total_lines}; call read_file with offset={} to continue]",
            lines + 1
        ));
    }
    Some(Page {
        lines,
        total_lines,
        text,
    })
}

fn section(path: &Path, page: &Page, text: &str) -> String {
    format!(
        "{} (lines 1-{} of {}):\n{text}",
        path.display(),
        page.lines,
        page.total_lines
    )
}

async fn read_through_dispatch(ctx: &ToolCtx, path: &Path, lines: usize) -> Option<String> {
    let input = json!({ "path": path.to_string_lossy(), "limit": lines });
    let results = crate::tools::dispatch_tools(
        vec![("compaction-restore".into(), "read_file".into(), input)],
        ctx,
    )
    .await;
    match results.into_iter().next()? {
        ContentBlock::ToolResult {
            is_error: false,
            content: kloop_protocol::ToolResultContent::Text(text),
            ..
        } => Some(text),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::tools::testutil::TestConfig;

    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    fn setup(tag: &str) -> (PathBuf, Arc<Config>) {
        let dir = std::env::temp_dir().join(format!(
            "kloop-restore-{tag}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        let mut cfg = TestConfig::new(&format!("restore-{tag}"))
            .build()
            .test_clone();
        cfg.cwd = dir.clone();
        (dir, Arc::new(cfg))
    }

    fn read_of(paths: &[&Path]) -> Vec<Message> {
        vec![Message::assistant(
            paths
                .iter()
                .enumerate()
                .map(|(index, path)| ContentBlock::ToolUse {
                    id: format!("r{index}"),
                    name: "read_file".into(),
                    input: json!({"path": path.to_str().unwrap()}),
                })
                .collect(),
        )]
    }

    async fn restore(cfg: &Arc<Config>, folded: &[Message], budget: u64) -> Option<String> {
        restore_files(cfg, folded, &[], budget, &CancellationToken::new())
            .await
            .map(|message| {
                assert_eq!(message.injected, Some(Injected::RestoredFiles));
                match message.content.as_slice() {
                    [ContentBlock::Text { text }] => text.clone(),
                    other => panic!("{other:?}"),
                }
            })
    }

    /// Past the per-file cap the head comes back with the same continuation
    /// line a `read_file` page ends with, so the model knows where to resume.
    #[tokio::test]
    async fn a_long_file_comes_back_as_its_head_with_a_continuation() {
        let (dir, cfg) = setup("long");
        let path = dir.join("big.txt");
        let line = "x".repeat(49);
        std::fs::write(&path, vec![line.as_str(); 1_000].join("\n")).unwrap();

        let text = restore(&cfg, &read_of(&[&path]), u64::MAX).await.unwrap();

        // Whole numbered lines, newline-separated, up to the cap.
        let mut shown = 0;
        let mut chars = 0;
        for number in 1..=1_000usize {
            let rendered = number.to_string().len() + 1 + line.len() + usize::from(number > 1);
            if chars + rendered > FILE_CHARS {
                break;
            }
            chars += rendered;
            shown = number;
        }
        assert!(
            text.starts_with(&format!(
                "{HEADER}\n\n{} (lines 1-{shown} of 1000):\n1\t{line}\n",
                path.display()
            )),
            "{text}"
        );
        assert!(
            text.ends_with(&format!(
                "\n{shown}\t{line}\n\n[showing lines 1-{shown} of 1000; call read_file with \
offset={} to continue]",
                shown + 1
            )),
            "{text}"
        );
    }

    /// Files go in newest first until the next would pass the total character
    /// cap; the rest stay out.
    #[tokio::test]
    async fn the_total_character_cap_stops_the_list() {
        let (dir, cfg) = setup("total");
        let paths: Vec<PathBuf> = (0..4).map(|i| dir.join(format!("f{i}.txt"))).collect();
        for path in &paths {
            std::fs::write(path, vec!["y".repeat(99); 108].join("\n")).unwrap();
        }
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();

        let text = restore(&cfg, &read_of(&refs), u64::MAX).await.unwrap();

        // Each section is ~11,200 characters: three fit under 40,000, four do not.
        assert!(text.len() <= TOTAL_CHARS);
        let listed: Vec<bool> = paths
            .iter()
            .map(|path| text.contains(&format!("{} (lines", path.display())))
            .collect();
        assert_eq!(listed, [false, true, true, true]);
    }

    /// The window share is the other cap: a budget that holds one section holds
    /// only the newest file. No budget for even one means nothing at all.
    #[tokio::test]
    async fn the_window_share_caps_the_list_too() {
        let (dir, cfg) = setup("window");
        let older = dir.join("older.txt");
        let newer = dir.join("newer.txt");
        std::fs::write(&older, "older body").unwrap();
        std::fs::write(&newer, "newer body").unwrap();
        let folded = read_of(&[&older, &newer]);
        let one = estimate_text_tokens(HEADER)
            + estimate_text_tokens(&format!(
                "{} (lines 1-1 of 1):\n1\tnewer body",
                newer.display()
            ));

        let text = restore(&cfg, &folded, one).await.unwrap();
        assert_eq!(
            text,
            format!(
                "{HEADER}\n\n{} (lines 1-1 of 1):\n1\tnewer body",
                newer.display()
            )
        );
        assert_eq!(restore(&cfg, &folded, one - 1).await, None);
    }

    /// A notebook has its own cell-aware read; a path the tail already reads
    /// is in context as it stands.
    #[tokio::test]
    async fn notebooks_and_paths_still_in_the_tail_are_skipped() {
        let (dir, cfg) = setup("skip");
        let notebook = dir.join("a.ipynb");
        let kept = dir.join("kept.txt");
        std::fs::write(&notebook, r#"{"cells":[]}"#).unwrap();
        std::fs::write(&kept, "kept").unwrap();
        let folded = read_of(&[&notebook, &kept]);
        let tail = read_of(&[&kept]);

        let restored =
            restore_files(&cfg, &folded, &tail, u64::MAX, &CancellationToken::new()).await;
        assert_eq!(restored, None);
    }
}
