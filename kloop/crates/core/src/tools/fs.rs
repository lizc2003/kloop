use std::ffi::OsStr;
use std::ffi::OsString;
use std::io::Read as _;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use kloop_protocol::ToolResultContent;
use serde_json::Value;

use super::notebook;
use super::resolve_path;
use super::str_arg;
use super::ToolCtx;
use crate::file_state::normalize_absolute_path;
use crate::file_state::FileObservation;
use crate::file_state::FileStateUpdate;
use crate::file_state::FileVersion;
use crate::image::detect_media_type;
use crate::image::image_block_from_bytes;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const READ_CONTENT_CHARS: usize = 7_000;

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

struct CommitOutcome {
    content: String,
    bytes: Vec<u8>,
    metadata: std::fs::Metadata,
}

struct TargetSnapshot {
    bytes: Vec<u8>,
    metadata: std::fs::Metadata,
    version: FileVersion,
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
}

pub(super) async fn prepare_read(input: &Value, ctx: &ToolCtx) -> Result<PreparedRead> {
    let path = str_arg(input, "path", "read_file")?.to_string();
    let requested = resolve_path(&ctx.cfg.effective_cwd(), &path);
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
    let checked_metadata = std::fs::metadata(&checked_path)
        .with_context(|| format!("read_file: cannot inspect {display_path}"))?;
    let opened_metadata = file
        .metadata()
        .with_context(|| format!("read_file: cannot inspect opened {display_path}"))?;
    if checked_path != resolved
        || file_identity(&checked_metadata)? != file_identity(&opened_metadata)?
    {
        bail!("read_file: path changed while preparing {display_path}; retry the call");
    }
    if has_multiple_hard_links(&opened_metadata) {
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

#[cfg(unix)]
fn open_read_target(
    parent: &std::fs::File,
    _parent_path: &Path,
    leaf: &OsStr,
    display_path: &str,
) -> Result<std::fs::File> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    let fd = rustix::fs::openat(
        parent,
        leaf,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .with_context(|| format!("read_file: cannot read {display_path}"))?;
    let file = std::fs::File::from(fd);
    if !file
        .metadata()
        .with_context(|| format!("read_file: cannot inspect {display_path}"))?
        .is_file()
    {
        bail!("read_file: cannot read {display_path}: not a regular file");
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_read_target(
    _parent: &std::fs::File,
    parent_path: &Path,
    leaf: &OsStr,
    display_path: &str,
) -> Result<std::fs::File> {
    let file = std::fs::File::open(parent_path.join(leaf))
        .with_context(|| format!("read_file: cannot read {display_path}"))?;
    if !file
        .metadata()
        .with_context(|| format!("read_file: cannot inspect {display_path}"))?
        .is_file()
    {
        bail!("read_file: cannot read {display_path}: not a regular file");
    }
    Ok(file)
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
    let display_path = path.to_string();
    let (before, bytes, after) = tokio::task::spawn_blocking(move || -> Result<_> {
        let before = file
            .metadata()
            .with_context(|| format!("read_file: cannot inspect {display_path}"))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .with_context(|| format!("read_file: cannot read {display_path}"))?;
        let after = file
            .metadata()
            .with_context(|| format!("read_file: cannot inspect {display_path} after reading"))?;
        Ok((before, bytes, after))
    })
    .await
    .with_context(|| format!("read_file: read worker failed for {path}"))??;

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
            FileObservation::full_notebook(&bytes, &before)
        } else {
            FileObservation::full(&bytes, &before)
        };
        let state_update = if observation.version().metadata_matches(&after) {
            FileStateUpdate::Observe {
                path: key,
                observation,
            }
        } else {
            FileStateUpdate::Clear { path: key }
        };
        return Ok(ReadFileOutput {
            content: output.content,
            state_update,
        });
    }

    // An image file returns a single image block (validated for format and the
    // 5 MiB cap); offset/limit are line concepts and simply do not apply.
    if detect_media_type(&bytes).is_some() {
        let block = image_block_from_bytes(&bytes)
            .with_context(|| format!("read_file: cannot read image {path}"))?;
        let observation = FileObservation::full(&bytes, &before);
        let state_update = if observation.version().metadata_matches(&after) {
            FileStateUpdate::Observe {
                path: key,
                observation,
            }
        } else {
            FileStateUpdate::Clear { path: key }
        };
        return Ok(ReadFileOutput {
            content: ToolResultContent::Blocks(vec![block]),
            state_update,
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
    let lines: Vec<&str> = content.split('\n').collect();
    let total_lines = lines.len();
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
        numbered_text_page(&lines, start, requested_end, offset)
    };
    let observation = FileObservation::from_read(
        &bytes,
        &before,
        total_lines as u64,
        start as u64..observed_end as u64,
        start == 0,
    );
    let state_update = if observation.version().metadata_matches(&after) {
        FileStateUpdate::Observe {
            path: key,
            observation,
        }
    } else {
        FileStateUpdate::Clear { path: key }
    };
    Ok(ReadFileOutput {
        content: ToolResultContent::Text(text),
        state_update,
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
    lines: &[&str],
    start: usize,
    requested_end: usize,
    display_start: usize,
) -> (String, usize) {
    let mut out = String::new();
    let mut chars = 0usize;
    let mut observed_end = start;
    let mut partial_line = None;
    for (relative, line) in lines[start..requested_end].iter().enumerate() {
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
    if observed_end < requested_end {
        if let Some(line) = partial_line {
            out.push_str(&format!(
                "\n\n[read output truncated within line {line}; use grep or a narrower reader to inspect it]"
            ));
        } else {
            out.push_str(&format!(
                "\n\n[read output truncated; call read_file with offset={} to continue]",
                observed_end + 1
            ));
        }
    }
    (out, observed_end)
}

pub(super) async fn prepare_mutation_input(
    tool: &str,
    input: &Value,
    ctx: &ToolCtx,
) -> Result<PreparedMutation> {
    prepare_mutation_input_with_key(tool, input, "path", ctx).await
}

pub(super) async fn prepare_notebook_mutation_input(
    input: &Value,
    ctx: &ToolCtx,
) -> Result<PreparedMutation> {
    let path = str_arg(input, "notebook_path", "notebook_edit")?;
    if Path::new(path)
        .extension()
        .is_none_or(|extension| extension != "ipynb")
    {
        bail!("File must be a Jupyter notebook (.ipynb file). For editing other file types, use edit_file.");
    }
    prepare_mutation_input_with_key("notebook_edit", input, "notebook_path", ctx).await
}

async fn prepare_mutation_input_with_key(
    tool: &str,
    input: &Value,
    path_key: &str,
    ctx: &ToolCtx,
) -> Result<PreparedMutation> {
    let path = str_arg(input, path_key, tool)?.to_string();
    if tool == "notebook_edit" && !Path::new(&path).is_absolute() {
        bail!("notebook_edit: notebook_path must be an absolute path");
    }
    let cwd = ctx.cfg.effective_cwd();
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
    )
    .await
}

pub(super) async fn edit_file_tool(
    input: &Value,
    prepared: &PreparedMutation,
    ctx: &ToolCtx,
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
    )
    .await
}

pub(super) async fn notebook_edit_tool(
    input: &Value,
    prepared: &PreparedMutation,
    ctx: &ToolCtx,
) -> Result<FileMutationOutput> {
    let path = str_arg(input, "notebook_path", "notebook_edit")?;
    let request = notebook::request_from_input(input)?;
    mutate_file(
        "notebook_edit",
        path,
        prepared,
        Mutation::Notebook { request },
        ctx,
    )
    .await
}

async fn mutate_file(
    tool: &'static str,
    path: &str,
    prepared: &PreparedMutation,
    mutation: Mutation,
    ctx: &ToolCtx,
) -> Result<FileMutationOutput> {
    let key = prepared.path.clone();
    let state = ctx.cfg.effective_file_state();
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
    let parent_path = prepared.parent_path.clone();
    let leaf = prepared.leaf.clone();

    // From this point the executor may have created a temp file or committed a
    // rename. Clear eagerly; only run_one's final successful tool_result stages
    // the replacement observation back in. Cancellation or any error therefore
    // leaves a conservative empty entry rather than stale write authority.
    state.apply(FileStateUpdate::Clear { path: key.clone() });
    let path_for_error = path.to_string();
    let (outcome, path_lock) = tokio::task::spawn_blocking(move || {
        let target = CommitTarget {
            parent: &parent,
            parent_path: &parent_path,
            leaf: &leaf,
            display_path: &path_for_error,
            tool,
        };
        let outcome = commit_mutation(target, expected.as_ref(), mutation, CommitFault::None);
        (outcome, path_lock)
    })
    .await
    .with_context(|| format!("{tool}: write worker failed for {path}"))?;
    let outcome = outcome?;

    let observation = if notebook_mutation {
        FileObservation::full_notebook(&outcome.bytes, &outcome.metadata)
    } else {
        FileObservation::full(&outcome.bytes, &outcome.metadata)
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
    let leaf = lexical
        .file_name()
        .with_context(|| format!("{tool}: {display_path} has no file name"))?
        .to_os_string();

    // Only the leaf may be absent. Creating intermediate directories after an
    // approval would let another process replace one with a symlink and retarget
    // the already-approved write.
    let parent_path = std::fs::canonicalize(parent).with_context(|| {
        format!("{tool}: parent directory for {display_path} must already exist")
    })?;
    let canonical_cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("{tool}: cannot inspect working directory"))?;
    let lexical_cwd = normalize_absolute_path(cwd, cwd);
    if lexical.starts_with(&lexical_cwd) && !parent_path.starts_with(&canonical_cwd) {
        bail!(
            "{tool}: {display_path} escapes the working directory through a symbolic-link ancestor"
        );
    }

    let parent_file = open_parent_directory(&parent_path, tool, display_path)?;
    let parent_identity = file_identity(
        &parent_file
            .metadata()
            .with_context(|| format!("{tool}: cannot inspect parent of {display_path}"))?,
    )?;

    // Bind the canonical spelling used by permission to the directory object we
    // actually opened. Any rename/symlink swap during preparation fails closed.
    let checked_parent = std::fs::canonicalize(parent)
        .with_context(|| format!("{tool}: parent directory changed for {display_path}"))?;
    let checked_metadata = std::fs::metadata(&checked_parent)
        .with_context(|| format!("{tool}: cannot inspect parent of {display_path}"))?;
    if checked_parent != parent_path
        || file_identity(&checked_metadata)? != parent_identity
        || !checked_metadata.is_dir()
    {
        bail!("{tool}: parent directory changed while preparing {display_path}; retry the call");
    }

    let path = parent_path.join(&leaf);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("{tool}: refuses to replace symbolic link {display_path}");
        }
        Ok(metadata) if !metadata.is_file() => {
            bail!("{tool}: {display_path} is not a regular file");
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("{tool}: cannot inspect {display_path}"));
        }
    }

    Ok(PreparedMutation {
        path,
        parent_path,
        parent: parent_file,
        leaf,
        parent_identity,
    })
}

impl PreparedMutation {
    pub(super) fn resolved_path(&self) -> &Path {
        &self.path
    }

    fn revalidate(&self, tool: &str, display_path: &str) -> Result<()> {
        verify_parent_binding(&self.parent, &self.parent_path, tool, display_path)?;
        let retained = self
            .parent
            .metadata()
            .with_context(|| format!("{tool}: cannot inspect parent of {display_path}"))?;
        if file_identity(&retained)? != self.parent_identity {
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
    let current = std::fs::metadata(&current_path)
        .with_context(|| format!("{tool}: cannot inspect parent directory for {display_path}"))?;
    let retained = parent
        .metadata()
        .with_context(|| format!("{tool}: cannot inspect retained parent for {display_path}"))?;
    if current_path != parent_path
        || !current.is_dir()
        || file_identity(&current)? != file_identity(&retained)?
    {
        bail!(
            "{tool}: parent directory changed while approval was pending for {display_path}; retry the call"
        );
    }
    Ok(())
}

#[cfg(unix)]
fn open_parent_directory(path: &Path, tool: &str, display_path: &str) -> Result<std::fs::File> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .with_context(|| format!("{tool}: cannot open parent directory for {display_path}"))?;
    Ok(std::fs::File::from(fd))
}

#[cfg(not(unix))]
fn open_parent_directory(path: &Path, tool: &str, display_path: &str) -> Result<std::fs::File> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("{tool}: cannot open parent directory for {display_path}"))?;
    if !file
        .metadata()
        .with_context(|| format!("{tool}: cannot inspect parent directory for {display_path}"))?
        .is_dir()
    {
        bail!("{tool}: parent of {display_path} is not a directory");
    }
    Ok(file)
}

#[cfg(unix)]
fn has_multiple_hard_links(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    metadata.nlink() > 1
}

#[cfg(windows)]
fn has_multiple_hard_links(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    metadata.number_of_links().is_some_and(|links| links > 1)
}

#[cfg(not(any(unix, windows)))]
fn has_multiple_hard_links(_metadata: &std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn file_identity(metadata: &std::fs::Metadata) -> Result<FileIdentity> {
    use std::os::windows::fs::MetadataExt as _;

    Ok(FileIdentity {
        device: u64::from(metadata.volume_serial_number().unwrap_or_default()),
        inode: metadata.file_index().unwrap_or_default(),
    })
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_metadata: &std::fs::Metadata) -> Result<FileIdentity> {
    bail!("safe file mutation is unsupported on this platform")
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
    let current = read_regular_target(parent, parent_path, leaf, tool, display_path)?;
    let (bytes, permissions, target_version, content) = match mutation {
        Mutation::Write { bytes } => {
            let target_version = match current {
                Some(snapshot) => {
                    validate_observation(expected, &snapshot, tool, display_path, false)?;
                    let permissions = snapshot.metadata.permissions();
                    let version = snapshot.version;
                    (Some(permissions), Some(version))
                }
                None => {
                    if expected.is_some() {
                        bail!("{tool}: {display_path} changed since it was read; read it again before modifying it");
                    }
                    (None, None)
                }
            };
            let content = format!("wrote {} bytes to {display_path}", bytes.len());
            (bytes, target_version.0, target_version.1, content)
        }
        Mutation::Edit {
            old,
            new,
            replace_all,
        } => {
            let snapshot =
                current.with_context(|| format!("edit_file: cannot read {display_path}"))?;
            validate_observation(expected, &snapshot, tool, display_path, false)?;
            let current = String::from_utf8(snapshot.bytes).map_err(|_| {
                anyhow::anyhow!("edit_file: {display_path} is not valid UTF-8 text")
            })?;
            let count = current.matches(&old).count();
            if count == 0 {
                bail!("edit_file: old_string not found in {display_path}");
            }
            if count > 1 && !replace_all {
                bail!("edit_file: old_string matches {count} times in {display_path}; add surrounding context to disambiguate or set replace_all");
            }
            let updated = if replace_all {
                current.replace(&old, &new)
            } else {
                current.replacen(&old, &new, 1)
            };
            let replacements = if replace_all { count } else { 1 };
            let content = format!("edited {display_path} ({replacements} replacement(s))");
            (
                updated.into_bytes(),
                Some(snapshot.metadata.permissions()),
                Some(snapshot.version),
                content,
            )
        }
        Mutation::Notebook { request } => {
            let snapshot = current
                .context("Notebook file is unavailable; read it again before editing it.")?;
            validate_observation(expected, &snapshot, tool, display_path, true)?;
            let mutation = notebook::apply_edit(&snapshot.bytes, &request)?;
            (
                mutation.bytes,
                Some(snapshot.metadata.permissions()),
                Some(snapshot.version),
                mutation.content,
            )
        }
    };

    atomic_replace(target, &bytes, permissions, target_version.as_ref(), fault)?;
    let committed = read_regular_target(parent, parent_path, leaf, tool, display_path)?
        .with_context(|| format!("{tool}: committed file disappeared: {display_path}"))?;
    if committed.bytes != bytes {
        bail!("{tool}: {display_path} changed immediately after commit; read it again before modifying it");
    }
    Ok(CommitOutcome {
        content,
        bytes,
        metadata: committed.metadata,
    })
}

fn validate_observation(
    expected: Option<&FileObservation>,
    current: &TargetSnapshot,
    tool: &str,
    path: &str,
    require_notebook: bool,
) -> Result<()> {
    let Some(expected) = expected else {
        if require_notebook {
            bail!("File has not been read yet. Read it first before writing to it.");
        }
        bail!("{tool}: must read {path} before modifying the existing file");
    };
    if require_notebook && !expected.is_notebook() {
        bail!("File has not been read as a complete notebook. Read it first before writing to it.");
    }
    if !expected.is_complete() {
        bail!("{tool}: must read the entire file {path} before modifying it");
    }
    if !expected
        .version()
        .matches(&current.bytes, &current.metadata)
    {
        if require_notebook {
            bail!("File has been modified since read, either by the user or by a linter. Read it again before attempting to write it.");
        }
        bail!("{tool}: {path} changed since it was read; read it again before modifying it");
    }
    Ok(())
}

#[cfg(unix)]
fn read_regular_target(
    parent: &std::fs::File,
    _parent_path: &Path,
    leaf: &OsStr,
    tool: &str,
    display_path: &str,
) -> Result<Option<TargetSnapshot>> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    let fd = match rustix::fs::openat(
        parent,
        leaf,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP) => {
            bail!("{tool}: refuses to replace symbolic link {display_path}")
        }
        Err(error) => {
            return Err(std::io::Error::from_raw_os_error(error.raw_os_error()))
                .with_context(|| format!("{tool}: cannot inspect {display_path}"));
        }
    };
    let mut file = std::fs::File::from(fd);
    let before = file
        .metadata()
        .with_context(|| format!("{tool}: cannot inspect {display_path}"))?;
    if !before.is_file() {
        bail!("{tool}: {display_path} is not a regular file");
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("{tool}: cannot read existing file {display_path}"))?;
    let after = file
        .metadata()
        .with_context(|| format!("{tool}: cannot inspect {display_path} after reading"))?;
    let version = FileVersion::new(&bytes, &before);
    if !version.metadata_matches(&after) {
        bail!(
            "{tool}: {display_path} changed while it was being read; retry after reading it again"
        );
    }
    Ok(Some(TargetSnapshot {
        bytes,
        metadata: after,
        version,
    }))
}

#[cfg(not(unix))]
fn read_regular_target(
    _parent: &std::fs::File,
    parent_path: &Path,
    leaf: &OsStr,
    tool: &str,
    display_path: &str,
) -> Result<Option<TargetSnapshot>> {
    let path = parent_path.join(leaf);
    let before = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("{tool}: cannot inspect {display_path}"));
        }
    };
    if before.file_type().is_symlink() {
        bail!("{tool}: refuses to replace symbolic link {display_path}");
    }
    if !before.is_file() {
        bail!("{tool}: {display_path} is not a regular file");
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("{tool}: cannot read existing file {display_path}"))?;
    let after = std::fs::symlink_metadata(&path)
        .with_context(|| format!("{tool}: cannot inspect {display_path} after reading"))?;
    let version = FileVersion::new(&bytes, &before);
    if !version.metadata_matches(&after) {
        bail!(
            "{tool}: {display_path} changed while it was being read; retry after reading it again"
        );
    }
    Ok(Some(TargetSnapshot {
        bytes,
        metadata: after,
        version,
    }))
}

#[cfg(unix)]
fn atomic_replace(
    target: CommitTarget<'_>,
    bytes: &[u8],
    permissions: Option<std::fs::Permissions>,
    target_version: Option<&FileVersion>,
    fault: CommitFault,
) -> Result<()> {
    let CommitTarget {
        parent,
        parent_path,
        leaf,
        display_path,
        tool,
    } = target;
    use rustix::fs::AtFlags;
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    verify_parent_binding(parent, parent_path, tool, display_path)?;
    let mut last_collision = None;
    for _ in 0..100 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp = format!(".kloop-write-{}-{sequence}.tmp", std::process::id());
        let fd = match rustix::fs::openat(
            parent,
            temp.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH,
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::EXIST) => {
                last_collision = Some(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "temporary file name collision",
                ));
                continue;
            }
            Err(error) => {
                return Err(std::io::Error::from_raw_os_error(error.raw_os_error())).with_context(
                    || format!("{tool}: cannot create temporary file for {display_path}"),
                );
            }
        };
        let mut file = std::fs::File::from(fd);
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
                std::fs::remove_file(parent_path.join(&temp)).with_context(|| {
                    format!("{tool}: cannot inject temporary replacement for {display_path}")
                })?;
                std::fs::write(parent_path.join(&temp), b"attacker-controlled bytes")
                    .with_context(|| {
                        format!("{tool}: cannot inject temporary replacement for {display_path}")
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
                target_version,
            )?;
            verify_parent_binding(parent, parent_path, tool, display_path)?;
            verify_temp_binding(
                parent,
                parent_path,
                OsStr::new(&temp),
                &file,
                bytes,
                display_path,
                tool,
            )?;
            rustix::fs::renameat(parent, temp.as_str(), parent, leaf)
                .with_context(|| format!("{tool}: cannot atomically replace {display_path}"))?;
            sync_parent(parent, tool, display_path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(parent, temp.as_str(), AtFlags::empty());
        }
        return result;
    }
    Err(last_collision.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "temporary file name collision",
        )
    }))
    .with_context(|| format!("{tool}: cannot create temporary file for {display_path}"))
}

#[cfg(not(unix))]
fn atomic_replace(
    target: CommitTarget<'_>,
    bytes: &[u8],
    permissions: Option<std::fs::Permissions>,
    target_version: Option<&FileVersion>,
    fault: CommitFault,
) -> Result<()> {
    let CommitTarget {
        parent,
        parent_path,
        leaf,
        display_path,
        tool,
    } = target;
    let path = parent_path.join(leaf);
    let mut last_collision = None;
    for _ in 0..100 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp = parent_path.join(format!(
            ".kloop-write-{}-{sequence}.tmp",
            std::process::id()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = match options.open(&temp) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_collision = Some(error);
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("{tool}: cannot create temporary file for {display_path}")
                });
            }
        };
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
                std::fs::remove_file(parent_path.join(&temp)).with_context(|| {
                    format!("{tool}: cannot inject temporary replacement for {display_path}")
                })?;
                std::fs::write(parent_path.join(&temp), b"attacker-controlled bytes")
                    .with_context(|| {
                        format!("{tool}: cannot inject temporary replacement for {display_path}")
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
                target_version,
            )?;
            verify_parent_binding(parent, parent_path, tool, display_path)?;
            verify_temp_binding(
                parent,
                parent_path,
                temp.as_os_str(),
                &file,
                bytes,
                display_path,
                tool,
            )?;
            std::fs::rename(&temp, &path)
                .with_context(|| format!("{tool}: cannot atomically replace {display_path}"))?;
            sync_parent(parent, tool, display_path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        return result;
    }
    Err(last_collision.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "temporary file name collision",
        )
    }))
    .with_context(|| format!("{tool}: cannot create temporary file for {display_path}"))
}

fn verify_target_unchanged(
    parent: &std::fs::File,
    parent_path: &Path,
    leaf: &OsStr,
    display_path: &str,
    tool: &str,
    expected: Option<&FileVersion>,
) -> Result<()> {
    match (
        expected,
        read_regular_target(parent, parent_path, leaf, tool, display_path)?,
    ) {
        (None, None) => Ok(()),
        (Some(expected), Some(current)) if expected.matches(&current.bytes, &current.metadata) => {
            Ok(())
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
    let opened_metadata = opened
        .metadata()
        .with_context(|| format!("{tool}: cannot inspect temporary file for {display_path}"))?;
    let named = read_regular_target(parent, parent_path, temp, tool, display_path)?
        .with_context(|| format!("{tool}: temporary file disappeared for {display_path}"))?;
    if file_identity(&opened_metadata)? != file_identity(&named.metadata)?
        || named.bytes != expected_bytes
    {
        bail!("{tool}: temporary file changed before replacing {display_path}; retry the call");
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent(parent: &std::fs::File, tool: &str, display_path: &str) -> Result<()> {
    parent
        .sync_all()
        .with_context(|| format!("{tool}: cannot sync parent directory for {display_path}"))
}

#[cfg(not(unix))]
fn sync_parent(parent: &std::fs::File, _tool: &str, _display_path: &str) -> Result<()> {
    let _ = parent.sync_all();
    Ok(())
}

pub(super) async fn read_offloaded_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let id = str_arg(input, "id", "read_offloaded")?;
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("read_offloaded: invalid id (only [A-Za-z0-9-] allowed)");
    }
    let path = ctx.cfg.offload_dir.join(format!("{id}.txt"));
    tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("read_offloaded: no offloaded output with id {id}"))
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
            &crate::permissions::PermissionRules::default(),
            cwd.to_path_buf(),
            Some(Arc::new(ChannelApprover { tx })),
            None,
        )
        .unwrap();
        let mut ctx = test_ctx(0, tag);
        let mut cfg = (*ctx.cfg).clone();
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
    ) -> super::CommitTarget<'a> {
        super::CommitTarget {
            parent: &prepared.parent,
            parent_path: &prepared.parent_path,
            leaf: &prepared.leaf,
            display_path,
            tool: "write_file",
        }
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
        assert_eq!(out, "2\tbeta\n3\tgamma");

        let (out, is_error) =
            run_tool("read_file", json!({"path": "/nonexistent/kloop"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("cannot read"));
        let _ = std::fs::remove_file(path);
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
        assert_eq!(out, "2\tsecond");

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
    async fn read_empty_pdf_and_character_budget_are_explicit() {
        let empty = temp_file("read-empty", "");
        let pdf = temp_bytes("read-pdf", b"%PDF-1.4\nminimal\n");
        let long = temp_file("read-budget", &"long line content\n".repeat(1_000));
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
        assert!(out.chars().count() < 8_000, "{} chars", out.chars().count());
        assert!(out.contains("[read output truncated; call read_file with offset="));
        assert!(!ctx
            .cfg
            .file_state
            .observation(&long_key)
            .unwrap()
            .is_complete());

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
        let mut cfg = (*ctx.cfg).clone();
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
            None,
        )
        .unwrap();
        let mut ctx = test_ctx(0, "read-approval-swap");
        let mut cfg = (*ctx.cfg).clone();
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
            .send(crate::permissions::Decision::Allow)
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
    async fn write_file_refuses_missing_parent_before_approval() {
        let dir = std::env::temp_dir().join(format!("kloop-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("deep/nested/file.txt");
        let (ctx, mut approvals) = approval_ctx("write", &dir);
        let (out, is_error) = run_tool(
            "write_file",
            json!({"path": path.to_str().unwrap(), "content": "created"}),
            &ctx,
        )
        .await;
        assert!(is_error, "{out}");
        assert!(out.contains("parent directory"), "{out}");
        assert!(!dir.join("deep").exists());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), approvals.recv())
                .await
                .is_err(),
            "permission approver must not be called"
        );
        let _ = std::fs::remove_dir_all(dir);
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
        assert!(ctx.cfg.file_state.observation(&key).is_none());

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
    async fn deleting_a_read_file_does_not_turn_overwrite_into_create() {
        let dir = std::env::temp_dir().join(format!("kloop-write-delete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("target.txt");
        std::fs::write(&path, "original\n").unwrap();
        let ctx = test_ctx(0, "write-delete");
        observe_whole(&path, &ctx).await;
        std::fs::remove_dir_all(&dir).unwrap();

        let (out, is_error) = run_tool(
            "write_file",
            json!({"path": path.to_str().unwrap(), "content": "recreated\n"}),
            &ctx,
        )
        .await;

        assert!(is_error);
        assert!(out.contains("parent directory"), "{out}");
        assert!(!dir.exists(), "stale failure must not recreate parents");
    }

    #[tokio::test]
    async fn stale_edit_rejects_even_when_old_string_remains_unique() {
        let path = temp_file("edit-stale", "alpha\nbeta\n");
        let key = std::fs::canonicalize(&path).unwrap();
        let ctx = test_ctx(0, "edit-stale");
        observe_whole(&path, &ctx).await;
        std::fs::write(&path, "external\nalpha\nbeta\n").unwrap();

        let (out, is_error) = run_tool(
            "edit_file",
            json!({
                "path": path.to_str().unwrap(),
                "old_string": "beta",
                "new_string": "BETA"
            }),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("changed since it was read"), "{out}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "external\nalpha\nbeta\n"
        );
        assert!(ctx.cfg.file_state.observation(&key).is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn injected_atomic_failure_preserves_original_and_cleans_temp() {
        let dir = std::env::temp_dir().join(format!("kloop-atomic-fault-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("target.txt");
        std::fs::write(&path, b"original").unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let expected = super::FileObservation::full(b"original", &metadata);

        let prepared =
            super::prepare_mutation(&dir, &path, "write_file", path.to_str().unwrap()).unwrap();
        let error = match super::commit_mutation(
            commit_target(&prepared, path.to_str().unwrap()),
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
    fn replaced_temporary_name_never_reaches_target() {
        let dir =
            std::env::temp_dir().join(format!("kloop-atomic-temp-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("target.txt");
        std::fs::write(&path, b"original").unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let expected = super::FileObservation::full(b"original", &metadata);
        let prepared =
            super::prepare_mutation(&dir, &path, "write_file", path.to_str().unwrap()).unwrap();

        let error = match super::commit_mutation(
            commit_target(&prepared, path.to_str().unwrap()),
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
        let metadata = std::fs::metadata(&path).unwrap();
        let expected = super::FileObservation::full(b"original", &metadata);

        let prepared =
            super::prepare_mutation(&dir, &path, "write_file", path.to_str().unwrap()).unwrap();
        super::commit_mutation(
            commit_target(&prepared, path.to_str().unwrap()),
            Some(&expected),
            super::Mutation::Write {
                bytes: b"first".to_vec(),
            },
            super::CommitFault::None,
        )
        .unwrap();
        let error = match super::commit_mutation(
            commit_target(&prepared, path.to_str().unwrap()),
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
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
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
        let mut cfg = (*ctx.cfg).clone();
        cfg.cwd = workspace.clone();
        ctx.cfg = std::sync::Arc::new(cfg);

        let frozen = json!({"path": "inside-link/frozen.txt", "content": "frozen"});
        let prepared = super::prepare_mutation_input("write_file", &frozen, &ctx)
            .await
            .unwrap();
        std::fs::remove_file(workspace.join("inside-link")).unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("inside-link")).unwrap();
        let output = super::write_file_tool(&frozen, &prepared, &ctx)
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
            None,
        )
        .unwrap();
        let mut deny_ctx = test_ctx(0, "write-alias-deny");
        let mut cfg = (*deny_ctx.cfg).clone();
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
            None,
        )
        .unwrap();
        let mut ask_ctx = test_ctx(0, "write-alias-ask");
        let mut cfg = (*ask_ctx.cfg).clone();
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
            .send(crate::permissions::Decision::Allow)
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
            .send(crate::permissions::Decision::Allow)
            .expect("approval receiver still waiting");

        let (out, is_error) = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("FIFO replacement must not block openat")
            .unwrap();
        assert!(is_error, "{out}");
        assert!(out.contains("not a regular file"), "{out}");
        let _ = std::fs::remove_dir_all(root);
    }

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
        let mut cfg = (*ctx.cfg).clone();
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
    async fn read_offloaded_round_trip_and_id_validation() {
        let ctx = test_ctx(0, "offloaded");
        std::fs::create_dir_all(&ctx.cfg.offload_dir).unwrap();
        std::fs::write(ctx.cfg.offload_dir.join("off-7777.txt"), "full payload").unwrap();

        let (out, is_error) = run_tool("read_offloaded", json!({"id": "off-7777"}), &ctx).await;
        assert!(!is_error);
        assert_eq!(out, "full payload");

        // Path traversal shapes are rejected before touching the filesystem.
        let (out, is_error) =
            run_tool("read_offloaded", json!({"id": "../../etc/passwd"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("invalid id"));
        let _ = std::fs::remove_dir_all(&ctx.cfg.offload_dir);
    }
}
