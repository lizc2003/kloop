use std::ffi::OsStr;
use std::ffi::OsString;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use kloop_protocol::ToolResultContent;
use serde_json::Value;

// One interface, three implementations, selected here and nowhere else: past this
// point the file has no `#[cfg]` left. `platform` holds descriptor-relative opens,
// the identity and hard-link probes, and the temp/rename primitives; everything
// below is what the tools mean.
#[cfg(unix)]
#[path = "fs/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "fs/windows.rs"]
mod platform;
#[cfg(not(any(unix, windows)))]
#[path = "fs/fallback.rs"]
mod platform;

use platform::*;

use super::ToolCtx;
use super::notebook;
use super::resolve_path;
use super::str_arg;
use crate::config::EffectiveWorkspace;
use crate::file_io::file_contents_equal;
use crate::file_io::fingerprint_file;
use crate::file_io::read_bounded;
use crate::file_state::FileIdentity;
use crate::file_state::FileObservation;
use crate::file_state::FileState;
use crate::file_state::FileStateUpdate;
use crate::file_state::FileVersion;
use crate::file_state::ReadDrift;
use crate::file_state::normalize_absolute_path;
use kloop_protocol::ContentBlock;

use crate::image::MAX_IMAGE_BYTES;
use crate::image::detect_media_type;
use crate::image::prepare_image_from_bytes;
use crate::text_edit::MatchLayer;
use crate::text_edit::apply_text_edit;

/// How many redundant rereads of one path pass between advisories.
///
/// Measured rather than picked: replaying 1 691 real `read_file` calls, 234
/// (13.8%) read over lines still in context, spread across 172 runs whose
/// longest reached 7. A 3/5/8 ladder therefore fired 15 times — 14 of them at
/// the 3, once at the 5, never at the 8. One period says the same thing with
/// one constant. `scripts/tool-usage.py read_file --since 20260903 --overlap`
/// recomputes the input; the corpus is 98.5% one project, so the rate is a
/// floor for that workload, not a universal one.
const REREAD_ADVISORY_EVERY: u32 = 3;

/// Tell the model it just re-read lines it already has, if this is the read to
/// say it on. Advisory only: the read ran, and the result it rides on is whole.
///
/// It rides on the tool result rather than a history entry of its own so that a
/// replayed rollout reproduces it in the same place, and it costs ~350 chars
/// against the 2 000 that separate `READ_CONTENT_CHARS` from `OFFLOAD_CAP_CHARS`.
///
/// Reached through the state update because `Observe` is `read_file`'s alone —
/// the writes carry `Replace`/`Clear`, and no other tool records an observation.
pub(super) fn reread_advisory(state: &FileState, update: &FileStateUpdate) -> Option<String> {
    let FileStateUpdate::Observe { path, observation } = update else {
        return None;
    };
    let rereads = state.note_context_read(path, observation)?;
    if rereads % REREAD_ADVISORY_EVERY != 0 {
        return None;
    }
    Some(format!(
        "<system-reminder>That read covered lines of {} you already have in this conversation; \
         {rereads} reads of this file have now done that. Those lines are still above — look back \
         at the earlier result instead of reading again. If re-reading is not getting you what you \
         need, change approach: widen the range, grep for what you are after, or look somewhere \
         else.</system-reminder>",
        path.display()
    ))
}

/// How many paths one changed-reads reminder names before it says "and N more".
///
/// Measured rather than picked: replaying 191 real sessions round by round
/// (each path named once per read), the reminder would fire 68 times, and 66 of
/// those named 8 paths or fewer — median 1, p90 4. The other two named 21 and 23:
/// a whole-tree formatter over everything read, where the list is the noise and
/// the count is the news. Any cap from 8 to 20 cuts exactly those two. The
/// corpus is an upper bound (it guesses writes from `bash` command text) and
/// mostly one project; `scripts/tool-usage.py bash --changed-reads` recomputes it.
const CHANGED_READS_NAMED_MAX: usize = 10;

/// Tell the model which files it read have changed on disk since, if any did
/// (plan 197). Names only, never content: after plan 195 a stale read costs an
/// edit nothing, so the reminder buys information and is priced like it — a
/// line per path, and the model decides whether to read again.
///
/// It names changes the model made itself through `bash` too. It knows it ran
/// the formatter; it does not know which of the files it read the formatter
/// touched.
///
/// Unlike [`reread_advisory`], this belongs to no tool result — it is about
/// what happened between two rounds — so the caller records it as a message of
/// its own at the round boundary, where a replayed rollout has it too.
pub(crate) fn changed_reads_reminder(state: &FileState, cwd: &Path) -> Option<String> {
    let changed = state.changed_since_read();
    if changed.is_empty() {
        return None;
    }
    let mut out = String::from(
        "<system-reminder>\nFiles you read have changed on disk since, whether by a command you \
         ran or by someone else. What you saw of them is out of date; read again before relying \
         on it:\n",
    );
    for read in changed.iter().take(CHANGED_READS_NAMED_MAX) {
        let path = display_under(cwd, &read.path);
        let suffix = match read.drift {
            ReadDrift::Rewritten => "",
            ReadDrift::Removed => " (deleted)",
        };
        out.push_str(&format!("- {path}{suffix}\n"));
    }
    if let Some(more) = changed.len().checked_sub(CHANGED_READS_NAMED_MAX)
        && more > 0
    {
        out.push_str(&format!("- and {more} more\n"));
    }
    out.push_str("</system-reminder>");
    Some(out)
}

/// A path relative to the workspace when it lies under it. Observation keys are
/// canonical, so the cwd is tried canonicalized too (`/tmp` vs `/private/tmp`).
fn display_under(cwd: &Path, path: &Path) -> String {
    let canonical = std::fs::canonicalize(cwd).ok();
    let relative = path
        .strip_prefix(cwd)
        .ok()
        .or_else(|| path.strip_prefix(canonical.as_deref()?).ok());
    relative.unwrap_or(path).display().to_string()
}

pub(super) struct ReadFileOutput {
    pub content: ToolResultContent,
    pub state_update: FileStateUpdate,
}

pub(super) struct FileMutationOutput {
    pub content: String,
    pub state_update: FileStateUpdate,
    pub path_lock: tokio::sync::OwnedMutexGuard<()>,
}

pub(super) struct PreparedRead {
    path: PathBuf,
    file: std::fs::File,
}

/// A mutation target prepared before permission is checked. Holding the parent
/// directory open is the security boundary: approval may block while the path
/// namespace changes, but commit IO stays relative to this directory object.
pub(super) struct PreparedMutation {
    path: PathBuf,
    parent_path: PathBuf,
    parent: std::fs::File,
    leaf: OsString,
    parent_identity: FileIdentity,
    missing_parents: Vec<OsString>,
    planned_parent_paths: Vec<PathBuf>,
}

#[derive(Debug)]
pub(super) struct UnsafeHardLink {
    path: String,
}

impl std::fmt::Display for UnsafeHardLink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "read_file: cannot read {}: files with multiple hard links cannot be classified safely",
            self.path
        )
    }
}

impl std::error::Error for UnsafeHardLink {}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// Cap on one `read_file` reply. Sized against the round trip, not the context:
/// measured on a real review, 46% of 392 reads hit the old 7,000-char cap and
/// paged, so a 1,000-line Go file cost six round trips to read once and the same
/// file was read 60 times across the turn. Context is the cheap resource here —
/// the provider served 93–95% of it from cache — and a round trip is ~20s. Must
/// stay below [`crate::history::OFFLOAD_CAP_CHARS`], or every large read spills
/// to disk and the model gets a preview instead.
const READ_CONTENT_CHARS: usize = 30_000;

#[derive(Clone)]
enum Mutation {
    Write {
        bytes: Vec<u8>,
    },
    Edit {
        old: String,
        new: String,
        replace_all: bool,
    },
    Notebook {
        request: notebook::NotebookEditRequest,
    },
}

#[derive(Debug)]
struct CommitOutcome {
    content: String,
    bytes: Vec<u8>,
    metadata: std::fs::Metadata,
    identity: FileIdentity,
}

struct TargetFile {
    file: std::fs::File,
    metadata: std::fs::Metadata,
}

struct ExpectedTarget {
    version: FileVersion,
    identity: FileIdentity,
}

struct CreatedDirectory {
    parent: std::fs::File,
    name: OsString,
    directory: std::fs::File,
    identity: FileIdentity,
}

#[derive(Clone, Copy)]
struct CommitTarget<'a> {
    parent: &'a std::fs::File,
    parent_path: &'a Path,
    leaf: &'a OsStr,
    display_path: &'a str,
    tool: &'a str,
}

#[derive(Clone, Copy)]
enum CommitFault {
    None,
    #[cfg(test)]
    BeforeRename,
    #[cfg(test)]
    ReplaceTempName,
    /// Rewrite the target between the staged read and the rename. Plan 195 left
    /// [`verify_target_unchanged`] as `edit_file`'s only freshness guard, so it
    /// needs a way to be exercised: nothing else can change a file inside the
    /// window between kloop reading it and kloop replacing it.
    #[cfg(test)]
    ChangeTargetBeforeRename,
}

pub(super) async fn prepare_read(
    input: &Value,
    workspace: &EffectiveWorkspace,
) -> Result<PreparedRead> {
    let path = str_arg(input, "path", "read_file")?.to_string();
    let requested = resolve_path(&workspace.cwd, &path);
    let path_for_worker = path.clone();
    tokio::task::spawn_blocking(move || prepare_read_target(&requested, &path_for_worker))
        .await
        .with_context(|| format!("read_file: path preparation worker failed for {path}"))?
}

pub(super) fn prepare_read_target(requested: &Path, display_path: &str) -> Result<PreparedRead> {
    let resolved = std::fs::canonicalize(requested)
        .with_context(|| format!("read_file: cannot read {display_path}"))?;
    bind_read_target(requested, resolved, display_path)
}

fn bind_read_target(
    requested: &Path,
    resolved: PathBuf,
    display_path: &str,
) -> Result<PreparedRead> {
    let parent_path = resolved
        .parent()
        .with_context(|| format!("read_file: {display_path} has no parent directory"))?;
    let leaf = resolved
        .file_name()
        .with_context(|| format!("read_file: {display_path} has no file name"))?;
    let parent = open_parent_directory(parent_path, "read_file", display_path)?;
    verify_parent_binding(&parent, parent_path, "read_file", display_path)?;
    let file = open_read_target(&parent, parent_path, leaf, display_path)?;

    // Bind both the model's spelling and the canonical permission path to the
    // opened inode. A parent swap between canonicalize and open fails closed.
    let checked_path = std::fs::canonicalize(requested)
        .with_context(|| format!("read_file: path changed while preparing {display_path}"))?;
    let checked_file = open_read_target(&parent, parent_path, leaf, display_path)?;
    if checked_path != resolved || file_identity(&checked_file)? != file_identity(&file)? {
        bail!("read_file: path changed while preparing {display_path}; retry the call");
    }
    if has_multiple_hard_links(&file)? {
        return Err(UnsafeHardLink {
            path: display_path.to_string(),
        }
        .into());
    }
    Ok(PreparedRead {
        path: resolved,
        file,
    })
}

impl PreparedRead {
    pub(super) fn resolved_path(&self) -> &Path {
        &self.path
    }

    pub(super) fn into_parts(self) -> (PathBuf, std::fs::File) {
        (self.path, self.file)
    }
}

/// read_file reads any file the model points at — text or image (cc's Read is
/// one tool for both). It reads the raw bytes, sniffs the format from magic
/// bytes (never the extension), and either returns an image block or numbers
/// the text lines. A binary file that is not a supported image is an error.
pub(super) async fn read_file_tool(
    input: &Value,
    prepared: &PreparedRead,
) -> Result<ReadFileOutput> {
    let path = str_arg(input, "path", "read_file")?;
    let key = prepared.path.clone();
    let mut file = prepared
        .file
        .try_clone()
        .with_context(|| format!("read_file: cannot retain {path}"))?;
    let byte_limit = if key
        .extension()
        .is_some_and(|extension| extension == "ipynb")
    {
        notebook::MAX_NOTEBOOK_BYTES
    } else {
        MAX_IMAGE_BYTES
    };
    let display_path = path.to_string();
    let snapshot = tokio::task::spawn_blocking(move || {
        read_bounded(&mut file, byte_limit)
            .map_err(|error| anyhow::anyhow!("read_file: cannot read {display_path}: {error}"))
    })
    .await
    .with_context(|| format!("read_file: read worker failed for {path}"))??;
    let bytes = snapshot.bytes;
    let metadata = snapshot.metadata;
    let identity = file_identity(&prepared.file)
        .with_context(|| format!("read_file: cannot identify {path}"))?;

    // Notebook dispatch is extension-based: unlike image media sniffing, JSON
    // notebooks have no magic bytes. A cell-aware read must be complete before
    // it grants notebook_edit authority.
    if key
        .extension()
        .is_some_and(|extension| extension == "ipynb")
    {
        let offset = integer_arg(input, "offset", "read_file")?;
        let limit = integer_arg(input, "limit", "read_file")?;
        if !matches!(offset, None | Some(0 | 1)) || !matches!(limit, None | Some(0)) {
            bail!("read_file: offset/limit paging is not supported for Jupyter notebooks");
        }
        let output = notebook::read_notebook(&bytes)?;
        let observation = if output.editable {
            FileObservation::full_notebook_with_identity(&bytes, &metadata, identity)
        } else {
            FileObservation::full_with_identity(&bytes, &metadata, identity)
        };
        return Ok(ReadFileOutput {
            content: output.content,
            state_update: FileStateUpdate::Observe {
                path: key,
                observation,
            },
        });
    }

    // An image file returns a single image block (validated for format, the
    // 5 MiB cap, and the wire's pixel budget); offset/limit are line concepts
    // and simply do not apply.
    if detect_media_type(&bytes).is_some() {
        let image_bytes = bytes.clone();
        let display_path = path.to_string();
        let prepared = tokio::task::spawn_blocking(move || {
            prepare_image_from_bytes(&image_bytes)
                .with_context(|| format!("read_file: cannot read image {display_path}"))
        })
        .await
        .with_context(|| format!("read_file: image worker failed for {path}"))??;
        let mut blocks = vec![prepared.block];
        // Never downscale silently: the model is about to read pixels, and any
        // coordinate it reports comes off the copy it was actually shown.
        if let Some(resized) = prepared.resized {
            let (from_width, from_height) = resized.from;
            let (to_width, to_height) = resized.to;
            blocks.push(ContentBlock::Text {
                text: format!(
                    "<system-reminder>This image was downscaled from {from_width}x{from_height} to {to_width}x{to_height} before it was sent. Any pixel coordinate you read off it refers to the downscaled copy.</system-reminder>"
                ),
            });
        }
        let observation = FileObservation::full_with_identity(&bytes, &metadata, identity);
        return Ok(ReadFileOutput {
            content: ToolResultContent::Blocks(blocks),
            state_update: FileStateUpdate::Observe {
                path: key,
                observation,
            },
        });
    }

    if bytes.starts_with(b"%PDF-")
        || key
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
    {
        bail!(
            "read_file: PDF files are not supported; use a PDF extraction tool or convert selected pages to images first"
        );
    }

    let content = std::str::from_utf8(&bytes).map_err(|_| {
        anyhow::anyhow!(
            "read_file: {path} is not UTF-8 text or a supported image (png/jpeg/gif/webp)"
        )
    })?;
    let offset = integer_arg(input, "offset", "read_file")?.unwrap_or(1);
    let limit = integer_arg(input, "limit", "read_file")?;
    let total_lines = content.split('\n').count();
    let start = if offset == 0 { 0 } else { offset - 1 };
    let requested_end = match limit {
        Some(0) | None => total_lines,
        Some(limit) => start.saturating_add(limit).min(total_lines),
    };

    let (text, observed_end) = if start >= total_lines {
        (
            format!(
                "<system-reminder>Warning: the file exists but is shorter than the provided offset ({offset}). The file has {total_lines} lines.</system-reminder>"
            ),
            start,
        )
    } else if bytes.is_empty() {
        (
            "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>"
                .to_string(),
            1,
        )
    } else {
        numbered_text_page(content, start, requested_end, offset, total_lines)
    };
    let observation = FileObservation::from_read_with_identity(
        &bytes,
        &metadata,
        identity,
        total_lines as u64,
        start as u64..observed_end as u64,
        start == 0,
    );
    Ok(ReadFileOutput {
        content: ToolResultContent::Text(text),
        state_update: FileStateUpdate::Observe {
            path: key,
            observation,
        },
    })
}

fn integer_arg(input: &Value, key: &str, tool: &str) -> Result<Option<usize>> {
    let value = &input[key];
    if value.is_null() {
        return Ok(None);
    }
    let parsed = if let Some(value) = value.as_u64() {
        value
    } else if let Some(value) = value.as_i64() {
        if value < 0 {
            bail!("{tool}: {key} must be a whole number of 0 or more, got {value}");
        }
        value as u64
    } else if let Some(value) = value.as_f64() {
        if value < 0.0 || value.fract() != 0.0 {
            bail!("{tool}: {key} must be a whole number of 0 or more, got {value}");
        }
        value as u64
    } else if let Some(value) = value.as_str() {
        value.trim().parse::<u64>().with_context(|| {
            format!("{tool}: {key} must be a whole number of 0 or more, got {value:?}")
        })?
    } else {
        bail!("{tool}: {key} must be a whole number of 0 or more");
    };
    usize::try_from(parsed)
        .map(Some)
        .with_context(|| format!("{tool}: {key} is too large"))
}

fn numbered_text_page(
    content: &str,
    start: usize,
    requested_end: usize,
    display_start: usize,
    total_lines: usize,
) -> (String, usize) {
    let mut out = String::new();
    let mut chars = 0usize;
    let mut observed_end = start;
    let mut partial_line = None;
    for (relative, line) in content
        .split('\n')
        .skip(start)
        .take(requested_end - start)
        .enumerate()
    {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let rendered = format!("{}\t{line}", display_start + relative);
        let separator = usize::from(!out.is_empty());
        let rendered_chars = rendered.chars().count();
        if chars + separator + rendered_chars > READ_CONTENT_CHARS {
            if out.is_empty() {
                let (prefix, _) = super::char_prefix(&rendered, READ_CONTENT_CHARS);
                out.push_str(prefix);
                partial_line = Some(display_start + relative);
            }
            break;
        }
        if separator == 1 {
            out.push('\n');
        }
        out.push_str(&rendered);
        chars += separator + rendered_chars;
        observed_end = start + relative + 1;
    }
    // Every exit that leaves content unread says how much is left and where to
    // resume. Without the total the model cannot tell a finished read from a
    // `limit`-shaped one, and its only recourse is to reread the same file with
    // a bigger limit — measured on one review session: one file read 7 times,
    // another 3, twice in a single round with overlapping ranges.
    if let Some(line) = partial_line {
        out.push_str(&format!(
            "\n\n[read output truncated within line {line} of {total_lines}; use grep or a narrower reader to inspect it]"
        ));
    } else if observed_end < total_lines {
        out.push_str(&format!(
            "\n\n[showing lines {}-{observed_end} of {total_lines}; call read_file with offset={} to continue]",
            display_start,
            observed_end + 1
        ));
    }
    (out, observed_end)
}

pub(super) async fn prepare_mutation_input(
    tool: &str,
    input: &Value,
    workspace: &EffectiveWorkspace,
) -> Result<PreparedMutation> {
    prepare_mutation_input_with_key(tool, input, "path", workspace).await
}

pub(super) async fn prepare_notebook_mutation_input(
    input: &Value,
    workspace: &EffectiveWorkspace,
) -> Result<PreparedMutation> {
    let path = str_arg(input, "notebook_path", "notebook_edit")?;
    if Path::new(path)
        .extension()
        .is_none_or(|extension| extension != "ipynb")
    {
        bail!(
            "File must be a Jupyter notebook (.ipynb file). For editing other file types, use edit_file."
        );
    }
    prepare_mutation_input_with_key("notebook_edit", input, "notebook_path", workspace).await
}

async fn prepare_mutation_input_with_key(
    tool: &str,
    input: &Value,
    path_key: &str,
    workspace: &EffectiveWorkspace,
) -> Result<PreparedMutation> {
    let path = str_arg(input, path_key, tool)?.to_string();
    if tool == "notebook_edit" && !Path::new(&path).is_absolute() {
        bail!("notebook_edit: notebook_path must be an absolute path");
    }
    let cwd = workspace.cwd.clone();
    let requested = resolve_path(&cwd, &path);
    let tool_for_worker = tool.to_string();
    let path_for_worker = path.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        prepare_mutation(&cwd, &requested, &tool_for_worker, &path_for_worker)
    })
    .await
    .with_context(|| format!("{tool}: path preparation worker failed for {path}"))??;
    Ok(prepared)
}

pub(super) async fn write_file_tool(
    input: &Value,
    prepared: &PreparedMutation,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
) -> Result<FileMutationOutput> {
    let path = str_arg(input, "path", "write_file")?;
    let content = str_arg(input, "content", "write_file")?;
    mutate_file(
        "write_file",
        path,
        prepared,
        Mutation::Write {
            bytes: content.as_bytes().to_vec(),
        },
        ctx,
        workspace,
    )
    .await
}

pub(super) async fn edit_file_tool(
    input: &Value,
    prepared: &PreparedMutation,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
) -> Result<FileMutationOutput> {
    let path = str_arg(input, "path", "edit_file")?;
    let old = str_arg(input, "old_string", "edit_file")?;
    let new = str_arg(input, "new_string", "edit_file")?;
    let replace_all = input["replace_all"].as_bool().unwrap_or(false);
    if old.is_empty() {
        bail!("edit_file: old_string must not be empty");
    }
    if old == new {
        bail!("edit_file: old_string and new_string must be different");
    }
    mutate_file(
        "edit_file",
        path,
        prepared,
        Mutation::Edit {
            old: old.to_string(),
            new: new.to_string(),
            replace_all,
        },
        ctx,
        workspace,
    )
    .await
}

pub(super) async fn notebook_edit_tool(
    input: &Value,
    prepared: &PreparedMutation,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
) -> Result<FileMutationOutput> {
    let path = str_arg(input, "notebook_path", "notebook_edit")?;
    let request = notebook::request_from_input(input)?;
    mutate_file(
        "notebook_edit",
        path,
        prepared,
        Mutation::Notebook { request },
        ctx,
        workspace,
    )
    .await
}

async fn mutate_file(
    tool: &'static str,
    path: &str,
    prepared: &PreparedMutation,
    mutation: Mutation,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
) -> Result<FileMutationOutput> {
    let key = prepared.path.clone();
    let state = Arc::clone(&workspace.file_state);
    let notebook_mutation = matches!(&mutation, Mutation::Notebook { .. });
    // Capture the qualification before waiting. Two concurrent mutations based
    // on one Read must not let the second inherit the first mutation's refresh.
    let expected = state.observation(&key);
    let path_lock = state.lock_path(&key).await;
    if ctx.cancel.is_cancelled() {
        bail!("{tool}: interrupted before writing {path}");
    }

    prepared.revalidate(tool, path)?;
    let parent = prepared
        .parent
        .try_clone()
        .with_context(|| format!("{tool}: cannot retain parent directory for {path}"))?;
    let missing_parents = prepared.missing_parents.clone();
    let parent_path = prepared
        .path
        .parent()
        .expect("prepared mutation target has a parent")
        .to_path_buf();
    let leaf = prepared.leaf.clone();

    // From this point the executor may have created directories, a temp file, or
    // committed a rename. Clear eagerly; only run_one's final successful
    // tool_result stages replacement authority back in. A failure that left the
    // file exactly as the read found it hands the qualification back below.
    state.apply(FileStateUpdate::Clear { path: key.clone() });
    let restore_state = Arc::clone(&state);
    let restore_key = key.clone();
    let path_for_error = path.to_string();
    let (outcome, path_lock) = tokio::task::spawn_blocking(move || {
        let materialized = materialize_parent(parent, &missing_parents, tool, &path_for_error);
        let outcome = materialized.and_then(|(parent, created)| {
            let target = CommitTarget {
                parent: &parent,
                parent_path: &parent_path,
                leaf: &leaf,
                display_path: &path_for_error,
                tool,
            };
            let outcome = commit_mutation(target, expected.as_ref(), mutation, CommitFault::None);
            if outcome.is_err() {
                if let Some(expected) = expected
                    && target_still_present(&parent, &parent_path, &leaf, tool, &path_for_error)
                {
                    restore_state.restore_cleared(&restore_key, expected);
                }
                drop(parent);
                cleanup_created_directories(created);
            }
            outcome
        });
        (outcome, path_lock)
    })
    .await
    .with_context(|| format!("{tool}: write worker failed for {path}"))?;
    let outcome = outcome?;

    let observation = if notebook_mutation {
        FileObservation::full_notebook_with_identity(
            &outcome.bytes,
            &outcome.metadata,
            outcome.identity,
        )
    } else {
        FileObservation::full_with_identity(&outcome.bytes, &outcome.metadata, outcome.identity)
    };
    Ok(FileMutationOutput {
        content: outcome.content,
        state_update: FileStateUpdate::Replace {
            path: key,
            observation,
        },
        path_lock,
    })
}

fn prepare_mutation(
    cwd: &Path,
    requested: &Path,
    tool: &str,
    display_path: &str,
) -> Result<PreparedMutation> {
    let lexical = normalize_absolute_path(cwd, requested);
    let parent = lexical
        .parent()
        .with_context(|| format!("{tool}: {display_path} has no parent directory"))?;
    let leaf = validate_component_name(
        lexical
            .file_name()
            .with_context(|| format!("{tool}: {display_path} has no file name"))?,
        tool,
        display_path,
    )?;

    let (parent_path, parent_source, missing_parents) = match std::fs::canonicalize(parent) {
        Ok(parent_path) => (parent_path, parent.to_path_buf(), Vec::new()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && tool == "write_file" => {
            let mut cursor = parent.to_path_buf();
            let mut missing = Vec::new();
            let (parent_path, parent_source) = loop {
                match std::fs::canonicalize(&cursor) {
                    Ok(path) => break (path, cursor),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        match std::fs::symlink_metadata(&cursor) {
                            Ok(metadata) if metadata.file_type().is_symlink() => {
                                bail!(
                                    "{tool}: refuses symbolic-link parent component in {display_path}"
                                )
                            }
                            Ok(_) => {
                                bail!(
                                    "{tool}: parent component of {display_path} is not a directory"
                                )
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => {
                                return Err(error).with_context(|| {
                                    format!("{tool}: cannot inspect parent of {display_path}")
                                });
                            }
                        }
                        let component = cursor.file_name().with_context(|| {
                            format!("{tool}: cannot find an existing ancestor for {display_path}")
                        })?;
                        missing.push(validate_component_name(component, tool, display_path)?);
                        cursor = cursor
                            .parent()
                            .with_context(|| {
                                format!(
                                    "{tool}: cannot find an existing ancestor for {display_path}"
                                )
                            })?
                            .to_path_buf();
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("{tool}: cannot inspect parent directory for {display_path}")
                        });
                    }
                }
            };
            missing.reverse();
            (parent_path, parent_source, missing)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(error).with_context(|| {
                format!("{tool}: parent directory for {display_path} must already exist")
            });
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("{tool}: cannot inspect parent directory for {display_path}")
            });
        }
    };

    let canonical_cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("{tool}: cannot inspect working directory"))?;
    let lexical_cwd = normalize_absolute_path(cwd, cwd);
    if lexical.starts_with(&lexical_cwd) && !parent_path.starts_with(&canonical_cwd) {
        bail!(
            "{tool}: {display_path} escapes the working directory through a symbolic-link ancestor"
        );
    }

    let parent_file = open_parent_directory(&parent_path, tool, display_path)?;
    let parent_identity = file_identity(&parent_file)?;

    // Bind the canonical spelling used by permission to the directory object we
    // actually opened. Any rename/symlink swap during preparation fails closed.
    let checked_parent = std::fs::canonicalize(&parent_source)
        .with_context(|| format!("{tool}: parent directory changed for {display_path}"))?;
    let checked_file = open_parent_directory(&checked_parent, tool, display_path)?;
    if checked_parent != parent_path || file_identity(&checked_file)? != parent_identity {
        bail!("{tool}: parent directory changed while preparing {display_path}; retry the call");
    }

    if missing_parents.is_empty() {
        inspect_mutation_leaf(&parent_file, &parent_path, &leaf, tool, display_path)?;
    }
    let mut effective_parent = parent_path.clone();
    let mut planned_parent_paths = Vec::with_capacity(missing_parents.len());
    for component in &missing_parents {
        effective_parent.push(component);
        planned_parent_paths.push(effective_parent.clone());
    }
    let path = effective_parent.join(&leaf);

    Ok(PreparedMutation {
        path,
        parent_path,
        parent: parent_file,
        leaf,
        parent_identity,
        missing_parents,
        planned_parent_paths,
    })
}

fn validate_component_name(component: &OsStr, tool: &str, display_path: &str) -> Result<OsString> {
    let bytes = component.as_encoded_bytes();
    if bytes.is_empty()
        || component == OsStr::new(".")
        || component == OsStr::new("..")
        || bytes.contains(&0)
    {
        bail!("{tool}: invalid path component in {display_path}");
    }
    reject_unsafe_component(component, tool, display_path)?;
    Ok(component.to_os_string())
}

fn inspect_mutation_leaf(
    parent: &std::fs::File,
    parent_path: &Path,
    leaf: &OsStr,
    tool: &str,
    display_path: &str,
) -> Result<()> {
    let existing = open_regular_target(parent, parent_path, leaf, tool, display_path)?;
    if tool != "write_file" && existing.is_none() {
        bail!("{tool}: cannot read {display_path}");
    }
    Ok(())
}

impl PreparedMutation {
    pub(super) fn resolved_path(&self) -> &Path {
        &self.path
    }

    pub(super) fn preview_context(&self) -> Option<crate::diff::MutationPreviewContext> {
        (!self.planned_parent_paths.is_empty()).then(|| crate::diff::MutationPreviewContext {
            directories_to_create: self.planned_parent_paths.clone(),
        })
    }

    fn revalidate(&self, tool: &str, display_path: &str) -> Result<()> {
        verify_parent_binding(&self.parent, &self.parent_path, tool, display_path)?;
        if file_identity(&self.parent)? != self.parent_identity {
            bail!(
                "{tool}: parent directory changed while approval was pending for {display_path}; retry the call"
            );
        }
        Ok(())
    }
}

fn verify_parent_binding(
    parent: &std::fs::File,
    parent_path: &Path,
    tool: &str,
    display_path: &str,
) -> Result<()> {
    let current_path = std::fs::canonicalize(parent_path).with_context(|| {
        format!("{tool}: parent directory changed while approval was pending for {display_path}")
    })?;
    let current = open_parent_directory(&current_path, tool, display_path)?;
    if current_path != parent_path || file_identity(&current)? != file_identity(parent)? {
        bail!(
            "{tool}: parent directory changed while approval was pending for {display_path}; retry the call"
        );
    }
    Ok(())
}

fn materialize_parent(
    mut parent: std::fs::File,
    missing: &[OsString],
    tool: &str,
    display_path: &str,
) -> Result<(std::fs::File, Vec<CreatedDirectory>)> {
    let mut created = Vec::new();
    for component in missing {
        let opened = open_or_create_child_directory(&parent, component, tool, display_path);
        let (child, created_now) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                drop(parent);
                cleanup_created_directories(created);
                return Err(error);
            }
        };
        if created_now {
            let identity = match file_identity(&child) {
                Ok(identity) => identity,
                Err(error) => {
                    drop(child);
                    drop(parent);
                    cleanup_created_directories(created);
                    return Err(error).with_context(|| {
                        format!("{tool}: cannot identify created parent for {display_path}")
                    });
                }
            };
            let retained = (|| -> Result<CreatedDirectory> {
                Ok(CreatedDirectory {
                    parent: parent.try_clone()?,
                    name: component.clone(),
                    directory: child.try_clone()?,
                    identity,
                })
            })();
            match retained {
                Ok(retained) => created.push(retained),
                Err(error) => {
                    created.push(CreatedDirectory {
                        parent,
                        name: component.clone(),
                        directory: child,
                        identity,
                    });
                    cleanup_created_directories(created);
                    return Err(error).with_context(|| {
                        format!("{tool}: cannot retain created parent for {display_path}")
                    });
                }
            }
            let sync = sync_parent(&parent, tool, display_path);
            if let Err(error) = sync {
                drop(child);
                drop(parent);
                cleanup_created_directories(created);
                return Err(error);
            }
        }
        parent = child;
    }
    Ok((parent, created))
}

/// Whether the path still holds a file the session's read can describe.
///
/// The clear ahead of a mutation is unconditional on purpose: past it a
/// directory, a temp file or a rename may already exist, and a cancelled `await`
/// must not leave a read standing over bytes kloop replaced. But a refusal that
/// never reached the write leaves the file as the read found it, and forgetting
/// there reports one root cause twice — the next `edit_file` of the same turn is
/// refused for never having read a file the model did read (plan 195). Every
/// reference harness that gates on a prior read records only after a successful
/// write, and none of them drops the record on a refusal.
///
/// Existence is the whole test, and deliberately not freshness: "the session
/// read this path" stays true however the bytes move afterwards, and that is the
/// only claim `edit_file` rests on. Refusing to restore a *changed* file would
/// re-impose, one call later, exactly the freshness gate plan 195 removed.
///
/// A target that is gone is the one record that has to go with it. Nothing
/// describes a missing path, reading one records nothing, and `write_file` reads
/// a leftover record as "changed since it was read" — so keeping it would refuse
/// every later write of that path with no way left to lift the refusal.
fn target_still_present(
    parent: &std::fs::File,
    parent_path: &Path,
    leaf: &OsStr,
    tool: &str,
    display_path: &str,
) -> bool {
    open_regular_target(parent, parent_path, leaf, tool, display_path)
        .is_ok_and(|target| target.is_some())
}

fn commit_mutation(
    target: CommitTarget<'_>,
    expected: Option<&FileObservation>,
    mutation: Mutation,
    fault: CommitFault,
) -> Result<CommitOutcome> {
    let CommitTarget {
        parent,
        parent_path,
        leaf,
        display_path,
        tool,
    } = target;
    verify_parent_binding(parent, parent_path, tool, display_path)?;
    let current = open_regular_target(parent, parent_path, leaf, tool, display_path)?;
    let (bytes, permissions, expected_target, content) = match mutation {
        Mutation::Write { bytes } => {
            let target_state = match current {
                Some(mut target) => {
                    let expected = validate_observation_metadata(
                        expected,
                        &target.metadata,
                        file_identity(&target.file)?,
                        tool,
                        display_path,
                        ReadRequirement::CompleteFile,
                        || None,
                    )?;
                    let snapshot = fingerprint_file(&mut target.file).with_context(|| {
                        format!("{tool}: cannot read existing file {display_path}")
                    })?;
                    validate_observation_version(
                        expected,
                        &snapshot.version,
                        tool,
                        display_path,
                        ReadRequirement::CompleteFile,
                    )?;
                    (
                        Some(snapshot.metadata.permissions()),
                        Some(ExpectedTarget {
                            version: snapshot.version,
                            identity: file_identity(&target.file)?,
                        }),
                    )
                }
                None => {
                    if expected.is_some() {
                        bail!(
                            "{tool}: {display_path} changed since it was read; read it again before modifying it"
                        );
                    }
                    (None, None)
                }
            };
            let content = format!("wrote {} bytes to {display_path}", bytes.len());
            (bytes, target_state.0, target_state.1, content)
        }
        Mutation::Edit {
            old,
            new,
            replace_all,
        } => {
            let mut target =
                current.with_context(|| format!("edit_file: cannot read {display_path}"))?;
            let identity = file_identity(&target.file)?;
            let expected = validate_observation_metadata(
                expected,
                &target.metadata,
                identity,
                tool,
                display_path,
                ReadRequirement::AnyRead,
                || unread_edit_hint(&mut target.file, &old),
            )?;
            let snapshot = read_bounded(&mut target.file, MAX_IMAGE_BYTES)
                .with_context(|| format!("edit_file: cannot read {display_path}"))?;
            // Where `validate_observation_version` used to stand. `edit_file`
            // reads the fact for itself so that relaxing it cannot reach
            // `write_file` or `notebook_edit` through a shared signature, and so
            // that all three outcomes below can carry it — the verdict that
            // decides whether this edit is honest is the anchor's, and that one
            // had not been computed yet at the refusal this replaces (plan 195).
            let note = TargetChange::between(expected, &snapshot.version).note();
            let current = String::from_utf8(snapshot.bytes).map_err(|_| {
                anyhow::anyhow!("edit_file: {display_path} is not valid UTF-8 text")
            })?;
            let edit = apply_text_edit(&current, &old, &new, replace_all);
            if edit.match_count == 0 {
                bail!("{}", not_found_message(display_path, note, &current, &old));
            }
            let tolerant = edit.layer == MatchLayer::PunctuationTolerant;
            if edit.match_count > 1 && !replace_all {
                let count = edit.match_count;
                let counted = if tolerant {
                    " (counted with punctuation/whitespace tolerance)"
                } else {
                    ""
                };
                bail!(
                    "edit_file: old_string matches {count} times in {display_path}{note}{counted}; add surrounding context to disambiguate or set replace_all"
                );
            }
            let updated = edit
                .updated
                .expect("a unique or replace-all edit produces updated text");
            let count = edit.replacement_count;
            let content = match (tolerant, edit.first_line) {
                (true, Some(line)) => format!(
                    "edited {display_path} ({count} replacement(s), punctuation/whitespace-tolerant match at line {line}){note}"
                ),
                _ => format!("edited {display_path} ({count} replacement(s)){note}"),
            };
            (
                updated.into_bytes(),
                Some(snapshot.metadata.permissions()),
                Some(ExpectedTarget {
                    version: snapshot.version,
                    identity: file_identity(&target.file)?,
                }),
                content,
            )
        }
        Mutation::Notebook { request } => {
            let mut target = current
                .context("Notebook file is unavailable; read it again before editing it.")?;
            let expected = validate_observation_metadata(
                expected,
                &target.metadata,
                file_identity(&target.file)?,
                tool,
                display_path,
                ReadRequirement::CompleteNotebook,
                || None,
            )?;
            let snapshot = read_bounded(&mut target.file, notebook::MAX_NOTEBOOK_BYTES)
                .context("Notebook file is unavailable; read it again before editing it.")?;
            validate_observation_version(
                expected,
                &snapshot.version,
                tool,
                display_path,
                ReadRequirement::CompleteNotebook,
            )?;
            let mutation = notebook::apply_edit(&snapshot.bytes, &request)?;
            (
                mutation.bytes,
                Some(snapshot.metadata.permissions()),
                Some(ExpectedTarget {
                    version: snapshot.version,
                    identity: file_identity(&target.file)?,
                }),
                mutation.content,
            )
        }
    };

    atomic_replace(target, &bytes, permissions, expected_target.as_ref(), fault)?;
    let mut committed = open_regular_target(parent, parent_path, leaf, tool, display_path)?
        .with_context(|| format!("{tool}: committed file disappeared: {display_path}"))?;
    let (matches, metadata) = file_contents_equal(&mut committed.file, &bytes)
        .with_context(|| format!("{tool}: cannot verify committed file {display_path}"))?;
    let identity = file_identity(&committed.file)?;
    if !matches {
        bail!(
            "{tool}: {display_path} changed immediately after commit; read it again before modifying it"
        );
    }
    Ok(CommitOutcome {
        content,
        bytes,
        metadata,
        identity,
    })
}

/// Wrap a hint for appending to a refusal, or nothing at all when there is
/// none to give.
fn parenthesized(hint: impl FnOnce() -> Option<String>) -> String {
    hint().map_or_else(String::new, |hint| format!(" ({hint})"))
}

/// How much of the target one mutation demands the session have already read —
/// and, since plan 195, whether a target that moved after that read ends the
/// call or is only reported. The two questions travel together because the
/// second follows from what the tool is about to write.
#[derive(Clone, Copy, Eq, PartialEq)]
enum ReadRequirement {
    /// `write_file` replaces every byte, so it demands having seen every byte,
    /// and refuses a target that changed: overwriting one discards whatever the
    /// change was.
    CompleteFile,
    /// `notebook_edit` is cell-aware, so a raw read of the same bytes does not
    /// qualify, and its refusals keep their own wording. It refuses a changed
    /// target for the same reason `write_file` does.
    CompleteNotebook,
    /// `edit_file`: one read of the path is the whole entrance fee (plan 155).
    /// A narrow read qualifies the file, including for an `old_string` outside
    /// the range that was read. What keeps the edit honest is `old_string`
    /// matching uniquely against the bytes on disk, not how much of those bytes
    /// the session has seen — and every reference implementation draws the line
    /// here or looser (plan 155, third section).
    AnyRead,
}

impl ReadRequirement {
    fn notebook(self) -> bool {
        matches!(self, Self::CompleteNotebook)
    }

    fn complete(self) -> bool {
        matches!(self, Self::CompleteFile | Self::CompleteNotebook)
    }

    /// Whether a target that moved after the read is a remark rather than a
    /// refusal. Only `edit_file` earns that: its anchor is `old_string` matching
    /// uniquely against the current bytes, and one read of *any* range
    /// re-qualifies the path — so refusing here charged a round trip without
    /// buying the freshness it named, and charged it before the anchor's verdict
    /// had even been computed (plan 195). The whole-file tools have no anchor to
    /// fall back on.
    fn tolerates_change(self) -> bool {
        matches!(self, Self::AnyRead)
    }
}

/// Whether the bytes `edit_file` is about to change are still the bytes the
/// session read.
///
/// Content only, from the observation's whole-file fingerprint: `touch` and
/// `chmod` move a version without moving a byte, and remarking on those would
/// make the note noise on every edit that follows one.
#[derive(Clone, Copy, Eq, PartialEq)]
enum TargetChange {
    UnchangedSinceRead,
    ChangedSinceRead,
}

impl TargetChange {
    fn between(read: &FileObservation, current: &FileVersion) -> Self {
        if read.version().same_content(current) {
            Self::UnchangedSinceRead
        } else {
            Self::ChangedSinceRead
        }
    }

    /// The one sentence `edit_file`'s three outcomes share, each placing it
    /// directly after the path so the fact sits next to what it is about.
    ///
    /// A fact and nothing more: a [`FileObservation`] keeps a fingerprint, not
    /// the bytes, so there is no diff to offer, and the two refusals already say
    /// what to do next.
    fn note(self) -> &'static str {
        match self {
            Self::UnchangedSinceRead => "",
            Self::ChangedSinceRead => "; the file changed since you read it",
        }
    }
}

/// Lines of lead-in the unread-edit hint asks for ahead of the match, and the
/// size of the window it asks for. One read of that window both qualifies the
/// file and puts the edit site in front of the model, which is only true
/// because `edit_file` qualifies on any read.
const UNREAD_HINT_LEAD_IN: usize = 20;
const UNREAD_HINT_LIMIT: usize = 60;

/// What `edit_file` can add to a "read it first" refusal: where the replacement
/// would land, and one read that both clears the refusal and shows the model
/// what it is about to change.
///
/// Plan 156 pointed this at the first *unread* line instead, because under the
/// complete-read rule a window around the match would be refused all over
/// again. Plan 155 removed that rule for `edit_file`, so the window is now both
/// the shorter read and the useful one.
///
/// Silent (`None`) when the file cannot be read or `old_string` is not in it:
/// a missing `old_string` is a different failure and keeps its own message.
fn unread_edit_hint(file: &mut std::fs::File, old: &str) -> Option<String> {
    let snapshot = read_bounded(file, MAX_IMAGE_BYTES).ok()?;
    let current = std::str::from_utf8(&snapshot.bytes).ok()?;
    let line = crate::text_edit::first_match_line(current, old)?;
    let total = current.split('\n').count();
    let offset = line.saturating_sub(UNREAD_HINT_LEAD_IN).max(1);
    Some(format!(
        "old_string is at line {line} of {total}; call read_file with offset={offset}, limit={UNREAD_HINT_LIMIT}"
    ))
}

/// The refusal for an `old_string` no layer could place. With a near match it
/// shows the file's own lines to copy; drifted too far for that, it names the
/// one read that shows the whole range; with nothing close, it says so rather
/// than guess. Every branch ends in something to do other than resend the same
/// `old_string`, which can only fail the same way.
fn not_found_message(path: &str, note: &str, current: &str, old: &str) -> String {
    use crate::text_edit::ClosestShape;
    let head = format!("edit_file: old_string not found in {path}{note}");
    let Some(closest) = crate::text_edit::closest_match(current, old) else {
        return format!(
            "{head}; nothing in the file is close enough to show. The text may have changed, or \
             differ by more than punctuation: re-read the small range you meant to edit, rebuild \
             old_string from that fresh text, and do not retry this old_string unchanged"
        );
    };
    let start = closest.start_line;
    let similar = closest.similarity_percent;
    match closest.shape {
        ClosestShape::Listed { lines, drift } => {
            let mut message =
                format!("{head}. The closest match starts at line {start} ({similar}% similar):");
            for line in &lines {
                let crate::text_edit::LineDifference {
                    file_line,
                    your_line,
                    file_text,
                    your_text,
                    first_difference,
                } = line;
                let column = first_difference.column;
                let yours = describe_char(first_difference.yours, column);
                let file = describe_char(first_difference.file, column);
                message.push_str(&format!(
                    "\n  file line {file_line}: {file_text}\n  your line {your_line}: {your_text}\n    \
                     first difference at column {column}: yours has {yours}, the file has {file}"
                ));
            }
            if drift.lines() > 0 {
                message.push_str(&format!(
                    "\n  line count differs: {}",
                    describe_drift(&drift)
                ));
            }
            message.push_str("\nCopy the file's lines exactly into old_string and retry.");
            message
        }
        ClosestShape::TooFar {
            changed_lines,
            drift,
        } => {
            let limit = closest.window_lines;
            let mut differences = Vec::new();
            if changed_lines > 0 {
                differences.push(format!("{changed_lines} changed line(s)"));
            }
            if drift.lines() > 0 {
                differences.push(describe_drift(&drift));
            }
            let differences = differences.join("; ");
            format!(
                "{head}. The closest match starts at line {start} ({similar}% similar), but differs \
                 by more than a few lines ({differences}); call read_file with offset={start}, \
                 limit={limit} and rebuild old_string from what it returns — do not retype the \
                 block from memory"
            )
        }
    }
}

fn describe_char(character: Option<char>, column: usize) -> String {
    match character {
        Some(character) => format!("{character:?} (U+{:04X})", u32::from(character)),
        None if column == 1 => "nothing (the line is empty)".to_string(),
        None => "nothing (the line ends there)".to_string(),
    }
}

fn describe_drift(drift: &crate::text_edit::LineDrift) -> String {
    let side = |extra: usize, all_blank: bool, whose: &str| {
        let blank = if all_blank { ", all blank" } else { "" };
        format!("{whose} has {extra} extra line(s){blank}")
    };
    let mut parts = Vec::new();
    if drift.your_extra > 0 {
        parts.push(side(
            drift.your_extra,
            drift.your_extra_all_blank,
            "your old_string",
        ));
    }
    if drift.file_extra > 0 {
        parts.push(side(
            drift.file_extra,
            drift.file_extra_all_blank,
            "the file",
        ));
    }
    parts.join(", ")
}

/// `unread_hint` is consulted only on the never-read verdict. That is the one
/// verdict a read can clear for the only tool that supplies a hint: `edit_file`
/// asks for [`ReadRequirement::AnyRead`], so it never reaches the incomplete
/// branch — nor, since plan 195, the changed-since-read one — and the tools that
/// do reach them supply no hint.
fn validate_observation_metadata<'a>(
    expected: Option<&'a FileObservation>,
    metadata: &std::fs::Metadata,
    identity: FileIdentity,
    tool: &str,
    path: &str,
    requirement: ReadRequirement,
    unread_hint: impl FnOnce() -> Option<String>,
) -> Result<&'a FileObservation> {
    let Some(expected) = expected else {
        if requirement.notebook() {
            bail!("File has not been read yet. Read it first before writing to it.");
        }
        let hint = parenthesized(unread_hint);
        bail!("{tool}: must read {path} before modifying the existing file{hint}");
    };
    if requirement.notebook() && !expected.is_notebook() {
        bail!("File has not been read as a complete notebook. Read it first before writing to it.");
    }
    if requirement.complete() && !expected.is_complete() {
        bail!("{tool}: must read the entire file {path} before modifying it");
    }
    if requirement.tolerates_change() {
        // Everything below is the freshness comparison, and `edit_file` runs its
        // own after reading the bytes: metadata cannot tell a rewrite from a
        // `touch`, and the verdict belongs after the anchor's, not before it
        // (plan 195). Returning here is also what keeps the relaxation off the
        // shared path — `write_file` and `notebook_edit` cannot reach it.
        return Ok(expected);
    }
    if expected.identity() != identity || !expected.version().metadata_matches(metadata) {
        if requirement.notebook() {
            bail!(
                "File has been modified since read, either by the user or by a linter. Read it again before attempting to write it."
            );
        }
        bail!("{tool}: {path} changed since it was read; read it again before modifying it");
    }
    Ok(expected)
}

fn validate_observation_version(
    expected: &FileObservation,
    current: &FileVersion,
    tool: &str,
    path: &str,
    requirement: ReadRequirement,
) -> Result<()> {
    if expected.version() != current {
        if requirement.notebook() {
            bail!(
                "File has been modified since read, either by the user or by a linter. Read it again before attempting to write it."
            );
        }
        bail!("{tool}: {path} changed since it was read; read it again before modifying it");
    }
    Ok(())
}

/// One unused `.kloop-write-*` name in the target directory. The pid keeps two
/// kloop processes apart; the counter keeps two threads of one process apart.
fn temp_name() -> OsString {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    OsString::from(format!(
        ".kloop-write-{}-{sequence}.tmp",
        std::process::id()
    ))
}

/// Rebind `name` to a different file while the caller's handle stays open on
/// the original — the race `verify_temp_binding` exists to catch. Staged with a
/// rename rather than unlink + create because Windows cannot unlink a name
/// whose file is still open, and the check being exercised has one copy now, so
/// the injection that exercises it has to reach both platforms. The new file
/// comes back so the failure path can drop it the same way on both.
#[cfg(test)]
fn rebind_temp_name(parent: &std::fs::File, name: &OsStr) -> Result<std::fs::File> {
    let decoy_name = temp_name();
    let mut decoy = create_temp(parent, &decoy_name)?;
    decoy.write_all(b"attacker-controlled bytes")?;
    commit_rename(
        &decoy,
        parent,
        &decoy_name,
        name,
        /* replace_existing */ true,
    )?;
    Ok(decoy)
}

/// Did the platform refuse because that temp name is already taken? The retry
/// loop turns on this and nothing else.
fn is_name_collision(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists)
}

fn atomic_replace(
    target: CommitTarget<'_>,
    bytes: &[u8],
    permissions: Option<std::fs::Permissions>,
    expected_target: Option<&ExpectedTarget>,
    fault: CommitFault,
) -> Result<()> {
    let CommitTarget {
        parent,
        parent_path,
        leaf,
        display_path,
        tool,
    } = target;

    verify_parent_binding(parent, parent_path, tool, display_path)?;
    let mut last_collision = None;
    for _ in 0..100 {
        let temp = temp_name();
        let mut file = match create_temp(parent, &temp) {
            Ok(file) => file,
            Err(error) if is_name_collision(&error) => {
                last_collision = Some(error);
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("{tool}: cannot create temporary file for {display_path}")
                });
            }
        };
        // Held open past the verification so the failure path can drop it the
        // same way on both platforms; see the `ReplaceTempName` arm below.
        #[cfg(test)]
        let mut decoy = None;
        let result = (|| -> Result<()> {
            file.write_all(bytes).with_context(|| {
                format!("{tool}: cannot write temporary file for {display_path}")
            })?;
            if let Some(permissions) = permissions.clone() {
                file.set_permissions(permissions).with_context(|| {
                    format!("{tool}: cannot preserve permissions for {display_path}")
                })?;
            }
            file.sync_all().with_context(|| {
                format!("{tool}: cannot sync temporary file for {display_path}")
            })?;
            #[cfg(test)]
            if matches!(fault, CommitFault::BeforeRename) {
                bail!("{tool}: injected failure before replacing {display_path}");
            }
            #[cfg(test)]
            if matches!(fault, CommitFault::ReplaceTempName) {
                decoy = Some(rebind_temp_name(parent, &temp).with_context(|| {
                    format!("{tool}: cannot inject temporary replacement for {display_path}")
                })?);
            }
            #[cfg(test)]
            if matches!(fault, CommitFault::ChangeTargetBeforeRename) {
                std::fs::write(parent_path.join(leaf), FAULT_TARGET_BYTES).with_context(|| {
                    format!("{tool}: cannot inject target change for {display_path}")
                })?;
            }
            #[cfg(not(test))]
            let _ = fault;
            verify_target_unchanged(
                parent,
                parent_path,
                leaf,
                display_path,
                tool,
                expected_target,
            )?;
            verify_parent_binding(parent, parent_path, tool, display_path)?;
            verify_temp_binding(parent, parent_path, &temp, &file, bytes, display_path, tool)?;
            commit_rename(
                &file,
                parent,
                &temp,
                leaf,
                /* replace_existing */ expected_target.is_some(),
            )
            .with_context(|| format!("{tool}: cannot atomically replace {display_path}"))?;
            sync_parent(parent, tool, display_path)?;
            Ok(())
        })();
        if result.is_err() {
            discard_temp(&file, parent, &temp);
            #[cfg(test)]
            if let Some(decoy) = &decoy {
                discard_temp(decoy, parent, &temp);
            }
        }
        return result;
    }
    Err(last_collision.unwrap_or_else(|| {
        anyhow::anyhow!(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "temporary file name collision",
        ))
    }))
    .with_context(|| format!("{tool}: cannot create temporary file for {display_path}"))
}

/// What [`CommitFault::ChangeTargetBeforeRename`] leaves at the target, so a
/// test can tell an injected change apart from the edit it interrupted.
#[cfg(test)]
const FAULT_TARGET_BYTES: &[u8] = b"changed underneath the commit\n";

fn verify_target_unchanged(
    parent: &std::fs::File,
    parent_path: &Path,
    leaf: &OsStr,
    display_path: &str,
    tool: &str,
    expected: Option<&ExpectedTarget>,
) -> Result<()> {
    match (
        expected,
        open_regular_target(parent, parent_path, leaf, tool, display_path)?,
    ) {
        (None, None) => Ok(()),
        (Some(expected), Some(mut current)) => {
            if file_identity(&current.file)? != expected.identity {
                bail!(
                    "{tool}: {display_path} changed immediately before commit; read it again and retry"
                );
            }
            let current = fingerprint_file(&mut current.file).with_context(|| {
                format!("{tool}: cannot verify {display_path} immediately before commit")
            })?;
            if expected.version == current.version {
                Ok(())
            } else {
                bail!(
                    "{tool}: {display_path} changed immediately before commit; read it again and retry"
                )
            }
        }
        _ => bail!(
            "{tool}: {display_path} changed immediately before commit; read it again and retry"
        ),
    }
}

fn verify_temp_binding(
    parent: &std::fs::File,
    parent_path: &Path,
    temp: &OsStr,
    opened: &std::fs::File,
    expected_bytes: &[u8],
    display_path: &str,
    tool: &str,
) -> Result<()> {
    let mut named = open_regular_target(parent, parent_path, temp, tool, display_path)?
        .with_context(|| format!("{tool}: temporary file disappeared for {display_path}"))?;
    let named_identity = file_identity(&named.file)?;
    let (matches, _named_metadata) = file_contents_equal(&mut named.file, expected_bytes)
        .with_context(|| format!("{tool}: cannot verify temporary file for {display_path}"))?;
    if file_identity(opened)? != named_identity || !matches {
        bail!("{tool}: temporary file changed before replacing {display_path}; retry the call");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tools::dispatch_tools;
    use crate::tools::testutil::*;
    use kloop_protocol::ContentBlock;
    use kloop_protocol::ImageSource;
    use kloop_protocol::ToolResultContent;
    use serde_json::json;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;

    type Approval = (
        crate::permissions::ConfirmRequest,
        tokio::sync::oneshot::Sender<crate::permissions::Decision>,
    );

    struct ChannelApprover {
        tx: tokio::sync::mpsc::UnboundedSender<Approval>,
    }

    impl crate::permissions::Approver for ChannelApprover {
        fn confirm(
            &self,
            req: crate::permissions::ConfirmRequest,
        ) -> Pin<Box<dyn Future<Output = crate::permissions::Decision> + Send + '_>> {
            let (reply, decision) = tokio::sync::oneshot::channel();
            let sent = self.tx.send((req, reply)).is_ok();
            Box::pin(async move {
                if !sent {
                    return crate::permissions::Decision::Deny;
                }
                decision.await.unwrap_or(crate::permissions::Decision::Deny)
            })
        }
    }

    /// A gate that asks for every write, so the tests below can exercise the
    /// approval seam (the frozen target, the pinned workspace generation) on
    /// ordinary in-cwd paths. Those are *contained* writes, which the gate
    /// runs without asking in every mode; `ask` rules are the layer above
    /// containment, and the same one line a user writes to get a confirmation
    /// back for every edit.
    fn approval_ctx(
        tag: &str,
        cwd: &std::path::Path,
    ) -> (
        crate::tools::ToolCtx,
        tokio::sync::mpsc::UnboundedReceiver<Approval>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let permissions = crate::permissions::Permissions::new(
            crate::permissions::Mode::Manual,
            &crate::permissions::PermissionRules {
                ask: vec!["write_file(**)".into(), "edit_file(**)".into()],
                ..Default::default()
            },
            cwd.to_path_buf(),
            Some(Arc::new(ChannelApprover { tx })),
        )
        .unwrap();
        let mut ctx = test_ctx(0, tag);
        let mut cfg = ctx.cfg.test_clone();
        cfg.cwd = cwd.to_path_buf();
        cfg.permissions = Arc::new(permissions);
        ctx.cfg = Arc::new(cfg);
        (ctx, rx)
    }

    fn temp_file(tag: &str, content: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("kloop-tool-{}-{tag}", std::process::id()));
        std::fs::write(&path, content).unwrap();
        path
    }

    fn temp_bytes(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("kloop-tool-{}-{tag}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn commit_target<'a>(
        prepared: &'a super::PreparedMutation,
        display_path: &'a str,
        tool: &'static str,
    ) -> super::CommitTarget<'a> {
        super::CommitTarget {
            parent: &prepared.parent,
            parent_path: &prepared.parent_path,
            leaf: &prepared.leaf,
            display_path,
            tool,
        }
    }

    fn full_observation(path: &std::path::Path, bytes: &[u8]) -> super::FileObservation {
        let file = std::fs::File::open(path).unwrap();
        let metadata = file.metadata().unwrap();
        let identity = super::file_identity(&file).unwrap();
        super::FileObservation::full_with_identity(bytes, &metadata, identity)
    }

    async fn observe_whole(path: &std::path::Path, ctx: &crate::tools::ToolCtx) {
        let (out, is_error) =
            run_tool("read_file", json!({"path": path.to_str().unwrap()}), ctx).await;
        assert!(!is_error, "{out}");
    }

    #[tokio::test]
    async fn read_file_numbers_lines_with_offset_and_limit() {
        let path = temp_file("read", "alpha\nbeta\ngamma\ndelta\n");
        let ctx = test_ctx(0, "read");
        let (out, is_error) = run_tool(
            "read_file",
            json!({"path": path.to_str().unwrap(), "offset": 2, "limit": 2}),
            &ctx,
        )
        .await;
        assert!(!is_error);
        assert_eq!(
            out,
            "2\tbeta\n3\tgamma\n\n[showing lines 2-3 of 5; call read_file with offset=4 to continue]"
        );

        let (out, is_error) =
            run_tool("read_file", json!({"path": "/nonexistent/kloop"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("cannot read"));
        let _ = std::fs::remove_file(path);
    }

    /// The three exits a paged read can take. The `limit`-shaped one used to say
    /// nothing at all, which is what invited the reread-with-a-bigger-limit loop
    /// (plan 118).
    #[tokio::test]
    async fn read_file_reports_what_is_left_on_every_partial_exit() {
        let body: String = (1..=50).map(|n| format!("line {n}\n")).collect();
        let path = temp_file("read-remaining", &body);
        let p = path.to_str().unwrap();
        let ctx = test_ctx(0, "read-remaining");

        // limit stops short: say how many lines exist and where to resume.
        let (out, is_error) = run_tool("read_file", json!({"path": p, "limit": 10}), &ctx).await;
        assert!(!is_error);
        assert!(
            out.ends_with("[showing lines 1-10 of 51; call read_file with offset=11 to continue]"),
            "{out}"
        );

        // Mid-file page: the window is reported from the requested offset.
        let (out, _) = run_tool(
            "read_file",
            json!({"path": p, "offset": 20, "limit": 5}),
            &ctx,
        )
        .await;
        assert!(
            out.ends_with("[showing lines 20-24 of 51; call read_file with offset=25 to continue]"),
            "{out}"
        );

        // Read to the end: nothing appended.
        let (out, _) = run_tool("read_file", json!({"path": p}), &ctx).await;
        assert!(!out.contains("showing lines"), "{out}");
        assert!(out.ends_with("50\tline 50\n51\t"), "{out}");
    }

    #[tokio::test]
    async fn read_text_contract_covers_trailing_empty_eof_and_numeric_inputs() {
        let path = temp_file("read-contract", "first\nsecond\nthird\n");
        let p = path.to_str().unwrap();
        let ctx = test_ctx(0, "read-contract");

        let (out, is_error) = run_tool("read_file", json!({"path": p}), &ctx).await;
        assert!(!is_error);
        assert_eq!(out, "1\tfirst\n2\tsecond\n3\tthird\n4\t");

        let (out, is_error) = run_tool(
            "read_file",
            json!({"path": p, "offset": 0, "limit": 0}),
            &ctx,
        )
        .await;
        assert!(!is_error);
        assert_eq!(out, "0\tfirst\n1\tsecond\n2\tthird\n3\t");

        let (out, is_error) = run_tool(
            "read_file",
            json!({"path": p, "offset": "2", "limit": "1"}),
            &ctx,
        )
        .await;
        assert!(!is_error);
        assert_eq!(
            out,
            "2\tsecond\n\n[showing lines 2-2 of 4; call read_file with offset=3 to continue]"
        );

        let (out, is_error) = run_tool("read_file", json!({"path": p, "offset": 100}), &ctx).await;
        assert!(!is_error);
        assert_eq!(
            out,
            "<system-reminder>Warning: the file exists but is shorter than the provided offset (100). The file has 4 lines.</system-reminder>"
        );

        for input in [
            json!({"path": p, "offset": -1}),
            json!({"path": p, "limit": 1.5}),
            json!({"path": p, "offset": false}),
        ] {
            let (out, is_error) = run_tool("read_file", input, &ctx).await;
            assert!(is_error, "{out}");
            assert!(out.contains("whole number"), "{out}");
        }
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn read_file_enforces_format_specific_raw_byte_limits() {
        let text_at_limit = temp_bytes("read-5m.txt", &vec![b'x'; super::MAX_IMAGE_BYTES]);
        let text_over_limit = temp_bytes(
            "read-5m-plus-one.txt",
            &vec![b'x'; super::MAX_IMAGE_BYTES + 1],
        );
        let mut notebook =
            br#"{"cells":[],"metadata":{},"nbformat":4,"nbformat_minor":5}"#.to_vec();
        notebook.resize(super::notebook::MAX_NOTEBOOK_BYTES, b' ');
        let notebook_at_limit = temp_bytes("read-10m.ipynb", &notebook);
        notebook.push(b' ');
        let notebook_over_limit = temp_bytes("read-10m-plus-one.ipynb", &notebook);
        let text_over_key = std::fs::canonicalize(&text_over_limit).unwrap();
        let notebook_over_key = std::fs::canonicalize(&notebook_over_limit).unwrap();
        let ctx = test_ctx(0, "read-raw-limits");

        for path in [&text_at_limit, &notebook_at_limit] {
            let (out, is_error) =
                run_tool("read_file", json!({"path": path.to_str().unwrap()}), &ctx).await;
            assert!(!is_error, "{}: {out}", path.display());
        }
        for path in [&text_over_limit, &notebook_over_limit] {
            let (out, is_error) =
                run_tool("read_file", json!({"path": path.to_str().unwrap()}), &ctx).await;
            assert!(is_error, "{}: {out}", path.display());
            assert!(out.contains("over the"), "{out}");
            assert!(out.contains("byte limit"), "{out}");
        }
        assert!(ctx.cfg.file_state.observation(&text_over_key).is_none());
        assert!(ctx.cfg.file_state.observation(&notebook_over_key).is_none());

        let _ = std::fs::remove_file(text_at_limit);
        let _ = std::fs::remove_file(text_over_limit);
        let _ = std::fs::remove_file(notebook_at_limit);
        let _ = std::fs::remove_file(notebook_over_limit);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sparse_oversized_read_fails_without_observation() {
        let path = temp_bytes("read-sparse.txt", b"");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len((super::MAX_IMAGE_BYTES + 1) as u64)
            .unwrap();
        let key = std::fs::canonicalize(&path).unwrap();
        let ctx = test_ctx(0, "read-sparse");

        let (out, is_error) =
            run_tool("read_file", json!({"path": path.to_str().unwrap()}), &ctx).await;

        assert!(is_error, "{out}");
        assert!(out.contains("byte limit"), "{out}");
        assert!(ctx.cfg.file_state.observation(&key).is_none());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn read_empty_pdf_and_character_budget_are_explicit() {
        let empty = temp_file("read-empty", "");
        let pdf = temp_bytes("read-pdf", b"%PDF-1.4\nminimal\n");
        // Long enough to exceed the read budget whatever it is set to.
        let long = temp_file(
            "read-budget",
            &"long line content\n".repeat(super::READ_CONTENT_CHARS / 8),
        );
        let long_key = std::fs::canonicalize(&long).unwrap();
        let ctx = test_ctx(0, "read-bounds");

        let (out, is_error) =
            run_tool("read_file", json!({"path": empty.to_str().unwrap()}), &ctx).await;
        assert!(!is_error);
        assert_eq!(
            out,
            "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>"
        );

        let (out, is_error) =
            run_tool("read_file", json!({"path": pdf.to_str().unwrap()}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("PDF files are not supported"), "{out}");

        let (out, is_error) =
            run_tool("read_file", json!({"path": long.to_str().unwrap()}), &ctx).await;
        assert!(!is_error);
        // Below the offload threshold, so a capped read still reaches the model
        // whole instead of spilling to disk — the two caps move together.
        assert!(
            out.chars().count() < crate::history::OFFLOAD_CAP_CHARS,
            "{} chars",
            out.chars().count()
        );
        assert!(
            out.contains("call read_file with offset=") || out.contains("truncated within line"),
            "{out}"
        );
        assert!(
            !ctx.cfg
                .file_state
                .observation(&long_key)
                .unwrap()
                .is_complete()
        );

        let _ = std::fs::remove_file(empty);
        let _ = std::fs::remove_file(pdf);
        let _ = std::fs::remove_file(long);
    }

    /// read_file sniffs the magic bytes: a PNG file returns a single image
    /// block (ToolResultContent::Blocks), not text — offset/limit do not apply.
    #[tokio::test]
    async fn read_file_returns_image_block_for_image_file() {
        // Minimal valid PNG magic bytes; content beyond the signature is opaque.
        let png: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
        let path = temp_bytes("readimg", png);
        let ctx = test_ctx(0, "readimg");
        let results = dispatch_tools(
            vec![(
                "t".into(),
                "read_file".into(),
                json!({"path": path.to_str().unwrap()}),
            )],
            &ctx,
        )
        .await;
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = &results[0]
        else {
            panic!("expected tool result");
        };
        assert!(!is_error);
        let ToolResultContent::Blocks(blocks) = content else {
            panic!("expected image blocks, got {content:?}");
        };
        assert!(matches!(
            blocks.as_slice(),
            [ContentBlock::Image {
                source: ImageSource::Base64 { media_type, .. },
            }] if media_type == "image/png"
        ));
        let _ = std::fs::remove_file(path);
    }

    /// A screenshot-shaped image that is small in bytes but over the wire's
    /// pixel budget comes back downscaled, and the model is told so in the same
    /// result — a silent downscale would have it reporting coordinates off a
    /// copy it does not know it is looking at.
    #[tokio::test]
    async fn read_file_downscales_a_pixel_oversized_image_and_says_so() {
        let buffer = image::RgbImage::from_fn(2100, 1400, |x, y| {
            image::Rgb([(x / 256) as u8, (y / 256) as u8, 128])
        });
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(buffer)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let path = temp_bytes("readimg-big", &png);
        let ctx = test_ctx(0, "readimg-big");
        let results = dispatch_tools(
            vec![(
                "t".into(),
                "read_file".into(),
                json!({"path": path.to_str().unwrap()}),
            )],
            &ctx,
        )
        .await;
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = &results[0]
        else {
            panic!("expected tool result");
        };
        assert!(!is_error);
        let ToolResultContent::Blocks(blocks) = content else {
            panic!("expected image blocks, got {content:?}");
        };
        let [ContentBlock::Image { .. }, ContentBlock::Text { text }] = blocks.as_slice() else {
            panic!("expected an image block followed by the notice, got {blocks:?}");
        };
        assert_eq!(
            text,
            "<system-reminder>This image was downscaled from 2100x1400 to 2000x1333 \
             before it was sent. Any pixel coordinate you read off it refers to the \
             downscaled copy.</system-reminder>"
        );
        let _ = std::fs::remove_file(path);
    }

    /// A binary file that is neither UTF-8 text nor a supported image is a
    /// clean error, not a panic or garbled read.
    #[tokio::test]
    async fn read_file_errors_on_non_image_binary() {
        let path = temp_bytes("readbin", &[0x00, 0xFF, 0xFE, 0x01, 0x80]);
        let ctx = test_ctx(0, "readbin");
        let (out, is_error) =
            run_tool("read_file", json!({"path": path.to_str().unwrap()}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("not UTF-8 text or a supported image"), "{out}");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn successful_read_commits_and_merges_session_observation() {
        let path = temp_file("observe", "alpha\nbeta\ngamma\n");
        let key = std::fs::canonicalize(&path).unwrap();
        let ctx = test_ctx(0, "observe");

        let (_, is_error) = run_tool(
            "read_file",
            json!({"path": path.to_str().unwrap(), "offset": 1, "limit": 1}),
            &ctx,
        )
        .await;
        assert!(!is_error);
        assert!(!ctx.cfg.file_state.observation(&key).unwrap().is_complete());

        let (_, is_error) = run_tool(
            "read_file",
            json!({"path": path.to_str().unwrap(), "offset": 2, "limit": 3}),
            &ctx,
        )
        .await;
        assert!(!is_error);
        assert!(ctx.cfg.file_state.observation(&key).unwrap().is_complete());
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_cancelled_during_post_hook_does_not_commit_observation() {
        use crate::hooks::HookDef;
        use crate::hooks::HookEvent;
        use crate::hooks::Hooks;
        use std::sync::Arc;

        let path = temp_file("observe-cancel", "visible bytes\n");
        let key = std::fs::canonicalize(&path).unwrap();
        let marker = temp_file("observe-cancel-marker", "");
        let _ = std::fs::remove_file(&marker);
        let mut ctx = test_ctx(0, "observe-cancel");
        let mut cfg = ctx.cfg.test_clone();
        cfg.hooks = Arc::new(Hooks {
            defs: vec![HookDef {
                event: HookEvent::PostTool,
                command: vec![
                    "sh".into(),
                    "-c".into(),
                    format!("touch '{}'; sleep 10", marker.display()),
                ],
                matcher: Some("read_file".into()),
                timeout_ms: 20_000,
            }],
        });
        ctx.cfg = Arc::new(cfg);

        let call_ctx = ctx.clone();
        let input_path = path.to_string_lossy().to_string();
        let task = tokio::spawn(async move {
            dispatch_tools(
                vec![(
                    "read-cancel".into(),
                    "read_file".into(),
                    json!({"path": input_path}),
                )],
                &call_ctx,
            )
            .await
        });
        for _ in 0..100 {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(marker.exists(), "post hook reached after executor read");
        ctx.cancel.cancel();
        assert_eq!(
            task.await.unwrap(),
            vec![ContentBlock::ToolResult {
                tool_use_id: "read-cancel".into(),
                content: "interrupted".into(),
                is_error: true,
            }]
        );
        assert!(ctx.cfg.file_state.observation(&key).is_none());
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(marker);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn approval_wait_cannot_retarget_read_alias_to_sensitive_file() {
        let root =
            std::env::temp_dir().join(format!("kloop-read-approval-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".kloop")).unwrap();
        let safe = root.join("safe.txt");
        let secret = root.join(".kloop/config.toml");
        let alias = root.join("alias.txt");
        std::fs::write(&safe, "safe bytes").unwrap();
        std::fs::write(&secret, "SENTINEL-SECRET").unwrap();
        std::os::unix::fs::symlink(&safe, &alias).unwrap();

        let (tx, mut approvals) = tokio::sync::mpsc::unbounded_channel();
        let rules = crate::permissions::PermissionRules {
            ask: vec!["read_file(alias.txt)".into()],
            ..Default::default()
        };
        let permissions = crate::permissions::Permissions::new(
            crate::permissions::Mode::Manual,
            &rules,
            root.clone(),
            Some(Arc::new(ChannelApprover { tx })),
        )
        .unwrap();
        let mut ctx = test_ctx(0, "read-approval-swap");
        let mut cfg = ctx.cfg.test_clone();
        cfg.cwd = root.clone();
        cfg.permissions = Arc::new(permissions);
        ctx.cfg = Arc::new(cfg);
        let call_ctx = ctx.clone();
        let task = tokio::spawn(async move {
            run_tool("read_file", json!({"path": "alias.txt"}), &call_ctx).await
        });
        let (_request, reply) =
            tokio::time::timeout(std::time::Duration::from_secs(1), approvals.recv())
                .await
                .expect("read ask rule must reach the approver")
                .expect("approval channel remains open");
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&secret, &alias).unwrap();
        reply
            .send(crate::permissions::Decision::Allow(
                crate::permissions::ApprovalScope::Once,
            ))
            .expect("approval receiver still waiting");

        let (out, is_error) = task.await.unwrap();
        assert!(!is_error, "{out}");
        assert!(out.contains("safe bytes"), "{out}");
        assert!(!out.contains("SENTINEL-SECRET"), "{out}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn read_binding_rejects_parent_swap_between_canonicalize_and_open() {
        let root =
            std::env::temp_dir().join(format!("kloop-read-prepare-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let safe_parent = root.join("safe");
        let moved_parent = root.join("safe-before-swap");
        let secret_parent = root.join(".kloop");
        std::fs::create_dir_all(&safe_parent).unwrap();
        std::fs::create_dir_all(&secret_parent).unwrap();
        let requested = safe_parent.join("config.toml");
        std::fs::write(&requested, "safe bytes").unwrap();
        std::fs::write(secret_parent.join("config.toml"), "SENTINEL-SECRET").unwrap();
        let resolved = std::fs::canonicalize(&requested).unwrap();

        std::fs::rename(&safe_parent, &moved_parent).unwrap();
        std::os::unix::fs::symlink(&secret_parent, &safe_parent).unwrap();
        let error = match super::bind_read_target(&requested, resolved, "safe/config.toml") {
            Ok(_) => panic!("swapped parent unexpectedly bound a read target"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("parent directory")
                || error.to_string().contains("path changed"),
            "{error:#}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_refuses_hard_link_aliases_without_observation() {
        let root = std::env::temp_dir().join(format!("kloop-read-hardlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let secret = root.join(".env");
        let alias = root.join("safe.txt");
        std::fs::write(&secret, "SENTINEL-SECRET").unwrap();
        std::fs::hard_link(&secret, &alias).unwrap();
        let key = std::fs::canonicalize(&alias).unwrap();
        let ctx = test_ctx(0, "read-hardlink");

        let (out, is_error) =
            run_tool("read_file", json!({"path": alias.to_str().unwrap()}), &ctx).await;
        assert!(is_error, "{out}");
        assert!(out.contains("multiple hard links"), "{out}");
        assert!(!out.contains("SENTINEL-SECRET"), "{out}");
        assert!(ctx.cfg.file_state.observation(&key).is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn write_file_creates_missing_parents_only_after_approval() {
        let dir = std::env::temp_dir().join(format!("kloop-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let effective_dir = std::fs::canonicalize(&dir).unwrap();
        let path = dir.join("deep/nested/file.txt");
        let denied = dir.join("denied/parent/file.txt");
        let (ctx, mut approvals) = approval_ctx("write", &dir);

        let call_ctx = ctx.clone();
        let call_path = path.to_string_lossy().to_string();
        let task = tokio::spawn(async move {
            run_tool(
                "write_file",
                json!({"path": call_path, "content": "created"}),
                &call_ctx,
            )
            .await
        });
        let (request, reply) =
            tokio::time::timeout(std::time::Duration::from_secs(1), approvals.recv())
                .await
                .expect("write must reach approval")
                .expect("approval channel remains open");
        assert!(
            !dir.join("deep").exists(),
            "approval wait has no side effects"
        );
        let preview = request
            .preview
            .expect("new write carries a preview")
            .text()
            .to_string();
        assert!(
            preview.contains("will create parent directories"),
            "{preview}"
        );
        assert!(
            preview.contains(&effective_dir.join("deep").display().to_string()),
            "{preview}"
        );
        assert!(
            preview.contains(&effective_dir.join("deep/nested").display().to_string()),
            "{preview}"
        );
        reply
            .send(crate::permissions::Decision::Allow(
                crate::permissions::ApprovalScope::Once,
            ))
            .expect("approval receiver still waiting");
        let (out, is_error) = task.await.unwrap();
        assert!(!is_error, "{out}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "created");

        let call_ctx = ctx.clone();
        let call_path = denied.to_string_lossy().to_string();
        let task = tokio::spawn(async move {
            run_tool(
                "write_file",
                json!({"path": call_path, "content": "denied"}),
                &call_ctx,
            )
            .await
        });
        let (_request, reply) =
            tokio::time::timeout(std::time::Duration::from_secs(1), approvals.recv())
                .await
                .expect("second write must reach approval")
                .expect("approval channel remains open");
        reply
            .send(crate::permissions::Decision::Deny)
            .expect("approval receiver still waiting");
        let (out, is_error) = task.await.unwrap();
        assert!(is_error, "{out}");
        assert!(!dir.join("denied").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recursive_write_rejects_symlink_and_leaf_races_but_reuses_safe_directories() {
        let root =
            std::env::temp_dir().join(format!("kloop-write-parent-races-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let outside = root.with_extension("outside");
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        let (ctx, mut approvals) = approval_ctx("write-parent-races", &root);

        let symlink_target = root.join("linked/nested/file.txt");
        let call_ctx = ctx.clone();
        let call_path = symlink_target.to_string_lossy().to_string();
        let task = tokio::spawn(async move {
            run_tool(
                "write_file",
                json!({"path": call_path, "content": "must-not-escape"}),
                &call_ctx,
            )
            .await
        });
        let (_request, reply) = approvals.recv().await.unwrap();
        std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
        reply
            .send(crate::permissions::Decision::Allow(
                crate::permissions::ApprovalScope::Once,
            ))
            .unwrap();
        let (out, is_error) = task.await.unwrap();
        assert!(is_error, "{out}");
        assert!(!outside.join("nested/file.txt").exists());

        let safe_target = root.join("safe/nested/file.txt");
        let call_ctx = ctx.clone();
        let call_path = safe_target.to_string_lossy().to_string();
        let task = tokio::spawn(async move {
            run_tool(
                "write_file",
                json!({"path": call_path, "content": "safe"}),
                &call_ctx,
            )
            .await
        });
        let (_request, reply) = approvals.recv().await.unwrap();
        std::fs::create_dir_all(root.join("safe/nested")).unwrap();
        reply
            .send(crate::permissions::Decision::Allow(
                crate::permissions::ApprovalScope::Once,
            ))
            .unwrap();
        let (out, is_error) = task.await.unwrap();
        assert!(!is_error, "{out}");
        assert_eq!(std::fs::read_to_string(&safe_target).unwrap(), "safe");

        let raced_target = root.join("raced/nested/file.txt");
        let call_ctx = ctx.clone();
        let call_path = raced_target.to_string_lossy().to_string();
        let task = tokio::spawn(async move {
            run_tool(
                "write_file",
                json!({"path": call_path, "content": "replacement"}),
                &call_ctx,
            )
            .await
        });
        let (_request, reply) = approvals.recv().await.unwrap();
        std::fs::create_dir_all(root.join("raced/nested")).unwrap();
        std::fs::write(&raced_target, "competitor").unwrap();
        reply
            .send(crate::permissions::Decision::Allow(
                crate::permissions::ApprovalScope::Once,
            ))
            .unwrap();
        let (out, is_error) = task.await.unwrap();
        assert!(is_error, "{out}");
        assert!(out.contains("must read"), "{out}");
        assert_eq!(
            std::fs::read_to_string(&raced_target).unwrap(),
            "competitor"
        );

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[tokio::test]
    async fn edit_failure_never_creates_missing_parents() {
        let root =
            std::env::temp_dir().join(format!("kloop-edit-no-parents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let edit_path = root.join("edit/nested/file.txt");
        let ctx = test_ctx(0, "edit-no-parents");

        let (out, is_error) = run_tool(
            "edit_file",
            json!({
                "path": edit_path.to_str().unwrap(),
                "old_string": "old",
                "new_string": "new"
            }),
            &ctx,
        )
        .await;

        assert!(is_error, "{out}");
        assert!(out.contains("parent directory"), "{out}");
        assert!(!root.join("edit").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn existing_write_requires_a_complete_fresh_read_and_refreshes_state() {
        let path = temp_file("write-fresh", "original\nsecond\n");
        let key = std::fs::canonicalize(&path).unwrap();
        let ctx = test_ctx(0, "write-fresh");

        let (out, is_error) = run_tool(
            "write_file",
            json!({"path": path.to_str().unwrap(), "content": "unread"}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("must read"), "{out}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "original\nsecond\n"
        );

        let (_, is_error) = run_tool(
            "read_file",
            json!({"path": path.to_str().unwrap(), "limit": 1}),
            &ctx,
        )
        .await;
        assert!(!is_error);
        let (out, is_error) = run_tool(
            "write_file",
            json!({"path": path.to_str().unwrap(), "content": "partial"}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("entire file"), "{out}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "original\nsecond\n"
        );

        observe_whole(&path, &ctx).await;
        std::fs::write(&path, "external\n").unwrap();
        let (out, is_error) = run_tool(
            "write_file",
            json!({"path": path.to_str().unwrap(), "content": "stale"}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("changed since it was read"), "{out}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external\n");
        // Nothing was written, so the read stays on record and keeps naming the
        // real problem — a cleared slot would refuse the next write for never
        // having read the file at all (plan 195).
        assert!(ctx.cfg.file_state.observation(&key).is_some());

        observe_whole(&path, &ctx).await;
        let (out, is_error) = run_tool(
            "write_file",
            json!({"path": path.to_str().unwrap(), "content": "committed\n"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "committed\n");
        let observation = ctx.cfg.file_state.observation(&key).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        assert!(observation.is_complete());
        assert!(observation.version().matches(b"committed\n", &metadata));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn explicit_write_content_over_text_read_limit_still_commits() {
        let dir =
            std::env::temp_dir().join(format!("kloop-large-explicit-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("large.txt");
        let content = "x".repeat(super::MAX_IMAGE_BYTES + 1);
        let ctx = test_ctx(0, "large-explicit-write");

        let (out, is_error) = run_tool(
            "write_file",
            json!({"path": path.to_str().unwrap(), "content": content}),
            &ctx,
        )
        .await;

        assert!(!is_error, "{out}");
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            (super::MAX_IMAGE_BYTES + 1) as u64
        );
        let key = std::fs::canonicalize(&path).unwrap();
        assert!(ctx.cfg.file_state.observation(&key).unwrap().is_complete());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn edit_over_text_read_limit_fails_before_temp_and_keeps_authority() {
        let dir = std::env::temp_dir().join(format!("kloop-large-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("large.txt");
        let bytes = vec![b'a'; super::MAX_IMAGE_BYTES + 1];
        std::fs::write(&path, &bytes).unwrap();
        let key = std::fs::canonicalize(&path).unwrap();
        let ctx = test_ctx(0, "large-edit");
        ctx.cfg
            .file_state
            .apply(crate::file_state::FileStateUpdate::Replace {
                path: key.clone(),
                observation: full_observation(&path, &bytes),
            });

        let (out, is_error) = run_tool(
            "edit_file",
            json!({
                "path": path.to_str().unwrap(),
                "old_string": "a",
                "new_string": "b",
                "replace_all": true
            }),
            &ctx,
        )
        .await;

        assert!(is_error, "{out}");
        assert!(out.contains("byte limit"), "{out}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        // Nothing was written, so the read still describes the file (plan 195).
        assert!(ctx.cfg.file_state.observation(&key).is_some());
        assert!(std::fs::read_dir(&dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".kloop-write-")
        }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn deleting_a_read_file_does_not_turn_overwrite_into_create() {
        let dir = std::env::temp_dir().join(format!("kloop-write-delete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("target.txt");
        std::fs::write(&path, "original\n").unwrap();
        let ctx = test_ctx(0, "write-delete");
        observe_whole(&path, &ctx).await;
        let key = std::fs::canonicalize(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        let (out, is_error) = run_tool(
            "write_file",
            json!({"path": path.to_str().unwrap(), "content": "recreated\n"}),
            &ctx,
        )
        .await;

        assert!(is_error);
        assert!(out.contains("changed since it was read"), "{out}");
        assert!(!path.exists(), "stale failure must not recreate the leaf");
        // The one refusal whose recovery depends on forgetting: a read of a path
        // that no longer exists is a dead qualification, and keeping it would
        // refuse every later write of this path with no way left to clear it.
        assert!(ctx.cfg.file_state.observation(&key).is_none());
        #[cfg(windows)]
        assert!(
            !dir.exists(),
            "Windows handle cleanup should remove the parent"
        );
        #[cfg(not(windows))]
        assert!(
            dir.is_dir(),
            "Unix conservatively retains the newly created parent"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The shape of the two real failures plan 195 came from: something rewrote
    /// the file elsewhere, and the anchor the model was holding is still there,
    /// still unique. One call, and the result says the file moved.
    ///
    /// What the old refusal bought was a round trip: the model re-read thirty
    /// arbitrary lines — which is all `AnyRead` ever asked for — and re-sent a
    /// byte-identical `old_string`. The freshness it named was never in the
    /// admission check; it is in the anchor, and in the commit's compare-and-swap.
    #[tokio::test]
    async fn a_changed_file_still_takes_an_anchored_edit() {
        let path = temp_file("edit-stale", "alpha\nbeta\n");
        let target = path.to_str().unwrap();
        let key = std::fs::canonicalize(&path).unwrap();
        let ctx = test_ctx(0, "edit-stale");
        observe_whole(&path, &ctx).await;
        std::fs::write(&path, "external\nalpha\nbeta\n").unwrap();

        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": target, "old_string": "beta", "new_string": "BETA"}),
            &ctx,
        )
        .await;

        assert_eq!(
            (out, is_error),
            (
                format!("edited {target} (1 replacement(s)); the file changed since you read it"),
                false
            )
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "external\nalpha\nBETA\n"
        );
        // The commit refreshes the qualification, so the change is reported once
        // and the next edit of the same turn owes nothing.
        assert!(ctx.cfg.file_state.observation(&key).unwrap().is_complete());
        let _ = std::fs::remove_file(path);
    }

    /// The fact the old order spent and threw away. A missing or ambiguous
    /// `old_string` is exactly when "the file moved under you" is worth hearing,
    /// and before plan 195 it was consumed by an admission check that ran before
    /// the anchor had been tested at all.
    #[tokio::test]
    async fn anchor_refusals_on_a_changed_file_say_it_changed() {
        let path = temp_file("edit-stale-refusal", "alpha\nbeta\n");
        let target = path.to_str().unwrap();
        let ctx = test_ctx(0, "edit-stale-refusal");
        observe_whole(&path, &ctx).await;
        std::fs::write(&path, "beta\nalpha\nbeta\n").unwrap();

        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": target, "old_string": "gamma", "new_string": "x"}),
            &ctx,
        )
        .await;
        assert_eq!(
            (out, is_error),
            (
                format!(
                    "edit_file: old_string not found in {target}; the file changed since you read \
                     it; nothing in the file is close enough to show. The text may have changed, or \
                     differ by more than punctuation: re-read the small range you meant to edit, \
                     rebuild old_string from that fresh text, and do not retry this old_string \
                     unchanged"
                ),
                true
            )
        );

        // Reached only because the refusal above kept the qualification: one
        // root cause, one diagnosis.
        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": target, "old_string": "beta", "new_string": "BETA"}),
            &ctx,
        )
        .await;
        assert_eq!(
            (out, is_error),
            (
                format!(
                    "edit_file: old_string matches 2 times in {target}; the file changed since you \
                     read it; add surrounding context to disambiguate or set replace_all"
                ),
                true
            )
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "beta\nalpha\nbeta\n"
        );
        let _ = std::fs::remove_file(path);
    }

    /// A `touch` or a `chmod` is not a change. The note has to stay rare to stay
    /// worth reading, and the whole-file fingerprint is what tells a rewrite from
    /// a metadata bump — which is also why the check this replaces could never
    /// have made the call: it ran before the bytes were read.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_metadata_only_change_draws_no_note() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = temp_file("edit-chmod", "alpha\nbeta\n");
        let target = path.to_str().unwrap();
        let key = std::fs::canonicalize(&path).unwrap();
        let ctx = test_ctx(0, "edit-chmod");
        observe_whole(&path, &ctx).await;
        let observation = ctx.cfg.file_state.observation(&key).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            !observation
                .version()
                .metadata_matches(&std::fs::metadata(&path).unwrap()),
            "the chmod must be the kind of change the old admission check refused"
        );

        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": target, "old_string": "beta", "new_string": "BETA"}),
            &ctx,
        )
        .await;

        assert_eq!(
            (out, is_error),
            (format!("edited {target} (1 replacement(s))"), false)
        );
        let _ = std::fs::remove_file(path);
    }

    /// Another harness's spelling reaches the same edit. `file_path` is
    /// claude-code's and grok-build's name for `path`; `old_str`/`new_str` are the
    /// Anthropic text-editor lineage's, which deepseek-harness follows. A model
    /// carries whichever its training saw most, and the edit it asked for is the
    /// edit that happens.
    #[tokio::test]
    async fn another_harnesses_spelling_reaches_the_same_edit() {
        let path = temp_file("edit-synonyms", "alpha\nbeta\n");
        let target = path.to_str().unwrap();
        let ctx = test_ctx(0, "edit-synonyms");
        observe_whole(&path, &ctx).await;

        let (out, is_error) = run_tool(
            "edit_file",
            json!({"file_path": target, "old_str": "beta", "new_str": "BETA"}),
            &ctx,
        )
        .await;
        assert_eq!(
            (out, is_error),
            (format!("edited {target} (1 replacement(s))"), false)
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha\nBETA\n");

        // `file_text` is the same lineage's name for write_file's `content`.
        let (out, is_error) = run_tool(
            "write_file",
            json!({"file_path": target, "file_text": "rewritten\n"}),
            &ctx,
        )
        .await;
        assert_eq!(
            (out, is_error),
            (format!("wrote 10 bytes to {target}"), false)
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "rewritten\n");
        let _ = std::fs::remove_file(path);
    }

    /// A name that is nobody's spelling is still a refusal, and it names both the
    /// argument the tool wanted and the keys the call did carry: these three tools
    /// take no allow-list, so an unknown key is dropped in silence and no other
    /// message would mention it.
    #[tokio::test]
    async fn an_unknown_argument_name_names_itself() {
        let path = temp_file("edit-wrong-arg", "alpha\n");
        let ctx = test_ctx(0, "edit-wrong-arg");

        let (out, is_error) = run_tool(
            "edit_file",
            json!({
                "filepath": path.to_str().unwrap(),
                "old_string": "alpha",
                "new_string": "ALPHA"
            }),
            &ctx,
        )
        .await;
        assert_eq!(
            (out, is_error),
            (
                "edit_file: missing required string argument 'path' \
                 (got: filepath, new_string, old_string)"
                    .to_string(),
                true
            )
        );

        // An argument that is present but not a string has nothing to point at,
        // and the refusal already names it: the message stays as it was.
        let (out, is_error) = run_tool("edit_file", json!({"path": 5}), &ctx).await;
        assert_eq!(
            (out, is_error),
            (
                "edit_file: missing required string argument 'path'".to_string(),
                true
            )
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha\n");
        let _ = std::fs::remove_file(path);
    }

    /// The mirror of the missing-argument refusal: an allow-list already names the
    /// key it refused, which left the model to guess the one it should have used.
    /// `path` never reaches here any more — the synonym rename turns it into
    /// `notebook_path` first — so what does reach it is a name nobody uses, and
    /// that is exactly when the accepted set is worth printing.
    #[tokio::test]
    async fn an_unexpected_notebook_field_names_what_is_accepted() {
        let notebook = r#"{"cells":[{"cell_type":"code","id":"c1","source":["x = 1"],"metadata":{},"outputs":[],"execution_count":null}],"metadata":{},"nbformat":4,"nbformat_minor":5}"#;
        let path = temp_file("notebook-field.ipynb", notebook);
        let target = path.to_str().unwrap();
        let ctx = test_ctx(0, "notebook-field");
        observe_whole(&path, &ctx).await;

        let (out, is_error) = run_tool(
            "notebook_edit",
            json!({"notebook_path": target, "cell_id": "c1", "source": "x = 2"}),
            &ctx,
        )
        .await;

        assert_eq!(
            (out, is_error),
            (
                "notebook_edit: unexpected input field \"source\"; accepted: \
                 notebook_path, cell_id, new_source, cell_type, edit_mode"
                    .to_string(),
                true
            )
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), notebook);
        let _ = std::fs::remove_file(path);
    }

    /// The reason the rename happens at the dispatch seam and not inside the tool:
    /// a path that arrived as `file_path` has to be the path the gate matches its
    /// rules against. Renaming after the gate would hand it a call with no path in
    /// it at all.
    #[tokio::test]
    async fn a_synonym_path_still_faces_the_permission_rules() {
        let root = std::env::temp_dir().join(format!("kloop-synonym-deny-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        std::fs::write(root.join("guarded.txt"), "original\n").unwrap();

        let deny = crate::permissions::PermissionRules {
            deny: vec!["edit_file(guarded.txt)".into()],
            ..Default::default()
        };
        let permissions = crate::permissions::Permissions::new(
            crate::permissions::Mode::Bypass,
            &deny,
            root.clone(),
            None,
        )
        .unwrap();
        let mut ctx = test_ctx(0, "synonym-deny");
        let mut cfg = ctx.cfg.test_clone();
        cfg.cwd = root.clone();
        cfg.permissions = Arc::new(permissions);
        ctx.cfg = Arc::new(cfg);

        let (out, is_error) = run_tool(
            "edit_file",
            json!({"file_path": "guarded.txt", "old_string": "original", "new_string": "DENIED"}),
            &ctx,
        )
        .await;

        assert!(is_error, "{out}");
        assert!(out.contains("blocked by a deny"), "{out}");
        assert_eq!(
            std::fs::read_to_string(root.join("guarded.txt")).unwrap(),
            "original\n"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// The line plan 195 does not cross, half one. `write_file` replaces every
    /// byte, so it has no anchor to be right about: overwriting a file that
    /// changed since the read discards whatever the change was, and no remark
    /// substitutes for not doing that. The same scenario `edit_file` now takes.
    #[tokio::test]
    async fn write_file_still_refuses_a_changed_file() {
        let path = temp_file("write-stale", "alpha\nbeta\n");
        let target = path.to_str().unwrap();
        let ctx = test_ctx(0, "write-stale");
        observe_whole(&path, &ctx).await;
        std::fs::write(&path, "external\nalpha\nbeta\n").unwrap();

        let (out, is_error) = run_tool(
            "write_file",
            json!({"path": target, "content": "replacement\n"}),
            &ctx,
        )
        .await;

        assert_eq!(
            (out, is_error),
            (
                format!(
                    "write_file: {target} changed since it was read; read it again before modifying it"
                ),
                true
            )
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "external\nalpha\nbeta\n"
        );
        let _ = std::fs::remove_file(path);
    }

    /// The line plan 195 does not cross, half two. `notebook_edit` asks for a
    /// complete notebook read because it addresses cells, not bytes; a file that
    /// moved has cells it never saw, and its refusals keep their own wording.
    #[tokio::test]
    async fn notebook_edit_still_refuses_a_changed_file() {
        let notebook = |source: &str| {
            format!(
                r#"{{"cells":[{{"cell_type":"code","id":"c1","source":["{source}"],"metadata":{{}},"outputs":[],"execution_count":null}}],"metadata":{{}},"nbformat":4,"nbformat_minor":5}}"#
            )
        };
        let path = temp_file("notebook-stale.ipynb", &notebook("x = 1"));
        let target = path.to_str().unwrap();
        let ctx = test_ctx(0, "notebook-stale");
        observe_whole(&path, &ctx).await;
        let changed = notebook("x = 2");
        std::fs::write(&path, &changed).unwrap();

        let (out, is_error) = run_tool(
            "notebook_edit",
            json!({"notebook_path": target, "cell_id": "c1", "new_source": "x = 3"}),
            &ctx,
        )
        .await;

        assert_eq!(
            (out, is_error),
            (
                "File has been modified since read, either by the user or by a linter. \
                 Read it again before attempting to write it."
                    .to_string(),
                true
            )
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), changed);
        let _ = std::fs::remove_file(path);
    }

    /// The tolerant layer says it was used and where, so the model can check
    /// the one line it did not spell exactly — and the file keeps its own
    /// typography everywhere the edit did not reach.
    #[tokio::test]
    async fn a_punctuation_tolerant_edit_says_so_and_where() {
        let path = temp_file("edit-tolerant", "intro\nlet s = “ok” — done;\n");
        let target = path.to_str().unwrap();
        let ctx = test_ctx(0, "edit-tolerant");
        observe_whole(&path, &ctx).await;

        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": target, "old_string": "let s = \"ok\" - done;", "new_string": "let t = \"ok\" - done;"}),
            &ctx,
        )
        .await;
        assert_eq!(
            (out, is_error),
            (
                format!(
                    "edited {target} (1 replacement(s), punctuation/whitespace-tolerant match at line 2)"
                ),
                false
            )
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "intro\nlet t = “ok” — done;\n"
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn a_tolerant_duplicate_says_how_it_was_counted() {
        let path = temp_file("edit-tolerant-dup", "“a”\n“a”\n");
        let target = path.to_str().unwrap();
        let ctx = test_ctx(0, "edit-tolerant-dup");
        observe_whole(&path, &ctx).await;

        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": target, "old_string": "\"a\"", "new_string": "b"}),
            &ctx,
        )
        .await;
        assert_eq!(
            (out, is_error),
            (
                format!(
                    "edit_file: old_string matches 2 times in {target} (counted with \
                     punctuation/whitespace tolerance); add surrounding context to disambiguate or \
                     set replace_all"
                ),
                true
            )
        );
        let _ = std::fs::remove_file(path);
    }

    /// A near miss shows the file's own line next to the model's, and the
    /// first character where they part — the two things a retry needs.
    #[tokio::test]
    async fn a_near_miss_lists_the_differing_line() {
        let body: String = (1..=12)
            .map(|n| format!("let value_{n} = compute({n});\n"))
            .collect();
        let path = temp_file("edit-near-miss", &body);
        let target = path.to_str().unwrap();
        let ctx = test_ctx(0, "edit-near-miss");
        observe_whole(&path, &ctx).await;

        let (out, is_error) = run_tool(
            "edit_file",
            json!({
                "path": target,
                "old_string": "let value_4 = compute(4);\n\nlet value_5 = compute(5)\nlet value_6 = compute(6);",
                "new_string": "x",
            }),
            &ctx,
        )
        .await;
        assert_eq!(
            (out, is_error),
            (
                format!(
                    "edit_file: old_string not found in {target}. The closest match starts at line 4 \
                     (97% similar):\n  \
                     file line 5: \"let value_5 = compute(5);\"\n  \
                     your line 3: \"let value_5 = compute(5)\"\n    \
                     first difference at column 25: yours has nothing (the line ends there), the file \
                     has ';' (U+003B)\n  \
                     line count differs: your old_string has 1 extra line(s), all blank\n\
                     Copy the file's lines exactly into old_string and retry."
                ),
                true
            )
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        let _ = std::fs::remove_file(path);
    }

    /// Past a few lines a listing would only invite retyping the block in
    /// between from memory; the advice becomes the one read that shows it.
    #[tokio::test]
    async fn a_far_miss_names_the_read_instead_of_listing() {
        let body: String = (1..=12)
            .map(|n| format!("let value_{n} = compute({n});\n"))
            .collect();
        let path = temp_file("edit-far-miss", &body);
        let target = path.to_str().unwrap();
        let ctx = test_ctx(0, "edit-far-miss");
        observe_whole(&path, &ctx).await;

        let old: String = (3..=8)
            .map(|n| format!("let value_{n} = compute({n}0);\n"))
            .collect();
        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": target, "old_string": old, "new_string": "x"}),
            &ctx,
        )
        .await;
        assert_eq!(
            (out, is_error),
            (
                format!(
                    "edit_file: old_string not found in {target}. The closest match starts at line 3 \
                     (96% similar), but differs by more than a few lines (6 changed line(s)); call \
                     read_file with offset=3, limit=6 and rebuild old_string from what it returns — \
                     do not retype the block from memory"
                ),
                true
            )
        );
        let _ = std::fs::remove_file(path);
    }

    /// One bad anchor must not poison the rest of the turn. The refusal left the
    /// file exactly as the read found it, so the qualification is still a true
    /// statement — and dropping it reported one root cause as two, the second
    /// edit being refused for never having read a file the model did read.
    #[tokio::test]
    async fn a_refused_edit_leaves_the_turn_still_qualified() {
        let path = temp_file("edit-batch", "alpha\nbeta\n");
        let target = path.to_str().unwrap();
        let key = std::fs::canonicalize(&path).unwrap();
        let ctx = test_ctx(0, "edit-batch");
        observe_whole(&path, &ctx).await;

        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": target, "old_string": "gamma", "new_string": "x"}),
            &ctx,
        )
        .await;
        assert_eq!(
            (out, is_error),
            (
                format!(
                    "edit_file: old_string not found in {target}; nothing in the file is close enough to show. The text may have changed, or \
                     differ by more than punctuation: re-read the small range you meant to edit, \
                     rebuild old_string from that fresh text, and do not retry this old_string \
                     unchanged"
                ),
                true
            )
        );
        assert!(ctx.cfg.file_state.observation(&key).is_some());

        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": target, "old_string": "beta", "new_string": "BETA"}),
            &ctx,
        )
        .await;
        assert_eq!(
            (out, is_error),
            (format!("edited {target} (1 replacement(s))"), false)
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn injected_atomic_failure_preserves_original_and_cleans_temp() {
        let dir = std::env::temp_dir().join(format!("kloop-atomic-fault-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("target.txt");
        std::fs::write(&path, b"original").unwrap();
        let expected = full_observation(&path, b"original");

        let prepared =
            super::prepare_mutation(&dir, &path, "write_file", path.to_str().unwrap()).unwrap();
        let error = match super::commit_mutation(
            commit_target(&prepared, path.to_str().unwrap(), "write_file"),
            Some(&expected),
            super::Mutation::Write {
                bytes: b"replacement".to_vec(),
            },
            super::CommitFault::BeforeRename,
        ) {
            Ok(_) => panic!("fault injection unexpectedly committed"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("injected failure"), "{error:#}");
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("target.txt")]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn recursive_parent_cleanup_is_conservative_on_unix() {
        let root =
            std::env::temp_dir().join(format!("kloop-recursive-cleanup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let root_handle =
            super::open_parent_directory(&root, "write_file", "nested/file.txt").unwrap();

        let missing = vec![std::ffi::OsString::from("a"), std::ffi::OsString::from("b")];
        let (parent, created) = super::materialize_parent(
            root_handle.try_clone().unwrap(),
            &missing,
            "write_file",
            "nested/file.txt",
        )
        .unwrap();
        let error = super::commit_mutation(
            super::CommitTarget {
                parent: &parent,
                parent_path: &root.join("a/b"),
                leaf: std::ffi::OsStr::new("file.txt"),
                display_path: "nested/file.txt",
                tool: "write_file",
            },
            None,
            super::Mutation::Write {
                bytes: b"replacement".to_vec(),
            },
            super::CommitFault::BeforeRename,
        )
        .unwrap_err();
        assert!(error.to_string().contains("injected failure"), "{error:#}");
        super::cleanup_created_directories(created);
        assert!(root.join("a/b").is_dir());

        let missing = vec![std::ffi::OsString::from("nonempty")];
        let (_parent, created) = super::materialize_parent(
            root_handle.try_clone().unwrap(),
            &missing,
            "write_file",
            "x",
        )
        .unwrap();
        std::fs::write(root.join("nonempty/blocker"), "keep").unwrap();
        super::cleanup_created_directories(created);
        assert!(root.join("nonempty/blocker").exists());

        let missing = vec![std::ffi::OsString::from("identity")];
        let (_parent, created) =
            super::materialize_parent(root_handle, &missing, "write_file", "x").unwrap();
        std::fs::rename(root.join("identity"), root.join("moved")).unwrap();
        std::fs::create_dir(root.join("identity")).unwrap();
        super::cleanup_created_directories(created);
        assert!(root.join("identity").is_dir());
        assert!(root.join("moved").is_dir());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn replaced_temporary_name_never_reaches_target() {
        let dir =
            std::env::temp_dir().join(format!("kloop-atomic-temp-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("target.txt");
        std::fs::write(&path, b"original").unwrap();
        let expected = full_observation(&path, b"original");
        let prepared =
            super::prepare_mutation(&dir, &path, "write_file", path.to_str().unwrap()).unwrap();

        let error = match super::commit_mutation(
            commit_target(&prepared, path.to_str().unwrap(), "write_file"),
            Some(&expected),
            super::Mutation::Write {
                bytes: b"replacement".to_vec(),
            },
            super::CommitFault::ReplaceTempName,
        ) {
            Ok(_) => panic!("temporary replacement unexpectedly committed"),
            Err(error) => error,
        };

        assert!(
            error.to_string().contains("temporary file changed"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("target.txt")]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn one_read_cannot_authorize_two_commits() {
        let dir = std::env::temp_dir().join(format!("kloop-double-commit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("target.txt");
        std::fs::write(&path, b"original").unwrap();
        let expected = full_observation(&path, b"original");

        let prepared =
            super::prepare_mutation(&dir, &path, "write_file", path.to_str().unwrap()).unwrap();
        super::commit_mutation(
            commit_target(&prepared, path.to_str().unwrap(), "write_file"),
            Some(&expected),
            super::Mutation::Write {
                bytes: b"first".to_vec(),
            },
            super::CommitFault::None,
        )
        .unwrap();
        let error = match super::commit_mutation(
            commit_target(&prepared, path.to_str().unwrap(), "write_file"),
            Some(&expected),
            super::Mutation::Write {
                bytes: b"second".to_vec(),
            },
            super::CommitFault::None,
        ) {
            Ok(_) => panic!("stale second commit unexpectedly succeeded"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("changed since it was read"));
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_write_preserves_existing_permissions_and_rejects_symlinks() {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("kloop-atomic-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("target.txt");
        let link = dir.join("link.txt");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let ctx = test_ctx(0, "write-mode");

        observe_whole(&path, &ctx).await;
        let (out, is_error) = run_tool(
            "write_file",
            json!({"path": path.to_str().unwrap(), "content": "new"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o640);

        let fresh = dir.join("fresh.txt");
        let umask = std::process::Command::new("sh")
            .args(["-c", "umask"])
            .output()
            .expect("shell reports inherited umask");
        assert!(umask.status.success());
        let umask =
            u32::from_str_radix(std::str::from_utf8(&umask.stdout).unwrap().trim(), 8).unwrap();
        let (out, is_error) = run_tool(
            "write_file",
            json!({"path": fresh.to_str().unwrap(), "content": "fresh"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert_eq!(
            std::fs::metadata(&fresh).unwrap().mode() & 0o777,
            0o666 & !umask
        );

        observe_whole(&link, &ctx).await;
        let (out, is_error) = run_tool(
            "write_file",
            json!({"path": link.to_str().unwrap(), "content": "through-link"}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("symbolic link"), "{out}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mutation_paths_share_internal_aliases_and_reject_parent_symlink_escapes() {
        let root =
            std::env::temp_dir().join(format!("kloop-mutation-parent-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        let real = workspace.join("real");
        let outside = root.join("outside");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&real, workspace.join("inside-link")).unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("outside-link")).unwrap();

        let alias = super::prepare_mutation(
            &workspace,
            &workspace.join("inside-link/new.txt"),
            "write_file",
            "inside-link/new.txt",
        )
        .unwrap();
        let direct = super::prepare_mutation(
            &workspace,
            &workspace.join("real/new.txt"),
            "write_file",
            "real/new.txt",
        )
        .unwrap();
        assert_eq!(alias.path, direct.path);

        let mut ctx = test_ctx(0, "write-parent-link");
        let mut cfg = ctx.cfg.test_clone();
        cfg.cwd = workspace.clone();
        ctx.cfg = std::sync::Arc::new(cfg);

        let frozen = json!({"path": "inside-link/frozen.txt", "content": "frozen"});
        let effective = ctx.cfg.effective_workspace();
        let prepared = super::prepare_mutation_input("write_file", &frozen, &effective)
            .await
            .unwrap();
        std::fs::remove_file(workspace.join("inside-link")).unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("inside-link")).unwrap();
        let output = super::write_file_tool(&frozen, &prepared, &ctx, &effective)
            .await
            .unwrap();
        drop(output.path_lock);
        assert_eq!(
            std::fs::read_to_string(real.join("frozen.txt")).unwrap(),
            "frozen"
        );
        assert!(!outside.join("frozen.txt").exists());

        let (out, is_error) = run_tool(
            "write_file",
            json!({"path": "outside-link/new.txt", "content": "escaped"}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("symbolic-link ancestor"), "{out}");
        assert!(!outside.join("new.txt").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dispatch_permission_rules_keep_original_parent_alias() {
        let root =
            std::env::temp_dir().join(format!("kloop-mutation-alias-rules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, root.join("inside-link")).unwrap();

        let deny = crate::permissions::PermissionRules {
            deny: vec!["write_file(inside-link/**)".into()],
            ..Default::default()
        };
        let permissions = crate::permissions::Permissions::new(
            crate::permissions::Mode::Bypass,
            &deny,
            root.clone(),
            None,
        )
        .unwrap();
        let mut deny_ctx = test_ctx(0, "write-alias-deny");
        let mut cfg = deny_ctx.cfg.test_clone();
        cfg.cwd = root.clone();
        cfg.permissions = Arc::new(permissions);
        deny_ctx.cfg = Arc::new(cfg);
        let (out, is_error) = run_tool(
            "write_file",
            json!({"path": "inside-link/denied.txt", "content": "denied"}),
            &deny_ctx,
        )
        .await;
        assert!(is_error, "{out}");
        assert!(out.contains("blocked by a deny"), "{out}");
        assert!(!real.join("denied.txt").exists());

        let (tx, mut approvals) = tokio::sync::mpsc::unbounded_channel();
        let ask = crate::permissions::PermissionRules {
            ask: vec!["write_file(inside-link/**)".into()],
            ..Default::default()
        };
        let permissions = crate::permissions::Permissions::new(
            crate::permissions::Mode::Bypass,
            &ask,
            root.clone(),
            Some(Arc::new(ChannelApprover { tx })),
        )
        .unwrap();
        let mut ask_ctx = test_ctx(0, "write-alias-ask");
        let mut cfg = ask_ctx.cfg.test_clone();
        cfg.cwd = root.clone();
        cfg.permissions = Arc::new(permissions);
        ask_ctx.cfg = Arc::new(cfg);
        let call_ctx = ask_ctx.clone();
        let task = tokio::spawn(async move {
            run_tool(
                "write_file",
                json!({"path": "inside-link/asked.txt", "content": "asked"}),
                &call_ctx,
            )
            .await
        });
        let (_request, reply) =
            tokio::time::timeout(std::time::Duration::from_secs(1), approvals.recv())
                .await
                .expect("alias ask rule must reach the approver")
                .expect("approval channel remains open");
        assert!(!real.join("asked.txt").exists());
        reply
            .send(crate::permissions::Decision::Deny)
            .expect("approval receiver still waiting");
        let (out, is_error) = task.await.unwrap();
        assert!(is_error, "{out}");
        assert!(out.contains("declined"), "{out}");
        assert!(!real.join("asked.txt").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn approval_wait_cannot_retarget_parent_into_git_hooks() {
        let root = std::env::temp_dir().join(format!(
            "kloop-mutation-approval-swap-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        let real = workspace.join("real");
        let parent = real.join("newdir");
        let moved_parent = real.join("newdir-before-swap");
        let hooks = workspace.join(".git/hooks");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::create_dir_all(&hooks).unwrap();
        std::os::unix::fs::symlink(&real, workspace.join("inside-link")).unwrap();

        let (ctx, mut approvals) = approval_ctx("write-approval-swap", &workspace);
        let call_ctx = ctx.clone();
        let task = tokio::spawn(async move {
            run_tool(
                "write_file",
                json!({
                    "path": "inside-link/newdir/pre-commit",
                    "content": "malicious hook"
                }),
                &call_ctx,
            )
            .await
        });

        let (_request, reply) =
            tokio::time::timeout(std::time::Duration::from_secs(1), approvals.recv())
                .await
                .expect("write must reach the approval wait")
                .expect("approval channel remains open");
        std::fs::rename(&parent, &moved_parent).unwrap();
        std::os::unix::fs::symlink(&hooks, &parent).unwrap();
        reply
            .send(crate::permissions::Decision::Allow(
                crate::permissions::ApprovalScope::Once,
            ))
            .expect("approval receiver still waiting");

        let (out, is_error) = task.await.unwrap();
        assert!(is_error, "{out}");
        assert!(out.contains("approval was pending"), "{out}");
        assert!(!hooks.join("pre-commit").exists());
        assert!(!moved_parent.join("pre-commit").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fifo_leaf_created_during_approval_fails_without_blocking() {
        let root =
            std::env::temp_dir().join(format!("kloop-mutation-fifo-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("target.txt");
        let (ctx, mut approvals) = approval_ctx("write-fifo-swap", &root);
        let call_ctx = ctx.clone();
        let task = tokio::spawn(async move {
            run_tool(
                "write_file",
                json!({"path": "target.txt", "content": "replacement"}),
                &call_ctx,
            )
            .await
        });
        let (_request, reply) =
            tokio::time::timeout(std::time::Duration::from_secs(1), approvals.recv())
                .await
                .expect("write must reach the approval wait")
                .expect("approval channel remains open");
        let status = std::process::Command::new("mkfifo")
            .arg(&target)
            .status()
            .expect("mkfifo is available on Unix");
        assert!(status.success());
        reply
            .send(crate::permissions::Decision::Allow(
                crate::permissions::ApprovalScope::Once,
            ))
            .expect("approval receiver still waiting");

        let (out, is_error) = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("FIFO replacement must not block openat")
            .unwrap();
        assert!(is_error, "{out}");
        assert!(out.contains("not a regular file"), "{out}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_post_hook_leaves_committed_write_unqualified() {
        use crate::hooks::HookDef;
        use crate::hooks::HookEvent;
        use crate::hooks::Hooks;
        use std::sync::Arc;

        let path = temp_file("write-cancel", "old\n");
        let key = std::fs::canonicalize(&path).unwrap();
        let marker = temp_file("write-cancel-marker", "");
        let _ = std::fs::remove_file(&marker);
        let mut ctx = test_ctx(0, "write-cancel");
        observe_whole(&path, &ctx).await;
        let mut cfg = ctx.cfg.test_clone();
        cfg.hooks = Arc::new(Hooks {
            defs: vec![HookDef {
                event: HookEvent::PostTool,
                command: vec![
                    "sh".into(),
                    "-c".into(),
                    format!("touch '{}'; sleep 10", marker.display()),
                ],
                matcher: Some("write_file".into()),
                timeout_ms: 20_000,
            }],
        });
        ctx.cfg = Arc::new(cfg);

        let call_ctx = ctx.clone();
        let input_path = path.to_string_lossy().to_string();
        let task = tokio::spawn(async move {
            dispatch_tools(
                vec![(
                    "write-cancel".into(),
                    "write_file".into(),
                    json!({"path": input_path, "content": "committed\n"}),
                )],
                &call_ctx,
            )
            .await
        });
        for _ in 0..100 {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(marker.exists(), "post hook reached after atomic commit");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "committed\n");
        ctx.cancel.cancel();
        assert_eq!(
            task.await.unwrap(),
            vec![ContentBlock::ToolResult {
                tool_use_id: "write-cancel".into(),
                content: "interrupted".into(),
                is_error: true,
            }]
        );
        assert!(ctx.cfg.file_state.observation(&key).is_none());
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(marker);
    }

    /// The refusal names the line the replacement lands on and one read that
    /// clears it: a window around the match, which is advice worth following
    /// only because `edit_file` now qualifies on any read (plan 155). Under the
    /// complete-read rule this same window was refused all over again, which is
    /// why plan 156 had to point at the first unread line instead.
    #[tokio::test]
    async fn unread_edit_names_the_line_and_the_read_that_clears_it() {
        let body: String = (1..=40).map(|n| format!("line {n}\n")).collect();
        let path = temp_file("edit-unread-hint", &body);
        let target = path.to_str().unwrap();
        let ctx = test_ctx(0, "edit-unread-hint");

        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": target, "old_string": "line 33", "new_string": "line 33!"}),
            &ctx,
        )
        .await;
        assert_eq!(
            (out, is_error),
            (
                format!(
                    "edit_file: must read {target} before modifying the existing file \
                     (old_string is at line 33 of 41; call read_file with offset=13, limit=60)"
                ),
                true
            )
        );

        // Absent old_string is a different failure: the refusal says only what
        // it always said, and never turns into "not found" ahead of its turn.
        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": target, "old_string": "nowhere", "new_string": "x"}),
            &ctx,
        )
        .await;
        assert_eq!(
            (out, is_error),
            (
                format!("edit_file: must read {target} before modifying the existing file"),
                true
            )
        );

        // The file is untouched by both refusals.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);

        // And the advice works in exactly the one read it asks for. Without
        // this the hint would be a suggestion that leads back to the refusal.
        let (_, is_error) = run_tool(
            "read_file",
            json!({"path": target, "offset": 13, "limit": 60}),
            &ctx,
        )
        .await;
        assert!(!is_error);
        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": target, "old_string": "line 33", "new_string": "line 33!"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            body.replace("line 33\n", "line 33!\n")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Plan 155 overturns plan 49's complete-read rule for `edit_file` alone:
    /// one narrow read qualifies the path, including for an `old_string` the
    /// read never showed, and including every match of a `replace_all` that
    /// straddles what was read. Uniqueness against the bytes on disk is what
    /// keeps that honest, so the entrance fee for a file too large to read in
    /// one call is one read, not one read per `READ_CONTENT_CHARS` of it.
    #[tokio::test]
    async fn a_narrow_read_qualifies_an_edit_anywhere_in_the_file() {
        let body: String = (1..=40).map(|n| format!("line {n}\n")).collect();
        let path = temp_file("edit-narrow-read", &body);
        let target = path.to_str().unwrap();
        let key = std::fs::canonicalize(&path).unwrap();
        let ctx = test_ctx(0, "edit-narrow-read");

        let (_, is_error) = run_tool("read_file", json!({"path": target, "limit": 10}), &ctx).await;
        assert!(!is_error);
        assert!(!ctx.cfg.file_state.observation(&key).unwrap().is_complete());

        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": target, "old_string": "line 33", "new_string": "line 33!"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            body.replace("line 33\n", "line 33!\n")
        );
        // The success refreshes the observation to a complete one, so the
        // entrance fee is paid once per file per session, not once per edit.
        assert!(ctx.cfg.file_state.observation(&key).unwrap().is_complete());
        let _ = std::fs::remove_file(&path);
    }

    /// `replace_all` asks no more of coverage than a single edit does: the
    /// matches on either side of what was read all go through. Plan 155 planned
    /// the opposite (all matches covered, or fall back to a complete read), and
    /// that rule died with the coverage check it was guarding.
    #[tokio::test]
    async fn replace_all_spans_read_and_unread_lines() {
        let body: String = (1..=40)
            .map(|n| {
                if n == 5 || n == 35 {
                    "token\n".to_string()
                } else {
                    format!("line {n}\n")
                }
            })
            .collect();
        let path = temp_file("edit-replace-all-span", &body);
        let target = path.to_str().unwrap();
        let ctx = test_ctx(0, "edit-replace-all-span");

        let (_, is_error) = run_tool("read_file", json!({"path": target, "limit": 10}), &ctx).await;
        assert!(!is_error);

        let (out, is_error) = run_tool(
            "edit_file",
            json!({
                "path": target,
                "old_string": "token",
                "new_string": "TOKEN",
                "replace_all": true
            }),
            &ctx,
        )
        .await;
        assert_eq!(
            (out, is_error),
            (format!("edited {target} (2 replacement(s))"), false)
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            body.replace("token\n", "TOKEN\n")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Plan 155 relaxed *eligibility* and left *freshness* where it was, on the
    /// reasoning that the two were separable. Plan 195 finished the thought: a
    /// narrow read qualifies a path it has barely seen, so a second narrow read
    /// re-qualifies bytes it has barely seen too — the rule could be satisfied
    /// without doing the thing it asked for. Here the read covered ten lines, the
    /// change landed above all of them, and the edit is on line 33: none of the
    /// three overlap, and the anchor is still what decides.
    #[tokio::test]
    async fn a_narrow_read_survives_an_external_change() {
        let body: String = (1..=40).map(|n| format!("line {n}\n")).collect();
        let path = temp_file("edit-narrow-stale", &body);
        let target = path.to_str().unwrap();
        let key = std::fs::canonicalize(&path).unwrap();
        let ctx = test_ctx(0, "edit-narrow-stale");

        let (_, is_error) = run_tool("read_file", json!({"path": target, "limit": 10}), &ctx).await;
        assert!(!is_error);
        let changed = format!("external\n{body}");
        std::fs::write(&path, &changed).unwrap();

        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": target, "old_string": "line 33", "new_string": "line 33!"}),
            &ctx,
        )
        .await;
        assert_eq!(
            (out, is_error),
            (
                format!("edited {target} (1 replacement(s)); the file changed since you read it"),
                false
            )
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            changed.replace("line 33\n", "line 33!\n")
        );
        assert!(ctx.cfg.file_state.observation(&key).unwrap().is_complete());
        let _ = std::fs::remove_file(&path);
    }

    /// Two edits on one narrow read — the shape of a parallel batch, where the
    /// second carries the qualification captured before the first committed.
    /// Both land now, and that is the point: each one's anchor was tested against
    /// the bytes that were actually on disk when it ran, which is the guarantee
    /// the version check was standing in for. What still stops a second commit
    /// from clobbering the first is the compare-and-swap inside the window
    /// between kloop's read and kloop's rename — see
    /// [`a_change_inside_the_commit_window_still_loses`].
    #[test]
    fn one_partial_read_authorizes_two_edits_that_both_still_anchor() {
        let dir = std::env::temp_dir().join(format!("kloop-double-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("target.txt");
        std::fs::write(&path, b"alpha\nbeta\n").unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let expected =
            super::FileObservation::from_read(b"alpha\nbeta\n", &metadata, 3, 0..1, true);
        assert!(!expected.is_complete());

        let prepared =
            super::prepare_mutation(&dir, &path, "edit_file", path.to_str().unwrap()).unwrap();
        let edit = |old: &str, new: &str| super::Mutation::Edit {
            old: old.to_string(),
            new: new.to_string(),
            replace_all: false,
        };
        super::commit_mutation(
            commit_target(&prepared, path.to_str().unwrap(), "write_file"),
            Some(&expected),
            edit("beta", "BETA"),
            super::CommitFault::None,
        )
        .unwrap();
        let outcome = super::commit_mutation(
            commit_target(&prepared, path.to_str().unwrap(), "edit_file"),
            Some(&expected),
            edit("alpha", "ALPHA"),
            super::CommitFault::None,
        )
        .unwrap();

        assert!(
            outcome
                .content
                .ends_with("; the file changed since you read it"),
            "{}",
            outcome.content
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"ALPHA\nBETA\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Plan 195 left this compare-and-swap as `edit_file`'s only freshness guard,
    /// and until now nothing exercised its version arm — no ordinary test can
    /// change a file inside the window between kloop reading the bytes and kloop
    /// renaming the replacement in. The injected change is what that window looks
    /// like from the outside, and it must still lose.
    #[test]
    fn a_change_inside_the_commit_window_still_loses() {
        let dir = std::env::temp_dir().join(format!("kloop-commit-window-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("target.txt");
        std::fs::write(&path, b"alpha\nbeta\n").unwrap();
        let expected = full_observation(&path, b"alpha\nbeta\n");
        let prepared =
            super::prepare_mutation(&dir, &path, "edit_file", path.to_str().unwrap()).unwrap();

        let error = super::commit_mutation(
            commit_target(&prepared, path.to_str().unwrap(), "edit_file"),
            Some(&expected),
            super::Mutation::Edit {
                old: "beta".to_string(),
                new: "BETA".to_string(),
                replace_all: false,
            },
            super::CommitFault::ChangeTargetBeforeRename,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("changed immediately before commit"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), super::FAULT_TARGET_BYTES);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn edit_file_replaces_errors_and_replace_all() {
        let path = temp_file("edit", "one two two three");
        let p = path.to_str().unwrap();
        let ctx = test_ctx(0, "edit");
        let (_, is_error) = run_tool("read_file", json!({"path": p}), &ctx).await;
        assert!(!is_error);

        // Ambiguous match without replace_all is an error and changes nothing.
        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": p, "old_string": "two", "new_string": "2"}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("2 times"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one two two three");

        // Missing old_string is an error. A failed mutation clears the prior
        // qualification, so the model must establish a fresh full Read.
        let (_, is_error) = run_tool("read_file", json!({"path": p}), &ctx).await;
        assert!(!is_error);
        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": p, "old_string": "zzz", "new_string": "2"}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("not found"));

        // replace_all rewrites every occurrence.
        let (_, is_error) = run_tool("read_file", json!({"path": p}), &ctx).await;
        assert!(!is_error);
        let (_, is_error) = run_tool(
            "edit_file",
            json!({"path": p, "old_string": "two", "new_string": "2", "replace_all": true}),
            &ctx,
        )
        .await;
        assert!(!is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one 2 2 three");

        // Unique match replaces exactly once.
        let (_, is_error) = run_tool(
            "edit_file",
            json!({"path": p, "old_string": "one", "new_string": "1"}),
            &ctx,
        )
        .await;
        assert!(!is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1 2 2 three");

        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": p, "old_string": "1", "new_string": "1"}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("must be different"), "{out}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1 2 2 three");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn read_lf_view_drives_crlf_edit_and_preview_matches_committed_bytes() {
        let dir = std::env::temp_dir().join(format!("kloop-crlf-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("target.txt");
        std::fs::write(&path, b"prefix\r\none\r\ntwo\r\nsuffix\r\n").unwrap();
        let (ctx, mut approvals) = approval_ctx("crlf-edit", &dir);

        let (read, is_error) =
            run_tool("read_file", json!({"path": path.to_str().unwrap()}), &ctx).await;
        assert!(!is_error, "{read}");
        assert!(read.contains("2\tone\n3\ttwo"), "{read:?}");
        assert!(!read.contains('\r'), "{read:?}");

        let call_ctx = ctx.clone();
        let call_path = path.to_string_lossy().to_string();
        let task = tokio::spawn(async move {
            run_tool(
                "edit_file",
                json!({
                    "path": call_path,
                    "old_string": "one\ntwo",
                    "new_string": "ONE\nTWO"
                }),
                &call_ctx,
            )
            .await
        });
        let (request, reply) =
            tokio::time::timeout(std::time::Duration::from_secs(1), approvals.recv())
                .await
                .expect("edit must reach approval")
                .expect("approval channel remains open");
        let preview = request
            .preview
            .expect("edit approval carries preview")
            .text()
            .to_string();
        assert!(preview.contains("-2  one"), "{preview}");
        assert!(preview.contains("+2  ONE"), "{preview}");
        reply
            .send(crate::permissions::Decision::Allow(
                crate::permissions::ApprovalScope::Once,
            ))
            .expect("approval receiver still waiting");

        let (out, is_error) = task.await.unwrap();
        assert!(!is_error, "{out}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"prefix\r\nONE\r\nTWO\r\nsuffix\r\n"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn approval_wait_pins_one_workspace_generation_through_commit() {
        let repository = temp_git_repo("workspace-snapshot-race");
        let target = repository.join("base-only.txt");
        let (ctx, mut approvals) = approval_ctx("workspace-snapshot-race", &repository);
        let base_state = Arc::clone(&ctx.cfg.file_state);
        let call_ctx = ctx.clone();
        let task = tokio::spawn(async move {
            run_tool(
                "write_file",
                json!({"path": "base-only.txt", "content": "base generation"}),
                &call_ctx,
            )
            .await
        });
        let (_request, reply) =
            tokio::time::timeout(std::time::Duration::from_secs(1), approvals.recv())
                .await
                .expect("write must reach approval")
                .expect("approval channel remains open");

        crate::worktree::enter(&ctx.cfg, "snapshot-race")
            .await
            .unwrap();
        let active = ctx.cfg.effective_workspace();
        assert_ne!(active.cwd, repository);
        assert!(!Arc::ptr_eq(&base_state, &active.file_state));
        reply
            .send(crate::permissions::Decision::Allow(
                crate::permissions::ApprovalScope::Once,
            ))
            .expect("approval receiver still waiting");

        let (out, is_error) = task.await.unwrap();
        assert!(!is_error, "{out}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "base generation");
        assert!(!active.cwd.join("base-only.txt").exists());
        assert!(base_state.observation(&target).is_some());
        assert!(active.file_state.observation(&target).is_none());

        crate::worktree::exit(&ctx.cfg, crate::worktree::ExitAction::Remove, false)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(repository);
    }
}
