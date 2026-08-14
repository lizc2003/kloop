//! The `run_program` tool: code-mode / CodeAct. The model writes a JavaScript program
//! that orchestrates the built-in tools and sub-agents; it runs in an isolated
//! QuickJS runtime (the `kloop-codemode` crate) and every `tools.<name>(...)`
//! or `agent(...)` call routes back here through [`CoreBridge`], which re-enters
//! the same gated dispatch (`run_one` / `run_agent_tool`) a direct tool call takes —
//! hooks, permission gate and sandbox all apply per call, unchanged. Only what
//! the program returns (plus `log()` output) comes back to the model; the
//! intermediate tool results stay in program variables, off the context window.
//!
//! The engine crate stays engine-only; the seam that reaches core's private
//! gate lives here because the gate is what makes code-mode safe.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::SourceCallBinding;
use super::ToolCtx;
use super::background_executions::ExecutionKind;
use super::background_executions::ExecutionStatus;
use super::run_store::RunId;
use super::run_store::RunLease;
use super::run_store::RunNamespace;
use super::run_store::RunStore;
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunProgramInput {
    #[serde(default)]
    description: Option<String>,
    source: String,
    #[serde(default)]
    background: bool,
    #[serde(default)]
    resume_from_run_id: Option<String>,
}

const PROGRAM_MANIFEST_VERSION: u8 = 1;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProgramManifest {
    version: u8,
    run_id: String,
}

pub(super) mod journal;
use journal::Claim;
use journal::Journal;

/// Tools NOT exposed to a program: recursive runners and model-loop background
/// controls. A program orchestrates synchronously through `agent()`/`parallel()`;
/// it cannot detach or cancel sibling executions from inside itself.
fn is_program_callable(name: &str) -> bool {
    !matches!(
        name,
        "run_program"
            | "workflow"
            | "run_agent"
            | "wait_for_activity"
            | "bash_output"
            | "stop_bash"
            | "stop_agent"
            | "stop_program"
            | "stop_workflow"
            | "send_message"
            | "list_agents"
            | "task_create"
            | "task_get"
            | "task_update"
            | "task_list"
            | "task_clear"
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProgramToolEntry {
    name: String,
    source: Option<SourceCallBinding>,
    readonly: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProgramToolManifest {
    entries: Vec<ProgramToolEntry>,
}

impl ProgramToolManifest {
    fn names(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect()
    }

    fn get(&self, name: &str) -> Option<&ProgramToolEntry> {
        self.entries.iter().find(|entry| entry.name == name)
    }
}

/// Capture the exact callable source owner/generation and readonly verdict that
/// accompany one provider request's generated Program API. The agent builds the
/// API between two equal manifests, so a dynamic refresh cannot splice an old
/// schema onto a newer runtime owner.
pub(crate) fn capture_program_tool_manifest(
    sources: &[Arc<dyn super::ToolSource>],
    shell_programs: &crate::shell_programs::ShellPrograms,
) -> ProgramToolManifest {
    let mut entries: Vec<ProgramToolEntry> = super::builtin_defs(0, shell_programs)
        .into_iter()
        .filter(|definition| is_program_callable(&definition.name))
        .map(|definition| ProgramToolEntry {
            name: definition.name,
            source: None,
            readonly: false,
        })
        .collect();
    entries.extend(
        super::merged_source_defs(sources)
            .into_iter()
            .filter(|definition| is_program_callable(&definition.name))
            .filter_map(|definition| super::source_definition_snapshot(sources, &definition.name))
            .filter_map(|snapshot| {
                let source = sources.get(snapshot.binding.source_slot)?;
                Some(ProgramToolEntry {
                    readonly: source.is_readonly(&snapshot.definition.name),
                    name: snapshot.definition.name,
                    source: Some(snapshot.binding),
                })
            }),
    );
    ProgramToolManifest { entries }
}

#[cfg(test)]
fn program_tool_names(
    sources: &[Arc<dyn super::ToolSource>],
    shell_programs: &crate::shell_programs::ShellPrograms,
) -> Vec<String> {
    capture_program_tool_manifest(sources, shell_programs).names()
}

pub(super) async fn run_program_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let parsed: RunProgramInput =
        serde_json::from_value(input.clone()).context("run_program: invalid input")?;
    let explicit_description = super::optional_display_description(input, "run_program")?;
    debug_assert_eq!(
        parsed.description.as_deref(),
        explicit_description.as_deref()
    );
    let description = explicit_description.unwrap_or_else(|| program_preview(&parsed.source));
    let RunProgramInput {
        description: _,
        source,
        background,
        resume_from_run_id,
    } = parsed;
    let program_tool_manifest = ctx.program_tool_manifest.clone().unwrap_or_else(|| {
        Arc::new(capture_program_tool_manifest(
            &ctx.cfg.tool_sources,
            &ctx.cfg.shell_programs,
        ))
    });
    let names = program_tool_manifest.names();
    let limits = ctx.cfg.program_limits;
    // Each run has a run_id and an agent()-call journal. A resume passes the old
    // run_id back, reusing the journal dir so completed agent() calls are
    // skipped instead of re-spawned (and re-charged) — plan 24 journal resume.
    let store = RunStore::new(&ctx.cfg.offload_dir, RunNamespace::Program)
        .map_err(|error| anyhow!("run_program: cannot open run store: {error:#}"))?;
    let (run_id, run_dir) = match resume_from_run_id.as_deref() {
        Some(raw) => {
            let id = RunId::parse(raw).map_err(|error| anyhow!("run_program: {error:#}"))?;
            let run = store
                .open(&id)
                .map_err(|error| anyhow!("run_program: cannot resume {raw}: {error:#}"))?;
            verify_program_source(&run, &source)
                .with_context(|| format!("run_program: cannot resume {raw}"))?;
            (id, run)
        }
        None => {
            let id = RunId::parse(&new_run_id()).expect("generated run id is valid");
            let run = store
                .create(&id)
                .map_err(|error| anyhow!("run_program: cannot create run: {error:#}"))?;
            persist_program_source(&run, &source)
                .context("run_program: cannot persist source contract")?;
            (id, run)
        }
    };
    let run_id = run_id.as_str().to_string();
    let lease = run_dir
        .acquire()
        .map_err(|error| anyhow!("run_program: run is already active: {error:#}"))?;
    let journal = Arc::new(Journal::open_run(run_dir, "journal.jsonl"));

    // Fire-and-forget: spawn detached, return a program id now, reinject the
    // return value at the next round boundary (reuses the plan-26 async path).
    if background {
        return spawn_background_program(
            ctx,
            source,
            names,
            program_tool_manifest,
            limits,
            run_id,
            description,
            journal,
            lease,
        );
    }
    let _lease = lease;
    let bridge = Arc::new(CoreBridge::new(
        ctx.clone(),
        limits,
        Some(journal.clone()),
        program_tool_manifest,
    ));
    // `log()` output already streamed live to the UI as it ran; only the
    // program's return value comes back to the model — keeping a program's
    // progress narration out of the context is the whole point of code-mode.
    let outcome =
        kloop_codemode::run_program(&source, &names, bridge, ctx.cancel.clone(), limits).await;
    match outcome {
        Ok(out) => Ok(program_output(out)),
        Err(e) => Err(resume_hint(e, &journal, &run_id)),
    }
}

fn persist_program_source(run_dir: &super::run_store::RunDir, source: &str) -> Result<()> {
    run_dir.write_atomic("source.js", source.as_bytes())?;
    run_dir.write_atomic(
        "manifest.json",
        &serde_json::to_vec_pretty(&ProgramManifest {
            version: PROGRAM_MANIFEST_VERSION,
            run_id: run_dir.id().as_str().to_string(),
        })?,
    )?;
    Ok(())
}

fn verify_program_source(run_dir: &super::run_store::RunDir, source: &str) -> Result<()> {
    let manifest: ProgramManifest = serde_json::from_slice(
        &run_dir
            .read("manifest.json")
            .context("stored run has no source manifest; legacy Program runs cannot be resumed")?,
    )
    .context("stored source manifest is invalid")?;
    if manifest.version != PROGRAM_MANIFEST_VERSION {
        bail!(
            "unsupported Program manifest version {}; expected {PROGRAM_MANIFEST_VERSION}",
            manifest.version
        );
    }
    if manifest.run_id != run_dir.id().as_str() {
        bail!("stored source manifest belongs to a different run");
    }
    let stored = run_dir
        .read("source.js")
        .context("stored run has no source bytes; legacy Program runs cannot be resumed")?;
    if stored != source.as_bytes() {
        bail!(
            "source differs from the original Program run; resume requires byte-identical source"
        );
    }
    Ok(())
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
/// register in the shared background-execution registry, launch a DETACHED worker on
/// its OWN cancel token (a finished parent turn must not kill a still-running
/// program), and return immediately. When the program ends it reinjects its
/// return value into the parent's inbox. Mirrors the sub-agent background path
/// and reuses the same execution registry and autowake.
#[allow(clippy::too_many_arguments)]
fn spawn_background_program(
    ctx: &ToolCtx,
    source: String,
    names: Vec<String>,
    program_tool_manifest: Arc<ProgramToolManifest>,
    limits: kloop_codemode::Limits,
    run_id: String,
    description: String,
    journal: Arc<Journal>,
    lease: RunLease,
) -> Result<String> {
    let label = format!("program-{}", PROGRAM_SEQ.fetch_add(1, Ordering::Relaxed));
    let own_cancel = CancellationToken::new();
    ctx.cfg
        .background_executions
        .register(
            ExecutionKind::Program,
            &label,
            &description,
            own_cancel.clone(),
        )
        .map_err(|msg| anyhow!("run_program: {msg}"))?;
    let parent_inbox = ctx.cfg.inbox.clone();
    let background_executions = ctx.cfg.background_executions.clone();
    let ui = ctx.ui.clone();
    // The program's tool calls run on the program's own cancel, not the parent
    // turn's — the parent may end while the program is still going.
    let mut bg_ctx = ctx.clone();
    bg_ctx.cancel = own_cancel.clone();
    let bridge = Arc::new(CoreBridge::new(
        bg_ctx,
        limits,
        Some(journal.clone()),
        program_tool_manifest,
    ));
    super::subagent::emit_background_task(
        &ui,
        &label,
        Some(&run_id),
        BackgroundTaskKind::Program,
        &description,
        ExecutionStatus::Running,
        None,
    );

    let worker_cancel = own_cancel.clone();
    let worker = tokio::spawn(async move {
        let _lease = lease;

        kloop_codemode::run_program(&source, &names, bridge, worker_cancel, limits).await
    });
    background_executions.attach_abort(&label, worker.abort_handle());
    let supervisor_run_id = run_id.clone();
    tokio::spawn({
        let label = label.clone();
        let ui = ui.clone();
        let description = description.clone();
        async move {
            let (status, reinject) = match worker.await {
                Ok(outcome) => classify_program(outcome, &own_cancel, &journal, &supervisor_run_id),
                Err(error) if error.is_cancelled() => (ExecutionStatus::Aborted, None),
                Err(error) => (
                    ExecutionStatus::Failed,
                    Some(format!(
                        "[background program failed] program worker panicked: {error}\nYou may re-run it or try another approach."
                    )),
                ),
            };
            let terminal = background_executions.finish(&label, status, |actual, deliver| {
                if deliver {
                    if let Some(summary) = reinject {
                        parent_inbox.push(InboxItem::ProgramResult {
                            label: label.clone(),
                            run_id: supervisor_run_id.clone(),
                            summary,
                        });
                    } else {
                        parent_inbox.notify_activity();
                    }
                } else {
                    debug_assert_eq!(actual, ExecutionStatus::Aborted);
                    parent_inbox.notify_activity();
                }
            });
            if let Some(terminal) = terminal {
                super::subagent::emit_background_task(
                    &ui,
                    &label,
                    Some(&supervisor_run_id),
                    BackgroundTaskKind::Program,
                    &description,
                    terminal,
                    super::subagent::execution_status_detail(terminal),
                );
            }
        }
    });
    Ok(format!(
        "Program({description}) started in the background.\nProgram ID: {label}\nRun ID: {run_id}\nKeep working; its return value will be delivered automatically as a message when it finishes. Call wait_for_activity once only if you need to block for any activity, stop only the program-N ID with stop_program {{\"program_id\": \"{label}\"}}, or resume a failed run-* ID with run_program.resume_from_run_id and the byte-identical source."
    ))
}

/// Map a background program's terminal outcome to (registry status, optional
/// reinjection). Success reinjects the return value; a failure reinjects a
/// framed, truncated error; a program stopped via `stop_program` reinjects
/// nothing because the caller already knows it was stopped.
fn classify_program(
    outcome: Result<String>,
    own_cancel: &CancellationToken,
    journal: &Journal,
    run_id: &str,
) -> (ExecutionStatus, Option<String>) {
    match outcome {
        Ok(out) => (ExecutionStatus::Completed, Some(program_output(out))),
        Err(_) if own_cancel.is_cancelled() => (ExecutionStatus::Aborted, None),
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
            (ExecutionStatus::Failed, Some(msg))
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
    // Exact callable names, source owner/generation and readonly verdict captured
    // with the provider request that exposed this Program API.
    program_tools: Arc<ProgramToolManifest>,
    // Total agent() calls so far and the hard ceiling; the (max_agents+1)th is
    // refused. A separate semaphore below paces finite concurrent fan-out.
    agent_count: AtomicU64,
    max_agents: u64,
    // Per-run live child bound. Unlike max_agents this paces, rather than
    // rejects, a finite fan-out; waiting observes the run cancellation token.
    agent_slots: Arc<tokio::sync::Semaphore>,
    // agent() call journal for resume (plan 24): a hit returns the cached result
    // and skips the spawn (and the cap charge). None when resume is off.
    journal: Option<Arc<Journal>>,
}

impl CoreBridge {
    fn new(
        ctx: ToolCtx,
        limits: kloop_codemode::Limits,
        journal: Option<Arc<Journal>>,
        program_tools: Arc<ProgramToolManifest>,
    ) -> Self {
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
            program_tools,
            agent_count: AtomicU64::new(0),
            max_agents: limits.max_agents,
            agent_slots: Arc::new(tokio::sync::Semaphore::new(limits.max_concurrency)),
            journal,
        }
    }
}

impl HostBridge for CoreBridge {
    fn call_tool(&self, name: String, args: Value) -> BoxFuture<Result<Value, String>> {
        let Some(tool) = self.program_tools.get(&name).cloned() else {
            return Box::pin(async move {
                Err(format!(
                    "program tool '{name}' is not in this run's callable catalog"
                ))
            });
        };
        if name == "bash" && args.get("background").and_then(Value::as_bool) == Some(true) {
            return Box::pin(async {
                Err("program bash calls must stay foreground; background resources are controlled by the parent session".into())
            });
        }
        let safe = match tool.source {
            Some(_) => tool.readonly,
            None => super::is_concurrency_safe(&name, &args, &[]),
        };
        let expected_source = tool.source;
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
            let block = super::run_one(id, name, args, ctx, expected_source).await;
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
        call_id: String,
        prompt: String,
        opts: Value,
    ) -> BoxFuture<Result<Value, String>> {
        let ctx = self.ctx.clone();
        // Journal replay is a synchronous decision keyed by a stable topology
        // identity plus the complete structured input. A hit skips both spawn and
        // cap charge.
        let journal = self.journal.clone();
        let claim = journal
            .as_ref()
            .map(|journal| journal.claim(&call_id, &prompt, &opts));
        let hit = matches!(claim, Some(Claim::Hit(_)));
        // Only a live (missed) call claims a cap slot; concurrent misses get
        // distinct counts from the sync fetch_add.
        let n = if hit {
            0
        } else {
            self.agent_count.fetch_add(1, Ordering::Relaxed)
        };
        let max = self.max_agents;
        let agent_slots = self.agent_slots.clone();
        let cancel = ctx.cancel.clone();
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
            let _permit = tokio::select! {
                permit = agent_slots.acquire_owned() => permit.map_err(|_| {
                    "program agent concurrency limiter closed unexpectedly".to_string()
                })?,
                _ = cancel.cancelled() => {
                    return Err("program interrupted while waiting for an agent slot".into());
                }
            };
            let mut agent_input = json!({ "prompt": prompt.clone() });
            for k in ["agent_type", "max_rounds"] {
                if let Some(v) = opts.get(k) {
                    agent_input[k] = v.clone();
                }
            }
            let workspace = ctx.cfg.effective_workspace();
            let result = super::subagent::run_agent_tool(&agent_input, &ctx, &workspace)
                .await
                .map(Value::String)
                .map_err(|e| format!("{e:#}"));
            // Record only a successful live call so a resume can skip it.
            if let (Ok(out), Some(journal)) = (&result, &journal) {
                journal.record(call_id, prompt, opts, out.clone());
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
/// the model calls tools). Depth-0 only, like `run_agent`.
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
    let has_source_tools = sources
        .iter()
        .chain(deferred)
        .any(|d| is_program_callable(&d.name));
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
        let (description, schema) = if def.name == "bash" {
            let mut schema = def.schema.clone();
            if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
                properties.remove("background");
            }
            (
                "Run one foreground shell command; Program cannot detach shell resources".into(),
                schema,
            )
        } else {
            (one_line(&def.description), def.schema.clone())
        };
        decls.push_str(&format!("  /** {description} */\n"));
        decls.push_str(&format!(
            "  {}(args: {}): Promise<{}>;\n",
            def.name,
            ts_type(&schema),
            builtin_output_type(&def.name)
        ));
    }
    for def in sources.iter().filter(|d| is_program_callable(&d.name)) {
        decls.push_str(&format!("  /** {} */\n", one_line(&def.description)));
        decls.push_str(&format!(
            "  {}(args: {}): Promise<CallToolResult>;\n",
            def.name,
            ts_type(&def.schema)
        ));
    }
    decls.push_str("};\n");
    decls.push_str(
        "type AgentOptions = { agent_type?: string; max_rounds?: number };\n\
         type OrchestrationScope = {\n\
           agent(prompt: string, opts?: AgentOptions): Promise<string>;\n\
           parallel<T>(thunks: Array<(scope: OrchestrationScope) => Promise<T> | T>): Promise<Array<T | null>>;\n\
           pipeline(items: any[], ...stages: Array<(prev: any, item: any, index: number, scope: OrchestrationScope) => any>): Promise<any[]>;\n\
         };\n\
         declare function agent(prompt: string, opts?: AgentOptions): Promise<string>;\n",
    );
    decls.push_str("declare function log(msg: unknown): void;\n");
    decls.push_str(
        "declare function parallel<T>(thunks: Array<(scope: OrchestrationScope) => Promise<T> | T>): Promise<Array<T | null>>;\n",
    );
    decls.push_str(
        "declare function pipeline(items: any[], ...stages: Array<(prev: any, item: any, index: number, scope: OrchestrationScope) => any>): Promise<any[]>;\n",
    );

    let mut description = format!(
        "Run a fixed JavaScript tool-orchestration program. Use Program for code-controlled loops, \
batches, filters, and pipelines whose control flow is known; use run_agent instead for an \
open-ended investigation. Writing one Program keeps intermediate results in program variables \
instead of flooding the context with one tool_result per step; only what you `return` (plus any \
`log(...)`) comes back. Program is not Workflow: it cannot launch or stop background resources, \
and it cannot bypass the top-level Workflow capability gate.\n\n\
The program body runs as an async function, so top-level `await` and `return` work. Each \
`tools.<name>(...)` and `agent(...)` returns a Promise and goes through the same hooks, permission, \
sandbox, and workspace gates as a direct call. Use `Promise.all` for independent tool calls. For \
concurrent agents, use `parallel([(scope) => scope.agent(...)])` or the fourth `scope` argument of \
a `pipeline` stage; global `agent`/`parallel`/`pipeline` inside a concurrent helper callback is \
rejected because it has no stable resume identity. Nested helpers use `scope.parallel` / \
`scope.pipeline`. Pipeline items advance independently with no stage barrier. Live agent calls are \
bounded and excess calls queue, while the total-call and helper-item caps still fail explicitly. \
There is no filesystem, network, module import, or console — the public API below is the only way \
to reach outside.\n\n\
Return your final result (a string, or an object which will be JSON-stringified).\n\n\
Set `background: true` to run the program detached: the launch response contains a transient \
program-N ID for stop/lifecycle and a durable run-* ID for resume. Optional `description` is \
display-only and falls back to a source preview. Its return value is delivered automatically as a \
later message. Call `wait_for_activity` once only when you truly need to block for any activity; \
never use it as a status/output polling loop. Stop only the program-N ID with `stop_program`. Use \
this for long fan-outs/migrations; omit it for a normal synchronous run.\n\n\
If a program fails after successful agent calls, call run_program again with the byte-identical \
source and its run-* `resume_from_run_id`. Journal v2 reuses only calls whose stable topology ID \
and complete structured input match. This is best-effort model-call memoization, not deterministic \
agent text, workspace-state validation, or exactly-once external side effects.\n\n\
Available API (TypeScript):\n```ts\n{decls}```"
    );

    if deferred.iter().any(|d| is_program_callable(&d.name)) {
        description.push_str(
            "\n\nThese additional tools are also callable on `tools` by name but are not typed \
above (there are too many to declare in full). Call them directly as `tools.<name>(args)`; each \
returns a `Promise<CallToolResult>`. You cannot call tool_search from inside a program — if you \
need a tool's exact argument schema, call tool_search for it in a normal turn first, then write \
the program:\n",
        );
        for def in deferred.iter().filter(|d| is_program_callable(&d.name)) {
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
                "description": {"type": "string", "minLength": 1, "maxLength": super::MAX_DISPLAY_DESCRIPTION_CHARS, "description": "Optional short, single-line display label. It never changes source identity, journal replay, or the result."},
                "source": {"type": "string", "description": "The JavaScript program to run"},
                "background": {"type": "boolean", "description": "Run detached: return a transient program-N stop ID plus a durable run-* resume ID immediately, then deliver the return value later (default false). Wait with wait_for_activity; stop only with stop_program(program-N)."},
                "resume_from_run_id": {"type": ["string", "null"], "pattern": "^run-[A-Za-z0-9_-]+$", "description": "Resume a failed run-* ID with the byte-identical source; only matching journal-v2 agent calls are reused."}
            },
            "required": ["source"],
            "additionalProperties": false
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
