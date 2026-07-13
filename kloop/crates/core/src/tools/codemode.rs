//! The `exec` tool: code-mode / CodeAct. The model writes a JavaScript program
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
use std::sync::Mutex;

use anyhow::anyhow;
use anyhow::Result;
use serde_json::json;
use serde_json::Value;

use super::ToolCtx;
use kloop_codemode::BoxFuture;
use kloop_codemode::HostBridge;
use kloop_protocol::ContentBlock;
use kloop_protocol::ToolDef;

/// Tools NOT exposed to a program: `exec` itself (no program-in-program) and
/// `task` (replaced by the `agent()` orchestration primitive).
fn is_program_callable(name: &str) -> bool {
    !matches!(name, "exec" | "task")
}

/// The tool names a program may call, taken from the depth-0 built-in set.
/// MCP tools are not exposed to programs yet (deferred).
fn program_tool_names() -> Vec<String> {
    super::tool_defs(0)
        .into_iter()
        .filter(|d| is_program_callable(&d.name))
        .map(|d| d.name)
        .collect()
}

pub(super) async fn exec_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let source = super::str_arg(input, "source", "exec")?;
    let names = program_tool_names();
    let bridge = Arc::new(CoreBridge::new(ctx.clone()));
    let result = kloop_codemode::run_program(
        source,
        &names,
        bridge.clone(),
        ctx.cancel.clone(),
        kloop_codemode::Limits::default(),
    )
    .await;
    let logs = bridge.logs.lock().unwrap().clone();
    match result {
        Ok(out) => Ok(format_output(&logs, &out)),
        // Surface logs even on failure — they show how far the program got
        // before the exception, which is exactly what the model needs to fix it.
        Err(e) => {
            let msg = format!("{e:#}");
            if logs.is_empty() {
                Err(anyhow!("{msg}"))
            } else {
                Err(anyhow!("{}\n\nprogram error: {msg}", logs.join("\n")))
            }
        }
    }
}

fn format_output(logs: &[String], result: &str) -> String {
    let mut sections = Vec::new();
    if !logs.is_empty() {
        sections.push(logs.join("\n"));
    }
    if !result.is_empty() {
        sections.push(result.to_string());
    }
    if sections.is_empty() {
        "(program completed with no output)".into()
    } else {
        sections.join("\n")
    }
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
    logs: Mutex<Vec<String>>,
}

impl CoreBridge {
    fn new(ctx: ToolCtx) -> Self {
        Self {
            ctx,
            seq: AtomicU64::new(0),
            gate: Arc::new(tokio::sync::RwLock::new(())),
            logs: Mutex::new(Vec::new()),
        }
    }
}

impl HostBridge for CoreBridge {
    fn call_tool(&self, name: String, args: Value) -> BoxFuture<Result<String, String>> {
        let safe = super::is_concurrency_safe(&name, &args, &self.ctx.cfg.tool_sources);
        let id = format!("exec-{}", self.seq.fetch_add(1, Ordering::Relaxed));
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

    fn spawn_agent(&self, prompt: String, opts: Value) -> BoxFuture<Result<String, String>> {
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
        self.logs.lock().unwrap().push(message);
    }
}

/// The `exec` tool definition. Its description carries the TypeScript API the
/// program can call, generated from `callable`'s schemas — the same trick the
/// references converge on (typed API declarations markedly improve how reliably
/// the model calls tools). Depth-0 only, like `task`.
pub(super) fn exec_def(callable: &[ToolDef]) -> ToolDef {
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

    let description = format!(
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

    ToolDef {
        name: "exec".into(),
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
