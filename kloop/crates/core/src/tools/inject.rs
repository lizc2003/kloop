//! Prompt injections for a `/name` invocation (plan 36, slice 2): embedded bash
//! (`` !`cmd` `` inline and ```` ```!\n…\n``` ```` blocks) executed through the
//! real bash permission gate with its output inlined, plus `@file` mentions
//! whose contents are appended (subject to the read-path gate). These run at
//! expansion time — after `$ARGUMENTS`/`${CLAUDE_SKILL_DIR}` substitution,
//! before the turn — so the model sees fresh command output and file contents,
//! not the raw markers.
//!
//! Only the user-initiated slash path uses this ([`expand_slash_injections`],
//! called from `commands::run`); a model-activated skill (the `skill` tool) does
//! not expand injections — running bash because the model picked a skill is a
//! different risk profile, left to a later slice.
//!
//! Both gates are the security boundary and are never bypassed here: `!cmd` runs
//! through the same `check_call("bash", …)` a real bash tool call faces (deny /
//! safety / ask / approver, sandbox), and `@file` honors `read_path_blocked`
//! (deny + sensitive-path list, plan 31). A denied or failed `!cmd` aborts the
//! expansion; the caller shows the error instead of running a turn.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::anyhow;
use anyhow::Result;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::bash;
use super::ToolCtx;
use crate::agent::Ui;
use crate::config::Config;

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
/// has no markers. On markers, builds a minimal top-level [`ToolCtx`] with a
/// silent UI — the only user-visible interaction is the bash permission prompt,
/// which rides the approver inside `cfg.permissions`, not the UI — and runs the
/// expansion. An error (denied / failed `!cmd`) propagates so the caller can
/// show it rather than start a turn.
pub(crate) async fn expand_slash_injections(
    body: &str,
    cfg: &Arc<Config>,
    cancel: &CancellationToken,
) -> Result<String> {
    if !has_injections(body) {
        return Ok(body.to_string());
    }
    let ctx = ToolCtx {
        cfg: cfg.clone(),
        ui: Arc::new(SilentUi),
        cancel: cancel.clone(),
        depth: 0,
        hook_context: Arc::new(Mutex::new(Vec::new())),
        from_program: false,
        parent_rollout_id: None,
        program_result: None,
    };
    expand(body, &ctx).await
}

/// A UI that swallows everything: an injected `!cmd` streams nothing and shows
/// no tool row (cc likewise creates no visible bash row for prompt injection);
/// the permission prompt is the only surface, and it goes through the approver.
struct SilentUi;
impl Ui for SilentUi {
    fn text_delta(&self, _s: &str) {}
    fn note(&self, _s: &str) {}
}

/// Run `!cmd` (inline output in place) then append `@file` contents. `@file`
/// mentions are scanned on the original `body`, not the bash-expanded result, so
/// command output can never drive a file read.
async fn expand(body: &str, ctx: &ToolCtx) -> Result<String> {
    let with_bash = run_embedded_bash(body, ctx).await?;
    let attachments = collect_file_attachments(body, ctx);
    Ok(format!("{with_bash}{attachments}"))
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
async fn run_embedded_bash(body: &str, ctx: &ToolCtx) -> Result<String> {
    let markers = find_embedded(body);
    if markers.is_empty() {
        return Ok(body.to_string());
    }
    let mut result = String::with_capacity(body.len());
    let mut last = 0;
    for m in &markers {
        let output = run_gated_bash(&m.command, ctx).await?;
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
async fn run_gated_bash(command: &str, ctx: &ToolCtx) -> Result<String> {
    let input = json!({ "command": command });
    let sandbox_auto = bash::sandbox_auto_allowed("bash", &input, ctx);
    ctx.cfg
        .effective_permissions()
        .check_call("bash", &input, ctx.depth, sandbox_auto)
        .await
        .map_err(|reason| anyhow!("!`{command}`: {reason}"))?;
    bash::bash_tool(&input, ctx).await
}

/// Append the contents of each `@file` mention that resolves to a readable file.
/// A mention that isn't an existing file is left alone (it's prose, e.g.
/// `@someone`); one blocked by the read gate is noted, not inlined (no leak);
/// duplicates and unreadable/binary files are skipped.
fn collect_file_attachments(body: &str, ctx: &ToolCtx) -> String {
    let cwd = ctx.cfg.effective_cwd();
    let perms = ctx.cfg.effective_permissions();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut out = String::new();
    for mention in find_mentions(body) {
        let path = cwd.join(&mention);
        if !path.is_file() {
            continue;
        }
        if !seen.insert(path.clone()) {
            continue;
        }
        if perms.read_path_blocked(&path) {
            out.push_str(&format!(
                "\n\n@{mention}: [access blocked by deny/sensitive rules]"
            ));
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            // Unreadable or non-UTF-8 (binary): skip silently.
            continue;
        };
        let (content, truncated) = if content.len() > MAX_ATTACH_BYTES {
            (
                &content[..floor_char_boundary(&content, MAX_ATTACH_BYTES)],
                true,
            )
        } else {
            (content.as_str(), false)
        };
        out.push_str(&format!("\n\n@{mention}:\n{content}"));
        if truncated {
            out.push_str(&format!("\n[truncated to {MAX_ATTACH_BYTES} bytes]"));
        }
    }
    out
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

/// Largest byte index `<= max` that is a char boundary (stable-Rust stand-in for
/// `str::floor_char_boundary`), so truncation never splits a UTF-8 sequence.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    let mut i = max.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
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
    fn floor_char_boundary_never_splits_utf8() {
        let s = "aé"; // 'é' is two bytes (indices 1..3)
        assert_eq!(floor_char_boundary(s, 2), 1);
        assert_eq!(floor_char_boundary(s, 3), 3);
        assert_eq!(floor_char_boundary(s, 99), 3);
    }
}
