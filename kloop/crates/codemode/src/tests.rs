use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Barrier;
use tokio_util::sync::CancellationToken;

use super::*;

#[derive(Clone, Copy)]
enum Behavior {
    Echo,
    Fail,
}

struct TestBridge {
    behavior: Behavior,
    logs: Mutex<Vec<String>>,
    // When present, every tool call parks on the barrier before returning, so a
    // batch that is genuinely concurrent releases and a serial one deadlocks.
    barrier: Option<Arc<Barrier>>,
}

impl TestBridge {
    fn echo() -> Arc<Self> {
        Arc::new(Self {
            behavior: Behavior::Echo,
            logs: Mutex::new(Vec::new()),
            barrier: None,
        })
    }
}

impl HostBridge for TestBridge {
    fn call_tool(&self, name: String, args: Value) -> BoxFuture<Result<String, String>> {
        let behavior = self.behavior;
        let barrier = self.barrier.clone();
        Box::pin(async move {
            if let Some(b) = barrier {
                b.wait().await;
            }
            match behavior {
                Behavior::Echo => Ok(format!("{name}({args})")),
                Behavior::Fail => Err(format!("tool {name} refused")),
            }
        })
    }

    fn spawn_agent(&self, prompt: String, opts: Value) -> BoxFuture<Result<String, String>> {
        Box::pin(async move { Ok(format!("agent[{opts}]: {prompt}")) })
    }

    fn log(&self, message: String) {
        self.logs.lock().unwrap().push(message);
    }
}

const TOOLS: &[&str] = &["read_file", "bash"];

fn tool_names() -> Vec<String> {
    TOOLS.iter().map(|s| s.to_string()).collect()
}

async fn run(source: &str, bridge: Arc<dyn HostBridge>) -> Result<String> {
    run_program(
        source,
        &tool_names(),
        bridge,
        CancellationToken::new(),
        Limits::default(),
    )
    .await
}

#[tokio::test]
async fn tool_call_bridges_through_host_and_returns_value() {
    let out = run(
        r#"const c = await tools.read_file({ path: "a.txt" });
           return c.toUpperCase();"#,
        TestBridge::echo(),
    )
    .await
    .unwrap();
    assert_eq!(out, r#"READ_FILE({"PATH":"A.TXT"})"#);
}

#[tokio::test]
async fn tool_error_is_a_catchable_exception() {
    let bridge = Arc::new(TestBridge {
        behavior: Behavior::Fail,
        logs: Mutex::new(Vec::new()),
        barrier: None,
    });
    let out = run(
        r#"try { await tools.bash({ command: "x" }); return "unreached"; }
           catch (e) { return "caught: " + e.message; }"#,
        bridge,
    )
    .await
    .unwrap();
    assert_eq!(out, "caught: tool bash refused");
}

#[tokio::test]
async fn promise_all_runs_tool_calls_concurrently() {
    // Both calls must be in flight at once to clear the 2-party barrier; a
    // serial implementation would block the first call forever (test times out).
    let bridge = Arc::new(TestBridge {
        behavior: Behavior::Echo,
        logs: Mutex::new(Vec::new()),
        barrier: Some(Arc::new(Barrier::new(2))),
    });
    let fut = run(
        r#"const [a, b] = await Promise.all([
               tools.read_file({ n: 1 }),
               tools.read_file({ n: 2 }),
           ]);
           return a + "|" + b;"#,
        bridge,
    );
    let out = tokio::time::timeout(Duration::from_secs(5), fut)
        .await
        .expect("concurrent tool calls should not deadlock")
        .unwrap();
    assert_eq!(out, r#"read_file({"n":1})|read_file({"n":2})"#);
}

#[tokio::test]
async fn parallel_helper_turns_failures_into_null() {
    let bridge = Arc::new(TestBridge {
        behavior: Behavior::Fail,
        logs: Mutex::new(Vec::new()),
        barrier: None,
    });
    let out = run(
        r#"const r = await parallel([
               () => tools.bash({ i: 1 }),
               () => tools.bash({ i: 2 }),
           ]);
           return JSON.stringify(r);"#,
        bridge,
    )
    .await
    .unwrap();
    assert_eq!(out, "[null,null]");
}

#[tokio::test]
async fn agent_bridges_through_host() {
    let out = run(
        r#"return await agent("find X", { agent_type: "researcher" });"#,
        TestBridge::echo(),
    )
    .await
    .unwrap();
    assert_eq!(out, r#"agent[{"agent_type":"researcher"}]: find X"#);
}

#[tokio::test]
async fn log_output_reaches_the_bridge() {
    let bridge = TestBridge::echo();
    run(
        r#"log("step 1"); log({ done: true }); return "ok";"#,
        bridge.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        *bridge.logs.lock().unwrap(),
        vec!["step 1", r#"{"done":true}"#]
    );
}

#[tokio::test]
async fn program_is_sandboxed_from_host_capabilities() {
    let out = run(
        r#"return JSON.stringify({
               fetch: typeof fetch,
               require: typeof require,
               process: typeof process,
               console: typeof console,
               XMLHttpRequest: typeof XMLHttpRequest,
           });"#,
        TestBridge::echo(),
    )
    .await
    .unwrap();
    let caps: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(caps["fetch"], "undefined");
    assert_eq!(caps["require"], "undefined");
    assert_eq!(caps["process"], "undefined");
    assert_eq!(caps["console"], "undefined");
    assert_eq!(caps["XMLHttpRequest"], "undefined");
}

#[tokio::test]
async fn module_import_is_rejected() {
    let err = run("import x from 'fs'; return x;", TestBridge::echo())
        .await
        .unwrap_err()
        .to_string();
    assert!(!err.is_empty(), "import should fail to compile");
}

#[tokio::test]
async fn result_coercion_covers_string_object_and_nullish() {
    let obj = run(r#"return { a: 1, b: [2, 3] };"#, TestBridge::echo())
        .await
        .unwrap();
    assert_eq!(obj, r#"{"a":1,"b":[2,3]}"#);
    let nothing = run("let x = 1;", TestBridge::echo()).await.unwrap();
    assert_eq!(nothing, "");
    let raw = run(r#"return "plain string";"#, TestBridge::echo())
        .await
        .unwrap();
    assert_eq!(raw, "plain string");
}

#[tokio::test]
async fn thrown_program_error_surfaces() {
    let err = run(r#"throw new Error("boom in program");"#, TestBridge::echo())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("boom in program"), "{err}");
}

#[tokio::test]
async fn runaway_cpu_loop_is_killed() {
    let err = run_program(
        "while (true) {}",
        &tool_names(),
        TestBridge::echo(),
        CancellationToken::new(),
        Limits {
            cpu_burst: Duration::from_millis(100),
            ..Limits::default()
        },
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("CPU time limit"), "{err}");
}

#[tokio::test]
async fn cancellation_interrupts_a_loop() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = run_program(
        "while (true) {}",
        &tool_names(),
        TestBridge::echo(),
        cancel,
        Limits::default(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("interrupted"), "{err}");
}

#[tokio::test]
async fn memory_limit_is_enforced() {
    let err = run_program(
        "const a = []; while (true) { a.push(new Array(100000).fill(7)); }",
        &tool_names(),
        TestBridge::echo(),
        CancellationToken::new(),
        Limits {
            memory_bytes: 1024 * 1024,
            cpu_burst: Duration::from_secs(30),
            ..Limits::default()
        },
    )
    .await;
    assert!(err.is_err(), "unbounded allocation should fail");
}
