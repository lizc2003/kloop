//! kloop-codemode — the QuickJS engine for code-mode (CodeAct).
//!
//! The model writes a JavaScript program that orchestrates tools and
//! sub-agents; this crate runs it in an isolated QuickJS runtime and calls
//! back into the host (kloop core) through the [`HostBridge`] seam for every
//! side effect. The program itself is a **pure orchestrator**: the engine
//! installs no filesystem, network, process or console — the only way a
//! program touches the outside world is `tools.<name>(...)` and `agent(...)`,
//! and both route through the bridge, which re-enters core's permission/hook/
//! sandbox gate for each call. Intermediate results stay in program variables;
//! only what the program returns (plus `log()` output) flows back to the model.
//!
//! A fresh [`rquickjs::AsyncRuntime`] is built and dropped per program, so
//! there is no cross-program state and QuickJS returns all its memory on drop
//! (unlike V8, which keeps process-global engine state alive).

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::anyhow;
use anyhow::Result;
use rquickjs::prelude::Async;
use rquickjs::AsyncContext;
use rquickjs::AsyncRuntime;
use rquickjs::CatchResultExt;
use rquickjs::Function;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// The host (kloop core) behind the JS program. Every side effect a program
/// can perform routes through one of these — the engine grants no other
/// capability. `Ok`/`Err` map onto the JS side as a resolved value / a thrown
/// exception (the program can `try/catch` a rejected tool).
pub trait HostBridge: Send + Sync + 'static {
    /// `tools.<name>(args)` — run one tool call through core's full gate
    /// (allowlist → deferred lock → hooks → permission → sandbox → execute).
    /// Ok(value) resolves the JS promise as-is: a built-in tool yields a JSON
    /// string, an MCP source tool its structured `CallToolResult` object.
    /// Err(reason) becomes a JS exception.
    fn call_tool(&self, name: String, args: Value) -> BoxFuture<Result<Value, String>>;
    /// `agent(prompt, opts)` — spawn a sub-agent (reuses core's task seam).
    /// `seq` is the call's monotonic index, assigned by the JS `agent()` wrapper
    /// (JS is single-threaded, so it is deterministic regardless of the order
    /// sub-agent futures resolve in) — the host uses it to journal/replay the
    /// call for resume.
    fn call_agent(&self, seq: u32, prompt: String, opts: Value)
        -> BoxFuture<Result<Value, String>>;
    /// `log(msg)` — progress output surfaced to the user and appended to the
    /// program's result. Fire-and-forget, never blocks the program.
    fn log(&self, message: String);
    /// `phase(title)` — update the detached workflow's live phase label. Program
    /// bridges ignore it; Workflow bridges override it.
    fn phase(&self, _title: String) {}
}

/// Resource ceilings for one program run. Engine-level limits (memory/stack/
/// cpu) guard the interpreter; the caps (agents/items) are hard ceilings on
/// orchestration fan-out — a model-written program loops and fans out
/// programmatically, so it needs runaway ceilings a hand-written tool_use
/// batch never hits. Values mirror cc's workflow caps (1000/4096). Two
/// deliberate non-caps: concurrency is NOT paced (a program firing N concurrent
/// `agent()` matches N concurrent `run_agent` calls, which kloop runs uncapped —
/// pacing here would break that precedent; the total ceiling is the guard); and
/// there is no token budget (cc's `budget.total` ships as a `null` placeholder,
/// never enforced, and kloop has no turn-level budget source, so it would be a
/// no-op — deferred, not built).
#[derive(Clone, Copy)]
pub struct Limits {
    /// QuickJS heap cap; the interpreter raises out-of-memory past it.
    pub memory_bytes: usize,
    /// Native stack cap, so deep JS recursion throws instead of aborting.
    pub max_stack_bytes: usize,
    /// Longest a single *synchronous* JS burst may run before the program is
    /// killed — guards `while(true){}` without penalizing await-heavy programs
    /// (the interpreter is suspended, not looping, while awaiting a tool).
    pub cpu_burst: Duration,
    /// Hard ceiling on total `agent()` calls in one program run — the runaway
    /// guard against `while(true){ agent(...) }` (each sub-agent costs tokens).
    /// The (N+1)th call throws. Enforced host-side in the bridge.
    pub max_agents: u64,
    /// Hard ceiling on the array length a single `parallel()`/`pipeline()` may
    /// take; over it throws (never silently truncates). Enforced in the prelude.
    pub max_items_per_call: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 64 * 1024 * 1024,
            max_stack_bytes: 512 * 1024,
            cpu_burst: Duration::from_secs(5),
            max_agents: 1000,
            max_items_per_call: 4096,
        }
    }
}

// Why the program was interrupted, set by the interrupt handler so the outcome
// can distinguish a user abort from a runaway loop from a bridge-side stop.
const STOP_NONE: u8 = 0;
const STOP_CANCEL: u8 = 1;
const STOP_CPU: u8 = 2;

const MAX_WORKFLOW_SOURCE_BYTES: usize = 512 * 1024;
const MAX_WORKFLOW_ARGS_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowPhase {
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowMeta {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub phases: Vec<WorkflowPhase>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedWorkflow {
    pub meta: WorkflowMeta,
    pub body: String,
}

/// Parse and validate the required first `export const meta = <pure literal>`
/// without executing model-written code. The returned body starts after that
/// declaration, ready for the hostless workflow runtime.
pub fn prepare_workflow(source: &str) -> Result<PreparedWorkflow> {
    if source.is_empty() || source.len() > MAX_WORKFLOW_SOURCE_BYTES {
        return Err(anyhow!(
            "workflow: script must contain 1..={MAX_WORKFLOW_SOURCE_BYTES} bytes"
        ));
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .map_err(|e| anyhow!("workflow: cannot initialize JavaScript parser: {e}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("workflow: JavaScript parser returned no tree"))?;
    if tree.root_node().has_error() {
        return Err(anyhow!(
            "workflow: script contains invalid JavaScript syntax"
        ));
    }
    let first = tree
        .root_node()
        .named_child(0)
        .ok_or_else(|| anyhow!("workflow: script must begin with `export const meta = ...`"))?;
    if first.kind() != "export_statement" {
        return Err(anyhow!(
            "workflow: first statement must be `export const meta = <pure literal>`"
        ));
    }
    let declaration = named_child_of_kind(first, "lexical_declaration")
        .ok_or_else(|| anyhow!("workflow: meta export must be a const declaration"))?;
    let declaration_text = node_text(declaration, source)?;
    if !declaration_text.trim_start().starts_with("const ") {
        return Err(anyhow!("workflow: meta export must use `const`"));
    }
    let declarator = named_child_of_kind(declaration, "variable_declarator")
        .ok_or_else(|| anyhow!("workflow: malformed meta declaration"))?;
    let name = declarator
        .child_by_field_name("name")
        .ok_or_else(|| anyhow!("workflow: meta declaration has no name"))?;
    if node_text(name, source)? != "meta" {
        return Err(anyhow!("workflow: first export must declare `meta`"));
    }
    let value = declarator
        .child_by_field_name("value")
        .ok_or_else(|| anyhow!("workflow: meta declaration has no value"))?;
    let value = literal_value(value, source)?;
    let meta: WorkflowMeta =
        serde_json::from_value(value).map_err(|e| anyhow!("workflow: invalid meta object: {e}"))?;
    validate_meta(&meta)?;

    let mut cursor = tree.root_node().walk();
    reject_nondeterminism(tree.root_node(), source, &mut cursor)?;
    let body = source[first.end_byte()..].to_string();
    Ok(PreparedWorkflow { meta, body })
}

fn named_child_of_kind<'a>(
    node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == kind);
    found
}

fn node_text<'a>(node: tree_sitter::Node<'_>, source: &'a str) -> Result<&'a str> {
    source
        .get(node.byte_range())
        .ok_or_else(|| anyhow!("workflow: parser produced an invalid source range"))
}

fn literal_value(node: tree_sitter::Node<'_>, source: &str) -> Result<Value> {
    match node.kind() {
        "object" => {
            let mut object = serde_json::Map::new();
            let mut cursor = node.walk();
            for pair in node.named_children(&mut cursor) {
                if pair.kind() != "pair" {
                    return Err(anyhow!("workflow: meta must contain only plain properties"));
                }
                let key = pair
                    .child_by_field_name("key")
                    .ok_or_else(|| anyhow!("workflow: meta property has no key"))?;
                let key = match key.kind() {
                    "property_identifier" | "identifier" => node_text(key, source)?.to_string(),
                    "string" => decode_js_string(node_text(key, source)?)?,
                    _ => return Err(anyhow!("workflow: meta property keys must be static")),
                };
                if object.contains_key(&key) {
                    return Err(anyhow!("workflow: duplicate meta property `{key}`"));
                }
                let value = pair
                    .child_by_field_name("value")
                    .ok_or_else(|| anyhow!("workflow: meta property `{key}` has no value"))?;
                object.insert(key, literal_value(value, source)?);
            }
            Ok(Value::Object(object))
        }
        "array" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .map(|child| literal_value(child, source))
                .collect::<Result<Vec<_>>>()
                .map(Value::Array)
        }
        "string" => Ok(Value::String(decode_js_string(node_text(node, source)?)?)),
        "number" => serde_json::from_str(node_text(node, source)?)
            .map_err(|e| anyhow!("workflow: invalid number in meta: {e}")),
        "true" => Ok(Value::Bool(true)),
        "false" => Ok(Value::Bool(false)),
        "null" => Ok(Value::Null),
        other => Err(anyhow!(
            "workflow: meta must be a pure literal; `{other}` is executable"
        )),
    }
}

fn decode_js_string(raw: &str) -> Result<String> {
    let mut chars = raw.chars();
    let quote = chars
        .next()
        .ok_or_else(|| anyhow!("workflow: empty string literal"))?;
    if !matches!(quote, '\'' | '"') || !raw.ends_with(quote) {
        return Err(anyhow!("workflow: meta strings must use plain quotes"));
    }
    let inner = &raw[quote.len_utf8()..raw.len() - quote.len_utf8()];
    let mut out = String::new();
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            out.push(match ch {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '\\' => '\\',
                '\'' => '\'',
                '"' => '"',
                other => other,
            });
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    if escaped {
        return Err(anyhow!("workflow: unterminated escape in meta string"));
    }
    Ok(out)
}

fn validate_meta(meta: &WorkflowMeta) -> Result<()> {
    if meta.name.is_empty() || meta.name.len() > 100 {
        return Err(anyhow!("workflow: meta.name must contain 1..=100 bytes"));
    }
    if meta.description.is_empty() || meta.description.len() > 500 {
        return Err(anyhow!(
            "workflow: meta.description must contain 1..=500 bytes"
        ));
    }
    if meta.phases.len() > 32 {
        return Err(anyhow!("workflow: meta.phases exceeds the 32-phase limit"));
    }
    let mut titles = std::collections::HashSet::new();
    for phase in &meta.phases {
        if phase.title.is_empty() || phase.title.len() > 100 || phase.detail.len() > 500 {
            return Err(anyhow!("workflow: phase title/detail exceeds its limit"));
        }
        if !titles.insert(&phase.title) {
            return Err(anyhow!("workflow: duplicate phase title `{}`", phase.title));
        }
    }
    Ok(())
}

fn reject_nondeterminism(
    node: tree_sitter::Node<'_>,
    source: &str,
    cursor: &mut tree_sitter::TreeCursor<'_>,
) -> Result<()> {
    let text = node_text(node, source)?;
    if node.kind() == "import_statement"
        || (node.kind() == "call_expression" && text.trim_start().starts_with("import("))
        || (node.kind() == "identifier" && text == "Date")
        || (node.kind() == "member_expression"
            && text.split_whitespace().collect::<String>() == "Math.random")
    {
        return Err(anyhow!(
            "workflow: imports, Date, and randomness are unavailable for deterministic resume"
        ));
    }
    if cursor.goto_first_child() {
        loop {
            reject_nondeterminism(cursor.node(), source, cursor)?;
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
    Ok(())
}

/// Run one program to completion. Returns the program's return value coerced to
/// a string (objects JSON-stringified, `undefined`/`null` → empty), or an error
/// carrying the JS exception / kill reason. `log()` output is delivered through
/// the bridge as it happens, not in this return value.
pub async fn run_program(
    source: &str,
    tool_names: &[String],
    bridge: Arc<dyn HostBridge>,
    cancel: CancellationToken,
    limits: Limits,
) -> Result<String> {
    run_js(
        wrap_source(source),
        build_prelude(tool_names, limits.max_items_per_call),
        bridge,
        cancel,
        limits,
        true,
        false,
    )
    .await
}

/// Serialize and bound the data exposed as a Workflow's immutable `args` value.
/// The launcher calls this before creating a run or registering background work;
/// the runtime calls it again so direct engine users get the same limit.
pub fn prepare_workflow_args(args: &Value) -> Result<String> {
    let args_json = serde_json::to_string(args)?;
    if args_json.len() > MAX_WORKFLOW_ARGS_BYTES {
        return Err(anyhow!(
            "workflow: args exceeds the {MAX_WORKFLOW_ARGS_BYTES}-byte limit"
        ));
    }
    Ok(args_json)
}

/// Run a prepared standalone Workflow. Unlike [`run_program`], no tool bridge
/// is installed: the script can only use args/meta, agent/log/phase and the
/// parallel/pipeline helpers. The return value stays structured JSON.
pub async fn run_workflow(
    workflow: &PreparedWorkflow,
    args: &Value,
    bridge: Arc<dyn HostBridge>,
    cancel: CancellationToken,
    limits: Limits,
) -> Result<Value> {
    let args_json = prepare_workflow_args(args)?;
    let meta_json = serde_json::to_string(&workflow.meta)?;
    let raw = run_js(
        wrap_workflow_source(&workflow.body),
        build_workflow_prelude(&args_json, &meta_json, limits.max_items_per_call),
        bridge,
        cancel,
        limits,
        false,
        true,
    )
    .await?;
    serde_json::from_str(&raw).map_err(|e| anyhow!("workflow: invalid result JSON: {e}"))
}

async fn run_js(
    wrapped: String,
    prelude: String,
    bridge: Arc<dyn HostBridge>,
    cancel: CancellationToken,
    limits: Limits,
    tools_enabled: bool,
    phase_enabled: bool,
) -> Result<String> {
    let rt = AsyncRuntime::new().map_err(|e| anyhow!("codemode: runtime init failed: {e}"))?;
    rt.set_memory_limit(limits.memory_bytes).await;
    rt.set_max_stack_size(limits.max_stack_bytes).await;
    let stop_reason = Arc::new(AtomicU8::new(STOP_NONE));
    install_interrupt_handler(&rt, cancel.clone(), stop_reason.clone(), limits.cpu_burst).await;
    let ctx = AsyncContext::full(&rt)
        .await
        .map_err(|e| anyhow!("codemode: context init failed: {e}"))?;

    ctx.async_with(async |ctx| {
        install_host_functions(&ctx, bridge, tools_enabled, phase_enabled)?;
        ctx.eval::<(), _>(prelude.as_bytes())
            .catch(&ctx)
            .map_err(|e| anyhow!("codemode: prelude failed: {e}"))?;
        let outcome: std::result::Result<String, String> = async {
            let promise: rquickjs::Promise = ctx
                .eval(wrapped.as_bytes())
                .catch(&ctx)
                .map_err(|e| e.to_string())?;
            promise
                .into_future::<String>()
                .await
                .catch(&ctx)
                .map_err(|e| e.to_string())
        }
        .await;
        outcome.map_err(|raw| match stop_reason.load(Ordering::Relaxed) {
            STOP_CANCEL => anyhow!("program interrupted"),
            STOP_CPU => anyhow!("program killed: exceeded CPU time limit"),
            _ => anyhow!("{raw}"),
        })
    })
    .await
}

async fn install_interrupt_handler(
    rt: &AsyncRuntime,
    cancel: CancellationToken,
    stop_reason: Arc<AtomicU8>,
    cpu_burst: Duration,
) {
    // The handler fires periodically *while JS executes*, not while it is
    // suspended awaiting a host future. So a gap between ticks means an await
    // happened → the current synchronous burst restarts; a burst that runs
    // longer than `cpu_burst` without such a gap is a runaway loop.
    const AWAIT_GAP: Duration = Duration::from_millis(50);
    let mut burst_start: Option<Instant> = None;
    let mut last_tick: Option<Instant> = None;
    rt.set_interrupt_handler(Some(Box::new(move || {
        if cancel.is_cancelled() {
            stop_reason.store(STOP_CANCEL, Ordering::Relaxed);
            return true;
        }
        let now = Instant::now();
        match last_tick {
            Some(t) if now.duration_since(t) < AWAIT_GAP => {}
            _ => burst_start = Some(now),
        }
        last_tick = Some(now);
        if burst_start.is_some_and(|s| now.duration_since(s) > cpu_burst) {
            stop_reason.store(STOP_CPU, Ordering::Relaxed);
            return true;
        }
        false
    })))
    .await;
}

fn install_host_functions(
    ctx: &rquickjs::Ctx<'_>,
    bridge: Arc<dyn HostBridge>,
    tools_enabled: bool,
    phase_enabled: bool,
) -> Result<()> {
    let globals = ctx.globals();

    if tools_enabled {
        let tool_bridge = bridge.clone();
        let call_tool = Function::new(
            ctx.clone(),
            Async(move |name: String, args_json: String| {
                let bridge = tool_bridge.clone();
                async move {
                    let args: Value = serde_json::from_str(&args_json).unwrap_or(Value::Null);
                    envelope(bridge.call_tool(name, args).await)
                }
            }),
        )
        .map_err(|e| anyhow!("codemode: install __call_tool: {e}"))?;
        globals
            .set("__call_tool", call_tool)
            .map_err(|e| anyhow!("codemode: set __call_tool: {e}"))?;
    }

    let agent_bridge = bridge.clone();
    let agent = Function::new(
        ctx.clone(),
        Async(move |prompt: String, opts_json: String, seq: u32| {
            let bridge = agent_bridge.clone();
            async move {
                let opts: Value = serde_json::from_str(&opts_json).unwrap_or(Value::Null);
                envelope(bridge.call_agent(seq, prompt, opts).await)
            }
        }),
    )
    .map_err(|e| anyhow!("codemode: install __agent: {e}"))?;
    globals
        .set("__agent", agent)
        .map_err(|e| anyhow!("codemode: set __agent: {e}"))?;

    let log_bridge = bridge.clone();
    let log = Function::new(ctx.clone(), move |msg: String| log_bridge.log(msg))
        .map_err(|e| anyhow!("codemode: install __log: {e}"))?;
    globals
        .set("__log", log)
        .map_err(|e| anyhow!("codemode: set __log: {e}"))?;

    if phase_enabled {
        let phase = Function::new(ctx.clone(), move |title: String| bridge.phase(title))
            .map_err(|e| anyhow!("codemode: install __phase: {e}"))?;
        globals
            .set("__phase", phase)
            .map_err(|e| anyhow!("codemode: set __phase: {e}"))?;
    }

    Ok(())
}

/// Host-function results cross to JS as a JSON envelope so the prelude wrappers
/// can turn an `Err` into a thrown exception without the engine needing to
/// build a JS exception across an await boundary. `value` is any JSON value —
/// a string for built-ins/agent, an object for an MCP tool's CallToolResult —
/// and the prelude returns it to the program after JSON.parse, so objects
/// arrive as objects.
fn envelope(result: Result<Value, String>) -> String {
    match result {
        Ok(value) => json!({ "ok": true, "value": value }).to_string(),
        Err(error) => json!({ "ok": false, "error": error }).to_string(),
    }
}

/// The JS prelude: builds the `tools` object (one method per tool name),
/// `agent`, `log`, `parallel` and `pipeline` on top of the raw `__call_tool`/
/// `__agent`/`__log` host functions. Kept tiny and dependency-free.
fn build_prelude(tool_names: &[String], max_items: usize) -> String {
    let names = serde_json::to_string(tool_names).unwrap_or_else(|_| "[]".into());
    let common = build_common_prelude(max_items);
    format!(
        r#"
        globalThis.tools = {{}};
        for (const __n of {names}) {{
            globalThis.tools[__n] = async (args) => {{
                const r = JSON.parse(await __call_tool(__n, JSON.stringify(args ?? {{}})));
                if (!r.ok) throw new Error(r.error);
                return r.value;
            }};
        }}
        {common}
        "#
    )
}

fn build_workflow_prelude(args: &str, meta: &str, max_items: usize) -> String {
    let common = build_common_prelude(max_items);
    let args_literal = serde_json::to_string(args).unwrap_or_else(|_| "\"null\"".into());
    let meta_literal = serde_json::to_string(meta).unwrap_or_else(|_| "\"null\"".into());
    format!(
        r#"
        const __deepFreeze = (value) => {{
            if (value && typeof value === 'object') {{
                Object.freeze(value);
                for (const child of Object.values(value)) __deepFreeze(child);
            }}
            return value;
        }};
        globalThis.args = __deepFreeze(JSON.parse({args_literal}));
        globalThis.meta = __deepFreeze(JSON.parse({meta_literal}));
        globalThis.phase = (title) => __phase(String(title));
        // Deterministic resume: time and randomness are deliberately absent.
        globalThis.Date = undefined;
        Math.random = undefined;
        {common}
        "#
    )
}

fn build_common_prelude(max_items: usize) -> String {
    format!(
        r#"
        globalThis.agent = (() => {{
            let __seq = 0;
            return async (prompt, opts) => {{
                const r = JSON.parse(await __agent(String(prompt), JSON.stringify(opts ?? {{}}), __seq++));
                if (!r.ok) throw new Error(r.error);
                return r.value;
            }};
        }})();
        globalThis.log = (msg) => __log(typeof msg === 'string' ? msg : JSON.stringify(msg));
        const __MAX_ITEMS = {max_items};
        const __checkItems = (n, who) => {{
            if (n > __MAX_ITEMS) throw new Error(
                who + ': ' + n + ' items exceeds the cap of ' + __MAX_ITEMS + ' per call');
        }};
        globalThis.parallel = (thunks) => {{
            __checkItems((thunks ?? []).length, 'parallel');
            return Promise.all(thunks.map((t) => Promise.resolve().then(t).catch(() => null)));
        }};
        globalThis.pipeline = (items, ...stages) => {{
            __checkItems((items ?? []).length, 'pipeline');
            return Promise.all((items ?? []).map(async (item, index) => {{
                let value = item;
                for (const stage of stages) {{
                    try {{ value = await stage(value, item, index); }}
                    catch {{ return null; }}
                }}
                return value;
            }}));
        }};
        "#
    )
}

/// Wrap the model's source so top-level `return`/`await` are legal and the
/// resolved value is always a string (objects JSON-stringified, nullish → "").
fn wrap_source(source: &str) -> String {
    format!(
        r#"
        (async () => {{
            const __r = await (async () => {{
                {source}
            }})();
            if (typeof __r === 'string') return __r;
            if (__r === undefined || __r === null) return '';
            return JSON.stringify(__r);
        }})()
        "#
    )
}

fn wrap_workflow_source(source: &str) -> String {
    format!(
        r#"
        (async () => {{
            const __r = await (async () => {{
                {source}
            }})();
            if (__r === undefined) return "null";
            return JSON.stringify(__r);
        }})()
        "#
    )
}

#[cfg(test)]
mod tests;
