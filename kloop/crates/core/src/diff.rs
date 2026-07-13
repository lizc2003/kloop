//! File-change previews for the approval gate: turning a `write_file` /
//! `edit_file` call into a unified-diff-shaped preview the human sees before
//! signing off. Approving a change you cannot see is meaningless — this is the
//! coding agent's trust core.
//!
//! Two shapes, both line-hunked with three lines of context via `similar`:
//! - `edit_file` diffs `old_string` against `new_string` directly (the
//!   changed region is exactly what the model is asking to swap; no file read
//!   needed).
//! - `write_file` is a whole-file overwrite: an existing file is read and
//!   diffed old→new; a not-yet-existing path shows a `(new file)` insert
//!   preview.
//!
//! The preview is a plain string carried on [`crate::permissions::ConfirmRequest`];
//! frontends render it (the TUI colors +/- lines, plain prints them). Large
//! diffs are capped and long lines clipped so a minified file can't blow up
//! the popup.

use serde_json::Value;
use similar::ChangeTag;
use similar::TextDiff;

/// Cap on preview body lines before a `… (N more line(s))` marker.
const MAX_PREVIEW_LINES: usize = 40;
/// Cap on a single line's characters before an ellipsis (minified files).
const MAX_LINE_LEN: usize = 200;

/// Build the change preview for a gated tool call, or `None` when there is
/// nothing to show (a non-file tool, a malformed input, or a no-op edit).
/// Async because `write_file` reads the existing file to diff against it.
pub async fn file_change_preview(name: &str, input: &Value) -> Option<String> {
    let preview = match name {
        "edit_file" => {
            let old = input["old_string"].as_str()?;
            let new = input["new_string"].as_str()?;
            line_diff(old, new)
        }
        "write_file" => {
            let path = input["path"].as_str()?;
            let content = input["content"].as_str()?;
            match tokio::fs::read_to_string(path).await {
                Ok(existing) => line_diff(&existing, content),
                // No existing file (or unreadable): show it as a fresh file.
                Err(_) => new_file_preview(content),
            }
        }
        _ => return None,
    };
    (!preview.is_empty()).then_some(preview)
}

/// A hunked +/- diff of two texts. Empty when the texts are identical.
fn line_diff(old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut lines: Vec<String> = Vec::new();
    for (i, group) in diff.grouped_ops(3).iter().enumerate() {
        if i > 0 {
            // Non-adjacent hunks are separated so context gaps are visible.
            lines.push("⋮".to_string());
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let sign = match change.tag() {
                    ChangeTag::Delete => '-',
                    ChangeTag::Insert => '+',
                    ChangeTag::Equal => ' ',
                };
                lines.push(format!("{sign}{}", clip(change.value())));
            }
        }
    }
    cap(lines)
}

/// A brand-new file: a header plus every content line as an insertion.
fn new_file_preview(content: &str) -> String {
    let mut lines = vec!["(new file)".to_string()];
    if content.is_empty() {
        lines.push("(empty)".to_string());
    } else {
        lines.extend(content.lines().map(|l| format!("+{}", clip(l))));
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

    #[test]
    fn edit_diff_shows_added_deleted_and_changed_lines() {
        // A changed middle line: the old line deleted, the new inserted,
        // surrounding lines kept as context.
        let diff = line_diff("alpha\nbeta\ngamma\n", "alpha\nBETA\ngamma\n");
        assert_eq!(diff, " alpha\n-beta\n+BETA\n gamma");

        // Pure addition and pure deletion.
        assert_eq!(line_diff("a\n", "a\nb\n"), " a\n+b");
        assert_eq!(line_diff("a\nb\n", "a\n"), " a\n-b");

        // Identical texts have no diff (caller drops the empty preview).
        assert_eq!(line_diff("same\n", "same\n"), "");
    }

    #[test]
    fn distant_changes_are_separated_into_hunks() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        let new = "X\n2\n3\n4\n5\n6\n7\n8\n9\nY\n";
        let diff = line_diff(old, new);
        // Two hunks (top and bottom), a `⋮` gap between them, the middle
        // context lines never all shown.
        assert!(diff.starts_with("-1\n+X\n 2\n 3\n 4\n"), "{diff}");
        assert!(diff.contains("\n⋮\n"), "{diff}");
        assert!(diff.ends_with(" 8\n 9\n-10\n+Y"), "{diff}");
        assert!(!diff.contains(" 5\n 6"), "middle context elided: {diff}");
    }

    #[test]
    fn long_lines_are_clipped() {
        let long = "x".repeat(500);
        let diff = line_diff("", &format!("{long}\n"));
        let shown = diff.strip_prefix('+').unwrap();
        assert_eq!(shown.chars().count(), MAX_LINE_LEN + 1); // +1 for the ellipsis
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn big_diffs_are_capped_with_a_remainder_marker() {
        let new: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let diff = line_diff("", &new);
        let lines: Vec<&str> = diff.lines().collect();
        assert_eq!(lines.len(), MAX_PREVIEW_LINES + 1);
        assert_eq!(lines[MAX_PREVIEW_LINES], "… (60 more line(s))");
    }

    #[tokio::test]
    async fn write_file_diffs_existing_and_previews_new() {
        let dir = std::env::temp_dir().join(format!("kloop-diff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let existing = dir.join("existing.txt");
        std::fs::write(&existing, "old line\nkeep\n").unwrap();

        // Overwriting an existing file diffs old→new.
        let preview = file_change_preview(
            "write_file",
            &json!({"path": existing.to_str().unwrap(), "content": "new line\nkeep\n"}),
        )
        .await
        .unwrap();
        assert_eq!(preview, "-old line\n+new line\n keep");

        // A path that does not exist yet is shown as a fresh file.
        let fresh = dir.join("fresh.txt");
        let preview = file_change_preview(
            "write_file",
            &json!({"path": fresh.to_str().unwrap(), "content": "hello\nworld\n"}),
        )
        .await
        .unwrap();
        assert_eq!(preview, "(new file)\n+hello\n+world");

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
    async fn edit_preview_and_non_file_tools() {
        let preview = file_change_preview(
            "edit_file",
            &json!({"path": "f", "old_string": "foo", "new_string": "bar"}),
        )
        .await
        .unwrap();
        assert_eq!(preview, "-foo\n+bar");

        // A no-op edit and non-file tools produce nothing.
        assert!(file_change_preview(
            "edit_file",
            &json!({"path": "f", "old_string": "x", "new_string": "x"})
        )
        .await
        .is_none());
        assert!(file_change_preview("bash", &json!({"command": "ls"}))
            .await
            .is_none());
    }
}
