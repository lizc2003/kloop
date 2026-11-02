//! Prompt injections for a `/name` invocation (plan 36, slice 2): embedded bash
//! (`` !`cmd` `` inline and ```` ```!\n…\n``` ```` blocks) executed through the
//! real bash permission gate with its output inlined, plus `@file` mentions
//! whose contents are appended (subject to the read-path gate). These run at
//! expansion time — before the turn — so the model sees fresh command output
//! and file contents, not the raw markers.
//!
//! Both trigger paths expand injections, on the same body: the user-initiated
//! slash path ([`expand_slash_injections`], from `commands::run`, which builds a
//! minimal top-level context) and a model-activated skill ([`expand_skill_body`],
//! from the `skill` tool, which already holds a real [`ToolCtx`]). A `!cmd` faces
//! the bash gate either way, so "the model picked a skill that runs bash" is gated
//! exactly like the model calling bash itself. The skill path also runs the same
//! `pre_tool` hook a bash call would (the slash path is outside a turn and keeps
//! its original shape). kloop has no remote/MCP skills, so cc's "never execute an
//! MCP skill's `!cmd`" carve-out has no analogue here.
//!
//! Arguments are never trusted to drive injections. The slash path substitutes
//! `$ARGUMENTS`/`${CLAUDE_SKILL_DIR}` first because the user typing the slash
//! command is the one who would run the command; the model's `skill` tool scans
//! markers on the author-written body only, so a model-supplied `arguments`
//! string containing `` !`cmd` `` or `@path` stays literal text.
//!
//! Both gates are the security boundary and are never bypassed here: `!cmd` runs
//! through the same `check_call("bash", …)` a real bash tool call faces (deny /
//! safety / ask / approver, sandbox), and `@file` honors `read_path_blocked`
//! (deny + sensitive-path list, plan 31). A denied or failed `!cmd` aborts the
//! expansion; the caller shows the error instead of running a turn.

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::ToolCtx;
use super::bash;
use crate::config::Config;
use crate::config::EffectiveWorkspace;
use crate::event::Event;
use crate::permissions::PermissionNotice;
use crate::permissions::Refusal;
use crate::permissions::user_stop;
use crate::skills::expand_body;
use crate::skills::substitute;
use crate::skills::substitute_skill_dir;

/// A referenced file's contents are truncated to this many bytes before being
/// appended, so a `@big.log` mention can't blow the context window.
const MAX_ATTACH_BYTES: usize = 100_000;

/// Cheap pre-check: does `body` contain any injection marker at all? The caller
/// path builds nothing when this is false, so a command with no injections stays
/// byte-identical and does zero work. A stray `@` only triggers a side-effect-
/// free scan (a mention is inlined only if it resolves to a readable file).
fn has_injections(body: &str) -> bool {
    body.contains("!`") || body.contains("```!") || body.contains('@')
}

/// Expand injections for a `/name` invocation. Returns `body` unchanged when it
/// has no markers. On markers, builds a [`ToolCtx::harness`] context (an
/// injected `!cmd` shows no tool row, as in cc; the bash permission prompt is
/// the only surface) and runs the expansion. An error (denied / failed `!cmd`)
/// propagates so the caller can show it rather than start a turn.
pub(crate) async fn expand_slash_injections(
    body: &str,
    cfg: &Arc<Config>,
    cancel: &CancellationToken,
) -> Result<String> {
    if !has_injections(body) {
        return Ok(body.to_string());
    }
    let ctx = ToolCtx::harness(cfg.clone(), cancel.clone());
    let workspace = cfg.effective_workspace();

    expand(body, &ctx, &workspace).await
}

/// Run `!cmd` (inline output in place) then append `@file` contents. `@file`
/// mentions are scanned on the original `body`, not the bash-expanded result, so
/// command output can never drive a file read. The slash path reaches this
/// through [`expand_slash_injections`]. A body with no markers returns
/// unchanged.
pub(super) async fn expand(
    body: &str,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
) -> Result<String> {
    if !has_injections(body) {
        return Ok(body.to_string());
    }
    let with_bash = run_embedded_bash(body, ctx, workspace).await?;
    let attachments = collect_file_attachments(body, workspace).await;
    Ok(format!("{with_bash}{attachments}"))
}

/// Expand a skill body for the `skill` tool. Markers are found on the
/// author-written `body` itself; `$ARGUMENTS`/`${CLAUDE_SKILL_DIR}` are then
/// substituted segment by segment, so arguments substituted *into* text — or
/// appended as the `ARGUMENTS:` line — are never scanned for injections. A
/// command's own text is substituted first, so `$ARGUMENTS` inside a `!cmd`
/// still works.
pub(super) async fn expand_skill_body(
    body: &str,
    skill_dir: &str,
    args: &str,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
) -> Result<String> {
    if !has_injections(body) {
        return Ok(expand_body(body, skill_dir, args));
    }
    let mut out = String::with_capacity(body.len());
    let mut used_args = false;
    let mut last = 0;
    for marker in find_embedded(body) {
        let (text, used) = substitute(&body[last..marker.start], skill_dir, args);
        used_args |= used;
        out.push_str(&text);
        let (command, used) = substitute(&marker.command, skill_dir, args);
        used_args |= used;
        let output = run_gated_bash(&command, ctx, workspace, Trigger::Skill).await?;
        out.push_str(output.trim_end());
        last = marker.end;
    }
    let (tail, used) = substitute(&body[last..], skill_dir, args);
    used_args |= used;
    out.push_str(&tail);
    // Mentions resolve against the author's text with only the skill directory
    // substituted: the directory is trusted, while `$ARGUMENTS` stays literal so
    // arguments cannot smuggle a `@path`.
    let mentions = substitute_skill_dir(body, skill_dir);
    out.push_str(&collect_file_attachments(&mentions, workspace).await);
    if !used_args && !args.trim().is_empty() {
        out.push_str(&format!("\n\nARGUMENTS: {}", args.trim()));
    }
    Ok(out)
}

/// Which surface asked for the expansion. The slash path runs before a turn
/// exists and keeps its original shape; the model-activated skill runs inside
/// a turn, so its `!cmd` faces the `pre_tool` hook a bash call would.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Trigger {
    Slash,
    Skill,
}

/// One embedded-bash occurrence: its byte span in the body and the command.
struct Embedded {
    start: usize,
    end: usize,
    command: String,
}

/// Find every embedded-bash marker, left to right and non-overlapping: a
/// ```` ```!…``` ```` block, or an inline `` !`…` `` whose `!` sits at
/// start-of-line or after whitespace (so `foo!`bar`` and `$!` don't match).
/// Scanning the block whole means a `` !` `` inside it is not also matched.
fn find_embedded(body: &str) -> Vec<Embedded> {
    let b = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < body.len() {
        if body[i..].starts_with("```!") {
            let content = i + 4;
            match body[content..].find("```") {
                Some(rel) => {
                    let close = content + rel;
                    let command = body[content..close].trim().to_string();
                    let end = close + 3;
                    if !command.is_empty() {
                        out.push(Embedded {
                            start: i,
                            end,
                            command,
                        });
                    }
                    i = end;
                    continue;
                }
                // Unterminated block: leave the rest verbatim.
                None => break,
            }
        }
        if body[i..].starts_with("!`") && (i == 0 || b[i - 1].is_ascii_whitespace()) {
            let content = i + 2;
            if let Some(rel) = body[content..].find('`') {
                let close = content + rel;
                let command = body[content..close].trim().to_string();
                let end = close + 1;
                if !command.is_empty() {
                    out.push(Embedded {
                        start: i,
                        end,
                        command,
                    });
                }
                i = end;
                continue;
            }
        }
        // Not a marker: advance one whole char (indices stay on boundaries —
        // every marker starts/ends on ASCII).
        i += body[i..].chars().next().map_or(1, char::len_utf8);
    }
    out
}

/// Replace each embedded-bash marker with the command's gated output. Sequential
/// (not concurrent) so overlapping approval prompts never race the terminal.
async fn run_embedded_bash(
    body: &str,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
) -> Result<String> {
    let markers = find_embedded(body);
    if markers.is_empty() {
        return Ok(body.to_string());
    }
    let mut result = String::with_capacity(body.len());
    let mut last = 0;
    for m in &markers {
        let output = run_gated_bash(&m.command, ctx, workspace, Trigger::Slash).await?;
        result.push_str(&body[last..m.start]);
        result.push_str(output.trim_end());
        last = m.end;
    }
    result.push_str(&body[last..]);
    Ok(result)
}

/// Run one `!cmd` through the same gate a real `bash` call faces (deny / safety
/// / ask / approver, then sandbox). A blocked command is an error that aborts
/// the whole expansion; a command that merely exits non-zero returns its output
/// (with the `[exit N]` tail), like the bash tool.
async fn run_gated_bash(
    command: &str,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
    trigger: Trigger,
) -> Result<String> {
    let input = json!({ "command": command });
    if trigger == Trigger::Skill {
        super::run_pre_tool_hook("bash", &input, ctx)
            .await
            .map_err(|e| anyhow!("!`{command}`: {e:#}"))?;
    }
    let sandbox_auto = bash::execution_context("bash", &input, workspace, &ctx.cfg.shell_login_env);
    match trigger {
        // The slash path runs outside a turn and keeps its original gate and
        // error text. The skill path runs inside one, so it must distinguish
        // a Stop (end the turn) from a denial and surface a notice.
        Trigger::Slash => {
            workspace
                .permissions
                .check_call_with_execution("bash", &input, ctx.depth, sandbox_auto)
                .await
                .map_err(|reason| anyhow!("!`{command}`: {reason}"))?;
        }
        Trigger::Skill => {
            let verdict = workspace
                .permissions
                .check_call_with_resolved_path("bash", &input, None, None, ctx.depth, sandbox_auto)
                .await;
            accept_permission(verdict, command, ctx)?;
        }
    }
    bash::bash_tool(&input, ctx, workspace).await
}

/// Apply the permission gate's verdict. `Stopped` must end the turn just as it
/// does for a real tool call — a skill that runs `!cmd` cannot turn the user's
/// No into an ordinary denial the model is free to route around — and a
/// `notice` (e.g. "project rule not saved") is surfaced, not dropped.
fn accept_permission(
    verdict: std::result::Result<Option<PermissionNotice>, Refusal>,
    command: &str,
    ctx: &ToolCtx,
) -> Result<()> {
    match verdict {
        Ok(Some(notice)) => ctx.ui.emit(&Event::Note(notice.message)),
        Ok(None) => {}
        Err(Refusal::Denied(reason)) => bail!("!`{command}`: {reason}"),
        Err(Refusal::Stopped) => {
            ctx.stop.cancel();
            bail!("!`{command}`: {}", user_stop("bash"));
        }
    }
    Ok(())
}

/// Append the contents of each `@file` mention that resolves to a readable file.
/// A mention that isn't an existing file is left alone (it's prose, e.g.
/// `@someone`); one blocked by the read gate is noted, not inlined (no leak);
/// duplicates and unreadable/binary files are skipped. Reads run on the blocking
/// pool and stop at [`MAX_ATTACH_BYTES`], so a multi-gigabyte log cannot stall a
/// runtime worker while it is read whole and then discarded.
async fn collect_file_attachments(body: &str, workspace: &EffectiveWorkspace) -> String {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut planned: Vec<(String, Option<PathBuf>)> = Vec::new();
    for mention in find_mentions(body) {
        let path = workspace.cwd.join(&mention);
        if !path.is_file() {
            continue;
        }
        if !seen.insert(path.clone()) {
            continue;
        }
        if workspace.permissions.read_path_blocked(&path) {
            planned.push((mention, None));
        } else {
            planned.push((mention, Some(path)));
        }
    }
    let read = tokio::task::spawn_blocking(move || {
        planned
            .into_iter()
            .map(|(mention, path)| match path {
                None => (mention, Attachment::Blocked),
                Some(path) => match read_attachment(&path) {
                    Some((text, truncated)) => (mention, Attachment::Content { text, truncated }),
                    None => (mention, Attachment::Skipped),
                },
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let mut out = String::new();
    for (mention, attachment) in read {
        match attachment {
            Attachment::Blocked => out.push_str(&format!(
                "\n\n@{mention}: [access blocked by deny/sensitive rules]"
            )),
            Attachment::Skipped => {}
            Attachment::Content { text, truncated } => {
                out.push_str(&format!("\n\n@{mention}:\n{text}"));
                if truncated {
                    out.push_str(&format!("\n[truncated to {MAX_ATTACH_BYTES} bytes]"));
                }
            }
        }
    }
    out
}

/// One mention's outcome after the blocking read pass.
enum Attachment {
    /// Deny/sensitive rules forbid the read: say so without inlining.
    Blocked,
    /// Gone, unreadable, or binary.
    Skipped,
    Content {
        text: String,
        truncated: bool,
    },
}

/// Read at most [`MAX_ATTACH_BYTES`] plus a little. A prefix that is valid
/// UTF-8 is kept; invalid bytes inside the cap still mean a binary file and
/// skip it, while an incomplete sequence cut by the cap is just truncation.
fn read_attachment(path: &Path) -> Option<(String, bool)> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_ATTACH_BYTES as u64 + 4)
        .read_to_end(&mut bytes)
        .ok()?;
    let truncated = bytes.len() > MAX_ATTACH_BYTES;
    let cut = MAX_ATTACH_BYTES.min(bytes.len());
    let text = match std::str::from_utf8(&bytes[..cut]) {
        Ok(text) => text.to_string(),
        Err(e) if e.error_len().is_none() => {
            String::from_utf8_lossy(&bytes[..e.valid_up_to()]).into_owned()
        }
        Err(_) => return None,
    };
    Some((text, truncated))
}

/// Find `@path` mentions: an `@` at start-of-line or after whitespace, followed
/// by a run of path characters. The path is resolved and existence-checked by
/// the caller, so this over-matches on purpose (a non-file `@word` is dropped
/// there) — an email `a@b.com` never matches (its `@` follows a non-space).
fn find_mentions(body: &str) -> Vec<String> {
    let b = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < body.len() {
        if b[i] == b'@' && (i == 0 || b[i - 1].is_ascii_whitespace()) {
            let start = i + 1;
            let mut j = start;
            while j < body.len() && is_path_char(b[j]) {
                j += 1;
            }
            if j > start {
                out.push(body[start..j].to_string());
            }
            i = j.max(i + 1);
            continue;
        }
        i += 1;
    }
    out
}

/// Path characters a `@mention` may contain. Deliberately excludes `~` (home
/// expansion) and `#` (line ranges) — both deferred.
fn is_path_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'/' | b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_embedded_matches_blocks_and_guarded_inline() {
        let markers = find_embedded("a !`date` b\n```!\nls -la\n```\ntail x!`no` $!`no`");
        let cmds: Vec<&str> = markers.iter().map(|m| m.command.as_str()).collect();
        // The guarded inline and the block match; `x!`no`` (no space before `!`)
        // and `$!`no`` do not.
        assert_eq!(cmds, vec!["date", "ls -la"]);
    }

    #[test]
    fn find_embedded_ignores_unterminated_and_empty() {
        assert!(find_embedded("!`").is_empty());
        assert!(find_embedded("!``").is_empty()); // empty command
        assert!(find_embedded("```!\nno close").is_empty());
        // Start-of-string inline is allowed (i == 0).
        assert_eq!(find_embedded("!`pwd`")[0].command, "pwd");
    }

    #[test]
    fn find_mentions_guards_on_preceding_char_and_path_chars() {
        assert_eq!(
            find_mentions("see @src/main.rs and @docs/x.md now"),
            vec!["src/main.rs", "docs/x.md"]
        );
        // Email-like `@` (preceded by a letter) is not a mention; a bare `@` is
        // skipped (no path chars follow).
        assert!(find_mentions("mail me at a@b.com or @ alone").is_empty());
        // Start-of-string mention is allowed.
        assert_eq!(find_mentions("@file.txt"), vec!["file.txt"]);
    }

    #[test]
    fn read_attachment_stops_at_the_cap_and_ignores_late_binary_bytes() {
        let dir = std::env::temp_dir().join(format!("kloop-inject-read-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.log");
        // A full cap of text, then bytes that are not UTF-8: the prefix is
        // still text, so the file is attached, truncated — not skipped whole.
        let mut body = vec![b'a'; MAX_ATTACH_BYTES];
        body.extend_from_slice(&[0xff, 0xfe, 0xff]);
        std::fs::write(&path, &body).unwrap();
        let (text, truncated) = read_attachment(&path).expect("prefix is text");
        assert_eq!(text.len(), MAX_ATTACH_BYTES);
        assert!(truncated);

        // A multi-byte char straddling the cap is dropped, not split.
        let path = dir.join("edge.md");
        let mut body = "a".repeat(MAX_ATTACH_BYTES - 1).into_bytes();
        body.extend_from_slice("é".as_bytes());
        std::fs::write(&path, &body).unwrap();
        let (text, truncated) = read_attachment(&path).unwrap();
        assert_eq!(text.len(), MAX_ATTACH_BYTES - 1);
        assert!(truncated);

        // Invalid UTF-8 inside the cap is still a binary file.
        let path = dir.join("bin.dat");
        std::fs::write(&path, [0x00, 0xff, 0x00]).unwrap();
        assert!(read_attachment(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A notice the gate attaches (e.g. "project rule not saved") reaches the
    /// UI; the expansion still runs. A `Stop` answer ends the turn.
    #[tokio::test]
    async fn permission_verdicts_surface_notices_and_end_the_turn_on_stop() {
        struct Recording(std::sync::Mutex<Vec<String>>);
        impl crate::agent::Ui for Recording {
            fn emit(&self, event: &Event) {
                if let Event::Note(note) = event {
                    self.0.lock().unwrap().push(note.clone());
                }
            }
        }

        let mut ctx = crate::tools::testutil::test_ctx(0, "inject-permission");
        let recording = std::sync::Arc::new(Recording(Default::default()));
        ctx.ui = recording.clone();
        accept_permission(
            Ok(Some(PermissionNotice {
                message: "rule not saved".into(),
            })),
            "echo x",
            &ctx,
        )
        .unwrap();
        assert_eq!(*recording.0.lock().unwrap(), vec!["rule not saved"]);

        let err = accept_permission(Err(Refusal::Stopped), "echo x", &ctx).unwrap_err();
        assert!(err.to_string().contains("stopped the turn"), "got: {err}");
        assert!(ctx.stop.is_cancelled());
        assert!(!ctx.cancel.is_cancelled(), "a stop is not an interrupt");
    }
}
