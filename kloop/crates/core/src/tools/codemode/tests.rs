use std::sync::Arc;
use std::sync::Mutex;

use serde_json::json;

use crate::agent::Ui;
use crate::permissions::Mode;
use crate::permissions::PermissionRules;
use crate::permissions::Permissions;
use crate::tools::testutil::run_tool;
use crate::tools::testutil::test_ctx;
use crate::tools::testutil::with_provider;
use crate::tools::ToolCtx;
use kloop_protocol::ToolDef;

use super::*;

fn tmp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("kloop-codemode-{}-{}", std::process::id(), name))
}

/// Records the UI signals a running program emits, so tests can assert what the
/// user actually sees while the program executes.
#[derive(Default)]
struct RecordUi(Mutex<Vec<String>>);
impl RecordUi {
    fn events(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}
impl Ui for RecordUi {
    fn text_delta(&self, _: &str) {}
    fn note(&self, s: &str) {
        self.0.lock().unwrap().push(format!("note: {s}"));
    }
    fn tool_start(&self, _agent: &str, _id: &str, name: &str, _summary: &str) {
        self.0.lock().unwrap().push(format!("tool_start: {name}"));
    }
    fn tool_end(&self, _agent: &str, _id: &str, ok: bool) {
        self.0.lock().unwrap().push(format!("tool_end: {ok}"));
    }
}

fn with_ui(mut ctx: ToolCtx, ui: Arc<RecordUi>) -> ToolCtx {
    ctx.ui = ui;
    ctx
}

fn with_permissions(mut ctx: ToolCtx, perms: Permissions) -> ToolCtx {
    let mut cfg = (*ctx.cfg).clone();
    cfg.permissions = Arc::new(perms);
    ctx.cfg = Arc::new(cfg);
    ctx
}

async fn exec(source: &str, ctx: &ToolCtx) -> (String, bool) {
    run_tool("exec", json!({ "source": source }), ctx).await
}

#[tokio::test]
async fn program_reads_a_file_through_the_real_dispatch() {
    let file = tmp("read");
    std::fs::write(&file, "hello codemode").unwrap();
    let ctx = test_ctx(0, "read");
    let (out, is_error) = exec(
        &format!(r#"return await tools.read_file({{ path: {:?} }});"#, file),
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("hello codemode"), "{out}");
    let _ = std::fs::remove_file(&file);
}

/// The op layer re-enters the same permission gate: a denied tool is refused
/// *inside* the program (the model catches it), while a read-only tool in the
/// very same program passes. This is the whole safety story of code-mode.
#[tokio::test]
async fn tool_calls_pass_the_permission_gate() {
    let readable = tmp("gate-read");
    std::fs::write(&readable, "visible").unwrap();
    let forbidden = tmp("gate-write");
    let _ = std::fs::remove_file(&forbidden);

    let rules = PermissionRules {
        allow: vec![],
        deny: vec!["write_file".into()],
        ask: vec![],
    };
    let perms = Permissions::new(Mode::Default, &rules, std::env::temp_dir(), None, None).unwrap();
    let ctx = with_permissions(test_ctx(0, "gate"), perms);

    let (out, is_error) = exec(
        &format!(
            r#"let w;
               try {{ await tools.write_file({{ path: {forbidden:?}, content: "x" }}); w = "WROTE"; }}
               catch (e) {{ w = "BLOCKED"; }}
               const r = await tools.read_file({{ path: {readable:?} }});
               return w + ":" + r.includes("visible");"#,
        ),
        &ctx,
    )
    .await;

    assert!(!is_error, "{out}");
    assert_eq!(out, "BLOCKED:true");
    assert!(
        !forbidden.exists(),
        "the denied write must not have touched disk"
    );
    let _ = std::fs::remove_file(&readable);
}

/// `agent()` reuses the task seam, spawning a real sub-agent that samples the
/// (scripted) provider and returns its final text.
#[tokio::test]
async fn agent_call_spawns_a_subagent() {
    let provider = kloop_provider::Provider::mock(vec![vec![kloop_protocol::ContentBlock::Text {
        text: "sub-agent result".into(),
    }]]);
    let ctx = with_provider(test_ctx(0, "agent"), provider);
    let (out, is_error) = exec(r#"return await agent("do the thing");"#, &ctx).await;
    assert!(!is_error, "{out}");
    assert_eq!(out, "sub-agent result");
}

/// Intermediate tool results live in program variables; only the return value
/// comes back. Two full file reads happen, but their content never appears in
/// the tool_result — exactly the context-window saving code-mode exists for.
#[tokio::test]
async fn intermediate_results_stay_off_the_result() {
    let file = tmp("intermediate");
    std::fs::write(&file, "SECRET_PAYLOAD").unwrap();
    let ctx = test_ctx(0, "intermediate");
    let (out, is_error) = exec(
        &format!(
            r#"const a = await tools.read_file({{ path: {file:?} }});
               const b = await tools.read_file({{ path: {file:?} }});
               return "combined_len:" + (a.length + b.length);"#,
        ),
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.starts_with("combined_len:"), "{out}");
    assert!(
        !out.contains("SECRET_PAYLOAD"),
        "file content must not leak into the result: {out}"
    );
    let _ = std::fs::remove_file(&file);
}

#[tokio::test]
async fn program_error_surfaces_the_exception_without_logs() {
    let ctx = test_ctx(0, "err");
    let (out, is_error) = exec(r#"log("before"); throw new Error("kaboom");"#, &ctx).await;
    assert!(is_error);
    assert!(out.contains("kaboom"), "{out}");
    // log() is a live user-facing channel, not part of the model's result.
    assert!(
        !out.contains("before"),
        "logs must not leak into the result: {out}"
    );
}

/// A running program is observable: its `log()` output streams live to the UI
/// (not buried in the final result) and each `tools.<name>()` op shows as a
/// tool line — so the program is not a black box while it runs.
#[tokio::test]
async fn program_logs_and_ops_stream_to_the_ui() {
    let file = tmp("observe");
    std::fs::write(&file, "data").unwrap();
    let rec = Arc::new(RecordUi::default());
    let ctx = with_ui(test_ctx(0, "observe"), rec.clone());
    let (out, is_error) = exec(
        &format!(
            r#"log("phase 1");
               await tools.read_file({{ path: {file:?} }});
               log("phase 2");
               return "ok";"#,
        ),
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(out, "ok", "only the return value comes back, no logs");

    let events = rec.events();
    // Both logs surfaced live, in order, with the inner op's tool line between
    // them.
    let pos = |needle: &str| {
        events
            .iter()
            .position(|e| e == needle)
            .unwrap_or_else(|| panic!("missing {needle:?} in {events:?}"))
    };
    assert!(
        pos("note: phase 1") < pos("tool_start: read_file")
            && pos("tool_start: read_file") < pos("note: phase 2"),
        "expected phase 1 → read_file → phase 2 in {events:?}"
    );
    assert!(events.iter().any(|e| e == "tool_end: true"), "{events:?}");
    let _ = std::fs::remove_file(&file);
}

#[test]
fn exec_def_renders_a_typescript_api() {
    let defs = vec![
        ToolDef {
            name: "read_file".into(),
            description: "Read   a text\nfile".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "limit": {"type": "integer"}
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "grep".into(),
            description: "Search".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "output_mode": {"type": "string", "enum": ["content", "count"]}
                },
                "required": ["pattern"]
            }),
        },
        ToolDef {
            name: "task".into(),
            description: "spawn".into(),
            schema: json!({"type": "object"}),
        },
    ];
    let def = exec_def(&defs);
    let d = &def.description;
    assert_eq!(def.name, "exec");
    assert!(d.contains("read_file(args: {"), "{d}");
    assert!(d.contains("path: string"), "{d}");
    assert!(d.contains("limit?: number"), "{d}");
    // Whitespace in the source description is collapsed to one line.
    assert!(d.contains("/** Read a text file */"), "{d}");
    // String enums render as a union.
    assert!(d.contains(r#"output_mode?: "content" | "count""#), "{d}");
    assert!(d.contains("declare function agent("), "{d}");
    assert!(d.contains("declare function parallel<T>"), "{d}");
    // task is not callable from a program (agent() replaces it); exec isn't either.
    assert!(!d.contains("task(args"), "{d}");
    assert!(!d.contains("exec(args"), "{d}");
}

#[test]
fn ts_type_covers_common_shapes() {
    assert_eq!(ts_type(&json!({"type": "string"})), "string");
    assert_eq!(ts_type(&json!({"type": "integer"})), "number");
    assert_eq!(ts_type(&json!({"type": "boolean"})), "boolean");
    assert_eq!(
        ts_type(&json!({"type": "string", "enum": ["a", "b"]})),
        r#""a" | "b""#
    );
    assert_eq!(
        ts_type(&json!({"type": "array", "items": {"type": "string"}})),
        "Array<string>"
    );
    assert_eq!(
        ts_type(&json!({
            "type": "object",
            "properties": {"x": {"type": "integer"}, "y": {"type": "string"}},
            "required": ["x"]
        })),
        "{ x: number; y?: string }"
    );
    // Unknown shapes degrade rather than lie.
    assert_eq!(ts_type(&json!({})), "unknown");
}

#[test]
fn program_surface_excludes_exec_and_task() {
    let names = program_tool_names();
    assert!(names.iter().any(|n| n == "read_file"));
    assert!(names.iter().any(|n| n == "bash"));
    assert!(!names.iter().any(|n| n == "exec"));
    assert!(!names.iter().any(|n| n == "task"));
}
