use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde_json::json;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::run_turn;
use crate::agent::EndReason;
use crate::agent::Ui;
use crate::config::Config;
use crate::history::History;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::ToolDef;

const SUBAGENT_MAX_ROUNDS: usize = 15;

/// Everything a tool execution needs; cheap to clone into spawned futures.
#[derive(Clone)]
pub struct ToolCtx {
    pub cfg: Arc<Config>,
    pub ui: Arc<dyn Ui>,
    pub cancel: CancellationToken,
    pub depth: u8,
}

pub fn tool_defs(depth: u8) -> Vec<ToolDef> {
    let mut defs = vec![
        ToolDef {
            name: "bash",
            description: "Run a shell command with `sh -lc`. stdout and stderr are merged; a non-zero exit status is appended. Default timeout 60s.",
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to run"},
                    "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default 60000)"}
                },
                "required": ["command"]
            }),
        },
        ToolDef {
            name: "read_file",
            description: "Read a text file, returning numbered lines formatted as `{n}\\t{line}`. Reads up to 2000 lines by default.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "description": "1-based line number to start from (default 1)"},
                    "limit": {"type": "integer", "description": "Max lines to return (default 2000)"}
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "write_file",
            description: "Write content to a file, creating parent directories as needed. Overwrites if the file exists.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        },
        ToolDef {
            name: "edit_file",
            description: "Replace old_string with new_string in a file. Fails if old_string is not found, or matches more than once without replace_all.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        },
        ToolDef {
            name: "read_offloaded",
            description: "Fetch the full content of an offloaded tool result by its id (e.g. off-0001).",
            schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Offload id from a truncation pointer, e.g. off-0001"}
                },
                "required": ["id"]
            }),
        },
    ];
    if depth == 0 {
        defs.push(ToolDef {
            name: "task",
            description: "Spawn a sub-agent with a fresh history to work on a self-contained prompt; returns its final text. Sub-agents cannot spawn further sub-agents.",
            schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "Complete standalone task description"},
                    "max_rounds": {"type": "integer", "description": "Round cap for the sub-agent (default and max 15)"}
                },
                "required": ["prompt"]
            }),
        });
    }
    defs
}

/// Concurrency safety by name AND input: read-only tools are always safe,
/// bash is safe only when every command segment is a known read-only command.
pub fn is_concurrency_safe(name: &str, input: &Value) -> bool {
    match name {
        "read_file" | "read_offloaded" => true,
        "bash" => input["command"].as_str().is_some_and(bash_is_readonly),
        _ => false,
    }
}

fn bash_is_readonly(cmd: &str) -> bool {
    const SAFE: &[&str] = &[
        "ls", "cat", "rg", "grep", "find", "head", "tail", "wc", "pwd", "which", "file", "stat",
        "tree", "du", "echo",
    ];
    const GIT_SAFE: &[&str] = &["status", "log", "diff", "show", "branch"];
    let normalized = cmd.replace("&&", ";").replace('|', ";");
    let segments: Vec<&str> = normalized
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if segments.is_empty() {
        return false;
    }
    segments.iter().all(|seg| {
        if seg.contains('>') {
            return false;
        }
        let mut tokens = seg.split_whitespace();
        let Some(first) = tokens.next() else {
            return false;
        };
        if first == "git" {
            tokens.next().is_some_and(|sub| GIT_SAFE.contains(&sub))
        } else {
            SAFE.contains(&first)
        }
    })
}

/// Execute one round of tool calls. Consecutive concurrency-safe calls run as
/// one concurrent batch (join_all); everything else runs sequentially. Every
/// tool_use always gets a paired tool_result: cancellation patches the
/// remaining calls with is_error "interrupted" results so history stays legal.
pub async fn dispatch_tools(
    tool_uses: Vec<(String, String, Value)>,
    ctx: &ToolCtx,
) -> Vec<ContentBlock> {
    let mut results = Vec::with_capacity(tool_uses.len());
    let mut i = 0;
    while i < tool_uses.len() {
        let safe = is_concurrency_safe(&tool_uses[i].1, &tool_uses[i].2);
        let mut j = i + 1;
        while j < tool_uses.len() && is_concurrency_safe(&tool_uses[j].1, &tool_uses[j].2) == safe {
            j += 1;
        }
        let batch = &tool_uses[i..j];
        if ctx.cancel.is_cancelled() {
            results.extend(batch.iter().map(|(id, _, _)| interrupted(id)));
        } else if safe {
            let futs = batch.iter().map(|(id, name, input)| {
                run_one(id.clone(), name.clone(), input.clone(), ctx.clone())
            });
            results.extend(futures::future::join_all(futs).await);
        } else {
            for (id, name, input) in batch {
                if ctx.cancel.is_cancelled() {
                    results.push(interrupted(id));
                } else {
                    results
                        .push(run_one(id.clone(), name.clone(), input.clone(), ctx.clone()).await);
                }
            }
        }
        i = j;
    }
    results
}

/// Also reused by rollout resume to patch tool_use blocks orphaned by a
/// killed session.
pub(crate) fn interrupted(tool_use_id: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: tool_use_id.into(),
        content: "interrupted".into(),
        is_error: true,
    }
}

async fn run_one(id: String, name: String, input: Value, ctx: ToolCtx) -> ContentBlock {
    let summary: String = input.to_string().chars().take(120).collect();
    ctx.ui.note(&format!("{name} {summary}"));
    tokio::select! {
        _ = ctx.cancel.cancelled() => interrupted(&id),
        r = execute_tool(&name, &input, &ctx) => match r {
            Ok(content) => ContentBlock::ToolResult {
                tool_use_id: id,
                content,
                is_error: false,
            },
            Err(e) => ContentBlock::ToolResult {
                tool_use_id: id,
                content: format!("{e:#}"),
                is_error: true,
            },
        },
    }
}

/// Returns an explicitly type-erased future: this is the recursion boundary
/// (execute_tool -> task -> run_turn -> dispatch_tools -> execute_tool), and
/// the `dyn Future + Send` signature is what lets rustc resolve the otherwise
/// cyclic Send inference for the recursive async call graph.
fn execute_tool<'a>(
    name: &'a str,
    input: &'a Value,
    ctx: &'a ToolCtx,
) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
    Box::pin(async move {
        match name {
            "bash" => bash_tool(input).await,
            "read_file" => read_file_tool(input).await,
            "write_file" => write_file_tool(input).await,
            "edit_file" => edit_file_tool(input).await,
            "read_offloaded" => read_offloaded_tool(input, ctx).await,
            "task" => task_tool(input, ctx).await,
            other => Err(anyhow!("unknown tool: {other}")),
        }
    })
}

fn str_arg<'a>(input: &'a Value, key: &str, tool: &str) -> Result<&'a str> {
    input[key]
        .as_str()
        .ok_or_else(|| anyhow!("{tool}: missing required string argument '{key}'"))
}

async fn bash_tool(input: &Value) -> Result<String> {
    let command = str_arg(input, "command", "bash")?;
    let timeout_ms = input["timeout_ms"].as_u64().unwrap_or(60_000);
    let output = tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        tokio::process::Command::new("sh")
            .arg("-lc")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow!("bash: command timed out after {timeout_ms}ms"))?
    .context("bash: failed to spawn sh")?;

    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        let code = output
            .status
            .code()
            .map_or_else(|| "killed by signal".into(), |c| format!("exit status {c}"));
        text.push_str(&format!("\n[{code}]"));
    }
    if text.is_empty() {
        text = "(no output)".into();
    }
    Ok(text)
}

async fn read_file_tool(input: &Value) -> Result<String> {
    let path = str_arg(input, "path", "read_file")?;
    let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = input["limit"].as_u64().unwrap_or(2000) as usize;
    let content = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read_file: cannot read {path}"))?;
    let out: Vec<String> = content
        .lines()
        .enumerate()
        .skip(offset - 1)
        .take(limit)
        .map(|(i, line)| format!("{}\t{line}", i + 1))
        .collect();
    if out.is_empty() {
        return Ok("(no lines in requested range)".into());
    }
    Ok(out.join("\n"))
}

async fn write_file_tool(input: &Value) -> Result<String> {
    let path = str_arg(input, "path", "write_file")?;
    let content = str_arg(input, "content", "write_file")?;
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("write_file: cannot create {}", parent.display()))?;
        }
    }
    tokio::fs::write(path, content)
        .await
        .with_context(|| format!("write_file: cannot write {path}"))?;
    Ok(format!("wrote {} bytes to {path}", content.len()))
}

async fn edit_file_tool(input: &Value) -> Result<String> {
    let path = str_arg(input, "path", "edit_file")?;
    let old = str_arg(input, "old_string", "edit_file")?;
    let new = str_arg(input, "new_string", "edit_file")?;
    let replace_all = input["replace_all"].as_bool().unwrap_or(false);
    if old.is_empty() {
        bail!("edit_file: old_string must not be empty");
    }
    let content = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("edit_file: cannot read {path}"))?;
    let count = content.matches(old).count();
    if count == 0 {
        bail!("edit_file: old_string not found in {path}");
    }
    if count > 1 && !replace_all {
        bail!("edit_file: old_string matches {count} times in {path}; add surrounding context to disambiguate or set replace_all");
    }
    let updated = if replace_all {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
    };
    tokio::fs::write(path, updated)
        .await
        .with_context(|| format!("edit_file: cannot write {path}"))?;
    let n = if replace_all { count } else { 1 };
    Ok(format!("edited {path} ({n} replacement(s))"))
}

async fn read_offloaded_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let id = str_arg(input, "id", "read_offloaded")?;
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("read_offloaded: invalid id (only [A-Za-z0-9-] allowed)");
    }
    let path = ctx.cfg.offload_dir.join(format!("{id}.txt"));
    tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("read_offloaded: no offloaded output with id {id}"))
}

async fn task_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    if ctx.depth >= 1 {
        bail!("task: sub-agents cannot spawn further sub-agents");
    }
    let prompt = str_arg(input, "prompt", "task")?.to_string();
    let max_rounds = input["max_rounds"]
        .as_u64()
        .map_or(SUBAGENT_MAX_ROUNDS, |n| {
            (n as usize).clamp(1, SUBAGENT_MAX_ROUNDS)
        });
    let sub_cfg = Arc::new(Config {
        max_rounds,
        ..(*ctx.cfg).clone()
    });
    let ui = ctx.ui.clone();
    let cancel = ctx.cancel.clone();
    let depth = ctx.depth + 1;
    // The sub-agent runs as its own tokio task. Besides matching the
    // semantics, this breaks the recursion cycle (execute_tool -> run_turn ->
    // dispatch_tools -> execute_tool): task_tool only holds a JoinHandle,
    // which is Send regardless of the recursive future's type.
    let handle = tokio::spawn(async move {
        let mut history = History::new(sub_cfg.offload_dir.clone());
        history.record(Message::user_text(prompt));
        run_turn(&sub_cfg, &mut history, &ui, &cancel, depth).await
    });
    let outcome = handle
        .await
        .map_err(|e| anyhow!("task: sub-agent panicked: {e}"))?;
    match outcome.reason {
        EndReason::Completed => Ok(outcome.final_text),
        EndReason::MaxRounds => Ok(format!(
            "[sub-agent stopped at its round limit]\n{}",
            outcome.final_text
        )),
        EndReason::Aborted => Err(anyhow!("task: sub-agent interrupted")),
        EndReason::Error(e) => Err(anyhow!("task: sub-agent failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kloop_provider::Provider;

    fn bash_input(cmd: &str) -> Value {
        json!({"command": cmd})
    }

    struct SilentUi;
    impl Ui for SilentUi {
        fn text_delta(&self, _: &str) {}
        fn note(&self, _: &str) {}
    }

    fn test_ctx(depth: u8, tag: &str) -> ToolCtx {
        ToolCtx {
            cfg: Arc::new(Config {
                provider: Arc::new(Provider::mock(vec![])),
                model: "mock".into(),
                system: "test".into(),
                max_rounds: 5,
                offload_dir: std::env::temp_dir().join(format!("kloop-tools-{tag}")),
                context_window: None,
                fallback_model: None,
            }),
            ui: Arc::new(SilentUi),
            cancel: CancellationToken::new(),
            depth,
        }
    }

    /// Run a single tool call through the real dispatch path and return
    /// (content, is_error).
    async fn run_tool(name: &str, input: Value, ctx: &ToolCtx) -> (String, bool) {
        let results = dispatch_tools(vec![("t".into(), name.into(), input)], ctx).await;
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = results.into_iter().next().unwrap()
        else {
            panic!("expected tool result");
        };
        (content, is_error)
    }

    fn temp_file(tag: &str, content: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("kloop-tool-{}-{tag}", std::process::id()));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[tokio::test]
    async fn bash_merges_output_and_reports_exit_status() {
        let ctx = test_ctx(0, "bash");
        let (out, is_error) = run_tool(
            "bash",
            bash_input("echo to-stdout; echo to-stderr 1>&2; exit 3"),
            &ctx,
        )
        .await;
        assert!(
            !is_error,
            "non-zero exit is reported in content, not as an error result"
        );
        assert!(out.contains("to-stdout"));
        assert!(out.contains("to-stderr"));
        assert!(out.contains("[exit status 3]"));

        let (out, _) = run_tool("bash", bash_input("true"), &ctx).await;
        assert_eq!(out, "(no output)");
    }

    #[tokio::test]
    async fn bash_times_out_and_kills_the_child() {
        let ctx = test_ctx(0, "bash-timeout");
        let started = std::time::Instant::now();
        let (out, is_error) = run_tool(
            "bash",
            json!({"command": "sleep 30", "timeout_ms": 100}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("timed out"));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
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
    async fn write_file_creates_parent_directories() {
        let dir = std::env::temp_dir().join(format!("kloop-write-{}", std::process::id()));
        let path = dir.join("deep/nested/file.txt");
        let ctx = test_ctx(0, "write");
        let (out, is_error) = run_tool(
            "write_file",
            json!({"path": path.to_str().unwrap(), "content": "created"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "created");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn edit_file_replaces_errors_and_replace_all() {
        let path = temp_file("edit", "one two two three");
        let p = path.to_str().unwrap();
        let ctx = test_ctx(0, "edit");

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

        // Missing old_string is an error.
        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": p, "old_string": "zzz", "new_string": "2"}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("not found"));

        // replace_all rewrites every occurrence.
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

    #[tokio::test]
    async fn task_is_refused_at_depth_one() {
        let ctx = test_ctx(1, "depth");
        let (out, is_error) = run_tool("task", json!({"prompt": "recurse"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("cannot spawn"));
    }

    #[test]
    fn tool_defs_expose_task_only_at_depth_zero() {
        let names = |depth| tool_defs(depth).iter().map(|t| t.name).collect::<Vec<_>>();
        assert!(names(0).contains(&"task"));
        assert!(!names(1).contains(&"task"));
    }

    #[tokio::test]
    async fn cancellation_mid_execution_interrupts_the_call() {
        let ctx = test_ctx(0, "midcancel");
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel.cancel();
        });
        let started = std::time::Instant::now();
        let (out, is_error) = run_tool("bash", bash_input("sleep 30"), &ctx).await;
        assert!(is_error);
        assert_eq!(out, "interrupted");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn dispatch_preserves_request_order_across_mixed_batches() {
        let ctx = test_ctx(0, "order");
        let results = dispatch_tools(
            vec![
                ("t1".into(), "bash".into(), bash_input("echo a")),
                ("t2".into(), "bash".into(), bash_input("true")), // unsafe
                ("t3".into(), "bash".into(), bash_input("echo c")),
            ],
            &ctx,
        )
        .await;
        let ids: Vec<&str> = results
            .iter()
            .map(|r| match r {
                ContentBlock::ToolResult { tool_use_id, .. } => tool_use_id.as_str(),
                _ => panic!(),
            })
            .collect();
        assert_eq!(ids, vec!["t1", "t2", "t3"]);
    }

    #[test]
    fn concurrency_safety_by_name_and_input() {
        assert!(is_concurrency_safe("read_file", &json!({"path": "x"})));
        assert!(is_concurrency_safe(
            "read_offloaded",
            &json!({"id": "off-0001"})
        ));
        assert!(!is_concurrency_safe(
            "write_file",
            &json!({"path": "x", "content": ""})
        ));
        assert!(!is_concurrency_safe("edit_file", &json!({})));
        assert!(!is_concurrency_safe("task", &json!({"prompt": "x"})));

        // read-only commands, incl. pipes and chains of safe segments
        assert!(is_concurrency_safe("bash", &bash_input("ls -la")));
        assert!(is_concurrency_safe(
            "bash",
            &bash_input("cat a.txt | grep foo")
        ));
        assert!(is_concurrency_safe(
            "bash",
            &bash_input("pwd && git status; wc -l f")
        ));
        assert!(is_concurrency_safe("bash", &bash_input("git log -5")));

        // unsafe: redirect, unknown command, unsafe git subcommand, empty
        assert!(!is_concurrency_safe(
            "bash",
            &bash_input("echo hi > out.txt")
        ));
        assert!(!is_concurrency_safe("bash", &bash_input("rm -rf /tmp/x")));
        assert!(!is_concurrency_safe(
            "bash",
            &bash_input("ls && make build")
        ));
        assert!(!is_concurrency_safe("bash", &bash_input("git push")));
        assert!(!is_concurrency_safe("bash", &bash_input("   ")));
        assert!(!is_concurrency_safe("bash", &json!({})));
    }

    #[tokio::test]
    async fn cancelled_dispatch_patches_every_tool_use() {
        use kloop_provider::Provider;

        struct NullUi;
        impl Ui for NullUi {
            fn text_delta(&self, _: &str) {}
            fn note(&self, _: &str) {}
        }

        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = ToolCtx {
            cfg: Arc::new(Config {
                provider: Arc::new(Provider::mock(vec![])),
                model: "mock".into(),
                system: "test".into(),
                max_rounds: 5,
                offload_dir: std::env::temp_dir().join("kloop-test-cancel"),
                context_window: None,
                fallback_model: None,
            }),
            ui: Arc::new(NullUi),
            cancel,
            depth: 0,
        };
        let results = dispatch_tools(
            vec![
                ("t1".into(), "bash".into(), bash_input("ls")),
                (
                    "t2".into(),
                    "write_file".into(),
                    json!({"path": "x", "content": "y"}),
                ),
            ],
            &ctx,
        )
        .await;
        assert_eq!(
            results,
            vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "interrupted".into(),
                    is_error: true,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "interrupted".into(),
                    is_error: true,
                },
            ]
        );
    }
}
