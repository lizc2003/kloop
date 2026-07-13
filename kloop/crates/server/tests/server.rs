//! Contract tests for the JSON-RPC server: a scripted Mock provider behind
//! the real serve() loop, driven over in-memory duplex pipes. What a client
//! sends and receives on the wire is asserted verbatim.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::DuplexStream;
use tokio::io::Lines;
use tokio::task::JoinHandle;

use kloop_core::permissions::Mode;
use kloop_core::permissions::PermissionRules;
use kloop_core::permissions::Permissions;
use kloop_core::Config;
use kloop_protocol::ContentBlock;
use kloop_provider::Provider;
use kloop_server::serve;
use kloop_server::ConfigFactory;
use kloop_server::ServerPaths;

struct TestClient {
    writer: DuplexStream,
    lines: Lines<BufReader<DuplexStream>>,
    server: JoinHandle<anyhow::Result<()>>,
}

impl TestClient {
    async fn send(&mut self, value: Value) {
        let line = format!("{value}\n");
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    async fn send_raw(&mut self, raw: &str) {
        self.writer
            .write_all(format!("{raw}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(10), self.lines.next_line())
            .await
            .expect("timed out waiting for a server line")
            .unwrap()
            .expect("server closed the stream");
        serde_json::from_str(&line).unwrap()
    }

    /// Collect messages (in order) until one satisfies the predicate; that
    /// message is included as the last element.
    async fn recv_until(&mut self, mut pred: impl FnMut(&Value) -> bool) -> Vec<Value> {
        let mut got = Vec::new();
        loop {
            let msg = self.recv().await;
            let done = pred(&msg);
            got.push(msg);
            if done {
                return got;
            }
        }
    }

    /// Close the input stream and wait for the server to exit cleanly.
    async fn shutdown(mut self) {
        self.writer.shutdown().await.unwrap();
        drop(self.writer);
        tokio::time::timeout(Duration::from_secs(10), self.server)
            .await
            .expect("server did not exit after input closed")
            .unwrap()
            .unwrap();
    }
}

fn start_server(factory: ConfigFactory, dirs: &TestDirs) -> TestClient {
    let (writer, server_input) = tokio::io::duplex(1 << 16);
    let (server_output, reader) = tokio::io::duplex(1 << 16);
    let paths = ServerPaths {
        sessions_dir: dirs.sessions.clone(),
        offload_dir: dirs.offload.clone(),
    };
    let server = tokio::spawn(serve(server_input, server_output, factory, paths));
    TestClient {
        writer,
        lines: BufReader::new(reader).lines(),
        server,
    }
}

struct TestDirs {
    root: PathBuf,
    sessions: PathBuf,
    offload: PathBuf,
}

fn test_dirs(tag: &str) -> TestDirs {
    let root = std::env::temp_dir().join(format!("kloop-server-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    TestDirs {
        sessions: root.join("sessions"),
        offload: root.join("offload"),
        root,
    }
}

fn text(t: &str) -> ContentBlock {
    ContentBlock::Text { text: t.into() }
}

/// Factory over a scripted Mock provider; every thread gets its own copy of
/// the script. `gated` = a real Default-mode permission gate wired to the
/// server's approver (approvals go out as approval/request); otherwise the
/// gate is wide open.
fn factory(turns: Vec<Vec<ContentBlock>>, offload: PathBuf, gated: bool) -> ConfigFactory {
    Arc::new(move |approver, _notify| {
        let permissions = if gated {
            Permissions::new(
                Mode::Default,
                &PermissionRules::default(),
                std::env::temp_dir(),
                Some(approver),
                None,
            )?
        } else {
            Permissions::allow_all()
        };
        Ok(Config {
            provider: Arc::new(Provider::mock(turns.clone())),
            model: "mock".into(),
            system: "test".into(),
            project_instructions: None,
            max_rounds: 10,
            offload_dir: offload.clone(),
            context_window: None,
            fallback_model: None,
            permissions: Arc::new(permissions),
            tool_sources: Vec::new(),
            session_id: String::new(),
            agent_label: String::new(),
            hooks: std::sync::Arc::new(kloop_core::hooks::Hooks::none()),
            background_shells: kloop_core::tools::BackgroundShells::new(),
            sandbox: None,
            agent_types: std::sync::Arc::new(Vec::new()),
            tool_allowlist: None,
            defer_threshold: 30,
            unlocked_tools: Default::default(),
            todos: Default::default(),
        })
    })
}

fn methods_for_thread<'a>(log: &'a [Value], thread_id: &str) -> Vec<&'a str> {
    log.iter()
        .filter(|m| m["params"]["threadId"] == thread_id)
        .filter_map(|m| m["method"].as_str())
        .collect()
}

#[tokio::test]
async fn turn_streams_deltas_and_completes() {
    let dirs = test_dirs("stream");
    let mut client = start_server(
        factory(vec![vec![text("hello there")]], dirs.offload.clone(), false),
        &dirs,
    );

    client
        .send(json!({"id": 1, "method": "thread/start", "params": {}}))
        .await;
    let started = client.recv().await;
    assert_eq!(started["id"], 1);
    let thread_id = started["result"]["threadId"].as_str().unwrap().to_string();

    client
        .send(json!({"id": 2, "method": "turn/start", "params": {"threadId": thread_id, "input": "hi"}}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;

    // The turn/start response and worker notifications share one pipe; the
    // response must be there, and the thread's notification subsequence must
    // be started -> deltas -> completed.
    assert!(log.iter().any(|m| m["id"] == 2 && m["result"] == json!({})));
    assert_eq!(
        methods_for_thread(&log, &thread_id),
        vec!["turn/started", "text/delta", "turn/completed"]
    );
    let delta_text: String = log
        .iter()
        .filter(|m| m["method"] == "text/delta")
        .map(|m| m["params"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(delta_text, "hello there");
    assert_eq!(log.last().unwrap()["params"]["reason"], "completed");

    client.shutdown().await;
    // The session was persisted: user message + assistant reply.
    let messages =
        kloop_core::rollout::load_session(&dirs.sessions.join(format!("{thread_id}.jsonl")))
            .unwrap();
    assert_eq!(messages.len(), 2);
    let _ = std::fs::remove_dir_all(&dirs.root);
}

/// A todo_write call surfaces as a `todo/updated` notification carrying the
/// full list; the main agent's carries no "agent" field.
#[tokio::test]
async fn todo_write_emits_a_todo_updated_notification() {
    let dirs = test_dirs("todo");
    let turns = vec![
        vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "todo_write".into(),
            input: json!({"todos": [
                {"content": "Parse", "activeForm": "Parsing", "status": "in_progress"},
                {"content": "Test", "activeForm": "Testing", "status": "pending"},
            ]}),
        }],
        vec![text("done")],
    ];
    let mut client = start_server(factory(turns, dirs.offload.clone(), false), &dirs);

    client
        .send(json!({"id": 1, "method": "thread/start", "params": {}}))
        .await;
    let thread_id = client.recv().await["result"]["threadId"]
        .as_str()
        .unwrap()
        .to_string();
    client
        .send(json!({"id": 2, "method": "turn/start", "params": {"threadId": thread_id, "input": "go"}}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;

    let todo = log
        .iter()
        .find(|m| m["method"] == "todo/updated")
        .expect("todo/updated notification");
    assert_eq!(todo["params"]["threadId"], thread_id);
    assert_eq!(
        todo["params"]["todos"],
        json!([
            {"content": "Parse", "activeForm": "Parsing", "status": "in_progress"},
            {"content": "Test", "activeForm": "Testing", "status": "pending"},
        ])
    );
    assert!(
        todo["params"].get("agent").is_none(),
        "the main agent's list carries no agent field"
    );

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

/// A task call surfaces the sub-agent lifecycle on the wire: agent/started
/// and agent/completed bracket it, and the sub-agent's own tool calls carry
/// an "agent" field while the main agent's calls stay unchanged.
#[tokio::test]
async fn subagent_notifications_carry_the_agent_label() {
    let dirs = test_dirs("subagent");
    let script = vec![
        // main: spawn the sub-agent
        vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "task".into(),
            input: json!({"prompt": "sub work"}),
        }],
        // consumed by the sub-agent: one tool call, then its answer
        vec![ContentBlock::ToolUse {
            id: "s1".into(),
            name: "bash".into(),
            input: json!({"command": "echo hi"}),
        }],
        vec![text("sub result")],
        // main wraps up
        vec![text("done")],
    ];
    let mut client = start_server(factory(script, dirs.offload.clone(), false), &dirs);

    client
        .send(json!({"id": 1, "method": "thread/start", "params": {}}))
        .await;
    let thread_id = client.recv().await["result"]["threadId"]
        .as_str()
        .unwrap()
        .to_string();
    client
        .send(json!({"id": 2, "method": "turn/start", "params": {"threadId": thread_id, "input": "delegate"}}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;

    let started = log
        .iter()
        .find(|m| m["method"] == "agent/started")
        .expect("agent/started notification");
    let label = started["params"]["agent"].as_str().unwrap().to_string();
    assert!(label.starts_with("agent-"), "got {label}");
    assert_eq!(started["params"]["task"], "sub work");

    // The main agent's task row has no agent field; the sub-agent's bash
    // row (and its completion) carries the label.
    let task_row = log
        .iter()
        .find(|m| m["method"] == "tool/started" && m["params"]["name"] == "task")
        .unwrap();
    assert!(task_row["params"]["agent"].is_null());
    let bash_row = log
        .iter()
        .find(|m| m["method"] == "tool/started" && m["params"]["name"] == "bash")
        .unwrap();
    assert_eq!(bash_row["params"]["agent"], label.as_str());
    assert!(log.iter().any(|m| m["method"] == "tool/completed"
        && m["params"]["agent"] == label.as_str()
        && m["params"]["ok"] == true));
    assert!(log.iter().any(|m| m["method"] == "agent/completed"
        && m["params"]["agent"] == label.as_str()
        && m["params"]["ok"] == true));

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn approval_denied_then_allowed() {
    let dirs = test_dirs("approval");
    let target = dirs.root.join("approved.txt");
    let tool_turn = vec![ContentBlock::ToolUse {
        id: "t1".into(),
        name: "write_file".into(),
        input: json!({"path": target.to_str().unwrap(), "content": "x"}),
    }];
    let script = vec![
        tool_turn.clone(),
        vec![text("after deny")],
        tool_turn,
        vec![text("after allow")],
    ];
    let mut client = start_server(factory(script, dirs.offload.clone(), true), &dirs);

    client
        .send(json!({"id": 1, "method": "thread/start", "params": {}}))
        .await;
    let thread_id = client.recv().await["result"]["threadId"]
        .as_str()
        .unwrap()
        .to_string();

    // Turn 1: the write_file call must come back as a server request.
    client
        .send(json!({"id": 2, "method": "turn/start", "params": {"threadId": thread_id, "input": "write it"}}))
        .await;
    let log = client
        .recv_until(|m| m["method"] == "approval/request")
        .await;
    let request = log.last().unwrap();
    let srv_id = request["id"].as_str().unwrap().to_string();
    assert!(srv_id.starts_with("srv-"));
    assert!(request["params"]["description"]
        .as_str()
        .unwrap()
        .contains("write_file"));
    assert!(request["params"]["rememberRules"].is_array());

    client
        .send(json!({"id": srv_id, "result": {"decision": "deny"}}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert!(
        log.iter()
            .any(|m| m["method"] == "tool/completed" && m["params"]["ok"] == false),
        "denied call must surface as a failed tool row: {log:?}"
    );
    assert!(!target.exists(), "deny must block the write");

    // Turn 2: same call, allowed this time.
    client
        .send(json!({"id": 3, "method": "turn/start", "params": {"threadId": thread_id, "input": "again"}}))
        .await;
    let log = client
        .recv_until(|m| m["method"] == "approval/request")
        .await;
    let srv_id = log.last().unwrap()["id"].as_str().unwrap().to_string();
    client
        .send(json!({"id": srv_id, "result": {"decision": "allow"}}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert!(log
        .iter()
        .any(|m| m["method"] == "tool/completed" && m["params"]["ok"] == true));
    assert!(target.exists(), "allow must let the write through");

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn parallel_threads_do_not_cross_streams() {
    let dirs = test_dirs("parallel");
    let mut client = start_server(
        factory(vec![vec![text("reply")]], dirs.offload.clone(), false),
        &dirs,
    );

    client
        .send(json!({"id": 1, "method": "thread/start", "params": {}}))
        .await;
    let t1 = client.recv().await["result"]["threadId"]
        .as_str()
        .unwrap()
        .to_string();
    client
        .send(json!({"id": 2, "method": "thread/start", "params": {}}))
        .await;
    let t2 = client.recv().await["result"]["threadId"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(t1, t2, "same-second thread ids must not collide");

    client
        .send(json!({"id": 3, "method": "turn/start", "params": {"threadId": t1, "input": "a"}}))
        .await;
    client
        .send(json!({"id": 4, "method": "turn/start", "params": {"threadId": t2, "input": "b"}}))
        .await;

    let mut completed = 0;
    let log = client
        .recv_until(|m| {
            if m["method"] == "turn/completed" {
                completed += 1;
            }
            completed == 2
        })
        .await;

    for tid in [&t1, &t2] {
        assert_eq!(
            methods_for_thread(&log, tid),
            vec!["turn/started", "text/delta", "turn/completed"],
            "thread {tid} event stream is broken: {log:?}"
        );
    }
    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn second_turn_while_running_is_rejected_and_interrupt_aborts() {
    let dirs = test_dirs("busy");
    // The tool call hangs at the approval gate until we answer or interrupt.
    let script = vec![vec![ContentBlock::ToolUse {
        id: "t1".into(),
        name: "write_file".into(),
        input: json!({"path": dirs.root.join("f").to_str().unwrap(), "content": "x"}),
    }]];
    let mut client = start_server(factory(script, dirs.offload.clone(), true), &dirs);

    client
        .send(json!({"id": 1, "method": "thread/start", "params": {}}))
        .await;
    let thread_id = client.recv().await["result"]["threadId"]
        .as_str()
        .unwrap()
        .to_string();
    client
        .send(json!({"id": 2, "method": "turn/start", "params": {"threadId": thread_id, "input": "go"}}))
        .await;
    client
        .recv_until(|m| m["method"] == "approval/request")
        .await;

    // Busy thread rejects a second turn.
    client
        .send(json!({"id": 3, "method": "turn/start", "params": {"threadId": thread_id, "input": "more"}}))
        .await;
    let log = client.recv_until(|m| m["id"] == 3).await;
    assert!(log.last().unwrap()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("already running"));

    // Interrupt instead of answering the approval.
    client
        .send(json!({"id": 4, "method": "turn/interrupt", "params": {"threadId": thread_id}}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert_eq!(log.last().unwrap()["params"]["reason"], "aborted");

    // The thread is usable again: mock script is exhausted, which surfaces
    // as an error turn — but it must start and complete.
    client
        .send(json!({"id": 5, "method": "turn/start", "params": {"threadId": thread_id, "input": "next"}}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert!(log.iter().any(|m| m["id"] == 5 && m["result"] == json!({})));

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn protocol_errors_do_not_kill_the_server() {
    let dirs = test_dirs("errors");
    let mut client = start_server(
        factory(vec![vec![text("ok")]], dirs.offload.clone(), false),
        &dirs,
    );

    client.send_raw("this is not json").await;
    let err = client.recv().await;
    assert_eq!(err["id"], Value::Null);
    assert_eq!(err["error"]["code"], -32700);

    client
        .send(json!({"id": 1, "method": "no/such/method", "params": {}}))
        .await;
    let err = client.recv().await;
    assert_eq!(err["id"], 1);
    assert_eq!(err["error"]["code"], -32601);

    client
        .send(
            json!({"id": 2, "method": "turn/start", "params": {"threadId": "ghost", "input": "x"}}),
        )
        .await;
    let err = client.recv().await;
    assert_eq!(err["error"]["code"], -32000);

    client
        .send(json!({"id": 3, "method": "thread/resume", "params": {"threadId": "ghost"}}))
        .await;
    let err = client.recv().await;
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no session"));

    client
        .send(json!({"id": 4, "method": "turn/start", "params": {}}))
        .await;
    let err = client.recv().await;
    assert_eq!(err["error"]["code"], -32602);

    // An approval response nobody asked for.
    client
        .send(json!({"id": "srv-99", "result": {"decision": "allow"}}))
        .await;
    let err = client.recv().await;
    assert_eq!(err["error"]["code"], -32000);

    // After all that abuse, normal work still runs.
    client
        .send(json!({"id": 5, "method": "thread/start", "params": {}}))
        .await;
    let thread_id = client.recv().await["result"]["threadId"]
        .as_str()
        .unwrap()
        .to_string();
    client
        .send(json!({"id": 6, "method": "turn/start", "params": {"threadId": thread_id, "input": "x"}}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert_eq!(log.last().unwrap()["params"]["reason"], "completed");

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn sessions_survive_a_server_restart() {
    let dirs = test_dirs("restart");
    let script = vec![vec![text("first answer")], vec![text("second answer")]];

    // Server instance one: create a thread, run a turn, shut down.
    let mut client = start_server(factory(script.clone(), dirs.offload.clone(), false), &dirs);
    client
        .send(json!({"id": 1, "method": "thread/start", "params": {}}))
        .await;
    let thread_id = client.recv().await["result"]["threadId"]
        .as_str()
        .unwrap()
        .to_string();
    client
        .send(json!({"id": 2, "method": "turn/start", "params": {"threadId": thread_id, "input": "q1"}}))
        .await;
    client.recv_until(|m| m["method"] == "turn/completed").await;
    client.shutdown().await;

    // Server instance two over the same directories.
    let mut client = start_server(factory(script, dirs.offload.clone(), false), &dirs);
    client
        .send(json!({"id": 1, "method": "thread/list", "params": {}}))
        .await;
    let list = client.recv().await;
    let threads = list["result"]["threads"].as_array().unwrap();
    let entry = threads
        .iter()
        .find(|t| t["id"] == thread_id.as_str())
        .expect("restarted server must list the old session");
    assert_eq!(entry["messages"], 2);
    assert_eq!(entry["snippet"], "q1");

    client
        .send(json!({"id": 2, "method": "thread/resume", "params": {"threadId": thread_id}}))
        .await;
    let resumed = client.recv().await;
    assert_eq!(resumed["result"]["messageCount"], 2);

    // Resuming twice is an error (already active).
    client
        .send(json!({"id": 3, "method": "thread/resume", "params": {"threadId": thread_id}}))
        .await;
    let err = client.recv().await;
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("already active"));

    client
        .send(json!({"id": 4, "method": "turn/start", "params": {"threadId": thread_id, "input": "q2"}}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert_eq!(log.last().unwrap()["params"]["reason"], "completed");

    client.shutdown().await;
    let messages =
        kloop_core::rollout::load_session(&dirs.sessions.join(format!("{thread_id}.jsonl")))
            .unwrap();
    assert_eq!(messages.len(), 4, "both turns persisted across the restart");
    let _ = std::fs::remove_dir_all(&dirs.root);
}
