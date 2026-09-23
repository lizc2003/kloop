//! Plan 197: at a round boundary, name the files the model read whose bytes
//! have changed on disk since — names only, never content, each change once.
//!
//! What is under test is the judgement, seen through the text the model gets:
//! which paths are named, that nothing of their bytes rides along, that a
//! metadata bump says nothing here exactly as it says nothing on `edit_file`,
//! and that one change is one mention until the model reads the path again.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;

use super::ToolCtx;
use super::changed_reads_reminder;
use super::testutil::{TestConfig, bash_input, run_tool, test_ctx_with_cfg};

static TEMP_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// A fresh workspace directory, canonical so the reminder's paths come out
/// relative to it, and a tool context whose cwd is that directory.
fn workspace(tag: &str) -> (PathBuf, ToolCtx) {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "kloop-plan197-{tag}-{}-{sequence}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = std::fs::canonicalize(dir).unwrap();
    let mut cfg = TestConfig::new(&format!("plan197-{tag}"))
        .build()
        .test_clone();
    cfg.cwd = dir.clone();
    (dir, test_ctx_with_cfg(0, Arc::new(cfg)))
}

async fn read(ctx: &ToolCtx, path: &Path) {
    let (out, is_error) = run_tool("read_file", json!({"path": path.to_str().unwrap()}), ctx).await;
    assert!(!is_error, "{out}");
}

fn reminder(ctx: &ToolCtx) -> Option<String> {
    let workspace = ctx.cfg.effective_workspace();
    changed_reads_reminder(&workspace.file_state, &workspace.cwd)
}

fn expected(lines: &[&str]) -> Option<String> {
    let mut out = String::from(
        "<system-reminder>\nFiles you read have changed on disk since, whether by a command you \
         ran or by someone else. What you saw of them is out of date; read again before relying \
         on it:\n",
    );
    for line in lines {
        out.push_str(&format!("- {line}\n"));
    }
    out.push_str("</system-reminder>");
    Some(out)
}

/// The acceptance line of the plan: read A and B, something else rewrites A,
/// and the next boundary names A, not B, and carries none of A's bytes — the
/// old or the new. Then once is once: the next boundary is quiet, a second
/// rewrite of A is still quiet (the model's impression of A has not changed),
/// and only a fresh read re-arms it.
#[tokio::test]
async fn names_the_changed_read_alone_without_its_content_and_only_once_per_read() {
    let (dir, ctx) = workspace("names");
    let a = dir.join("a.txt");
    let b = dir.join("b.txt");
    std::fs::write(&a, "old alpha\n").unwrap();
    std::fs::write(&b, "beta\n").unwrap();
    read(&ctx, &a).await;
    read(&ctx, &b).await;
    assert_eq!(reminder(&ctx), None, "nothing changed yet");

    std::fs::write(&a, "new alpha\n").unwrap();
    let text = reminder(&ctx);
    assert_eq!(text, expected(&["a.txt"]));
    let text = text.unwrap();
    assert!(!text.contains("alpha"), "names only: {text}");

    assert_eq!(reminder(&ctx), None, "said once");
    std::fs::write(&a, "newer alpha\n").unwrap();
    assert_eq!(reminder(&ctx), None, "still the same stale read");

    read(&ctx, &a).await;
    std::fs::write(&a, "newest alpha\n").unwrap();
    assert_eq!(
        reminder(&ctx),
        expected(&["a.txt"]),
        "a new read re-arms it"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// A `touch` and a `chmod` are not a change — here, and on `edit_file`'s note.
/// Both go through `FileVersion::same_content`; this test is what keeps them
/// from drifting apart, by asking both places about the same two events: a
/// metadata-only bump (both silent), then a real rewrite (both speak).
#[cfg(unix)]
#[tokio::test]
async fn a_metadata_bump_is_silent_at_the_boundary_and_on_edit_file_alike() {
    use std::os::unix::fs::PermissionsExt as _;

    let (dir, ctx) = workspace("metadata");
    let path = dir.join("m.txt");
    let target = path.to_str().unwrap();
    std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
    read(&ctx, &path).await;

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600))
        .unwrap();
    assert_eq!(reminder(&ctx), None, "boundary: touch + chmod");
    assert_eq!(reminder(&ctx), None, "and not on a second look either");
    let (out, is_error) = run_tool(
        "edit_file",
        json!({"path": target, "old_string": "beta", "new_string": "BETA"}),
        &ctx,
    )
    .await;
    assert_eq!(
        (out, is_error),
        (format!("edited {target} (1 replacement(s))"), false),
        "edit_file: touch + chmod"
    );

    std::fs::write(&path, "alpha\nBETA\ngamma\ndelta\n").unwrap();
    assert_eq!(reminder(&ctx), expected(&["m.txt"]), "boundary: rewrite");
    let (out, is_error) = run_tool(
        "edit_file",
        json!({"path": target, "old_string": "gamma", "new_string": "GAMMA"}),
        &ctx,
    )
    .await;
    assert_eq!(
        (out, is_error),
        (
            format!("edited {target} (1 replacement(s)); the file changed since you read it"),
            false
        ),
        "edit_file: rewrite"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Whose change it was does not decide. A `bash` write the model issued itself
/// is named — it knows it ran the command, not which of its reads the command
/// touched — while its own `edit_file` is not, because that refreshed the read.
/// A deleted path is named as deleted.
#[tokio::test]
async fn a_bash_write_is_named_an_edit_is_not_and_a_deletion_says_so() {
    let (dir, ctx) = workspace("own");
    let edited = dir.join("edited.txt");
    let shelled = dir.join("shelled.txt");
    let removed = dir.join("removed.txt");
    for path in [&edited, &shelled, &removed] {
        std::fs::write(path, "before\n").unwrap();
        read(&ctx, path).await;
    }

    let (out, is_error) = run_tool(
        "edit_file",
        json!({"path": edited.to_str().unwrap(), "old_string": "before", "new_string": "after"}),
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");
    let (out, is_error) = run_tool(
        "bash",
        bash_input("printf 'after\\n' > shelled.txt && rm removed.txt"),
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");

    assert_eq!(
        reminder(&ctx),
        expected(&["removed.txt (deleted)", "shelled.txt"])
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Past `CHANGED_READS_NAMED_MAX` the list stops and the count takes over — the
/// shape of a formatter run over everything the model read.
#[tokio::test]
async fn a_long_list_is_cut_to_the_measured_cap_and_counted() {
    let (dir, ctx) = workspace("cap");
    let paths: Vec<PathBuf> = (0..12).map(|n| dir.join(format!("f{n:02}.rs"))).collect();
    for path in &paths {
        std::fs::write(path, "fn  main(){}\n").unwrap();
        read(&ctx, path).await;
    }
    for path in &paths {
        std::fs::write(path, "fn main() {}\n").unwrap();
    }

    let names: Vec<String> = (0..10).map(|n| format!("f{n:02}.rs")).collect();
    let mut lines: Vec<&str> = names.iter().map(String::as_str).collect();
    lines.push("and 2 more");
    assert_eq!(reminder(&ctx), expected(&lines));
    let _ = std::fs::remove_dir_all(dir);
}
