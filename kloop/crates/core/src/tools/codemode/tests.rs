use std::sync::Arc;
use std::sync::Mutex;

use serde_json::json;

use crate::agent::Ui;
use crate::permissions::Mode;
use crate::permissions::PermissionRules;
use crate::permissions::Permissions;
use crate::tools::testutil::run_tool;
use crate::tools::testutil::test_ctx;
use crate::tools::testutil::test_ctx_with_sources;
use crate::tools::testutil::with_defer_threshold;
use crate::tools::testutil::with_provider;
use crate::tools::ToolCtx;
use crate::tools::ToolSource;
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

fn with_program_limits(mut ctx: ToolCtx, limits: kloop_codemode::Limits) -> ToolCtx {
    let mut cfg = (*ctx.cfg).clone();
    cfg.program_limits = limits;
    ctx.cfg = Arc::new(cfg);
    ctx
}

async fn run(source: &str, ctx: &ToolCtx) -> (String, bool) {
    run_tool("run_program", json!({ "source": source }), ctx).await
}

#[tokio::test]
async fn program_reads_a_file_through_the_real_dispatch() {
    let file = tmp("read");
    std::fs::write(&file, "hello codemode").unwrap();
    let ctx = test_ctx(0, "read");
    let (out, is_error) = run(
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

    let (out, is_error) = run(
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
    let (out, is_error) = run(r#"return await agent("do the thing");"#, &ctx).await;
    assert!(!is_error, "{out}");
    assert_eq!(out, "sub-agent result");
}

/// The agent cap refuses runaway sub-agent fan-out: with max_agents=2 the third
/// agent() call throws (caught in-program here) before it can spawn, so only two
/// sub-agents ever run.
#[tokio::test]
async fn agent_cap_refuses_runaway_fanout() {
    let text = |t: &str| vec![kloop_protocol::ContentBlock::Text { text: t.into() }];
    let provider = kloop_provider::Provider::mock(vec![text("one"), text("two")]);
    let ctx = with_program_limits(
        with_provider(test_ctx(0, "agentcap"), provider),
        kloop_codemode::Limits {
            max_agents: 2,
            ..Default::default()
        },
    );
    let (out, is_error) = run(
        r#"
        const r = [];
        for (let i = 0; i < 3; i++) {
            try { r.push(await agent("go " + i)); }
            catch (e) { r.push("ERR:" + e.message); }
        }
        return JSON.stringify(r);
        "#,
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(
        out.contains("one") && out.contains("two"),
        "first two ran: {out}"
    );
    assert!(out.contains("agent cap (2"), "the third hit the cap: {out}");
}

/// Fire-and-forget: run_program {"background": true} returns a "started" message
/// (NOT the result), and the detached program reinjects its return value into
/// the PARENT's inbox as a framed ProgramResult when it finishes.
#[tokio::test]
async fn background_program_returns_immediately_and_reinjects() {
    use crate::inbox::InboxItem;
    let ctx = test_ctx(0, "bg-program");
    let (out, is_error) = run_tool(
        "run_program",
        json!({ "source": "return 'PROG_DONE';", "background": true }),
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("started in the background"), "{out}");
    assert!(
        !out.contains("PROG_DONE"),
        "the result is NOT returned inline: {out}"
    );

    for _ in 0..300 {
        if !ctx.cfg.inbox.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let items = ctx.cfg.inbox.drain();
    assert_eq!(items.len(), 1, "one reinjected result");
    match &items[0] {
        InboxItem::ProgramResult { label, summary } => {
            assert!(label.starts_with("program-"), "{label}");
            assert_eq!(summary, "PROG_DONE");
        }
        other => panic!("expected ProgramResult, got {other:?}"),
    }
    assert_eq!(ctx.cfg.async_agents.running_count(), 0, "slot freed");
}

/// A background program cancelled via stop_agent ends Aborted and reinjects
/// NOTHING (codex's is_final) — only a wake so a blocked wait re-evaluates.
#[tokio::test]
async fn stopped_background_program_does_not_reinject() {
    let ctx = test_ctx(0, "bg-prog-stop");
    // The program blocks on a long bash so stop_agent can catch it running.
    let (out, _) = run_tool(
        "run_program",
        json!({ "source": "return await tools.bash({ command: 'sleep 30' });", "background": true }),
        &ctx,
    )
    .await;
    let id = out
        .split_whitespace()
        .find(|w| w.starts_with("program-"))
        .unwrap()
        .to_string();
    for _ in 0..100 {
        if ctx.cfg.async_agents.running_count() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (stop_out, is_error) = run_tool("stop_agent", json!({ "agent_id": id }), &ctx).await;
    assert!(!is_error, "{stop_out}");
    assert!(stop_out.contains("Stopping"), "{stop_out}");

    for _ in 0..300 {
        if ctx.cfg.async_agents.running_count() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        ctx.cfg.inbox.is_empty(),
        "an interrupted program reinjects nothing"
    );
}

/// Intermediate tool results live in program variables; only the return value
/// comes back. Two full file reads happen, but their content never appears in
/// the tool_result — exactly the context-window saving code-mode exists for.
#[tokio::test]
async fn intermediate_results_stay_off_the_result() {
    let file = tmp("intermediate");
    std::fs::write(&file, "SECRET_PAYLOAD").unwrap();
    let ctx = test_ctx(0, "intermediate");
    let (out, is_error) = run(
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
    let (out, is_error) = run(r#"log("before"); throw new Error("kaboom");"#, &ctx).await;
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
    let (out, is_error) = run(
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
fn run_program_def_renders_a_typescript_api() {
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
    let def = run_program_def(&defs, &[], &[]);
    let d = &def.description;
    assert_eq!(def.name, "run_program");
    assert!(d.contains("read_file(args: {"), "{d}");
    assert!(d.contains("path: string"), "{d}");
    assert!(d.contains("limit?: number"), "{d}");
    // Whitespace in the source description is collapsed to one line.
    assert!(d.contains("/** Read a text file */"), "{d}");
    // String enums render as a union.
    assert!(d.contains(r#"output_mode?: "content" | "count""#), "{d}");
    assert!(d.contains("declare function agent("), "{d}");
    assert!(d.contains("declare function parallel<T>"), "{d}");
    assert!(d.contains("declare function pipeline("), "{d}");
    // task is not callable from a program (agent() replaces it); run_program
    // (the tool itself) isn't either.
    assert!(!d.contains("task(args"), "{d}");
    assert!(!d.contains("run_program(args"), "{d}");
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
fn program_surface_excludes_run_program_and_task() {
    let names = program_tool_names(&[]);
    assert!(names.iter().any(|n| n == "read_file"));
    assert!(names.iter().any(|n| n == "bash"));
    assert!(!names.iter().any(|n| n == "run_program"));
    assert!(!names.iter().any(|n| n == "task"));
}

// ---- External source (MCP) tools exposed to programs (plan 27) ----

/// A minimal external tool source: `srv__echo` (read-only, echoes its `text`
/// arg), `srv__danger` (a mutating tool), and `srv__data` (returns a structured
/// CallToolResult). Mirrors the shape an MCP tool reaches the bridge with.
struct Srv {
    defs: Vec<ToolDef>,
}

fn srv() -> std::sync::Arc<dyn ToolSource> {
    let def = |name: &str, desc: &str| ToolDef {
        name: name.into(),
        description: desc.into(),
        schema: json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"]
        }),
    };
    std::sync::Arc::new(Srv {
        defs: vec![
            def("srv__echo", "Echo the text argument back"),
            def("srv__danger", "A mutating tool"),
            def("srv__data", "Return a structured result"),
        ],
    })
}

impl ToolSource for Srv {
    fn defs(&self) -> &[ToolDef] {
        &self.defs
    }
    fn is_readonly(&self, tool: &str) -> bool {
        tool == "srv__echo" || tool == "srv__data"
    }
    fn call<'a>(
        &'a self,
        tool: &'a str,
        input: &'a serde_json::Value,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = anyhow::Result<crate::tools::SourceOutput>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let text = input.get("text").and_then(|v| v.as_str()).unwrap_or("?");
            if tool == "srv__data" {
                // A real MCP CallToolResult: flat text plus structuredContent.
                return Ok(crate::tools::SourceOutput {
                    text: format!("count={}", text.len()),
                    structured: Some(json!({
                        "content": [{"type": "text", "text": format!("count={}", text.len())}],
                        "structuredContent": {"len": text.len(), "echo": text}
                    })),
                });
            }
            Ok(crate::tools::SourceOutput::text(format!(
                "{tool} echoes {text}"
            )))
        })
    }
}

/// Slice 1: below the defer threshold, an external source tool is callable from
/// a program by its name, routed through the real gate to the source.
#[tokio::test]
async fn program_calls_an_mcp_source_tool() {
    let ctx = test_ctx_with_sources(0, "mcp-inline", vec![srv()]);
    let (out, is_error) = run(r#"return await tools.srv__echo({ text: "hi" });"#, &ctx).await;
    assert!(!is_error, "{out}");
    assert_eq!(out, "srv__echo echoes hi");
}

/// Slice 1: the permission gate still applies inside a program — a denied
/// source tool is refused exactly like a denied built-in.
#[tokio::test]
async fn denied_mcp_source_tool_is_refused_in_a_program() {
    let rules = PermissionRules {
        allow: vec![],
        deny: vec!["srv__echo".into()],
        ask: vec![],
    };
    let perms = Permissions::new(Mode::Default, &rules, std::env::temp_dir(), None, None).unwrap();
    let ctx = with_permissions(test_ctx_with_sources(0, "mcp-deny", vec![srv()]), perms);
    let (out, is_error) = run(
        r#"try { await tools.srv__echo({ text: "x" }); return "RAN"; }
           catch (e) { return "BLOCKED"; }"#,
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(out, "BLOCKED");
}

/// Slice 2: past the defer threshold the source tool is deferred — a top-level
/// direct call bounces on the lock gate — yet it stays callable from inside a
/// program, which bypasses that gate (the tool is exposed on `tools`). Every
/// other gate still runs; here the permission gate allows.
#[tokio::test]
async fn program_calls_a_deferred_mcp_tool_that_top_level_cannot() {
    let ctx = with_defer_threshold(test_ctx_with_sources(0, "mcp-deferred", vec![srv()]), 0);

    // Top-level direct call bounces: the model would have to tool_search first.
    let (out, is_error) = run_tool("srv__echo", json!({ "text": "x" }), &ctx).await;
    assert!(is_error, "{out}");
    assert!(out.contains("deferred and not loaded"), "{out}");

    // The same tool, called from inside a program, runs.
    let (out, is_error) = run(r#"return await tools.srv__echo({ text: "hi" });"#, &ctx).await;
    assert!(!is_error, "{out}");
    assert_eq!(out, "srv__echo echoes hi");
}

/// The program's callable surface always includes source tools, deferred or not.
#[test]
fn program_surface_includes_source_tools() {
    let names = program_tool_names(&[srv()]);
    assert!(names.iter().any(|n| n == "srv__echo"));
    assert!(names.iter().any(|n| n == "srv__danger"));
    assert!(names.iter().any(|n| n == "bash"));
    assert!(!names.iter().any(|n| n == "run_program"));
}

/// Slice 3: an MCP tool with a structured result reaches the program as the
/// `CallToolResult` object (content blocks + structuredContent), not flat text,
/// so the program reads typed fields directly. A built-in in the same program
/// still returns a plain string.
#[tokio::test]
async fn program_receives_structured_calltoolresult_from_mcp() {
    let ctx = test_ctx_with_sources(0, "mcp-structured", vec![srv()]);
    let (out, is_error) = run(
        r#"const r = await tools.srv__data({ text: "hello" });
           const b = typeof (await tools.grep({ pattern: "zzz_nomatch_zzz" }));
           return JSON.stringify({
               structured: r.structuredContent.len,
               echo: r.structuredContent.echo,
               firstBlock: r.content[0].text,
               builtinType: b,
           });"#,
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");
    // structuredContent.len = "hello".len() = 5; content[0].text is the flat
    // text; a built-in tool still resolves to a string.
    assert_eq!(
        out,
        r#"{"structured":5,"echo":"hello","firstBlock":"count=5","builtinType":"string"}"#
    );
}

/// Slice 3: inline source tools are declared returning `Promise<CallToolResult>`
/// (built-ins keep `Promise<string>`), and the `CallToolResult` type is defined.
#[test]
fn run_program_def_types_source_tools_as_calltoolresult() {
    let builtins = vec![ToolDef {
        name: "read_file".into(),
        description: "Read".into(),
        schema: json!({"type": "object"}),
    }];
    let sources = vec![ToolDef {
        name: "srv__data".into(),
        description: "Structured".into(),
        schema: json!({"type": "object"}),
    }];
    let def = run_program_def(&builtins, &sources, &[]);
    let d = &def.description;
    assert!(d.contains("type CallToolResult"), "{d}");
    assert!(
        d.contains("srv__data(args: Record<string, unknown>): Promise<CallToolResult>;"),
        "{d}"
    );
    // Built-ins keep the string contract.
    assert!(
        d.contains("read_file(args: Record<string, unknown>): Promise<string>;"),
        "{d}"
    );
}

/// `glob` is the one built-in whose result is naturally a list: a program gets a
/// `string[]` of paths (not the newline-joined text the model sees), and the
/// declaration says so.
#[tokio::test]
async fn program_receives_glob_paths_as_an_array() {
    let dir = tmp("glob-array");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "x").unwrap();
    std::fs::write(dir.join("b.txt"), "y").unwrap();
    let ctx = test_ctx(0, "glob-array");
    let (out, is_error) = run(
        &format!(
            r#"const files = await tools.glob({{ pattern: "*.txt", path: {dir:?} }});
               return JSON.stringify({{ isArray: Array.isArray(files), count: files.length }});"#,
        ),
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(out, r#"{"isArray":true,"count":2}"#);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_program_def_declares_glob_as_string_array() {
    let def = run_program_def(
        &[ToolDef {
            name: "glob".into(),
            description: "Find files".into(),
            schema: json!({"type": "object"}),
        }],
        &[],
        &[],
    );
    assert!(
        def.description
            .contains("glob(args: Record<string, unknown>): Promise<string[]>;"),
        "{}",
        def.description
    );
}
