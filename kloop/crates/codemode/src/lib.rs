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
    fn call_agent(&self, prompt: String, opts: Value) -> BoxFuture<Result<String, String>>;
    /// `log(msg)` — progress output surfaced to the user and appended to the
    /// program's result. Fire-and-forget, never blocks the program.
    fn log(&self, message: String);
}

/// Resource ceilings for one program run. Engine-level limits (memory/stack/
/// cpu) guard the interpreter; the caps (agents/items) are hard ceilings on
/// orchestration fan-out — a model-written program loops and fans out
/// programmatically, so it needs runaway ceilings a hand-written tool_use
/// batch never hits. Values mirror cc's workflow caps (1000/4096). Two
/// deliberate non-caps: concurrency is NOT paced (a program firing N concurrent
/// `agent()` matches N concurrent `task` calls, which kloop runs uncapped —
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
    let rt = AsyncRuntime::new().map_err(|e| anyhow!("codemode: runtime init failed: {e}"))?;
    rt.set_memory_limit(limits.memory_bytes).await;
    rt.set_max_stack_size(limits.max_stack_bytes).await;

    // Shared with the interrupt handler and read after the run to tell why a
    // program stopped. The handler is the only mechanism that can break a
    // CPU-bound JS loop (an await-blocked program is handled by the bridge's
    // own cancellation, but a `while(true){}` never yields to it).
    let stop_reason = Arc::new(AtomicU8::new(STOP_NONE));
    install_interrupt_handler(&rt, cancel.clone(), stop_reason.clone(), limits.cpu_burst).await;

    let ctx = AsyncContext::full(&rt)
        .await
        .map_err(|e| anyhow!("codemode: context init failed: {e}"))?;

    let prelude = build_prelude(tool_names, limits.max_items_per_call);
    let wrapped = wrap_source(source);

    ctx.async_with(async |ctx| {
        install_host_functions(&ctx, bridge)?;
        ctx.eval::<(), _>(prelude.as_bytes())
            .catch(&ctx)
            .map_err(|e| anyhow!("codemode: prelude failed: {e}"))?;

        // A synchronous runaway loop is interrupted inside `eval` (before the
        // first await), an await-heavy one inside `into_future`; both funnel
        // through the same stop-reason mapping so the model gets the real cause
        // instead of QuickJS's bare "interrupted" exception text.
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
    // rt/ctx drop here: the whole QuickJS instance is released, no state bleeds.
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

fn install_host_functions(ctx: &rquickjs::Ctx<'_>, bridge: Arc<dyn HostBridge>) -> Result<()> {
    let globals = ctx.globals();

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

    let agent_bridge = bridge.clone();
    let agent = Function::new(
        ctx.clone(),
        Async(move |prompt: String, opts_json: String| {
            let bridge = agent_bridge.clone();
            async move {
                let opts: Value = serde_json::from_str(&opts_json).unwrap_or(Value::Null);
                // agent() yields text; wrap it in the same value envelope as a tool.
                envelope(bridge.call_agent(prompt, opts).await.map(Value::String))
            }
        }),
    )
    .map_err(|e| anyhow!("codemode: install __agent: {e}"))?;
    globals
        .set("__agent", agent)
        .map_err(|e| anyhow!("codemode: set __agent: {e}"))?;

    let log = Function::new(ctx.clone(), move |msg: String| bridge.log(msg))
        .map_err(|e| anyhow!("codemode: install __log: {e}"))?;
    globals
        .set("__log", log)
        .map_err(|e| anyhow!("codemode: set __log: {e}"))?;

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
        globalThis.agent = async (prompt, opts) => {{
            const r = JSON.parse(await __agent(String(prompt), JSON.stringify(opts ?? {{}})));
            if (!r.ok) throw new Error(r.error);
            return r.value;
        }};
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
        // Each item flows through every stage as its own independent async
        // chain — NO barrier between stages, so a fast item can reach stage 3
        // while a slow one is still in stage 1. A stage that throws drops that
        // item to null and skips its remaining stages, mirroring `parallel`.
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

#[cfg(test)]
mod tests;
