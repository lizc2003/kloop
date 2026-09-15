//! File-change previews for the approval gate: turning a `write_file` /
//! `edit_file` call into a line-numbered diff the human sees before signing
//! off. Approving a change you cannot see is meaningless — this is the coding
//! agent's trust core.
//!
//! Shape follows the two "real" reference implementations (claude-code's
//! `structuredPatch` + codex-rs's `diffy` render), which independently
//! converge on: **read the file, apply the edit, diff the whole file with
//! real line numbers** — the changed region shown in its actual context, not
//! the edit strings in isolation. `edit_file` reads the target, applies the
//! same replace the tool will, and diffs old→new; `write_file` diffs an
//! existing file old→new, or shows a `(new file)` insert preview. When the
//! file can't be read (or is too large to diff cheaply, or the edit's
//! `old_string` doesn't uniquely match), we fall back to diffing the edit's
//! two strings directly — claude-code's same degradation, line numbers then
//! relative to the string.
//!
//! Each line is `{sign}{line-number}  {content}`: `+`/`-`/space in column one
//! (so a frontend colors by first char), then a right-aligned gutter number.
//! Hunks carry three lines of context, separated by `⋮`; the total is capped
//! and long lines clipped. The TUI popup scrolls (plan 25), so the line cap is
//! a generous ceiling — ordinary edits never hit it; it only stops a minified
//! whole-file overwrite from turning into a pathologically huge preview string.
//!
//! The preview is a plain string on [`crate::permissions::ConfirmRequest`];
//! frontends render it (the TUI/plain color +/- lines, the server forwards it).

use anyhow::Context as _;
use serde_json::Value;
use similar::ChangeTag;
use similar::TextDiff;

use crate::file_io::BoundedRead;
use crate::file_io::FileReadError;
use crate::file_io::read_bounded;
use crate::text_edit::apply_text_edit;

/// Cap on preview body lines before a `… (N more line(s))` marker. Generous:
/// the popup scrolls now (plan 25), so this only bounds a runaway minified
/// whole-file overwrite, not ordinary edits.
const MAX_PREVIEW_LINES: usize = 500;
/// Cap on a single line's content characters before an ellipsis.
const MAX_LINE_LEN: usize = 200;
/// Files larger than this are diffed via the two-string fallback (or, for
/// write, not at all) — full-file diffing a huge input isn't worth the cost
/// in a synchronous approval path.
const MAX_DIFF_INPUT: usize = 1 << 20;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MutationPreviewContext {
    pub directories_to_create: Vec<std::path::PathBuf>,
}

/// Build the change preview for a gated tool call, or `None` when there is
/// nothing to show (a non-file tool, a malformed input, or a no-op edit).
/// Async because it reads the target file to diff against its real contents.
pub async fn file_change_preview(name: &str, input: &Value) -> Option<String> {
    file_change_preview_with_context(name, input, None).await
}

pub(crate) async fn file_change_preview_with_context(
    name: &str,
    input: &Value,
    context: Option<&MutationPreviewContext>,
) -> Option<String> {
    let preview = match name {
        "edit_file" => {
            let old = input["old_string"].as_str()?;
            let new = input["new_string"].as_str()?;
            let path = input["path"].as_str()?;
            edit_preview(
                path,
                old,
                new,
                input["replace_all"].as_bool().unwrap_or(false),
            )
            .await
        }
        "write_file" => {
            let path = input["path"].as_str()?;
            let content = input["content"].as_str()?;
            match read_preview_file(path).await {
                Ok(snapshot) => match String::from_utf8(snapshot.bytes) {
                    Ok(existing) => numbered_diff(&existing, content),
                    Err(_) => new_file_preview(content),
                },
                Err(error) => match oversized_file(&error) {
                    Some(actual) => {
                        format!("(overwriting existing file, {actual} bytes)")
                    }
                    None => new_file_preview(content),
                },
            }
        }
        "notebook_edit" => {
            let path = input["notebook_path"].as_str()?;
            match read_preview_file(path).await {
                Ok(snapshot) => {
                    let preview =
                        crate::tools::notebook::change_preview(&snapshot.bytes, input).ok()?;
                    let diff = numbered_diff(&preview.old_source, &preview.new_source);
                    if diff.is_empty() {
                        preview.header
                    } else {
                        format!("{}\n{diff}", preview.header)
                    }
                }
                Err(error) => match oversized_file(&error) {
                    Some(actual) => format!("(editing notebook cell; file is {actual} bytes)"),
                    None => return None,
                },
            }
        }
        _ => return None,
    };
    let preview = if name == "write_file" {
        match context.filter(|context| !context.directories_to_create.is_empty()) {
            Some(context) => {
                let directories = context
                    .directories_to_create
                    .iter()
                    .map(|path| format!("  - {}", path.display()))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("(will create parent directories)\n{directories}\n\n{preview}")
            }
            None => preview,
        }
    } else {
        preview
    };
    (!preview.is_empty()).then_some(preview)
}

async fn read_preview_file(path: &str) -> anyhow::Result<BoundedRead> {
    let path = path.to_string();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path)
            .with_context(|| format!("cannot open preview target {path}"))?;
        read_bounded(&mut file, MAX_DIFF_INPUT).map_err(anyhow::Error::from)
    })
    .await
    .context("file preview worker failed")?
}

fn oversized_file(error: &anyhow::Error) -> Option<u64> {
    error
        .downcast_ref::<FileReadError>()
        .and_then(FileReadError::too_large)
        .map(|(actual, _)| actual)
}

/// Apply the edit to the real file and diff old→new with file line numbers;
/// fall back to diffing the two strings when the file can't be read, is too
/// large, or `old_string` doesn't uniquely match (the same degradation
/// claude-code uses).
async fn edit_preview(path: &str, old: &str, new: &str, replace_all: bool) -> String {
    if let Ok(snapshot) = read_preview_file(path).await
        && let Ok(content) = String::from_utf8(snapshot.bytes)
    {
        let edit = apply_text_edit(&content, old, new, replace_all);
        if let Some(updated) = edit.updated {
            return numbered_diff(&content, &updated);
        }
    }
    numbered_diff(old, new)
}

/// A hunked, line-numbered +/- diff of two texts. Empty when identical.
fn numbered_diff(old: &str, new: &str) -> String {
    cap(numbered_lines(old, new))
}

/// The +/- context lines with a right-aligned line-number gutter and `⋮`
/// between non-adjacent hunks (no cap applied — callers cap).
fn numbered_lines(old: &str, new: &str) -> Vec<String> {
    let diff = TextDiff::from_lines(old, new);
    let width = old
        .lines()
        .count()
        .max(new.lines().count())
        .max(1)
        .to_string()
        .len();
    let mut lines: Vec<String> = Vec::new();
    for (i, group) in diff.grouped_ops(3).iter().enumerate() {
        if i > 0 {
            lines.push("⋮".to_string());
        }
        for op in group {
            for change in diff.iter_changes(op) {
                // Deletions carry the old line number, insertions and context
                // the new one — the number the reader would see in the file.
                let (sign, index) = match change.tag() {
                    ChangeTag::Delete => ('-', change.old_index()),
                    ChangeTag::Insert => ('+', change.new_index()),
                    ChangeTag::Equal => (' ', change.new_index()),
                };
                let gutter = match index {
                    Some(i) => format!("{:>width$}", i + 1),
                    None => " ".repeat(width),
                };
                lines.push(format!("{sign}{gutter}  {}", clip(change.value())));
            }
        }
    }
    lines
}

/// A brand-new file: a header plus every content line as a numbered insertion.
fn new_file_preview(content: &str) -> String {
    let mut lines = vec!["(new file)".to_string()];
    if content.is_empty() {
        lines.push("(empty)".to_string());
    } else {
        lines.extend(numbered_lines("", content));
    }
    cap(lines)
}

/// Drop the line terminator and clip an over-long line to keep one minified
/// line from dominating the preview.
fn clip(line: &str) -> String {
    let line = line.trim_end_matches(['\n', '\r']);
    if line.chars().count() > MAX_LINE_LEN {
        let head: String = line.chars().take(MAX_LINE_LEN).collect();
        format!("{head}…")
    } else {
        line.to_string()
    }
}

/// Join the preview lines, capping the total with a remainder marker.
fn cap(mut lines: Vec<String>) -> String {
    if lines.len() > MAX_PREVIEW_LINES {
        let extra = lines.len() - MAX_PREVIEW_LINES;
        lines.truncate(MAX_PREVIEW_LINES);
        lines.push(format!("… ({extra} more line(s))"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kloop-diff-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn numbered_diff_shows_added_deleted_and_changed_lines_with_line_numbers() {
        // A changed middle line: old deleted, new inserted, context kept, each
        // carrying its file line number.
        let diff = numbered_diff("alpha\nbeta\ngamma\n", "alpha\nBETA\ngamma\n");
        assert_eq!(diff, " 1  alpha\n-2  beta\n+2  BETA\n 3  gamma");

        // Pure addition and pure deletion.
        assert_eq!(numbered_diff("a\n", "a\nb\n"), " 1  a\n+2  b");
        assert_eq!(numbered_diff("a\nb\n", "a\n"), " 1  a\n-2  b");

        // Identical texts have no diff (caller drops the empty preview).
        assert_eq!(numbered_diff("same\n", "same\n"), "");
    }

    #[test]
    fn distant_changes_are_separated_into_hunks() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        let new = "X\n2\n3\n4\n5\n6\n7\n8\n9\nY\n";
        let diff = numbered_diff(old, new);
        // Two hunks (top and bottom), a `⋮` gap, the middle context elided.
        // Width is 2 (max line number 10), so gutters are right-aligned.
        assert!(diff.starts_with("- 1  1\n+ 1  X\n  2  2\n"), "{diff}");
        assert!(diff.contains("\n⋮\n"), "{diff}");
        assert!(diff.ends_with("  9  9\n-10  10\n+10  Y"), "{diff}");
        assert!(
            !diff.contains("  5  5\n  6  6"),
            "middle context elided: {diff}"
        );
    }

    #[test]
    fn long_lines_are_clipped() {
        let long = "x".repeat(500);
        let diff = numbered_diff("", &format!("{long}\n"));
        assert!(diff.starts_with("+1  "), "{diff}");
        assert!(diff.ends_with('…'));
        assert_eq!(diff.matches('x').count(), MAX_LINE_LEN); // clipped to 200
    }

    #[test]
    fn big_diffs_are_capped_with_a_remainder_marker() {
        // Ordinary-sized diffs (well under the generous cap) are never cut.
        let modest: String = (0..100).map(|i| format!("line {i}\n")).collect();
        assert_eq!(numbered_diff("", &modest).lines().count(), 100);

        // Only a runaway preview (over the cap) gets the remainder marker.
        let huge: String = (0..MAX_PREVIEW_LINES + 100)
            .map(|i| format!("line {i}\n"))
            .collect();
        let diff = numbered_diff("", &huge);
        let lines: Vec<&str> = diff.lines().collect();
        assert_eq!(lines.len(), MAX_PREVIEW_LINES + 1);
        assert_eq!(lines[MAX_PREVIEW_LINES], "… (100 more line(s))");
    }

    #[tokio::test]
    async fn write_file_diffs_existing_and_previews_new() {
        let dir = temp_dir("write");
        let existing = dir.join("existing.txt");
        std::fs::write(&existing, "old line\nkeep\n").unwrap();

        // Overwriting an existing file diffs old→new with line numbers.
        let preview = file_change_preview(
            "write_file",
            &json!({"path": existing.to_str().unwrap(), "content": "new line\nkeep\n"}),
        )
        .await
        .unwrap();
        assert_eq!(preview, "-1  old line\n+1  new line\n 2  keep");

        // A path that does not exist yet is shown as a fresh, numbered file.
        let fresh = dir.join("fresh.txt");
        let preview = file_change_preview(
            "write_file",
            &json!({"path": fresh.to_str().unwrap(), "content": "hello\nworld\n"}),
        )
        .await
        .unwrap();
        assert_eq!(preview, "(new file)\n+1  hello\n+2  world");

        // An empty new file is labelled, not left blank.
        let preview = file_change_preview(
            "write_file",
            &json!({"path": dir.join("blank.txt").to_str().unwrap(), "content": ""}),
        )
        .await
        .unwrap();
        assert_eq!(preview, "(new file)\n(empty)");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn preview_reads_are_bounded_before_whole_file_diffing() {
        let dir = temp_dir("bounded");
        let at_limit = dir.join("at-limit.txt");
        let over_limit = dir.join("over-limit.txt");
        let notebook_over_limit = dir.join("over-limit.ipynb");
        std::fs::write(&at_limit, "x".repeat(MAX_DIFF_INPUT)).unwrap();
        std::fs::write(&over_limit, "x".repeat(MAX_DIFF_INPUT + 1)).unwrap();
        std::fs::write(&notebook_over_limit, vec![b' '; MAX_DIFF_INPUT + 1]).unwrap();

        let preview = file_change_preview(
            "write_file",
            &json!({"path": at_limit.to_str().unwrap(), "content": "replacement"}),
        )
        .await
        .unwrap();
        assert!(!preview.contains("overwriting existing file"), "{preview}");

        let preview = file_change_preview(
            "write_file",
            &json!({"path": over_limit.to_str().unwrap(), "content": "replacement"}),
        )
        .await
        .unwrap();
        assert_eq!(
            preview,
            format!("(overwriting existing file, {} bytes)", MAX_DIFF_INPUT + 1)
        );

        let preview = file_change_preview(
            "edit_file",
            &json!({
                "path": over_limit.to_str().unwrap(),
                "old_string": "old",
                "new_string": "new"
            }),
        )
        .await
        .unwrap();
        assert_eq!(preview, "-1  old\n+1  new");

        let preview = file_change_preview(
            "notebook_edit",
            &json!({
                "notebook_path": notebook_over_limit.to_str().unwrap(),
                "cell_id": "cell-1",
                "new_source": "new"
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            preview,
            format!(
                "(editing notebook cell; file is {} bytes)",
                MAX_DIFF_INPUT + 1
            )
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edit_file_reads_the_file_and_falls_back_to_two_strings() {
        let dir = temp_dir("edit");
        let path = dir.join("code.rs");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();

        // The edit is applied to the real file and diffed with file line
        // numbers — context is the surrounding file, not the edit strings.
        let preview = file_change_preview(
            "edit_file",
            &json!({"path": path.to_str().unwrap(), "old_string": "two", "new_string": "TWO"}),
        )
        .await
        .unwrap();
        assert_eq!(preview, " 1  one\n-2  two\n+2  TWO\n 3  three");

        // A path that cannot be read degrades to diffing the two strings,
        // numbered from 1.
        let preview = file_change_preview(
            "edit_file",
            &json!({"path": "/no/such/file", "old_string": "foo", "new_string": "bar"}),
        )
        .await
        .unwrap();
        assert_eq!(preview, "-1  foo\n+1  bar");

        // A no-op edit and non-file tools produce nothing.
        assert!(
            file_change_preview(
                "edit_file",
                &json!({"path": "/no/such/file", "old_string": "x", "new_string": "x"})
            )
            .await
            .is_none()
        );
        assert!(
            file_change_preview("bash", &json!({"command": "ls"}))
                .await
                .is_none()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
