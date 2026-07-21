//! Contract tests for the native agent protocol server: a scripted Mock
//! provider behind the real serve() loop, driven over in-memory duplex pipes.
//! What a client sends and receives on the wire is asserted verbatim. Every
//! session opens with the `initialize` handshake (the gate the server enforces
//! before any other method).

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
use kloop_server::PROTOCOL_VERSION;

struct TestClient {
    writer: DuplexStream,
    lines: Lines<BufReader<DuplexStream>>,
    server: JoinHandle<anyhow::Result<()>>,
    next_id: i64,
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

    /// Send a request with an auto-allocated id and return that id (so a test
    /// can match the response without hand-tracking ids).
    async fn request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        id
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

    /// The `initialize` handshake every session opens with; returns the
    /// server's capabilities object. Asserts the protocol version echoes back.
    async fn initialize(&mut self) -> Value {
        let id = self
            .request(
                "initialize",
                json!({
                    "clientInfo": {"name": "test", "version": "0"},
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                }),
            )
            .await;
        let resp = self.recv().await;
        assert_eq!(resp["id"], id);
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        resp["result"]["capabilities"].clone()
    }

    /// Handshake, then start a fresh thread; returns its id.
    async fn init_and_start(&mut self) -> String {
        self.initialize().await;
        let id = self.request("thread/start", json!({})).await;
        let resp = self.recv().await;
        assert_eq!(resp["id"], id);
        resp["result"]["thread"]["id"].as_str().unwrap().to_string()
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
        next_id: 1,
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
/// the script. `gated` = a real Manual-mode permission gate wired to the
/// server's approver (approvals go out as approval/request); otherwise the
/// gate is wide open.
fn factory(turns: Vec<Vec<ContentBlock>>, offload: PathBuf, gated: bool) -> ConfigFactory {
    Arc::new(move |approver, _notify| {
        let permissions = if gated {
            Permissions::new(
                Mode::Manual,
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
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            offload_dir: offload.clone(),
            // Siblings under the same test root (test_dirs), matching the
            // ServerPaths the server lists/creates threads from.
            sessions_dir: offload.with_file_name("sessions"),
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
            inbox: Default::default(),
            background_tasks: Default::default(),
            program_limits: Default::default(),
            skills: Default::default(),
            active_worktree: std::sync::Arc::new(std::sync::RwLock::new(None)),
            worktree_enabled: false,
        })
    })
}

fn tool_use(id: &str, name: &str, input: Value) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }
}

/// A throwaway git repo (canonical path) to serve as a thread's cwd, so
/// enter_worktree has somewhere to branch from.
fn temp_git_repo(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kloop-srv-wt-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    for a in [
        &["init", "-q"][..],
        &["config", "user.email", "t@e.com"],
        &["config", "user.name", "t"],
        &["commit", "--allow-empty", "-qm", "base"],
    ] {
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(a)
            .status()
            .unwrap()
            .success());
    }
    std::fs::canonicalize(&root).unwrap()
}

/// Like `factory` but with worktree mode ON and cwd pointed at a real git repo
/// (allow_all gate — the worktree tools auto-allow anyway).
fn worktree_factory(
    turns: Vec<Vec<ContentBlock>>,
    offload: PathBuf,
    cwd: PathBuf,
) -> ConfigFactory {
    Arc::new(move |_approver, _notify| {
        Ok(Config {
            provider: Arc::new(Provider::mock(turns.clone())),
            model: "mock".into(),
            system: "test".into(),
            project_instructions: None,
            max_rounds: 10,
            cwd: cwd.clone(),
            offload_dir: offload.clone(),
            sessions_dir: offload.with_file_name("sessions"),
            context_window: None,
            fallback_model: None,
            permissions: Arc::new(Permissions::allow_all()),
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
            inbox: Default::default(),
            background_tasks: Default::default(),
            program_limits: Default::default(),
            skills: Default::default(),
            active_worktree: std::sync::Arc::new(std::sync::RwLock::new(None)),
            worktree_enabled: true,
        })
    })
}

fn methods_for_thread<'a>(log: &'a [Value], thread_id: &str) -> Vec<&'a str> {
    log.iter()
        .filter(|m| m["params"]["threadId"] == thread_id)
        .filter_map(|m| m["method"].as_str())
        .collect()
}

/// The handshake gates every other method, negotiates the protocol version,
/// and reports structured capabilities.
#[tokio::test]
async fn handshake_gates_and_negotiates() {
    let dirs = test_dirs("handshake");
    let mut client = start_server(
        factory(vec![vec![text("ok")]], dirs.offload.clone(), false),
        &dirs,
    );

    // A method before `initialize` is rejected.
    client.request("thread/start", json!({})).await;
    let err = client.recv().await;
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not initialized"));

    // A version we don't speak is a hard error (no silent downgrade).
    client
        .request(
            "initialize",
            json!({"protocolVersion": "0.9", "capabilities": {}}),
        )
        .await;
    let err = client.recv().await;
    assert_eq!(err["error"]["code"], -32602);
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unsupported protocolVersion"));

    // A good handshake reports capabilities and unlocks the rest.
    let caps = client.initialize().await;
    assert_eq!(caps["streaming"], true);
    assert_eq!(caps["approvals"], true);
    let id = client.request("thread/start", json!({})).await;
    let resp = client.recv().await;
    assert_eq!(resp["id"], id);
    assert!(resp["result"]["thread"]["id"].is_string());

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

/// A thread whose model enters a worktree, writes inside it, and exits: the
/// client gets `thread/cwd/updated` notifications on both switches, the write
/// lands in the tree (not the main repo), and the dirty tree is kept on exit.
#[tokio::test]
async fn worktree_enter_write_exit_notifies_and_isolates() {
    let dirs = test_dirs("worktree");
    let repo = temp_git_repo("srv");
    let turns = vec![
        vec![tool_use("t1", "enter_worktree", json!({"name": "srv"}))],
        vec![tool_use(
            "t2",
            "write_file",
            json!({"path": "s.txt", "content": "S"}),
        )],
        vec![tool_use("t3", "exit_worktree", json!({}))],
        vec![text("done")],
    ];
    let mut client = start_server(
        worktree_factory(turns, dirs.offload.clone(), repo.clone()),
        &dirs,
    );

    let thread_id = client.init_and_start().await;
    client
        .request("turn/start", json!({"threadId": thread_id, "input": "go"}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;

    // Two thread/cwd/updated notifications: enter (branch set) then exit.
    let wt: Vec<&Value> = log
        .iter()
        .filter(|m| m["method"] == "thread/cwd/updated")
        .collect();
    assert_eq!(wt.len(), 2, "enter + exit notifications");
    assert_eq!(wt[0]["params"]["branch"], "kloop/worktree/srv");
    assert!(wt[0]["params"]["cwd"]
        .as_str()
        .unwrap()
        .ends_with(".kloop-worktrees/srv"));
    assert_eq!(wt[1]["params"]["branch"], Value::Null);

    // The write landed in the worktree, not the main repo; the dirty tree is
    // kept (model exited with default keep).
    assert!(repo.join(".kloop-worktrees/srv/s.txt").exists());
    assert!(!repo.join("s.txt").exists());

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&repo);
}

/// A model that enters a worktree but never exits: the tree is torn down (or
/// kept if dirty) when the thread ends on server shutdown, not leaked.
#[tokio::test]
async fn unexited_worktree_is_cleaned_up_on_shutdown() {
    let dirs = test_dirs("wt-shutdown");
    let repo = temp_git_repo("noexit");
    let turns = vec![
        vec![tool_use("t1", "enter_worktree", json!({"name": "leak"}))],
        vec![text("entered, not exiting")],
    ];
    let mut client = start_server(
        worktree_factory(turns, dirs.offload.clone(), repo.clone()),
        &dirs,
    );
    let thread_id = client.init_and_start().await;
    client
        .request("turn/start", json!({"threadId": thread_id, "input": "go"}))
        .await;
    client.recv_until(|m| m["method"] == "turn/completed").await;
    assert!(
        repo.join(".kloop-worktrees/leak").exists(),
        "tree exists mid-session"
    );

    // Shutdown closes the turn channel; the worker tears down the clean tree.
    client.shutdown().await;
    assert!(
        !repo.join(".kloop-worktrees/leak").exists(),
        "clean tree removed on shutdown"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

#[tokio::test]
async fn turn_streams_item_events_and_completes() {
    let dirs = test_dirs("stream");
    let mut client = start_server(
        factory(vec![vec![text("hello there")]], dirs.offload.clone(), false),
        &dirs,
    );

    let thread_id = client.init_and_start().await;
    let turn_id = client
        .request("turn/start", json!({"threadId": thread_id, "input": "hi"}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;

    // The turn/start response carries the allocated turn id; the thread's
    // notifications are the assistant message's full item lifecycle bracketed
    // by turn/started and turn/completed, with a token-usage update before the
    // close.
    let turn = log
        .iter()
        .find(|m| m["id"] == turn_id)
        .expect("turn/start response");
    let allocated = turn["result"]["turn"]["id"].as_u64().unwrap();
    assert_eq!(
        methods_for_thread(&log, &thread_id),
        vec![
            "turn/started",
            "item/started",
            "item/delta",
            "item/completed",
            "thread/tokenUsage/updated",
            "turn/completed",
        ]
    );
    // Every item event carries the turn id.
    for m in &log {
        if let Some(t) = m["params"]["turnId"].as_u64() {
            assert_eq!(t, allocated);
        }
    }
    // The delta streamed the assistant text; the completed item carries the
    // finalized text.
    let delta = log
        .iter()
        .find(|m| m["method"] == "item/delta")
        .expect("a text delta");
    assert_eq!(delta["params"]["channel"], "text");
    assert_eq!(delta["params"]["text"], "hello there");
    let completed = log
        .iter()
        .find(|m| m["method"] == "item/completed")
        .unwrap();
    assert_eq!(completed["params"]["item"]["type"], "assistantMessage");
    assert_eq!(completed["params"]["item"]["text"], "hello there");
    assert_eq!(log.last().unwrap()["params"]["turn"]["status"], "completed");

    client.shutdown().await;
    // The session was persisted: user message + assistant reply.
    let messages =
        kloop_core::rollout::load_session(&dirs.sessions.join(format!("{thread_id}.jsonl")))
            .unwrap();
    assert_eq!(messages.len(), 2);
    let _ = std::fs::remove_dir_all(&dirs.root);
}

/// A todo_write call surfaces as a `todo` item (not a generic tool row)
/// carrying the full list; the main agent's carries no "agent" field, and there
/// is no separate `toolCall` item for it (core no longer double-emits).
#[tokio::test]
async fn todo_write_surfaces_as_a_todo_item() {
    let dirs = test_dirs("todo");
    let turns = vec![
        vec![tool_use(
            "t1",
            "todo_write",
            json!({"todos": [
                {"content": "Parse", "activeForm": "Parsing", "status": "in_progress"},
                {"content": "Test", "activeForm": "Testing", "status": "pending"},
            ]}),
        )],
        vec![text("done")],
    ];
    let mut client = start_server(factory(turns, dirs.offload.clone(), false), &dirs);

    let thread_id = client.init_and_start().await;
    client
        .request("turn/start", json!({"threadId": thread_id, "input": "go"}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;

    // No toolCall item for todo_write — only the todo item.
    assert!(
        !log.iter().any(|m| m["params"]["item"]["type"] == "toolCall"
            && m["params"]["item"]["name"] == "todo_write"),
        "todo_write must not surface as a tool row: {log:?}"
    );
    let todo = log
        .iter()
        .find(|m| m["params"]["item"]["type"] == "todo")
        .expect("a todo item");
    assert_eq!(todo["method"], "item/completed");
    assert_eq!(
        todo["params"]["item"]["todos"],
        json!([
            {"content": "Parse", "activeForm": "Parsing", "status": "in_progress"},
            {"content": "Test", "activeForm": "Testing", "status": "pending"},
        ])
    );
    assert!(
        todo["params"]["item"].get("agent").is_none(),
        "the main agent's list carries no agent field"
    );

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

/// A task call surfaces the sub-agent lifecycle on the wire: a `subAgent` item
/// brackets it, and the sub-agent's own tool calls carry an "agent" field while
/// the main agent's calls stay unadorned.
#[tokio::test]
async fn subagent_items_carry_the_agent_label() {
    let dirs = test_dirs("subagent");
    let script = vec![
        // main: spawn the sub-agent
        vec![tool_use("t1", "task", json!({"prompt": "sub work"}))],
        // consumed by the sub-agent: one tool call, then its answer
        vec![tool_use("s1", "bash", json!({"command": "echo hi"}))],
        vec![text("sub result")],
        // main wraps up
        vec![text("done")],
    ];
    let mut client = start_server(factory(script, dirs.offload.clone(), false), &dirs);

    let thread_id = client.init_and_start().await;
    client
        .request(
            "turn/start",
            json!({"threadId": thread_id, "input": "delegate"}),
        )
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;

    let started = log
        .iter()
        .find(|m| m["method"] == "item/started" && m["params"]["item"]["type"] == "subAgent")
        .expect("subAgent started item");
    let label = started["params"]["item"]["label"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(label.starts_with("agent-"), "got {label}");
    assert_eq!(started["params"]["item"]["task"], "sub work");

    // The main agent's task row has no agent field; the sub-agent's bash row
    // (and its completion) carries the label.
    let task_row = log
        .iter()
        .find(|m| {
            m["params"]["item"]["type"] == "toolCall" && m["params"]["item"]["name"] == "task"
        })
        .unwrap();
    assert!(task_row["params"]["item"].get("agent").is_none());
    let bash_row = log
        .iter()
        .find(|m| {
            m["params"]["item"]["type"] == "toolCall" && m["params"]["item"]["name"] == "bash"
        })
        .unwrap();
    assert_eq!(bash_row["params"]["item"]["agent"], label.as_str());
    assert!(log.iter().any(|m| m["method"] == "item/completed"
        && m["params"]["item"]["type"] == "subAgent"
        && m["params"]["item"]["label"] == label.as_str()
        && m["params"]["item"]["status"] == "completed"));

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn approval_declined_then_accepted() {
    let dirs = test_dirs("approval");
    let target = dirs.root.join("approved.txt");
    let tool_turn = vec![tool_use(
        "t1",
        "write_file",
        json!({"path": target.to_str().unwrap(), "content": "x"}),
    )];
    let script = vec![
        tool_turn.clone(),
        vec![text("after deny")],
        tool_turn,
        vec![text("after allow")],
    ];
    let mut client = start_server(factory(script, dirs.offload.clone(), true), &dirs);

    let thread_id = client.init_and_start().await;

    // Turn 1: the write_file call must come back as a server reverse request
    // with a fileChange kind (it carries a diff preview).
    client
        .request(
            "turn/start",
            json!({"threadId": thread_id, "input": "write it"}),
        )
        .await;
    let log = client
        .recv_until(|m| m["method"] == "approval/request")
        .await;
    let request = log.last().unwrap();
    let srv_id = request["id"].as_i64().unwrap();
    assert_eq!(request["params"]["kind"], "fileChange");
    assert!(request["params"]["description"]
        .as_str()
        .unwrap()
        .contains("write_file"));
    assert!(request["params"]["rememberRules"].is_array());
    // The change preview reaches the client (a fresh path is a new file).
    assert_eq!(request["params"]["preview"], "(new file)\n+1  x");

    client
        .send(json!({"jsonrpc": "2.0", "id": srv_id, "result": {"decision": "decline"}}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert!(
        log.iter().any(|m| m["method"] == "item/completed"
            && m["params"]["item"]["type"] == "toolCall"
            && m["params"]["item"]["status"] == "failed"),
        "declined call must surface as a failed tool item: {log:?}"
    );
    assert!(!target.exists(), "decline must block the write");

    // Turn 2: same call, accepted this time.
    client
        .request(
            "turn/start",
            json!({"threadId": thread_id, "input": "again"}),
        )
        .await;
    let log = client
        .recv_until(|m| m["method"] == "approval/request")
        .await;
    let srv_id = log.last().unwrap()["id"].as_i64().unwrap();
    client
        .send(json!({"jsonrpc": "2.0", "id": srv_id, "result": {"decision": "accept"}}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert!(log.iter().any(|m| m["method"] == "item/completed"
        && m["params"]["item"]["type"] == "toolCall"
        && m["params"]["item"]["status"] == "completed"));
    assert!(target.exists(), "accept must let the write through");

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

/// `turn/steer` pushed while a turn is hung at an approval gate is delivered
/// into that same turn: the worker drains the inbox at the next round boundary,
/// after the tool executes. Proves the server wires steering into a *running*
/// turn (not just queued for the next one), and that steer echoes the running
/// turn's id.
#[tokio::test]
async fn steer_folds_into_a_running_turn() {
    let dirs = test_dirs("steer");
    let target = dirs.root.join("steer.txt");
    let script = vec![
        // Round 0: a write_file that hangs at the approval gate.
        vec![tool_use(
            "t1",
            "write_file",
            json!({"path": target.to_str().unwrap(), "content": "x"}),
        )],
        // Round 1 (after the tool runs and the steer drains): wrap up.
        vec![text("done")],
    ];
    let mut client = start_server(factory(script, dirs.offload.clone(), true), &dirs);

    let thread_id = client.init_and_start().await;
    let start_id = client
        .request("turn/start", json!({"threadId": thread_id, "input": "go"}))
        .await;
    let log = client
        .recv_until(|m| m["method"] == "approval/request")
        .await;
    let turn_id = log.iter().find(|m| m["id"] == start_id).unwrap()["result"]["turn"]["id"]
        .as_u64()
        .unwrap();
    let srv_id = log.last().unwrap()["id"].as_i64().unwrap();

    // Steer while the turn is parked at the gate; the server pushes it to the
    // thread's inbox and acks with the running turn's id.
    let steer_id = client
        .request(
            "turn/steer",
            json!({"threadId": thread_id, "input": "also check the logs"}),
        )
        .await;
    let log = client.recv_until(|m| m["id"] == steer_id).await;
    assert_eq!(
        log.iter().find(|m| m["id"] == steer_id).unwrap()["result"]["turnId"],
        turn_id
    );

    // Release the tool; the turn resumes, drains the steer, and completes.
    client
        .send(json!({"jsonrpc": "2.0", "id": srv_id, "result": {"decision": "accept"}}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert_eq!(log.last().unwrap()["params"]["turn"]["status"], "completed");

    client.shutdown().await;
    let messages =
        kloop_core::rollout::load_session(&dirs.sessions.join(format!("{thread_id}.jsonl")))
            .unwrap();
    let steered = messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(b, ContentBlock::Text { text }
                if text.contains(kloop_core::inbox::STEERING_PREFIX)
                    && text.contains("also check the logs"))
        })
    });
    assert!(
        steered,
        "the steer must be delivered as a framed user message in the turn: {messages:?}"
    );
    let _ = std::fs::remove_dir_all(&dirs.root);
}

/// `turn/steer` to a nonexistent thread is a clean error, not a panic.
#[tokio::test]
async fn steer_to_a_missing_thread_errors() {
    let dirs = test_dirs("steer-missing");
    let mut client = start_server(
        factory(vec![vec![text("ok")]], dirs.offload.clone(), false),
        &dirs,
    );
    client.initialize().await;
    let id = client
        .request("turn/steer", json!({"threadId": "ghost", "input": "hi"}))
        .await;
    let err = client.recv().await;
    assert_eq!(err["id"], id);
    assert_eq!(err["error"]["code"], -32000);
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

    client.initialize().await;
    let s1 = client.request("thread/start", json!({})).await;
    let t1 = client.recv().await["result"]["thread"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let _ = s1;
    client.request("thread/start", json!({})).await;
    let t2 = client.recv().await["result"]["thread"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(t1, t2, "same-second thread ids must not collide");

    client
        .request("turn/start", json!({"threadId": t1, "input": "a"}))
        .await;
    client
        .request("turn/start", json!({"threadId": t2, "input": "b"}))
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
            vec![
                "turn/started",
                "item/started",
                "item/delta",
                "item/completed",
                "thread/tokenUsage/updated",
                "turn/completed",
            ],
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
    let script = vec![vec![tool_use(
        "t1",
        "write_file",
        json!({"path": dirs.root.join("f").to_str().unwrap(), "content": "x"}),
    )]];
    let mut client = start_server(factory(script, dirs.offload.clone(), true), &dirs);

    let thread_id = client.init_and_start().await;
    client
        .request("turn/start", json!({"threadId": thread_id, "input": "go"}))
        .await;
    client
        .recv_until(|m| m["method"] == "approval/request")
        .await;

    // Busy thread rejects a second turn.
    let busy_id = client
        .request(
            "turn/start",
            json!({"threadId": thread_id, "input": "more"}),
        )
        .await;
    let log = client.recv_until(|m| m["id"] == busy_id).await;
    assert!(log.last().unwrap()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("already running"));

    // Interrupt instead of answering the approval.
    client
        .request("turn/interrupt", json!({"threadId": thread_id}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert_eq!(log.last().unwrap()["params"]["turn"]["status"], "aborted");

    // The thread is usable again: mock script is exhausted, which surfaces
    // as an error turn — but it must start and complete.
    let next_id = client
        .request(
            "turn/start",
            json!({"threadId": thread_id, "input": "next"}),
        )
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert!(log
        .iter()
        .any(|m| m["id"] == next_id && m["result"]["turn"]["id"].is_u64()));

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
    client.initialize().await;

    client.send_raw("this is not json").await;
    let err = client.recv().await;
    assert_eq!(err["id"], Value::Null);
    assert_eq!(err["error"]["code"], -32700);

    let id = client.request("no/such/method", json!({})).await;
    let err = client.recv().await;
    assert_eq!(err["id"], id);
    assert_eq!(err["error"]["code"], -32601);

    client
        .request("turn/start", json!({"threadId": "ghost", "input": "x"}))
        .await;
    let err = client.recv().await;
    assert_eq!(err["error"]["code"], -32000);

    client
        .request("thread/resume", json!({"threadId": "ghost"}))
        .await;
    let err = client.recv().await;
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no session"));

    // Missing required params.
    client.request("turn/start", json!({})).await;
    let err = client.recv().await;
    assert_eq!(err["error"]["code"], -32602);

    // An approval response nobody asked for.
    client
        .send(json!({"jsonrpc": "2.0", "id": 9999, "result": {"decision": "accept"}}))
        .await;
    let err = client.recv().await;
    assert_eq!(err["error"]["code"], -32000);

    // After all that abuse, normal work still runs.
    let thread_id = {
        let id = client.request("thread/start", json!({})).await;
        let resp = client.recv().await;
        assert_eq!(resp["id"], id);
        resp["result"]["thread"]["id"].as_str().unwrap().to_string()
    };
    client
        .request("turn/start", json!({"threadId": thread_id, "input": "x"}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert_eq!(log.last().unwrap()["params"]["turn"]["status"], "completed");

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn sessions_survive_a_server_restart() {
    let dirs = test_dirs("restart");
    let script = vec![vec![text("first answer")], vec![text("second answer")]];

    // Server instance one: create a thread, run a turn, shut down.
    let mut client = start_server(factory(script.clone(), dirs.offload.clone(), false), &dirs);
    let thread_id = client.init_and_start().await;
    client
        .request("turn/start", json!({"threadId": thread_id, "input": "q1"}))
        .await;
    client.recv_until(|m| m["method"] == "turn/completed").await;
    client.shutdown().await;

    // Server instance two over the same directories.
    let mut client = start_server(factory(script, dirs.offload.clone(), false), &dirs);
    client.initialize().await;
    client.request("thread/list", json!({})).await;
    let list = client.recv().await;
    let threads = list["result"]["threads"].as_array().unwrap();
    let entry = threads
        .iter()
        .find(|t| t["id"] == thread_id.as_str())
        .expect("restarted server must list the old session");
    assert_eq!(entry["messages"], 2);
    assert_eq!(entry["snippet"], "q1");

    client
        .request("thread/resume", json!({"threadId": thread_id}))
        .await;
    let resumed = client.recv().await;
    assert_eq!(resumed["result"]["thread"]["id"], thread_id.as_str());
    assert_eq!(resumed["result"]["messageCount"], 2);

    // Resuming twice is an error (already active).
    client
        .request("thread/resume", json!({"threadId": thread_id}))
        .await;
    let err = client.recv().await;
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("already active"));

    client
        .request("turn/start", json!({"threadId": thread_id, "input": "q2"}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert_eq!(log.last().unwrap()["params"]["turn"]["status"], "completed");

    client.shutdown().await;
    let messages =
        kloop_core::rollout::load_session(&dirs.sessions.join(format!("{thread_id}.jsonl")))
            .unwrap();
    assert_eq!(messages.len(), 4, "both turns persisted across the restart");
    let _ = std::fs::remove_dir_all(&dirs.root);
}

/// `thread/fork` copies a session prefix into a fresh thread and spawns it
/// live: the client can turn/start on the fork immediately, the fork's lineage
/// points back into the source at the cut, and the source file is untouched.
#[tokio::test]
async fn thread_fork_branches_a_session_into_a_live_thread() {
    let dirs = test_dirs("fork");
    let script = vec![vec![text("first answer")], vec![text("second answer")]];
    let mut client = start_server(factory(script, dirs.offload.clone(), false), &dirs);

    // Source thread with one complete user/assistant exchange.
    let src = client.init_and_start().await;
    client
        .request("turn/start", json!({"threadId": src, "input": "q1"}))
        .await;
    client.recv_until(|m| m["method"] == "turn/completed").await;

    // Fork at the end (no cut) into a new live thread.
    let fork_req = client
        .request("thread/fork", json!({"threadId": src}))
        .await;
    let log = client.recv_until(|m| m["id"] == fork_req).await;
    let resp = log.iter().find(|m| m["id"] == fork_req).unwrap();
    let fork_id = resp["result"]["thread"]["id"].as_str().unwrap().to_string();
    assert_ne!(fork_id, src, "the fork gets its own thread id");
    assert_eq!(resp["result"]["messageCount"], 2);

    // Lineage: the fork's first line carries a cross-file parent at the cut.
    let fork_path = dirs.sessions.join(format!("{fork_id}.jsonl"));
    assert_eq!(
        kloop_core::rollout::fork_origin(&fork_path),
        Some(format!("{src}#2"))
    );

    // The fork is live: a turn runs on it and completes.
    client
        .request("turn/start", json!({"threadId": fork_id, "input": "q2"}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert_eq!(log.last().unwrap()["params"]["turn"]["status"], "completed");

    client.shutdown().await;
    // Source untouched (2 lines); fork grew (2 copied + q2 + its answer).
    let src_msgs =
        kloop_core::rollout::load_session(&dirs.sessions.join(format!("{src}.jsonl"))).unwrap();
    assert_eq!(
        src_msgs.len(),
        2,
        "the source must be untouched by the fork"
    );
    let fork_msgs = kloop_core::rollout::load_session(&fork_path).unwrap();
    assert_eq!(fork_msgs.len(), 4);
    let _ = std::fs::remove_dir_all(&dirs.root);
}

/// An illegal cut (splitting a user/assistant pair) and a missing source both
/// come back as clean errors; the illegal-cut error lists the legal points.
#[tokio::test]
async fn thread_fork_rejects_an_illegal_cut() {
    let dirs = test_dirs("fork-illegal");
    let mut client = start_server(
        factory(vec![vec![text("answer")]], dirs.offload.clone(), false),
        &dirs,
    );
    let src = client.init_and_start().await;
    client
        .request("turn/start", json!({"threadId": src, "input": "q"}))
        .await;
    client.recv_until(|m| m["method"] == "turn/completed").await;

    // Cut #1 lands between the user message and its assistant reply — illegal.
    let bad = client
        .request("thread/fork", json!({"threadId": src, "cut": 1}))
        .await;
    let log = client.recv_until(|m| m["id"] == bad).await;
    let msg = log.iter().find(|m| m["id"] == bad).unwrap()["error"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        msg.contains("cannot fork") && msg.contains("#2"),
        "illegal-cut error must list the legal points: {msg}"
    );

    // Forking a session that does not exist is a clean error too.
    client
        .request("thread/fork", json!({"threadId": "ghost"}))
        .await;
    let err = client.recv().await;
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no session"));

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

/// A slash-command `turn/start` input runs the command (not the model): its
/// output comes back as a `system` notification bracketed by turn/started and
/// turn/completed, no item events for a message, and no user message recorded.
#[tokio::test]
async fn slash_commands_surface_as_system_notifications() {
    let dirs = test_dirs("slash");
    // The script is never consumed — slash commands don't sample the model.
    let mut client = start_server(
        factory(vec![vec![text("unused")]], dirs.offload.clone(), false),
        &dirs,
    );
    let tid = client.init_and_start().await;

    // /help lists the builtins as a system note; no item events appear.
    client
        .request("turn/start", json!({"threadId": tid, "input": "/help"}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert_eq!(
        methods_for_thread(&log, &tid),
        vec![
            "turn/started",
            "system",
            "thread/tokenUsage/updated",
            "turn/completed",
        ]
    );
    let help_text = log.iter().find(|m| m["method"] == "system").unwrap()["params"]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        help_text.contains("/help") && help_text.contains("/compact"),
        "help must list the builtins: {help_text}"
    );
    assert_eq!(log.last().unwrap()["params"]["turn"]["status"], "completed");

    // An unknown command lists the available ones, still as a system note.
    client
        .request(
            "turn/start",
            json!({"threadId": tid, "input": "/frobnicate"}),
        )
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert!(
        log.iter().find(|m| m["method"] == "system").unwrap()["params"]["text"]
            .as_str()
            .unwrap()
            .contains("unknown command")
    );

    client.shutdown().await;
    // Commands record no user turns: the session stayed empty.
    let messages =
        kloop_core::rollout::load_session(&dirs.sessions.join(format!("{tid}.jsonl"))).unwrap();
    assert!(
        messages.is_empty(),
        "slash commands must not record user turns: {messages:?}"
    );
    let _ = std::fs::remove_dir_all(&dirs.root);
}

/// `/clear` emits `thread/cleared` so the client resets its view, then reports
/// via `system` on the now-blank transcript (order matches the TUI), and the
/// cleared History persists (the session replays to empty).
#[tokio::test]
async fn clear_command_empties_history_and_notifies() {
    let dirs = test_dirs("slash-clear");
    let mut client = start_server(
        factory(vec![vec![text("an answer")]], dirs.offload.clone(), false),
        &dirs,
    );
    let tid = client.init_and_start().await;

    // A real turn populates history first.
    client
        .request("turn/start", json!({"threadId": tid, "input": "hello"}))
        .await;
    client.recv_until(|m| m["method"] == "turn/completed").await;

    client
        .request("turn/start", json!({"threadId": tid, "input": "/clear"}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert_eq!(
        methods_for_thread(&log, &tid),
        vec![
            "turn/started",
            "thread/cleared",
            "system",
            "thread/tokenUsage/updated",
            "turn/completed",
        ]
    );
    assert_eq!(
        log.iter().find(|m| m["method"] == "system").unwrap()["params"]["text"],
        "conversation cleared"
    );

    client.shutdown().await;
    let messages =
        kloop_core::rollout::load_session(&dirs.sessions.join(format!("{tid}.jsonl"))).unwrap();
    assert!(
        messages.is_empty(),
        "after /clear the session must replay to empty: {messages:?}"
    );
    let _ = std::fs::remove_dir_all(&dirs.root);
}
