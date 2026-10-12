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

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use kloop_protocol::ContentBlock;
use kloop_protocol::Injected;
use kloop_protocol::Message;
use kloop_protocol::ToolResultContent;
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

/// Re-read the ranges most recently read in `folded` that `tail` does not
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
    let mut in_tail: HashMap<PathBuf, Vec<Range<usize>>> = HashMap::new();
    for read in successful_reads(tail, &workspace.cwd) {
        in_tail.entry(read.path).or_default().push(read.range);
    }
    let ctx = ToolCtx::harness(Arc::clone(cfg), cancel.clone());
    let mut sections = Vec::new();
    let mut chars = HEADER.chars().count();
    let mut tokens = estimate_text_tokens(HEADER);
    let mut seen = HashSet::new();
    let mut files = 0;
    'candidates: for read in successful_reads(folded, &workspace.cwd) {
        if files == MAX_FILES || cancel.is_cancelled() {
            break;
        }
        let path = read.path;
        if !seen.insert(path.clone())
            || path
                .extension()
                .is_some_and(|extension| extension == "ipynb")
            || workspace.permissions.read_path_blocked(&path)
        {
            continue;
        }
        let ranges = uncovered(read.range, in_tail.get(&path).cloned().unwrap_or_default());
        let minimum_chars = minimum_section_chars(&path);
        let mut file_chars = 0;
        for range in ranges {
            if cancel.is_cancelled() {
                break;
            }
            let remaining_chars = TOTAL_CHARS.saturating_sub(chars);
            let remaining_tokens = token_budget.saturating_sub(tokens);
            if remaining_chars == 0 || remaining_tokens == 0 {
                break 'candidates;
            }
            let remaining_file_chars = FILE_CHARS.saturating_sub(file_chars);
            if remaining_file_chars < minimum_chars {
                break;
            }
            let Some(page) = fit_page(
                &path,
                range,
                remaining_chars.min(remaining_file_chars),
                remaining_tokens,
            ) else {
                continue;
            };
            let Some(text) = read_through_dispatch(&ctx, &path, &page).await else {
                continue;
            };
            let rendered = format!("\n\n{}", section(&path, &page, &text));
            let rendered_chars = rendered.chars().count();
            chars += rendered_chars;
            file_chars += rendered_chars;
            tokens += estimate_text_tokens(&rendered);
            sections.push(rendered);
        }
        files += usize::from(file_chars > 0);
    }
    if sections.is_empty() {
        return None;
    }
    Some(Message::injected(
        Injected::RestoredFiles,
        format!("{HEADER}{}", sections.concat()),
    ))
}

struct ReadSpan {
    path: PathBuf,
    range: Range<usize>,
}

fn successful_reads(messages: &[Message], cwd: &Path) -> Vec<ReadSpan> {
    let mut results = HashMap::new();
    let mut reads = Vec::new();
    for block in messages
        .iter()
        .rev()
        .flat_map(|message| message.content.iter().rev())
    {
        match block {
            ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                content,
            } => {
                let text = match content {
                    ToolResultContent::Text(text) if !is_error => Some(text.as_str()),
                    _ => None,
                };
                results.insert(tool_use_id.as_str(), text);
            }
            ContentBlock::ToolUse { id, name, input } if name == "read_file" => {
                let Some(text) = results.remove(id.as_str()).flatten() else {
                    continue;
                };
                let (Some(raw), Some(range)) = (input["path"].as_str(), displayed_range(text))
                else {
                    continue;
                };
                reads.push(ReadSpan {
                    path: normalize_absolute_path(cwd, Path::new(raw)),
                    range,
                });
            }
            _ => {}
        }
    }
    reads
}

fn displayed_range(text: &str) -> Option<Range<usize>> {
    let first = text
        .lines()
        .next()?
        .split_once('\t')?
        .0
        .parse::<usize>()
        .ok()?;
    let mut count = 0;
    for line in text.lines() {
        let numbered = line
            .split_once('\t')
            .and_then(|(number, _)| number.parse::<usize>().ok());
        if numbered != first.checked_add(count) {
            break;
        }
        count += 1;
    }
    if text
        .lines()
        .any(|line| line.starts_with("[read output truncated within line "))
    {
        count = count.saturating_sub(1);
    }
    let start = first.saturating_sub(1);
    (count > 0).then(|| start..start.saturating_add(count))
}

fn uncovered(range: Range<usize>, mut covered: Vec<Range<usize>>) -> Vec<Range<usize>> {
    covered.sort_by_key(|range| range.start);
    let mut start = range.start;
    let mut ranges = Vec::new();
    for seen in covered {
        if start < seen.start {
            let end = seen.start.min(range.end);
            if start < end {
                ranges.push(start..end);
            }
        }
        start = start.max(seen.end);
        if start >= range.end {
            break;
        }
    }
    if start < range.end {
        ranges.push(start..range.end);
    }
    ranges
}

struct Page {
    offset: usize,
    lines: usize,
    total_lines: usize,
    text: String,
}

fn minimum_section_chars(path: &Path) -> usize {
    format!("\n\n{} (lines 1-1 of 1):\n1\t", path.display())
        .chars()
        .count()
}

/// Only sizes the read — the content that reaches the model is what the
/// dispatched `read_file` returns. A path that is gone, is not a regular file,
/// is not text, or is empty has nothing to restore.
fn fit_page(
    path: &Path,
    range: Range<usize>,
    char_budget: usize,
    token_budget: u64,
) -> Option<Page> {
    if char_budget < minimum_section_chars(path) {
        return None;
    }
    #[cfg(test)]
    let _ = tests::SIZED_PATHS.try_with(|paths| paths.borrow_mut().push(path.to_path_buf()));
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
    let mut ends = Vec::new();
    for (relative, line) in content
        .split('\n')
        .skip(range.start)
        .take(range.end - range.start)
        .enumerate()
    {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let rendered = format!("{}\t{line}", range.start + relative + 1);
        let separator = usize::from(relative > 0);
        let rendered_chars = rendered.chars().count();
        if chars + separator + rendered_chars > char_budget {
            break;
        }
        if separator == 1 {
            text.push('\n');
        }
        text.push_str(&rendered);
        chars += separator + rendered_chars;
        ends.push(text.len());
    }
    while let Some(end) = ends.pop() {
        text.truncate(end);
        let lines = ends.len() + 1;
        let last = range.start + lines;
        let mut shown = text.clone();
        if last < total_lines {
            shown.push_str(&format!(
                "\n\n[showing lines {}-{last} of {total_lines}; call read_file with offset={} to continue]",
                range.start + 1, last + 1
            ));
        }
        let page = Page {
            offset: range.start + 1,
            lines,
            total_lines,
            text: shown,
        };
        let rendered = format!("\n\n{}", section(path, &page, &page.text));
        if rendered.chars().count() <= char_budget
            && estimate_text_tokens(&rendered) <= token_budget
        {
            return Some(page);
        }
    }
    None
}

fn section(path: &Path, page: &Page, text: &str) -> String {
    format!(
        "{} (lines {}-{} of {}):\n{text}",
        path.display(),
        page.offset,
        page.offset + page.lines - 1,
        page.total_lines
    )
}

async fn read_through_dispatch(ctx: &ToolCtx, path: &Path, page: &Page) -> Option<String> {
    let input =
        json!({ "path": path.to_string_lossy(), "offset": page.offset, "limit": page.lines });
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
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::tools::testutil::TestConfig;

    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    tokio::task_local! {
        pub(super) static SIZED_PATHS: RefCell<Vec<PathBuf>>;
    }

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
        vec![
            Message::assistant(
                paths
                    .iter()
                    .enumerate()
                    .map(|(index, path)| ContentBlock::ToolUse {
                        id: format!("r{index}"),
                        name: "read_file".into(),
                        input: json!({"path": path.to_str().unwrap()}),
                    })
                    .collect(),
            ),
            Message::tool_results(
                paths
                    .iter()
                    .enumerate()
                    .map(|(index, path)| ContentBlock::ToolResult {
                        tool_use_id: format!("r{index}"),
                        content: std::fs::read_to_string(path)
                            .unwrap()
                            .split('\n')
                            .enumerate()
                            .map(|(index, line)| format!("{}\t{line}", index + 1))
                            .collect::<Vec<_>>()
                            .join("\n")
                            .into(),
                        is_error: false,
                    })
                    .collect(),
            ),
        ]
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

    async fn restore_with_sized_paths(
        cfg: &Arc<Config>,
        folded: &[Message],
        tail: &[Message],
        budget: u64,
    ) -> (Option<Message>, Vec<PathBuf>) {
        SIZED_PATHS
            .scope(RefCell::new(Vec::new()), async {
                let restored =
                    restore_files(cfg, folded, tail, budget, &CancellationToken::new()).await;
                let paths = SIZED_PATHS.with(|paths| paths.borrow().clone());
                (restored, paths)
            })
            .await
    }

    #[tokio::test]
    async fn an_exhausted_initial_token_budget_stops_before_sizing_files() {
        let (dir, cfg) = setup("empty-budget");
        let path = dir.join("notes.txt");
        std::fs::write(&path, "body").unwrap();
        let folded = read_of(&[&path]);

        for budget in [0, estimate_text_tokens(HEADER)] {
            assert_eq!(
                restore_with_sized_paths(&cfg, &folded, &[], budget).await,
                (None, Vec::new())
            );
        }
    }

    fn write_files_leaving_total_chars(dir: &Path, remaining: usize) -> (Vec<PathBuf>, String) {
        let target = TOTAL_CHARS - remaining;
        let mut expected = HEADER.to_owned();
        let mut paths = Vec::new();
        for index in 0..4 {
            let path = dir.join(format!("f{index}.txt"));
            let prefix = format!("\n\n{} (lines 1-1 of 1):\n1\t", path.display());
            let chars = FILE_CHARS.min(target - expected.chars().count());
            let body = "x".repeat(chars - prefix.chars().count());
            std::fs::write(&path, &body).unwrap();
            expected.push_str(&prefix);
            expected.push_str(&body);
            paths.push(path);
        }
        assert_eq!(expected.chars().count(), target);
        (paths, expected)
    }

    #[tokio::test]
    async fn the_total_character_budget_stops_before_sizing_older_files() {
        let (dir, cfg) = setup("exhausted-chars");
        let older = dir.join("older.txt");
        std::fs::write(&older, "older body").unwrap();
        let (paths, expected) = write_files_leaving_total_chars(&dir, 0);
        let refs: Vec<&Path> = std::iter::once(older.as_path())
            .chain(paths.iter().rev().map(PathBuf::as_path))
            .collect();
        let folded = read_of(&refs);

        let (restored, sized_paths) = restore_with_sized_paths(&cfg, &folded, &[], u64::MAX).await;
        assert_eq!(sized_paths, paths);
        assert_eq!(
            restored,
            Some(Message::injected(Injected::RestoredFiles, expected))
        );
    }

    #[tokio::test]
    async fn a_small_total_character_remainder_skips_disk_reads_for_unfit_ranges() {
        let (dir, cfg) = setup("small-total-remainder");
        let older: Vec<PathBuf> = (0..3)
            .map(|index| dir.join(format!("older-{index}.txt")))
            .collect();
        let remaining = 34;
        let mut tail = Vec::new();
        for (index, path) in older.iter().enumerate() {
            std::fs::write(path, "first\nkept\nlast").unwrap();
            tail.extend(paged_read(path, &format!("tail{index}"), 2, 1));
            assert!(
                format!("\n\n{} (lines 1-1 of 1):\n1\t", path.display())
                    .chars()
                    .count()
                    > remaining
            );
        }
        let (paths, expected) = write_files_leaving_total_chars(&dir, remaining);
        let refs: Vec<&Path> = older
            .iter()
            .map(PathBuf::as_path)
            .chain(paths.iter().rev().map(PathBuf::as_path))
            .collect();
        let folded = read_of(&refs);

        let (restored, sized_paths) =
            restore_with_sized_paths(&cfg, &folded, &tail, u64::MAX).await;

        assert_eq!(sized_paths, paths);
        assert_eq!(
            restored,
            Some(Message::injected(Injected::RestoredFiles, expected))
        );
    }

    #[tokio::test]
    async fn a_shorter_path_can_still_fit_after_a_candidate_exceeds_the_total_remainder() {
        let (dir, cfg) = setup("shorter-path");
        let short = dir.join("s");
        let long = dir.join("longer-candidate-name.txt");
        std::fs::write(&short, "ok").unwrap();
        std::fs::write(&long, "first\nkept\nlast").unwrap();
        let short_section = format!("\n\n{} (lines 1-1 of 1):\n1\tok", short.display());
        let remaining = short_section.chars().count();
        assert!(
            format!("\n\n{} (lines 1-1 of 1):\n1\t", long.display())
                .chars()
                .count()
                > remaining
        );
        let (mut paths, expected) = write_files_leaving_total_chars(&dir, remaining);
        let refs: Vec<&Path> = [short.as_path(), long.as_path()]
            .into_iter()
            .chain(paths.iter().rev().map(PathBuf::as_path))
            .collect();
        let folded = read_of(&refs);
        let tail = paged_read(&long, "tail", 2, 1);

        let (restored, sized_paths) =
            restore_with_sized_paths(&cfg, &folded, &tail, u64::MAX).await;

        paths.push(short);
        assert_eq!(sized_paths, paths);
        assert_eq!(
            restored,
            Some(Message::injected(
                Injected::RestoredFiles,
                format!("{expected}{short_section}")
            ))
        );
    }

    #[tokio::test]
    async fn the_token_budget_stops_before_sizing_more_ranges_or_files() {
        let (dir, cfg) = setup("exhausted-tokens");
        let older = dir.join("older.txt");
        let newer = dir.join("newer.txt");
        std::fs::write(&older, "older body").unwrap();
        std::fs::write(&newer, "newer body\nalready in tail\nunrestored body").unwrap();
        let folded = read_of(&[&older, &newer]);
        let tail = paged_read(&newer, "tail", 2, 1);
        let section = format!(
            "\n\n{} (lines 1-1 of 3):\n1\tnewer body\n\n[showing lines 1-1 of 3; call read_file with offset=2 to continue]",
            newer.display()
        );
        let budget = estimate_text_tokens(HEADER) + estimate_text_tokens(&section);

        assert_eq!(
            restore_with_sized_paths(&cfg, &folded, &tail, budget).await,
            (
                Some(Message::injected(
                    Injected::RestoredFiles,
                    format!("{HEADER}{section}")
                )),
                vec![newer]
            )
        );
    }

    fn write_split_file_with_remaining_budget(path: &Path, remaining: usize) -> String {
        let prefix = format!("\n\n{} (lines 1-1 of 3):\n1\t", path.display());
        let suffix = "\n\n[showing lines 1-1 of 3; call read_file with offset=2 to continue]";
        let body =
            "界".repeat(FILE_CHARS - remaining - prefix.chars().count() - suffix.chars().count());
        std::fs::write(path, format!("{body}\nalready in tail\n")).unwrap();
        format!("{prefix}{body}{suffix}")
    }

    #[tokio::test]
    async fn a_spent_file_budget_skips_more_ranges_but_restores_the_next_file() {
        let (dir, cfg) = setup("spent-file-budget");
        let older = dir.join("older.txt");
        let path = dir.join("范围.txt");
        std::fs::write(&older, "older body").unwrap();
        let last = format!("\n\n{} (lines 3-3 of 3):\n3\t", path.display());
        let older_section = format!("\n\n{} (lines 1-1 of 1):\n1\tolder body", older.display());

        for remaining in [last.chars().count() - 1, 0] {
            let first = write_split_file_with_remaining_budget(&path, remaining);
            let folded = read_of(&[&older, &path]);
            let tail = paged_read(&path, "tail", 2, 1);

            let (restored, sized_paths) =
                restore_with_sized_paths(&cfg, &folded, &tail, u64::MAX).await;

            assert_eq!(sized_paths, vec![path.clone(), older.clone()]);
            assert_eq!(
                restored,
                Some(Message::injected(
                    Injected::RestoredFiles,
                    format!("{HEADER}{first}{older_section}")
                ))
            );
        }
    }

    #[tokio::test]
    async fn the_minimum_remaining_file_budget_can_still_restore_an_empty_line() {
        let (dir, cfg) = setup("minimum-file-budget");
        let older = dir.join("older.txt");
        let path = dir.join("范围.txt");
        std::fs::write(&older, "older body").unwrap();
        let last = format!("\n\n{} (lines 3-3 of 3):\n3\t", path.display());
        let first = write_split_file_with_remaining_budget(&path, last.chars().count());
        let older_section = format!("\n\n{} (lines 1-1 of 1):\n1\tolder body", older.display());
        let folded = read_of(&[&older, &path]);
        let tail = paged_read(&path, "tail", 2, 1);

        let (restored, sized_paths) =
            restore_with_sized_paths(&cfg, &folded, &tail, u64::MAX).await;

        assert_eq!(sized_paths, vec![path.clone(), path, older]);
        assert_eq!(
            restored,
            Some(Message::injected(
                Injected::RestoredFiles,
                format!("{HEADER}{first}{last}{older_section}")
            ))
        );
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

        let mut shown = 0;
        for number in 1..=1_000usize {
            let numbered = (1..=number)
                .map(|index| format!("{index}\t{line}"))
                .collect::<Vec<_>>()
                .join("\n");
            let rendered = format!(
                "\n\n{} (lines 1-{number} of 1000):\n{numbered}\n\n[showing lines 1-{number} of 1000; call read_file with offset={} to continue]",
                path.display(),
                number + 1
            );
            if rendered.chars().count() > FILE_CHARS {
                break;
            }
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

    #[tokio::test]
    async fn the_total_character_cap_shrinks_the_last_file() {
        let (dir, cfg) = setup("total");
        let paths: Vec<PathBuf> = (0..4).map(|i| dir.join(format!("f{i}.txt"))).collect();
        for path in &paths {
            std::fs::write(path, vec!["y".repeat(99); 108].join("\n")).unwrap();
        }
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();

        let text = restore(&cfg, &read_of(&refs), u64::MAX).await.unwrap();

        assert!(text.chars().count() <= TOTAL_CHARS);
        let listed: Vec<bool> = paths
            .iter()
            .map(|path| text.contains(&format!("{} (lines", path.display())))
            .collect();
        assert_eq!(listed, [true, true, true, true]);
        assert!(
            !text.contains(&format!("{} (lines 1-108", paths[0].display())),
            "{text}"
        );
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
                "\n\n{} (lines 1-1 of 1):\n1\tnewer body",
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

    fn paged_read(path: &Path, id: &str, offset: usize, limit: usize) -> Vec<Message> {
        let text = std::fs::read_to_string(path).unwrap();
        let result = text
            .split('\n')
            .skip(offset.saturating_sub(1))
            .take(limit)
            .enumerate()
            .map(|(index, line)| format!("{}\t{line}", offset + index))
            .collect::<Vec<_>>()
            .join("\n");
        vec![
            Message::assistant(vec![ContentBlock::ToolUse {
                id: id.into(),
                name: "read_file".into(),
                input: json!({"path": path, "offset": offset, "limit": limit}),
            }]),
            Message::tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: result.into(),
                is_error: false,
            }]),
        ]
    }

    #[test]
    fn displayed_ranges_follow_actual_output_including_zero_offset_and_truncation() {
        let cases = [
            (
                "11\ta\n12\tb\n\n[showing lines 11-12 of 100; call read_file with offset=13 to continue]",
                Some(10..12),
            ),
            ("0\ta\n1\tb", Some(0..2)),
            (
                "1\tpartial\n\n[read output truncated within line 1 of 10; use grep or a narrower reader to inspect it]",
                None,
            ),
            (
                "<system-reminder>Warning: shorter than offset</system-reminder>",
                None,
            ),
            ("Full result saved to a file", None),
            ("1\ta\n3\tb", Some(0..1)),
        ];
        for (text, expected) in cases {
            assert_eq!(displayed_range(text), expected, "{text}");
        }
    }

    #[tokio::test]
    async fn the_latest_successful_page_wins_over_an_older_file_head() {
        let (dir, cfg) = setup("latest-page");
        let path = dir.join("notes.md");
        std::fs::write(
            &path,
            (1..=300)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let mut folded = paged_read(&path, "old", 1, 200);
        folded.extend(paged_read(&dir.join("./notes.md"), "new", 250, 2));
        let mut failed = paged_read(&path, "failed", 1, 1);
        let ContentBlock::ToolResult { is_error, .. } = &mut failed[1].content[0] else {
            panic!()
        };
        *is_error = true;
        folded.extend(failed);
        folded.push(Message::assistant(vec![ContentBlock::ToolUse {
            id: "pending".into(),
            name: "read_file".into(),
            input: json!({"path": path}),
        }]));

        assert_eq!(
            restore(&cfg, &folded, u64::MAX).await,
            Some(format!(
                "{HEADER}\n\n{} (lines 250-251 of 300):\n250\tline 250\n251\tline 251\n\n[showing lines 250-251 of 300; call read_file with offset=252 to continue]",
                path.display()
            ))
        );
        assert_eq!(
            cfg.effective_file_state()
                .observation(&path)
                .unwrap()
                .read_range(),
            Some(249..251)
        );
    }

    #[tokio::test]
    async fn overlapping_tail_pages_are_subtracted_without_losing_other_ranges() {
        let (dir, cfg) = setup("tail-pages");
        let path = dir.join("notes.md");
        std::fs::write(
            &path,
            (1..=30)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let folded = paged_read(&path, "folded", 11, 10);
        let mut tail = paged_read(&path, "tail1", 13, 3);
        tail.extend(paged_read(&path, "tail2", 14, 3));
        tail.extend(paged_read(&path, "tail3", 18, 2));
        let mut failed = paged_read(&path, "failed", 11, 10);
        let ContentBlock::ToolResult { is_error, .. } = &mut failed[1].content[0] else {
            panic!()
        };
        *is_error = true;
        tail.extend(failed);

        let expected = [(11, 12), (17, 17), (20, 20)].map(|(start, end)| {
            let numbered = (start..=end).map(|index| format!("{index}\tline {index}")).collect::<Vec<_>>().join("\n");
            format!("{} (lines {start}-{end} of 30):\n{numbered}\n\n[showing lines {start}-{end} of 30; call read_file with offset={} to continue]", path.display(), end + 1)
        });
        assert_eq!(
            restore_files(&cfg, &folded, &tail, u64::MAX, &CancellationToken::new()).await,
            Some(Message::injected(
                Injected::RestoredFiles,
                format!("{HEADER}\n\n{}", expected.join("\n\n"))
            ))
        );
    }

    #[tokio::test]
    async fn an_oversized_recent_file_does_not_hide_a_smaller_candidate() {
        let (dir, cfg) = setup("oversized");
        let small = dir.join("small.txt");
        let big = dir.join("big.txt");
        std::fs::write(&small, "保留").unwrap();
        std::fs::write(&big, "界".repeat(FILE_CHARS)).unwrap();
        let folded = read_of(&[&small, &big]);
        let expected = format!("{HEADER}\n\n{} (lines 1-1 of 1):\n1\t保留", small.display());
        let budget = estimate_text_tokens(HEADER)
            + estimate_text_tokens(expected.strip_prefix(HEADER).unwrap());
        assert_eq!(restore(&cfg, &folded, budget).await, Some(expected));
        assert!(cfg.effective_file_state().observation(&big).is_none());
    }

    #[tokio::test]
    async fn the_token_budget_shrinks_a_non_head_page_before_dispatch() {
        let (dir, cfg) = setup("shrink-page");
        let path = dir.join("notes.txt");
        let line = "界".repeat(64);
        std::fs::write(&path, vec![line.as_str(); 100].join("\n")).unwrap();
        let folded = paged_read(&path, "page", 3, 30);
        let expected_section = format!(
            "\n\n{} (lines 3-3 of 100):\n3\t{line}\n\n[showing lines 3-3 of 100; call read_file with offset=4 to continue]",
            path.display()
        );
        let budget = estimate_text_tokens(HEADER) + estimate_text_tokens(&expected_section);
        assert_eq!(
            restore(&cfg, &folded, budget).await,
            Some(format!("{HEADER}{expected_section}"))
        );
        assert_eq!(
            cfg.effective_file_state()
                .observation(&path)
                .unwrap()
                .read_range(),
            Some(2..3)
        );
    }
}
