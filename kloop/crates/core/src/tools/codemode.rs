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
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::Result;
use serde_json::json;
use serde_json::Value;

use super::ToolCtx;
use kloop_codemode::BoxFuture;
use kloop_codemode::HostBridge;
use kloop_protocol::ContentBlock;
use kloop_protocol::ToolDef;

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
    let source = super::str_arg(input, "source", "run_program")?;
    let names = program_tool_names(&ctx.cfg.tool_sources);
    let limits = ctx.cfg.program_limits;
    let bridge = Arc::new(CoreBridge::new(ctx.clone(), limits));
    // `log()` output already streamed live to the UI as it ran; only the
    // program's return value comes back to the model — keeping a program's
    // progress narration out of the context is the whole point of code-mode.
    let out =
        kloop_codemode::run_program(source, &names, bridge, ctx.cancel.clone(), limits).await?;
    Ok(if out.is_empty() {
        "(program completed with no output)".into()
    } else {
        out
    })
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
}

impl CoreBridge {
    fn new(ctx: ToolCtx, limits: kloop_codemode::Limits) -> Self {
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
            if is_error {
                Err(content)
            } else {
                // Source (MCP) tools resolve to their structured CallToolResult;
                // built-ins keep the string contract (text as a JSON string).
                Ok(slot
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap_or(Value::String(content)))
            }
        })
    }

    fn call_agent(&self, prompt: String, opts: Value) -> BoxFuture<Result<String, String>> {
        let ctx = self.ctx.clone();
        // Claim a slot synchronously so concurrent calls get distinct counts;
        // the (max_agents+1)th is refused before it can spawn.
        let n = self.agent_count.fetch_add(1, Ordering::Relaxed);
        let max = self.max_agents;
        Box::pin(async move {
            if n >= max {
                return Err(format!(
                    "program exceeds the agent cap ({max} agent() calls); it likely fans out \
                     sub-agents without bound — narrow the work or process items in batches"
                ));
            }
            let mut task_input = json!({ "prompt": prompt });
            for key in ["agent_type", "max_rounds"] {
                if let Some(v) = opts.get(key) {
                    task_input[key] = v.clone();
                }
            }
            super::task::task_tool(&task_input, &ctx)
                .await
                .map_err(|e| format!("{e:#}"))
        })
    }

    fn log(&self, message: String) {
        // Live progress to the user, through the same note channel every
        // frontend already renders (plain line / TUI cell / server note).
        self.ctx.ui.note(&message);
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
                "source": {"type": "string", "description": "The JavaScript program to run"}
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
