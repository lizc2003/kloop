//! The `run_program` tool: code-mode / CodeAct. The model writes a JavaScript program
//! that orchestrates the built-in tools and sub-agents; it runs in an isolated
//! QuickJS runtime (the `kloop-codemode` crate) and every `tools.<name>(...)`
//! or `agent(...)` call routes back here through [`CoreBridge`], which re-enters
//! the same gated dispatch (`run_one` / `task_tool`) a direct tool call takes —
//! hooks, permission gate and sandbox all apply per call, unchanged. Only what
//! the program returns (plus `log()` output) comes back to the model; the
//! intermediate tool results stay in program variables, off the context window.
//!
//! The engine crate stays engine-only; the seam that reaches core's private
//! gate lives here because the gate is what makes code-mode safe.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::anyhow;
use anyhow::Result;
use serde_json::json;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::background_tasks::TaskStatus;
use super::ToolCtx;
use crate::event::BackgroundTaskKind;
use crate::inbox::InboxItem;
use kloop_codemode::BoxFuture;
use kloop_codemode::HostBridge;
use kloop_protocol::ContentBlock;
use kloop_protocol::ToolDef;

/// Cap on a background program's reinjected error text, matching the sub-agent
/// error cap (~900 tokens). A successful return value is passed through; only a
/// failure is truncated so its noise can't crowd the parent's context.
const MAX_PROGRAM_ERROR_CHARS: usize = 3600;

/// Process-global so parallel background spawns never collide on a label — same
/// reasoning as the offload/agent counters.
static PROGRAM_SEQ: AtomicUsize = AtomicUsize::new(1);

mod journal;
use journal::agent_call_key;
use journal::Claim;
use journal::Journal;

/// Tools NOT exposed to a program: `run_program` itself (no program-in-program),
/// `task` (replaced by the `agent()` orchestration primitive), and the
/// background-dispatch tools `wait`/`stop_agent` (a program orchestrates
/// synchronously via `agent()`/`parallel()`; fire-and-forget is a model-loop
/// concept with no meaning inside one program run).
fn is_program_callable(name: &str) -> bool {
    !matches!(name, "run_program" | "task" | "wait" | "stop_agent")
}

/// The tool names a program may call: the depth-0 built-ins plus every external
/// source (MCP) tool, deduplicated. Source tools are always included — deferred
/// or not — so a `tools.<name>()` call never lands on a missing method; deferral
/// only trims what the `run_program` *description* declares in full, never what
/// the runtime exposes (a program call bypasses the deferred-tool lock gate).
fn program_tool_names(sources: &[Arc<dyn super::ToolSource>]) -> Vec<String> {
    let mut names: Vec<String> = super::builtin_defs(0)
        .into_iter()
        .filter(|d| is_program_callable(&d.name))
        .map(|d| d.name)
        .collect();
    names.extend(
        super::merged_source_defs(sources)
            .into_iter()
            .map(|d| d.name),
    );
    names
}

pub(super) async fn run_program_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let source = super::str_arg(input, "source", "run_program")?.to_string();
    let names = program_tool_names(&ctx.cfg.tool_sources);
    let limits = ctx.cfg.program_limits;
    // Each run has a run_id and an agent()-call journal. A resume passes the old
    // run_id back, reusing the journal dir so completed agent() calls are
    // skipped instead of re-spawned (and re-charged) — plan 24 journal resume.
    let run_id = input["resume_from_run_id"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(new_run_id);
    let journal = Arc::new(Journal::open(journal_path(&ctx.cfg.offload_dir, &run_id)));

    // Fire-and-forget: spawn detached, return a program id now, reinject the
    // return value at the next round boundary (reuses the plan-26 async path).
    if input["background"].as_bool().unwrap_or(false) {
        return spawn_background_program(ctx, source, names, limits, run_id, journal);
    }
    let bridge = Arc::new(CoreBridge::new(ctx.clone(), limits, Some(journal.clone())));
    // `log()` output already streamed live to the UI as it ran; only the
    // program's return value comes back to the model — keeping a program's
    // progress narration out of the context is the whole point of code-mode.
    match kloop_codemode::run_program(&source, &names, bridge, ctx.cancel.clone(), limits).await {
        Ok(out) => Ok(program_output(out)),
        Err(e) => Err(resume_hint(e, &journal, &run_id)),
    }
}

/// Process-global run counter; combined with a wall-clock second it makes a
/// run_id unique within a process and (near-certainly) across processes. No
/// rand/Date dependency.
static PROGRAM_RUN_SEQ: AtomicUsize = AtomicUsize::new(1);

fn new_run_id() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!(
        "run-{secs}-{}",
        PROGRAM_RUN_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// `.kloop/program-runs/<run_id>/journal.jsonl` — a sibling of the offload dir
/// (so it lives under `.kloop/` without a new Config field). Created lazily on
/// the first journaled agent() call.
fn journal_path(offload_dir: &std::path::Path, run_id: &str) -> std::path::PathBuf {
    offload_dir
        .parent()
        .unwrap_or(offload_dir)
        .join("program-runs")
        .join(run_id)
        .join("journal.jsonl")
}

/// Append resume guidance to a program failure, but only if at least one
/// agent() call was journaled — otherwise there is nothing to skip on resume.
fn resume_hint(e: anyhow::Error, journal: &Journal, run_id: &str) -> anyhow::Error {
    if journal.is_active() {
        anyhow!(
            "{e:#}\n[This program journaled its completed agent() calls. To resume without \
             re-running them, call run_program again with the same source and \
             resume_from_run_id: \"{run_id}\".]"
        )
    } else {
        e
    }
}

fn program_output(out: String) -> String {
    if out.is_empty() {
        "(program completed with no output)".into()
    } else {
        out
    }
}

/// First line of the source, truncated — the label a UI/registry shows for a
/// background program while it runs.
fn program_preview(source: &str) -> String {
    let line = source.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut preview: String = line.trim().chars().take(80).collect();
    if preview.len() < line.trim().len() {
        preview.push('…');
    }
    preview
}

/// Fire-and-forget program spawn (plan 24, `run_program {"background": true}`):
/// register in the shared async-task registry, launch a DETACHED tokio task on
/// its OWN cancel token (a finished parent turn must not kill a still-running
/// program), and return immediately. When the program ends it reinjects its
/// return value into the parent's inbox. Mirrors the sub-agent background path
/// (plan 26) and reuses the same registry, `wait`, `stop_agent` and autowake.
fn spawn_background_program(
    ctx: &ToolCtx,
    source: String,
    names: Vec<String>,
    limits: kloop_codemode::Limits,
    run_id: String,
    journal: Arc<Journal>,
) -> Result<String> {
    let label = format!("program-{}", PROGRAM_SEQ.fetch_add(1, Ordering::Relaxed));
    let own_cancel = CancellationToken::new();
    let preview = program_preview(&source);
    ctx.cfg
        .background_tasks
        .register(&label, &preview, own_cancel.clone())
        .map_err(|msg| anyhow!("run_program: {msg}"))?;
    let parent_inbox = ctx.cfg.inbox.clone();
    let background_tasks = ctx.cfg.background_tasks.clone();
    let ui = ctx.ui.clone();
    // The program's tool calls run on the program's own cancel, not the parent
    // turn's — the parent may end while the program is still going.
    let mut bg_ctx = ctx.clone();
    bg_ctx.cancel = own_cancel.clone();
    let bridge = Arc::new(CoreBridge::new(bg_ctx, limits, Some(journal.clone())));
    super::task::emit_background_task(
        &ui,
        &label,
        BackgroundTaskKind::Program,
        &preview,
        TaskStatus::Running,
        None,
    );

    let worker_cancel = own_cancel.clone();
    let worker = tokio::spawn(async move {
        kloop_codemode::run_program(&source, &names, bridge, worker_cancel, limits).await
    });
    background_tasks.attach_abort(&label, worker.abort_handle());
    tokio::spawn({
        let label = label.clone();
        let ui = ui.clone();
        let preview = preview.clone();
        async move {
            let (status, reinject) = match worker.await {
                Ok(outcome) => classify_program(outcome, &own_cancel, &journal, &run_id),
                Err(error) if error.is_cancelled() => (TaskStatus::Aborted, None),
                Err(error) => (
                    TaskStatus::Failed,
                    Some(format!(
                        "[background program failed] program task panicked: {error}\nYou may re-run it or try another approach."
                    )),
                ),
            };
            let terminal = background_tasks.finish(&label, status, |actual, deliver| {
                if deliver {
                    if let Some(summary) = reinject {
                        parent_inbox.push(InboxItem::ProgramResult {
                            label: label.clone(),
                            summary,
                        });
                    } else {
                        parent_inbox.notify_activity();
                    }
                } else {
                    debug_assert_eq!(actual, TaskStatus::Aborted);
                    parent_inbox.notify_activity();
                }
            });
            if let Some(terminal) = terminal {
                super::task::emit_background_task(
                    &ui,
                    &label,
                    BackgroundTaskKind::Program,
                    &preview,
                    terminal,
                    super::task::task_status_detail(terminal),
                );
            }
        }
    });
    Ok(format!(
        "Program {label} started in the background. Keep working; its return value will be \
         delivered to you as a message when it finishes. Block for it with the wait tool, or \
         stop it with stop_agent."
    ))
}

/// Map a background program's terminal outcome to (registry status, optional
/// reinjection). Success reinjects the return value; a failure reinjects a
/// framed, truncated error; a program stopped via `stop_agent` (its own cancel
/// fired) reinjects nothing — the model that stopped it already knows (codex's
/// is_final).
fn classify_program(
    outcome: Result<String>,
    own_cancel: &CancellationToken,
    journal: &Journal,
    run_id: &str,
) -> (TaskStatus, Option<String>) {
    match outcome {
        Ok(out) => (TaskStatus::Completed, Some(program_output(out))),
        Err(_) if own_cancel.is_cancelled() => (TaskStatus::Aborted, None),
        Err(e) => {
            let mut msg = format!(
                "[background program failed] {}",
                truncate_program_error(&format!("{e:#}"))
            );
            // Resumable iff it journaled completed agent() calls.
            if journal.is_active() {
                msg.push_str(&format!(
                    "\nTo resume without re-running completed agent() calls, run_program again \
                     with the same source and resume_from_run_id: \"{run_id}\"."
                ));
            } else {
                msg.push_str("\nYou may re-run it or try another approach.");
            }
            (TaskStatus::Failed, Some(msg))
        }
    }
}

fn truncate_program_error(e: &str) -> String {
    if e.chars().count() <= MAX_PROGRAM_ERROR_CHARS {
        return e.to_string();
    }
    let truncated: String = e.chars().take(MAX_PROGRAM_ERROR_CHARS).collect();
    format!("{truncated}… (error truncated)")
}

/// The bridge core hands the engine: it owns a [`ToolCtx`] clone and turns each
/// program-side call back into a fully gated dispatch.
struct CoreBridge {
    ctx: ToolCtx,
    // Synthetic tool_use ids for the UI (each op call shows as a tool line).
    seq: AtomicU64,
    // Mirrors dispatch's concurrency rule for calls a program fires with
    // Promise.all: read-only calls share the read lock (run concurrently),
    // writes take the write lock (serialized) so a program can't race two
    // edits to the same file past the ordering a normal round would enforce.
    gate: Arc<tokio::sync::RwLock<()>>,
    // Total agent() calls so far and the ceiling; the (max_agents+1)th is
    // refused — the runaway guard against unbounded sub-agent fan-out. Note we
    // cap the TOTAL, not the concurrency: a program firing N concurrent agent()
    // is the same as a model emitting N concurrent `task` calls, which kloop
    // already runs uncapped (join_all) — so pacing concurrency here would break
    // that precedent. The hard total ceiling is the guard that matters.
    agent_count: AtomicU64,
    max_agents: u64,
    // agent() call journal for resume (plan 24): a hit returns the cached result
    // and skips the spawn (and the cap charge). None when resume is off.
    journal: Option<Arc<Journal>>,
}

impl CoreBridge {
    fn new(ctx: ToolCtx, limits: kloop_codemode::Limits, journal: Option<Arc<Journal>>) -> Self {
        // Calls a program fires are "from a program": they skip the deferred-tool
        // lock gate, since the tool is already exposed on the program's `tools`
        // object. All other gates (deny, permission, sandbox, hooks) still apply.
        let ctx = ToolCtx {
            from_program: true,
            ..ctx
        };
        Self {
            ctx,
            seq: AtomicU64::new(0),
            gate: Arc::new(tokio::sync::RwLock::new(())),
            agent_count: AtomicU64::new(0),
            max_agents: limits.max_agents,
            journal,
        }
    }
}

impl HostBridge for CoreBridge {
    fn call_tool(&self, name: String, args: Value) -> BoxFuture<Result<Value, String>> {
        let safe = super::is_concurrency_safe(&name, &args, &self.ctx.cfg.tool_sources);
        let id = format!("run_program-{}", self.seq.fetch_add(1, Ordering::Relaxed));
        // Fresh per-call sink: execute_tool drops a source tool's structured
        // CallToolResult here, so the program receives the object rather than
        // the flattened text. One slot per call → concurrent calls never race.
        let slot = Arc::new(std::sync::Mutex::new(None));
        let mut ctx = self.ctx.clone();
        ctx.program_result = Some(slot.clone());
        let gate = self.gate.clone();
        Box::pin(async move {
            let _guard: Box<dyn std::any::Any + Send> = if safe {
                Box::new(gate.read_owned().await)
            } else {
                Box::new(gate.write_owned().await)
            };
            let block = super::run_one(id, name, args, ctx).await;
            let ContentBlock::ToolResult {
                content, is_error, ..
            } = block
            else {
                unreachable!("run_one always builds a tool_result")
            };
            // A program orchestrates — it cannot "see" an image, so a block
            // result (read_file on an image) flattens to its `[image: …]` text.
            let text = content.as_text().into_owned();
            if is_error {
                Err(text)
            } else {
                // Source (MCP) tools resolve to their structured CallToolResult;
                // built-ins keep the string contract (text as a JSON string).
                Ok(slot.lock().unwrap().take().unwrap_or(Value::String(text)))
            }
        })
    }

    fn call_agent(
        &self,
        seq: u32,
        prompt: String,
        opts: Value,
    ) -> BoxFuture<Result<String, String>> {
        let ctx = self.ctx.clone();
        // Journal replay is a synchronous, deterministic decision (seq comes
        // from JS): a hit reuses a prior run's result and skips the spawn — and
        // the cap charge, since a replayed call already ran last time.
        let key = agent_call_key(&prompt, &opts);
        let journal = self.journal.clone();
        let claim = journal.as_ref().map(|j| j.claim(seq, &key));
        let hit = matches!(claim, Some(Claim::Hit(_)));
        // Only a live (missed) call claims a cap slot; concurrent misses get
        // distinct counts from the sync fetch_add.
        let n = if hit {
            0
        } else {
            self.agent_count.fetch_add(1, Ordering::Relaxed)
        };
        let max = self.max_agents;
        Box::pin(async move {
            if let Some(Claim::Hit(cached)) = claim {
                return Ok(cached);
            }
            if n >= max {
                return Err(format!(
                    "program exceeds the agent cap ({max} agent() calls); it likely fans out \
                     sub-agents without bound — narrow the work or process items in batches"
                ));
            }
            let mut task_input = json!({ "prompt": prompt });
            for k in ["agent_type", "max_rounds"] {
                if let Some(v) = opts.get(k) {
                    task_input[k] = v.clone();
                }
            }
            let result = super::task::task_tool(&task_input, &ctx)
                .await
                .map_err(|e| format!("{e:#}"));
            // Record only a successful live call so a resume can skip it.
            if let (Ok(out), Some(j)) = (&result, &journal) {
                j.record(seq, key, out.clone());
            }
            result
        })
    }

    fn log(&self, message: String) {
        // Live progress to the user, through the same note channel every
        // frontend already renders (plain line / TUI cell / server note).
        self.ctx.ui.emit(&crate::event::Event::Note(message));
    }
}

/// The `run_program` tool definition. Its description carries the TypeScript API the
/// program can call, generated from `callable`'s schemas — the same trick the
/// references converge on (typed API declarations markedly improve how reliably
/// the model calls tools). Depth-0 only, like `task`.
///
/// `builtins` are declared returning `Promise<string>`. `sources` are inline
/// external (MCP) tools, declared returning `Promise<CallToolResult>` (a program
/// gets the structured result object, not flat text). `deferred` are source
/// tools held behind the defer threshold: too many to type in full, so they get
/// a compact name + description manifest instead — still callable at runtime and
/// still returning a `CallToolResult`, just without a declared signature (the
/// model can `tool_search` one in a normal turn to see its schema first).
pub(super) fn run_program_def(
    builtins: &[ToolDef],
    sources: &[ToolDef],
    deferred: &[ToolDef],
) -> ToolDef {
    let has_source_tools = !sources.is_empty() || !deferred.is_empty();
    let mut decls = String::new();
    if has_source_tools {
        // Structured result an MCP tool resolves to (a subset of the MCP spec's
        // CallToolResult; isError surfaces as a thrown exception, not here).
        decls.push_str(
            "type CallToolResult<T = unknown> = { content: Array<{ type: string; text?: string; [k: string]: unknown }>; structuredContent?: T; [k: string]: unknown };\n",
        );
    }
    decls.push_str("declare const tools: {\n");
    for def in builtins.iter().filter(|d| is_program_callable(&d.name)) {
        decls.push_str(&format!("  /** {} */\n", one_line(&def.description)));
        decls.push_str(&format!(
            "  {}(args: {}): Promise<{}>;\n",
            def.name,
            ts_type(&def.schema),
            builtin_output_type(&def.name)
        ));
    }
    for def in sources {
        decls.push_str(&format!("  /** {} */\n", one_line(&def.description)));
        decls.push_str(&format!(
            "  {}(args: {}): Promise<CallToolResult>;\n",
            def.name,
            ts_type(&def.schema)
        ));
    }
    decls.push_str("};\n");
    decls.push_str(
        "declare function agent(prompt: string, opts?: { agent_type?: string; max_rounds?: number }): Promise<string>;\n",
    );
    decls.push_str("declare function log(msg: unknown): void;\n");
    decls.push_str(
        "declare function parallel<T>(thunks: Array<() => Promise<T>>): Promise<Array<T | null>>;\n",
    );
    decls.push_str(
        "declare function pipeline(items: any[], ...stages: Array<(prev: any, item: any, index: number) => any>): Promise<any[]>;\n",
    );

    let mut description = format!(
        "Run a JavaScript program that orchestrates tools instead of calling them one at a time. \
Use this when a task is a loop, a fan-out, a pipeline, or a filter over many items — writing it \
as one program keeps intermediate results in program variables instead of flooding the context \
with one tool_result per step; only what you `return` (plus any `log(...)`) comes back.\n\n\
The program body runs as an async function, so top-level `await` and `return` work. Each \
`tools.<name>(...)` and `agent(...)` returns a Promise and goes through the exact same permission \
and sandbox checks as a direct tool call. Run independent calls concurrently with `Promise.all` or \
`parallel([...])`. There is no filesystem, network, module import, or console — the tools and \
`agent()` are the only way to reach outside.\n\n\
Return your final result (a string, or an object which will be JSON-stringified).\n\n\
Set `background: true` to run the program detached: you get a program id back \
immediately and its return value is delivered to you as a message when it \
finishes (block for it with the `wait` tool, or cancel it with `stop_agent`). \
Use this for long fan-outs/migrations so they don't hold up the turn; omit it \
for a normal synchronous run.\n\n\
If a program fails partway through a long agent() fan-out, it reports a \
run_id; call run_program again with the SAME source and `resume_from_run_id` \
set to it to skip the agent() calls that already completed (their results are \
replayed from a journal) and only re-run the rest.\n\n\
Available API (TypeScript):\n```ts\n{decls}```"
    );

    if !deferred.is_empty() {
        description.push_str(
            "\n\nThese additional tools are also callable on `tools` by name but are not typed \
above (there are too many to declare in full). Call them directly as `tools.<name>(args)`; each \
returns a `Promise<CallToolResult>`. You cannot call tool_search from inside a program — if you \
need a tool's exact argument schema, call tool_search for it in a normal turn first, then write \
the program:\n",
        );
        for def in deferred {
            description.push_str(&format!(
                "- tools.{}: {}\n",
                def.name,
                one_line(&def.description)
            ));
        }
    }

    ToolDef {
        name: "run_program".into(),
        description,
        schema: json!({
            "type": "object",
            "properties": {
                "source": {"type": "string", "description": "The JavaScript program to run"},
                "background": {"type": "boolean", "description": "Run detached: returns a program id immediately and delivers the return value as a message when it finishes (block with wait, cancel with stop_agent). Omit for a synchronous run."},
                "resume_from_run_id": {"type": "string", "description": "Resume a failed run: pass the run_id it reported (with the same source) to skip agent() calls that already completed."}
            },
            "required": ["source"]
        }),
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A built-in's TypeScript return type in a program. Almost all resolve to their
/// text as a string; `glob` hands back its path list as an array (the one
/// built-in whose result is naturally a list — it stashes a `string[]` into the
/// program-result sink, so the declaration must match).
fn builtin_output_type(name: &str) -> &'static str {
    match name {
        "glob" => "string[]",
        _ => "string",
    }
}

/// Minimal JSON-Schema → TypeScript type. Covers the shapes the built-in tool
/// schemas actually use (object/array/string+enum/number/boolean); anything
/// unrecognized degrades to `unknown` rather than lying about the type.
fn ts_type(schema: &Value) -> String {
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => match schema.get("enum").and_then(Value::as_array) {
            Some(variants) => variants
                .iter()
                .filter_map(Value::as_str)
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(" | "),
            None => "string".into(),
        },
        Some("integer") | Some("number") => "number".into(),
        Some("boolean") => "boolean".into(),
        Some("array") => {
            let item = schema
                .get("items")
                .map_or_else(|| "unknown".into(), ts_type);
            format!("Array<{item}>")
        }
        Some("object") => ts_object(schema),
        _ => "unknown".into(),
    }
}

fn ts_object(schema: &Value) -> String {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return "Record<string, unknown>".into();
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let fields: Vec<String> = props
        .iter()
        .map(|(name, sub)| {
            let opt = if required.contains(&name.as_str()) {
                ""
            } else {
                "?"
            };
            format!("{name}{opt}: {}", ts_type(sub))
        })
        .collect();
    format!("{{ {} }}", fields.join("; "))
}

#[cfg(test)]
mod tests;
