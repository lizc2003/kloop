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

/// Tools NOT exposed to a program: `run_program` itself (no program-in-program)
/// and `task` (replaced by the `agent()` orchestration primitive).
fn is_program_callable(name: &str) -> bool {
    !matches!(name, "run_program" | "task")
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
    let bridge = Arc::new(CoreBridge::new(ctx.clone()));
    // `log()` output already streamed live to the UI as it ran; only the
    // program's return value comes back to the model — keeping a program's
    // progress narration out of the context is the whole point of code-mode.
    let out = kloop_codemode::run_program(
        source,
        &names,
        bridge,
        ctx.cancel.clone(),
        kloop_codemode::Limits::default(),
    )
    .await?;
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
}

impl CoreBridge {
    fn new(ctx: ToolCtx) -> Self {
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
        }
    }
}

impl HostBridge for CoreBridge {
    fn call_tool(&self, name: String, args: Value) -> BoxFuture<Result<String, String>> {
        let safe = super::is_concurrency_safe(&name, &args, &self.ctx.cfg.tool_sources);
        let id = format!("run_program-{}", self.seq.fetch_add(1, Ordering::Relaxed));
        let ctx = self.ctx.clone();
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
                Ok(content)
            }
        })
    }

    fn call_agent(&self, prompt: String, opts: Value) -> BoxFuture<Result<String, String>> {
        let ctx = self.ctx.clone();
        Box::pin(async move {
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
/// `callable` is the set declared with a full typed signature (built-ins, plus
/// external source tools when they are inline). `deferred` is the source tools
/// held behind the defer threshold: too many to type in full, so they get a
/// compact name + description manifest instead — still callable at runtime, just
/// without a declared signature (the model can `tool_search` one in a normal
/// turn to see its schema before writing the program).
pub(super) fn run_program_def(callable: &[ToolDef], deferred: &[ToolDef]) -> ToolDef {
    let mut decls = String::from("declare const tools: {\n");
    for def in callable.iter().filter(|d| is_program_callable(&d.name)) {
        decls.push_str(&format!("  /** {} */\n", one_line(&def.description)));
        decls.push_str(&format!(
            "  {}(args: {}): Promise<string>;\n",
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
above (there are too many to declare in full). Call them directly as `tools.<name>(args)` — you \
cannot call tool_search from inside a program. If you need a tool's exact argument schema, call \
tool_search for it in a normal turn first, then write the program:\n",
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
