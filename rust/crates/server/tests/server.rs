//! Contract tests for the native agent protocol server: a scripted Mock
//! provider behind the real serve() loop, driven over in-memory duplex pipes.
//! What a client sends and receives on the wire is asserted verbatim. Every
//! session opens with the `initialize` handshake (the gate the server enforces
//! before any other method).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::DuplexStream;
use tokio::io::Lines;
use tokio::task::JoinHandle;

use kloop_core::Config;
use kloop_core::permissions::Mode;
use kloop_core::permissions::PermissionRules;
use kloop_core::permissions::Permissions;
use kloop_core::rollout::Rollout;
use kloop_core::rollout::SessionRuntime;
use kloop_core::session_store::SessionStore;
use kloop_protocol::AssistantBlock;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_provider::Provider;
use kloop_server::ConfigFactory;
use kloop_server::ConfigSnapshot;
use kloop_server::McpServerState;
use kloop_server::McpServerStatus;
use kloop_server::McpToolInfo;
use kloop_server::McpTransportKind;
use kloop_server::PROTOCOL_VERSION;
use kloop_server::SandboxConfigInfo;
use kloop_server::ServerConfig;
use kloop_server::ServerPaths;
use kloop_server::SkillContext;
use kloop_server::SkillInfo;
use kloop_server::SkillScope;
use kloop_server::SkillsSnapshot;
use kloop_server::ThreadStartOptions;
use kloop_server::serve;

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
        self.recv_with_timeout(Duration::from_secs(30)).await
    }

    async fn recv_with_timeout(&mut self, timeout: Duration) -> Value {
        let line = tokio::time::timeout(timeout, self.lines.next_line())
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
        self.initialize_with_capabilities(json!({})).await
    }

    async fn initialize_with_capabilities(&mut self, capabilities: Value) -> Value {
        let id = self
            .request(
                "initialize",
                json!({
                    "clientInfo": {"name": "test", "version": "0"},
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": capabilities,
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

    async fn init_questions_and_start(&mut self) -> String {
        self.initialize_with_capabilities(json!({"questions": true}))
            .await;
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
    let paths = ServerPaths {
        store: dirs.store.clone(),
    };
    start_server_with_config(ServerConfig::new(factory, paths))
}

fn start_server_with_config(config: ServerConfig) -> TestClient {
    let (writer, server_input) = tokio::io::duplex(1 << 16);
    let (server_output, reader) = tokio::io::duplex(1 << 16);
    let server = tokio::spawn(serve(server_input, server_output, config));
    TestClient {
        writer,
        lines: BufReader::new(reader).lines(),
        server,
        next_id: 1,
    }
}

struct TestDirs {
    root: PathBuf,
    /// Hermetic store: one unpartitioned bucket under `<root>/.kloop`, so a
    /// test can write transcripts straight into `sessions` and still hand the
    /// server the same storage.
    store: SessionStore,
    sessions: PathBuf,
    offload: PathBuf,
}

fn test_dirs(tag: &str) -> TestDirs {
    let root = std::env::temp_dir().join(format!("kloop-server-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = SessionStore::hermetic(&root);
    let dirs = store.dirs(&root);
    TestDirs {
        sessions: dirs.sessions,
        offload: dirs.offload,
        store,
        root,
    }
}

fn text(t: &str) -> AssistantBlock {
    AssistantBlock::Text { text: t.into() }
}

fn mock_route() -> kloop_core::provider_route::FrozenProviderRoute {
    kloop_core::provider_route::ProviderCatalog::from_provider(
        "mock",
        Provider::mock(Vec::new()),
        "mock",
        vec!["mock".into(), "model-a".into(), "model-b".into()],
    )
    .unwrap()
    .1
}

/// the script. `gated` = a real Manual-mode permission gate wired to the
/// server's approver (approvals go out as approval/request); otherwise the
/// gate is wide open.
fn factory(turns: Vec<Vec<AssistantBlock>>, offload: PathBuf, gated: bool) -> ConfigFactory {
    Arc::new(move |options, _catalog, approver, questioner, _notify| {
        let permissions = if gated {
            Permissions::new(
                Mode::Manual,
                &PermissionRules::default(),
                std::env::temp_dir(),
                Some(approver),
            )?
        } else {
            Permissions::allow_all()
        };
        let model = options.model.unwrap_or_else(|| "mock".into());
        let (provider_catalog, provider_route) =
            kloop_core::provider_route::ProviderCatalog::from_provider(
                "mock",
                Provider::mock(turns.clone()),
                model.clone(),
                vec![model],
            )
            .map_err(anyhow::Error::msg)?;
        let questions = questioner.is_some();
        let inbox = Arc::new(kloop_core::inbox::Inbox::default());
        Ok(Config {
            provider_catalog,
            provider_route,
            system: "test".into(),
            project_instructions: None,
            max_rounds: Some(10),
            cwd: options.cwd,
            offload_dir: offload.clone(),
            // Siblings under the same test root (test_dirs), matching the
            // ServerPaths the server lists/creates threads from.
            sessions_dir: offload.with_file_name("sessions"),
            context_window: None,
            permissions: Arc::new(permissions),
            questioner,
            file_state: Default::default(),
            tool_sources: Vec::new(),
            session_id: String::new(),
            local_agent: kloop_core::agent_mailbox::LocalAgentContext::root(Arc::clone(&inbox)),
            hooks: std::sync::Arc::new(kloop_core::hooks::Hooks::none()),
            background_shells: kloop_core::tools::BackgroundShells::new(),
            shell_programs: std::sync::Arc::new(
                kloop_core::shell_programs::ShellPrograms::test_fixture(),
            ),
            powershell_execution_gate: Default::default(),
            sandbox: None,
            agent_types: std::sync::Arc::new(Vec::new()),
            tool_allowlist: None,
            defer_threshold: 30,
            unlocked_tools: Default::default(),
            tasks: Default::default(),
            inbox: Arc::clone(&inbox),
            scheduler: kloop_core::scheduler::Scheduler::in_memory(inbox),
            background_executions: Default::default(),
            program_limits: Default::default(),
            skills: Default::default(),
            active_worktree: std::sync::Arc::new(
                kloop_core::worktree::ActiveWorktreeState::default(),
            ),
            surface: kloop_core::config::SurfaceCapabilities {
                questions,
                plan_control: true,
                program: false,
                workflow: true,
                worktree: false,
                scheduler: false,
            },
        })
    })
}

fn clocked_scheduler_factory(
    turns: Vec<Vec<AssistantBlock>>,
    offload: PathBuf,
    clock: Arc<kloop_core::scheduler::ManualClock>,
) -> ConfigFactory {
    let inner = factory(turns, offload, false);
    Arc::new(move |options, catalog, approver, questioner, notify| {
        let mut cfg = inner(options, catalog, approver, questioner, notify)?;
        let inbox = Arc::new(kloop_core::inbox::Inbox::default());
        cfg.local_agent = kloop_core::agent_mailbox::LocalAgentContext::root(Arc::clone(&inbox));
        cfg.inbox = Arc::clone(&inbox);
        cfg.scheduler = kloop_core::scheduler::Scheduler::with_clock(
            inbox,
            None,
            clock.clone(),
            kloop_core::scheduler::SchedulerTimeZone::named("UTC")?,
        );
        cfg.surface.scheduler = true;
        Ok(cfg)
    })
}

fn partial_factory(offload: PathBuf) -> ConfigFactory {
    use kloop_provider::MockTurn;

    let inner = factory(Vec::new(), offload, false);
    Arc::new(move |options, catalog, approver, questioner, notify| {
        let mut cfg = inner(options, catalog, approver, questioner, notify)?;
        let (provider_catalog, provider_route) =
            kloop_core::provider_route::ProviderCatalog::from_provider(
                "mock",
                Provider::mock_scripted(vec![
                    MockTurn::PartialError(vec![text("half answer")], "stream dropped".into()),
                    MockTurn::Blocks(vec![text(" and the rest")]),
                ]),
                cfg.provider_route.primary_model(),
                cfg.provider_route.allowed_models().to_vec(),
            )
            .map_err(anyhow::Error::msg)?;
        cfg.provider_catalog = provider_catalog;
        cfg.provider_route = provider_route;
        Ok(cfg)
    })
}

fn recording_factory(
    turns: Vec<Vec<AssistantBlock>>,
    offload: PathBuf,
    seen: Arc<Mutex<Vec<ThreadStartOptions>>>,
) -> ConfigFactory {
    let inner = factory(turns, offload, false);
    Arc::new(move |options, catalog, approver, questioner, notify| {
        seen.lock().unwrap().push(options.clone());
        inner(options, catalog, approver, questioner, notify)
    })
}

fn switch_factory(
    offload: PathBuf,
    seen_a: Arc<Mutex<Vec<kloop_provider::MockRequest>>>,
    seen_b: Arc<Mutex<Vec<kloop_provider::MockRequest>>>,
) -> ConfigFactory {
    let inner = factory(Vec::new(), offload, false);
    Arc::new(move |options, catalog, approver, questioner, notify| {
        let selected_provider = options.provider_id.clone().unwrap_or_else(|| "a".into());
        let selected_model = options.model.clone().unwrap_or_else(|| "shared".into());
        let mut cfg = inner(options, catalog, approver, questioner, notify)?;
        let provider_a = Provider::Mock {
            turns: Mutex::new(vec![kloop_provider::MockTurn::Blocks(vec![text("from a")])].into()),
            seen: Arc::clone(&seen_a),
        };
        let provider_b = Provider::Mock {
            turns: Mutex::new(vec![kloop_provider::MockTurn::Blocks(vec![text("from b")])].into()),
            seen: Arc::clone(&seen_b),
        };
        let entry = |id: &str, provider: Provider| {
            let provider = Arc::new(Mutex::new(Some(provider)));
            kloop_core::provider_route::ProviderCatalogEntry {
                id: id.into(),
                api_family: kloop_protocol::ProviderApiFamily::Mock,
                endpoint_fingerprint: Provider::mock(Vec::new()).endpoint_fingerprint(),
                default_model: "shared".into(),
                models: vec!["shared".into(), format!("{id}-other")],
                availability: kloop_protocol::ProviderAvailabilityCode::Ready,
                default_effort: None,
                factory: Arc::new(move || {
                    provider
                        .lock()
                        .unwrap()
                        .take()
                        .ok_or(kloop_protocol::ProviderAvailabilityCode::InvalidConfiguration)
                }),
            }
        };
        let catalog = Arc::new(
            kloop_core::provider_route::ProviderCatalog::new(vec![
                entry("a", provider_a),
                entry("b", provider_b),
            ])
            .map_err(anyhow::Error::msg)?,
        );
        let route = catalog
            .initial_route(&selected_provider, Some(&selected_model))
            .map_err(anyhow::Error::new)?;
        cfg.provider_catalog = catalog;
        cfg.provider_route = route;
        Ok(cfg)
    })
}

/// A catalog whose configured providers can change between server restarts —
/// the shape of a user editing `~/.kloop/config.toml` between two sessions. An
/// explicitly named provider must resolve (like the real factory); nothing else
/// is assumed about what the thread was written on.
fn renaming_factory(offload: PathBuf, configured: Arc<Mutex<Vec<String>>>) -> ConfigFactory {
    let inner = factory(Vec::new(), offload, false);
    Arc::new(move |options, catalog, approver, questioner, notify| {
        // `thread/start`'s explicit choice, if any; a reopened thread carries
        // none and takes the first configured provider — this fixture's default.
        let requested = options.provider_id.clone();
        let mut cfg = inner(options, catalog, approver, questioner, notify)?;
        let ids = configured.lock().unwrap().clone();
        let entries = ids
            .iter()
            .map(|id| kloop_core::provider_route::ProviderCatalogEntry {
                id: id.clone(),
                api_family: kloop_protocol::ProviderApiFamily::Mock,
                endpoint_fingerprint: Provider::mock(Vec::new()).endpoint_fingerprint(),
                default_model: "shared".into(),
                models: vec!["shared".into()],
                availability: kloop_protocol::ProviderAvailabilityCode::Ready,
                default_effort: None,
                factory: Arc::new(|| Ok(Provider::mock(vec![vec![text("answer")]]))),
            })
            .collect();
        let catalog = Arc::new(
            kloop_core::provider_route::ProviderCatalog::new(entries)
                .map_err(anyhow::Error::msg)?,
        );
        let selected = requested.unwrap_or_else(|| ids[0].clone());
        cfg.provider_route = catalog
            .initial_route(&selected, None)
            .map_err(anyhow::Error::new)?;
        cfg.provider_catalog = catalog;
        Ok(cfg)
    })
}

fn real_switch_factory(offload: PathBuf) -> ConfigFactory {
    let inner = factory(Vec::new(), offload, false);
    Arc::new(move |options, catalog, approver, questioner, notify| {
        let provider_id = options
            .provider_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("real route test requires an explicit provider"))?;
        let model = options
            .model
            .clone()
            .ok_or_else(|| anyhow::anyhow!("real route test requires an explicit model"))?;
        let mut cfg = inner(options, Arc::clone(&catalog), approver, questioner, notify)?;
        cfg.provider_route = catalog
            .initial_route(&provider_id, Some(&model))
            .map_err(anyhow::Error::new)?;
        cfg.provider_catalog = catalog;
        cfg.system = "Answer the requested sentinel directly. Do not call tools.".into();
        cfg.max_rounds = Some(3);
        Ok(cfg)
    })
}

fn required_real_env(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| panic!("set {name} to run the ignored real route-switch contract"))
}

async fn run_real_provider_turn(client: &mut TestClient, thread_id: &str, prompt: &str) {
    let request_id = client
        .request(
            "turn/start",
            json!({"threadId": thread_id, "input": prompt}),
        )
        .await;
    let mut accepted = false;
    loop {
        let message = client.recv_with_timeout(Duration::from_secs(600)).await;
        if message["id"] == request_id {
            assert!(
                message.get("error").is_none(),
                "real turn request was rejected"
            );
            accepted = true;
        }
        if message["method"] == "turn/completed" {
            assert!(accepted, "turn completed before its request was accepted");
            assert_eq!(
                message["params"]["turn"]["status"], "completed",
                "real provider turn did not complete"
            );
            return;
        }
    }
}

/// A sweep row's outcome. A rate-limited row is deliberately not `Refused`:
/// measured on a throttled proxy, 429s land on alternating requests while the
/// same levels succeed either side of them, so folding them into "the model
/// refused this level" would invent a contract the endpoint never stated.
#[derive(Debug, PartialEq, Eq)]
enum EffortOutcome {
    Accepted,
    Refused(String),
    RateLimited(String),
}

/// Sample one level, retrying through rate limits before giving up on the row.
async fn probe_real_effort(
    client: &mut TestClient,
    thread_id: &str,
    prompt: &str,
) -> EffortOutcome {
    let mut last = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        match try_real_provider_turn(client, thread_id, prompt).await {
            Ok(()) => return EffortOutcome::Accepted,
            Err(error) => {
                // A 429 says "later", never "not that value".
                if !error.contains("429") {
                    return EffortOutcome::Refused(error);
                }
                last = error;
            }
        }
    }
    EffortOutcome::RateLimited(last)
}

/// Like [`run_real_provider_turn`] but reports the outcome instead of asserting
/// it — the effort sweep needs to record which levels a live model refuses.
async fn try_real_provider_turn(
    client: &mut TestClient,
    thread_id: &str,
    prompt: &str,
) -> Result<(), String> {
    let request_id = client
        .request(
            "turn/start",
            json!({"threadId": thread_id, "input": prompt}),
        )
        .await;
    loop {
        let message = client.recv_with_timeout(Duration::from_secs(600)).await;
        if message["id"] == request_id {
            assert!(message.get("error").is_none(), "real turn was rejected");
        }
        if message["method"] == "turn/completed" {
            return match message["params"]["turn"]["status"].as_str() {
                Some("completed") => Ok(()),
                _ => Err(message["params"]["turn"]["error"]
                    .as_str()
                    .unwrap_or("unknown provider failure")
                    .to_string()),
            };
        }
    }
}

async fn switch_real_provider(
    client: &mut TestClient,
    thread_id: &str,
    provider_id: &str,
    model: &str,
    expected_revision: u64,
) -> Value {
    let request_id = client
        .request(
            "thread/provider/switch",
            json!({
                "threadId": thread_id,
                "providerId": provider_id,
                "model": model,
                "expectedRouteRevision": expected_revision,
            }),
        )
        .await;
    let mut changed = None;
    loop {
        let message = client.recv_with_timeout(Duration::from_secs(60)).await;
        if message["method"] == "thread/provider/changed"
            && message["params"]["route"]["providerId"] == provider_id
        {
            changed = Some(message["params"]["route"].clone());
        }
        if message["id"] == request_id {
            assert!(
                message.get("error").is_none(),
                "real provider switch was rejected"
            );
            let route = message["result"]["route"].clone();
            assert_eq!(route["providerId"], provider_id);
            assert_eq!(route["revision"], expected_revision + 1);
            assert_eq!(changed.as_ref(), Some(&route));
            return route;
        }
    }
}

/// Drive one slash command through `turn/start` and return its `system` output.
/// A command keeps the ordinary turn bracket, so completion is the signal that
/// it ran — unlike `thread/provider/switch`, which has its own transaction.
async fn run_real_command(client: &mut TestClient, thread_id: &str, line: &str) -> String {
    let request_id = client
        .request("turn/start", json!({"threadId": thread_id, "input": line}))
        .await;
    let mut text = None;
    let mut accepted = false;
    loop {
        let message = client.recv_with_timeout(Duration::from_secs(120)).await;
        if message["id"] == request_id {
            assert!(
                message.get("error").is_none(),
                "slash command request was rejected: {message}"
            );
            accepted = true;
        }
        if message["method"] == "system" {
            text = message["params"]["text"].as_str().map(str::to_string);
        }
        if message["method"] == "turn/completed" {
            assert!(
                accepted,
                "command completed before its request was accepted"
            );
            return text.expect("slash command produced no system output");
        }
    }
}

fn tool_use(id: &str, name: &str, input: Value) -> AssistantBlock {
    AssistantBlock::ToolUse {
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
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(a)
                .status()
                .unwrap()
                .success()
        );
    }
    std::fs::canonicalize(&root).unwrap()
}

/// Like `factory` but with worktree mode ON and cwd pointed at a real git repo
/// (a bypass gate bound to the same project identity).
fn worktree_factory(
    turns: Vec<Vec<AssistantBlock>>,
    offload: PathBuf,
    cwd: PathBuf,
) -> ConfigFactory {
    Arc::new(move |_options, _catalog, _approver, questioner, _notify| {
        let (provider_catalog, provider_route) =
            kloop_core::provider_route::ProviderCatalog::from_provider(
                "mock",
                Provider::mock(turns.clone()),
                "mock",
                vec!["mock".into()],
            )
            .map_err(anyhow::Error::msg)?;
        let questions = questioner.is_some();
        let inbox = Arc::new(kloop_core::inbox::Inbox::default());
        Ok(Config {
            provider_catalog,
            provider_route,
            system: "test".into(),
            project_instructions: None,
            max_rounds: Some(10),
            cwd: cwd.clone(),
            offload_dir: offload.clone(),
            sessions_dir: offload.with_file_name("sessions"),
            context_window: None,
            permissions: Arc::new(Permissions::new(
                Mode::Bypass,
                &PermissionRules::default(),
                cwd.clone(),
                None,
            )?),
            questioner,
            file_state: Default::default(),
            tool_sources: Vec::new(),
            session_id: String::new(),
            local_agent: kloop_core::agent_mailbox::LocalAgentContext::root(Arc::clone(&inbox)),
            hooks: std::sync::Arc::new(kloop_core::hooks::Hooks::none()),
            background_shells: kloop_core::tools::BackgroundShells::new(),
            shell_programs: std::sync::Arc::new(
                kloop_core::shell_programs::ShellPrograms::test_fixture(),
            ),
            powershell_execution_gate: Default::default(),
            sandbox: None,
            agent_types: std::sync::Arc::new(Vec::new()),
            tool_allowlist: None,
            defer_threshold: 30,
            unlocked_tools: Default::default(),
            tasks: Default::default(),
            inbox: Arc::clone(&inbox),
            scheduler: kloop_core::scheduler::Scheduler::in_memory(inbox),
            background_executions: Default::default(),
            program_limits: Default::default(),
            skills: Default::default(),
            active_worktree: std::sync::Arc::new(
                kloop_core::worktree::ActiveWorktreeState::default(),
            ),
            surface: kloop_core::config::SurfaceCapabilities {
                questions,
                plan_control: true,
                program: false,
                workflow: true,
                worktree: true,
                scheduler: false,
            },
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
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not initialized")
    );

    // A version we don't speak is a hard error (no silent downgrade).
    client
        .request(
            "initialize",
            json!({"protocolVersion": "1.0", "capabilities": {}}),
        )
        .await;
    let err = client.recv().await;
    assert_eq!(err["error"]["code"], -32602);
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unsupported protocolVersion")
    );

    // A good handshake reports capabilities and unlocks the rest.
    let caps = client.initialize().await;
    assert_eq!(caps["streaming"], true);
    assert_eq!(
        caps["approvals"],
        json!({"scopes": ["once", "workspaceSession", "project"]})
    );
    assert_eq!(caps["providers"], json!({"catalog": true, "switch": true}));
    assert_eq!(caps["config"], json!({"read": true}));
    assert_eq!(caps["skills"], json!({"list": true}));
    assert_eq!(caps["mcpServers"], json!({"status": true}));
    assert_eq!(
        caps["events"],
        json!({"sequence": true, "sync": true, "snapshot": true})
    );
    assert_eq!(
        caps["threads"],
        json!({"list": true, "read": true, "resume": true, "fork": true})
    );
    let id = client.request("thread/start", json!({})).await;
    let resp = client.recv().await;
    assert_eq!(resp["id"], id);
    assert!(resp["result"]["thread"]["id"].is_string());

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn read_surfaces_are_scoped_safe_and_read_only() {
    let dirs = test_dirs("read-surfaces");
    let project = dirs.root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = std::fs::canonicalize(project).unwrap();
    let project_text = project.to_string_lossy().to_string();
    let config_reads = Arc::new(Mutex::new(Vec::new()));
    let skill_reads = Arc::new(Mutex::new(Vec::new()));

    let paths = ServerPaths {
        store: dirs.store.clone(),
    };
    let mut server = ServerConfig::new(
        factory(vec![vec![text("ok")]], dirs.offload.clone(), false),
        paths,
    );
    server.provider_catalog = kloop_core::provider_route::ProviderCatalog::from_provider(
        "mock",
        Provider::mock(Vec::new()),
        "model-default",
        vec!["model-default".into(), "thread-model".into()],
    )
    .unwrap()
    .0;
    server.mcp_servers = vec![
        McpServerStatus {
            name: "memory".into(),
            transport: McpTransportKind::Stdio,
            state: McpServerState::Connected,
            tools: vec![McpToolInfo {
                name: "memory__search".into(),
                description: "Search memory".into(),
            }],
            message: None,
        },
        McpServerStatus {
            name: "remote".into(),
            transport: McpTransportKind::Http,
            state: McpServerState::Unavailable,
            tools: Vec::new(),
            message: Some("connection or tool discovery failed; see engine log".into()),
        },
    ];
    let config_reads_for_reader = config_reads.clone();
    server.config_reader = Arc::new(move |cwd| {
        config_reads_for_reader
            .lock()
            .unwrap()
            .push(cwd.to_path_buf());
        Ok(ConfigSnapshot {
            cwd: cwd.to_string_lossy().to_string(),
            route: Some(kloop_protocol::ActiveProviderRoute {
                revision: 1,
                provider_id: "mock".into(),
                api_family: kloop_protocol::ProviderApiFamily::Mock,
                model: "model-default".into(),
                continuity: kloop_protocol::ReasoningContinuity::Preserved,
                effort: None,
            }),
            permission_mode: "manual".into(),
            context_window: Some(200_000),
            defer_threshold: 30,
            sandbox: SandboxConfigInfo {
                enabled: true,
                allow_network: false,
                auto_allow: true,
                escalate: true,
            },
            worktree_enabled: true,
        })
    });
    let skill_reads_for_reader = skill_reads.clone();
    let skill_path = project.join(".kloop/skills/review/SKILL.md");
    server.skills_reader = Arc::new(move |cwd| {
        skill_reads_for_reader
            .lock()
            .unwrap()
            .push(cwd.to_path_buf());
        Ok(SkillsSnapshot {
            cwd: cwd.to_string_lossy().to_string(),
            skills: vec![SkillInfo {
                name: "review".into(),
                description: "Review changes".into(),
                path: skill_path.to_string_lossy().to_string(),
                scope: SkillScope::Project,
                context: SkillContext::Fork,
                model: None,
            }],
            warnings: vec!["one malformed skill was skipped".into()],
        })
    });

    let mut client = start_server_with_config(server);
    let caps = client.initialize().await;
    assert_eq!(
        caps,
        json!({
            "approvals": {"scopes": ["once", "workspaceSession", "project"]},
            "config": {"read": true},
            "events": {"sequence": true, "snapshot": true, "sync": true},
            "images": true,
            "mcp": true,
            "mcpServers": {"status": true},
            "providers": {"catalog": true, "switch": true},
            "questions": true,
            "skills": {"list": true},
            "streaming": true,
            "subagents": true,
            "threads": {"fork": true, "list": true, "read": true, "resume": true},
        })
    );

    let id = client.request("provider/catalog/read", json!({})).await;
    assert_eq!(
        client.recv().await,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"providers": [{
                "id": "mock",
                "apiFamily": "mock",
                "defaultModel": "model-default",
                "models": ["model-default", "thread-model"],
                "availability": "ready",
            }]},
        })
    );

    let id = client
        .request("config/read", json!({"cwd": project_text}))
        .await;
    assert_eq!(
        client.recv().await,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"config": {
                "cwd": project_text,
                "route": {
                    "revision": 1,
                    "providerId": "mock",
                    "apiFamily": "mock",
                    "model": "model-default",
                    "continuity": "preserved",
                },
                "permissionMode": "manual",
                "contextWindow": 200_000,
                "deferThreshold": 30,
                "sandbox": {
                    "enabled": true,
                    "allowNetwork": false,
                    "autoAllow": true,
                    "escalate": true,
                },
                "worktreeEnabled": true,
            }},
        })
    );

    let id = client
        .request(
            "skills/list",
            json!({"cwd": project_text, "forceReload": true}),
        )
        .await;
    assert_eq!(
        client.recv().await,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "cwd": project_text,
                "skills": [{
                    "name": "review",
                    "description": "Review changes",
                    "path": project.join(".kloop/skills/review/SKILL.md"),
                    "scope": "project",
                    "context": "fork",
                    "model": null,
                }],
                "warnings": ["one malformed skill was skipped"],
            },
        })
    );

    let id = client.request("mcpServerStatus/list", json!({})).await;
    assert_eq!(
        client.recv().await,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"servers": [
                {
                    "name": "memory",
                    "transport": "stdio",
                    "state": "connected",
                    "tools": [{"name": "memory__search", "description": "Search memory"}],
                    "message": null,
                },
                {
                    "name": "remote",
                    "transport": "http",
                    "state": "unavailable",
                    "tools": [],
                    "message": "connection or tool discovery failed; see engine log",
                },
            ]},
        })
    );

    let id = client
        .request(
            "thread/start",
            json!({"cwd": project_text, "model": "thread-model"}),
        )
        .await;
    let response = client.recv().await;
    assert_eq!(response["id"], id);
    let thread_id = response["result"]["thread"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let id = client
        .request("config/read", json!({"threadId": thread_id}))
        .await;
    let response = client.recv().await;
    assert_eq!(response["id"], id);
    assert_eq!(response["result"]["config"]["cwd"], project_text);
    assert_eq!(
        response["result"]["config"]["route"]["model"],
        "thread-model"
    );

    // Slice 4 is deliberately read-only. No capability advertises mutation and
    // the stale plan-table mention of config/write stays METHOD_NOT_FOUND.
    let id = client.request("config/write", json!({})).await;
    let response = client.recv().await;
    assert_eq!(response["id"], id);
    assert_eq!(response["error"]["code"], -32601);

    assert_eq!(
        *config_reads.lock().unwrap(),
        vec![project.clone(), project.clone()]
    );
    assert_eq!(*skill_reads.lock().unwrap(), vec![project.clone()]);

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn read_surface_params_fail_closed() {
    let dirs = test_dirs("read-params");
    let mut client = start_server(factory(Vec::new(), dirs.offload.clone(), false), &dirs);
    client.initialize().await;

    for (method, params) in [
        ("provider/catalog/read", json!({"unexpected": true})),
        ("mcpServerStatus/list", json!([])),
        ("config/read", json!({"threadId": "x", "cwd": "."})),
        ("config/read", json!({"threadID": "x"})),
        ("skills/list", json!({"forceReload": "yes"})),
        ("skills/list", json!({"cwd": ".", "unexpected": true})),
    ] {
        let id = client.request(method, params).await;
        let response = client.recv().await;
        assert_eq!(response["id"], id);
        assert_eq!(response["error"]["code"], -32602);
    }

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn thread_start_resolves_per_thread_cwd_and_model() {
    let dirs = test_dirs("thread-options");
    let project_a = dirs.root.join("a");
    let project_b = dirs.root.join("b");
    std::fs::create_dir_all(&project_a).unwrap();
    std::fs::create_dir_all(&project_b).unwrap();
    let project_a = std::fs::canonicalize(project_a).unwrap();
    let project_b = std::fs::canonicalize(project_b).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut client = start_server(
        recording_factory(vec![vec![text("ok")]], dirs.offload.clone(), seen.clone()),
        &dirs,
    );
    client.initialize().await;

    for (cwd, model) in [(&project_a, Some("model-a")), (&project_b, Some("model-b"))] {
        let id = client
            .request("thread/start", json!({"cwd": cwd, "model": model}))
            .await;
        let response = client.recv().await;
        assert_eq!(response["id"], id);
        assert!(response["result"]["thread"]["id"].is_string());
    }

    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            ThreadStartOptions {
                cwd: project_a,
                provider_id: None,
                model: Some("model-a".into()),
            },
            ThreadStartOptions {
                cwd: project_b,
                provider_id: None,
                model: Some("model-b".into()),
            },
        ]
    );

    for params in [
        json!({"cwd": ""}),
        json!({"cwd": dirs.root.join("missing")}),
        json!({"cwd": dirs.root.join("sessions").join("not-a-dir")}),
        json!({"cwd": 3}),
        json!({"model": ""}),
        json!({"model": 3}),
    ] {
        if let Some(path) = params.get("cwd").and_then(Value::as_str)
            && path.ends_with("not-a-dir")
        {
            if let Some(parent) = std::path::Path::new(path).parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, "x").unwrap();
        }
        let id = client.request("thread/start", params).await;
        let response = client.recv().await;
        assert_eq!(response["id"], id);
        assert_eq!(response["error"]["code"], -32602);
    }
    assert_eq!(seen.lock().unwrap().len(), 2);

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn provider_switch_commits_revision_before_next_turn_and_emits_event() {
    let dirs = test_dirs("provider-switch");
    let seen_a = Arc::new(Mutex::new(Vec::new()));
    let seen_b = Arc::new(Mutex::new(Vec::new()));
    let mut server = ServerConfig::new(
        switch_factory(
            dirs.offload.clone(),
            Arc::clone(&seen_a),
            Arc::clone(&seen_b),
        ),
        ServerPaths {
            store: dirs.store.clone(),
        },
    );
    server.provider_catalog = Arc::new(
        kloop_core::provider_route::ProviderCatalog::new(vec![
            kloop_core::provider_route::ProviderCatalogEntry {
                id: "a".into(),
                api_family: kloop_protocol::ProviderApiFamily::Mock,
                endpoint_fingerprint: Provider::mock(Vec::new()).endpoint_fingerprint(),
                default_model: "shared".into(),
                models: vec!["shared".into(), "a-other".into()],
                availability: kloop_protocol::ProviderAvailabilityCode::Ready,
                default_effort: None,
                factory: Arc::new(|| Ok(Provider::mock(Vec::new()))),
            },
            kloop_core::provider_route::ProviderCatalogEntry {
                id: "b".into(),
                api_family: kloop_protocol::ProviderApiFamily::Mock,
                endpoint_fingerprint: Provider::mock(Vec::new()).endpoint_fingerprint(),
                default_model: "shared".into(),
                models: vec!["shared".into(), "b-other".into()],
                availability: kloop_protocol::ProviderAvailabilityCode::Ready,
                default_effort: None,
                factory: Arc::new(|| Ok(Provider::mock(Vec::new()))),
            },
        ])
        .unwrap(),
    );
    let mut client = start_server_with_config(server);
    client.initialize().await;
    let start_id = client
        .request(
            "thread/start",
            json!({"providerId": "a", "model": "shared"}),
        )
        .await;
    let started = client.recv().await;
    assert_eq!(started["id"], start_id);
    let thread_id = started["result"]["thread"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(started["result"]["thread"]["route"]["providerId"], "a");
    assert_eq!(
        started["result"]["thread"]["route"]["continuity"],
        "preserved"
    );

    let switch_id = client
        .request(
            "thread/provider/switch",
            json!({
                "threadId": thread_id,
                "providerId": "b",
                "model": "shared",
                "expectedRouteRevision": 1,
            }),
        )
        .await;
    let switch_messages = client
        .recv_until(|message| message["id"] == switch_id)
        .await;
    assert!(
        switch_messages.iter().any(|message| {
            message["method"] == "thread/provider/changed"
                && message["params"]["route"]["providerId"] == "b"
                && message["params"]["route"]["revision"] == 2
                && message["params"]["continuity"] == "preserved"
                && message["params"]["route"]["continuity"] == "preserved"
        }),
        "{switch_messages:?}"
    );
    let switched = switch_messages
        .iter()
        .find(|message| message["id"] == switch_id)
        .unwrap();
    assert_eq!(switched["result"]["route"]["revision"], 2);
    assert_eq!(switched["result"]["continuity"], "preserved");
    assert_eq!(switched["result"]["route"]["continuity"], "preserved");
    assert!(seen_a.lock().unwrap().is_empty());
    assert!(seen_b.lock().unwrap().is_empty());

    client
        .request(
            "turn/start",
            json!({"threadId": thread_id, "input": "after switch"}),
        )
        .await;
    let turn_messages = client
        .recv_until(|message| message["method"] == "turn/completed")
        .await;
    assert!(seen_a.lock().unwrap().is_empty());
    assert_eq!(seen_b.lock().unwrap().len(), 1, "{turn_messages:?}");

    client
        .request("thread/read", json!({"threadId": thread_id}))
        .await;
    let read = client.recv().await;
    assert_eq!(read["result"]["thread"]["route"]["providerId"], "b");
    assert_eq!(read["result"]["thread"]["route"]["revision"], 2);
    assert_eq!(read["result"]["thread"]["route"]["continuity"], "preserved");

    client
        .request("thread/events/sync", json!({"threadId": thread_id}))
        .await;
    let snapshot = client.recv().await;
    assert_eq!(
        snapshot["result"]["snapshot"]["thread"]["route"],
        json!({
            "revision": 2,
            "providerId": "b",
            "apiFamily": "mock",
            "model": "shared",
            "continuity": "preserved",
        })
    );
    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires real Anthropic, OpenAI Chat, and OpenAI Responses credentials"]
async fn real_three_rail_route_switch_contract() {
    let anthropic_key = required_real_env("ANTHROPIC_API_KEY");
    let anthropic_base = required_real_env("ANTHROPIC_BASE_URL")
        .trim_end_matches('/')
        .to_string();
    let anthropic_model = required_real_env("ANTHROPIC_MODEL");
    let responses_key = required_real_env("OPENAI_API_KEY");
    let responses_base = required_real_env("OPENAI_BASE_URL")
        .trim_end_matches('/')
        .to_string();
    let responses_model = required_real_env("OPENAI_MODEL");
    let responses_effort: kloop_protocol::ReasoningEffort = std::env::var("KLOOP_EFFORT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map_or(Ok(kloop_protocol::ReasoningEffort::High), |value| {
            value.parse()
        })
        .expect("KLOOP_EFFORT must name a known effort level");

    let anthropic_factory_key = anthropic_key.clone();
    let anthropic_factory_base = anthropic_base.clone();
    let chat_factory_key = responses_key.clone();
    let chat_factory_base = responses_base.clone();
    let responses_factory_key = responses_key.clone();
    let responses_factory_base = responses_base.clone();
    let catalog = Arc::new(
        kloop_core::provider_route::ProviderCatalog::new(vec![
            kloop_core::provider_route::ProviderCatalogEntry {
                id: "anthropic-real".into(),
                api_family: kloop_protocol::ProviderApiFamily::AnthropicMessages,
                endpoint_fingerprint: Provider::endpoint_fingerprint_for(
                    kloop_protocol::ProviderApiFamily::AnthropicMessages,
                    &anthropic_base,
                ),
                default_model: anthropic_model.clone(),
                models: vec![anthropic_model.clone()],
                availability: kloop_protocol::ProviderAvailabilityCode::Ready,
                default_effort: None,
                factory: Arc::new(move || {
                    Ok(Provider::Anthropic {
                        key: anthropic_factory_key.clone(),
                        base: anthropic_factory_base.clone(),
                        cache: true,
                        thinking: kloop_provider::ThinkingMode::Unset,
                    })
                }),
            },
            kloop_core::provider_route::ProviderCatalogEntry {
                id: "chat-real".into(),
                api_family: kloop_protocol::ProviderApiFamily::OpenAiChatCompletions,
                endpoint_fingerprint: Provider::endpoint_fingerprint_for(
                    kloop_protocol::ProviderApiFamily::OpenAiChatCompletions,
                    &responses_base,
                ),
                default_model: responses_model.clone(),
                models: vec![responses_model.clone()],
                availability: kloop_protocol::ProviderAvailabilityCode::Ready,
                default_effort: None,
                factory: Arc::new(move || {
                    Ok(Provider::OpenAiCompat {
                        key: chat_factory_key.clone(),
                        base: chat_factory_base.clone(),
                    })
                }),
            },
            kloop_core::provider_route::ProviderCatalogEntry {
                id: "responses-real".into(),
                api_family: kloop_protocol::ProviderApiFamily::OpenAiResponses,
                endpoint_fingerprint: Provider::endpoint_fingerprint_for(
                    kloop_protocol::ProviderApiFamily::OpenAiResponses,
                    &responses_base,
                ),
                default_model: responses_model.clone(),
                models: vec![responses_model.clone()],
                availability: kloop_protocol::ProviderAvailabilityCode::Ready,
                default_effort: Some(responses_effort),
                factory: Arc::new(move || {
                    Ok(Provider::OpenAiResponses {
                        key: responses_factory_key.clone(),
                        base: responses_factory_base.clone(),
                    })
                }),
            },
        ])
        .unwrap(),
    );

    let dirs = test_dirs("real-provider-switch");
    std::fs::create_dir_all(&dirs.root).unwrap();
    let cwd = std::fs::canonicalize(&dirs.root).unwrap();
    let mut server = ServerConfig::new(
        real_switch_factory(dirs.offload.clone()),
        ServerPaths {
            store: dirs.store.clone(),
        },
    );
    server.provider_catalog = Arc::clone(&catalog);
    let mut client = start_server_with_config(server);
    client.initialize().await;

    let catalog_id = client.request("provider/catalog/read", json!({})).await;
    let catalog_response = client.recv().await;
    assert_eq!(catalog_response["id"], catalog_id);
    let provider_ids = catalog_response["result"]["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|provider| provider["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        provider_ids,
        ["anthropic-real", "chat-real", "responses-real"]
    );

    let start_id = client
        .request(
            "thread/start",
            json!({
                "cwd": cwd,
                "providerId": "anthropic-real",
                "model": anthropic_model,
            }),
        )
        .await;
    let started = client.recv_with_timeout(Duration::from_secs(60)).await;
    assert_eq!(started["id"], start_id);
    assert!(started.get("error").is_none(), "real thread start failed");
    let thread_id = started["result"]["thread"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        started["result"]["thread"]["route"]["providerId"],
        "anthropic-real"
    );
    assert_eq!(started["result"]["thread"]["route"]["revision"], 1);

    run_real_provider_turn(
        &mut client,
        &thread_id,
        "Reply exactly ANTHROPIC_ROUTE_A_92 and nothing else.",
    )
    .await;
    let route_chat =
        switch_real_provider(&mut client, &thread_id, "chat-real", &responses_model, 1).await;
    run_real_provider_turn(
        &mut client,
        &thread_id,
        "Reply exactly CHAT_ROUTE_C_92 and nothing else.",
    )
    .await;
    let route_responses = switch_real_provider(
        &mut client,
        &thread_id,
        "responses-real",
        &responses_model,
        2,
    )
    .await;
    run_real_provider_turn(
        &mut client,
        &thread_id,
        "Reply exactly RESPONSES_ROUTE_B_92 and nothing else.",
    )
    .await;
    let route_a = switch_real_provider(
        &mut client,
        &thread_id,
        "anthropic-real",
        &anthropic_model,
        3,
    )
    .await;
    run_real_provider_turn(
        &mut client,
        &thread_id,
        "Reply exactly ANTHROPIC_ROUTE_A_RETURN_92 and nothing else.",
    )
    .await;

    let read_id = client
        .request("thread/read", json!({"threadId": thread_id}))
        .await;
    let read = loop {
        let message = client.recv().await;
        if message["id"] == read_id {
            break message;
        }
    };
    assert!(
        read.get("error").is_none(),
        "thread/read failed: code={} kind={} message={}",
        read["error"]["code"],
        read["error"]["data"]["kind"],
        read["error"]["message"],
    );
    assert_eq!(read["result"]["thread"]["route"], route_a);
    let public = serde_json::to_string(&read).unwrap();
    for private in [
        anthropic_key.as_str(),
        anthropic_base.as_str(),
        responses_key.as_str(),
        responses_base.as_str(),
        "providerProvenance",
        "endpointFingerprint",
    ] {
        assert!(
            !public.contains(private),
            "public thread/read leaked private route data"
        );
    }
    client.shutdown().await;

    let session = dirs.sessions.join(format!("{thread_id}.jsonl"));
    let inspected = kloop_core::rollout::inspect_session(&session).unwrap();
    let snapshot = inspected.snapshot();
    assert_eq!(
        snapshot
            .provider_routes
            .iter()
            .map(|route| (route.revision, route.provider_id.as_str()))
            .collect::<Vec<_>>(),
        [
            (1, "anthropic-real"),
            (2, "chat-real"),
            (3, "responses-real"),
            (4, "anthropic-real"),
        ]
    );
    let assistants = snapshot
        .messages
        .iter()
        .filter(|message| message.role == kloop_protocol::Role::Assistant)
        .collect::<Vec<_>>();
    assert_eq!(assistants.len(), 4);
    assert_eq!(
        assistants
            .iter()
            .map(|message| {
                let provenance = message.provider_provenance.as_ref().unwrap();
                (provenance.route_revision, provenance.provider_id.as_str())
            })
            .collect::<Vec<_>>(),
        [
            (1, "anthropic-real"),
            (2, "chat-real"),
            (3, "responses-real"),
            (4, "anthropic-real"),
        ]
    );
    let expected_chat_continuity = if assistants[0].has_reasoning() {
        kloop_protocol::ReasoningContinuity::Filtered
    } else {
        kloop_protocol::ReasoningContinuity::Preserved
    };
    assert!(
        !assistants[1].has_reasoning(),
        "Chat assistant history must not contain reasoning"
    );
    let expected_responses_continuity = expected_chat_continuity;
    let expected_a_continuity = if assistants[2].has_reasoning() {
        kloop_protocol::ReasoningContinuity::Filtered
    } else {
        kloop_protocol::ReasoningContinuity::Preserved
    };
    assert_eq!(
        snapshot.provider_routes[1].continuity,
        expected_chat_continuity
    );
    assert_eq!(
        snapshot.provider_routes[2].continuity,
        expected_responses_continuity
    );
    assert_eq!(
        snapshot.provider_routes[3].continuity,
        expected_a_continuity
    );
    assert_eq!(
        route_chat["continuity"],
        serde_json::to_value(expected_chat_continuity).unwrap()
    );
    assert_eq!(
        route_responses["continuity"],
        serde_json::to_value(expected_responses_continuity).unwrap()
    );
    assert_eq!(
        route_a["continuity"],
        serde_json::to_value(expected_a_continuity).unwrap()
    );
    assert_eq!(
        inspected
            .provider_usage()
            .records()
            .iter()
            .map(|record| (record.route_revision, record.provider_id.as_str()))
            .collect::<Vec<_>>(),
        [
            (1, "anthropic-real"),
            (2, "chat-real"),
            (3, "responses-real"),
            (4, "anthropic-real"),
        ]
    );

    println!(
        "real route-switch acceptance passed: anthropic({anthropic_model}) -> chat({responses_model}) -> responses({responses_model}) -> anthropic({anthropic_model}); revisions=1,2,3,4; continuity={},{},{}; usage_records=4; public_redaction=pass",
        route_chat["continuity"].as_str().unwrap(),
        route_responses["continuity"].as_str().unwrap(),
        route_a["continuity"].as_str().unwrap(),
    );
    let _ = std::fs::remove_dir_all(&dirs.root);
}

/// Plan 132. A thread reopens on the configured default, not on whatever it was
/// written on: an in-session `/provider` belongs to the session that ran it.
/// Rebuilding the recorded route as a revision-1 initial route used to refuse
/// the resume outright, and a provider that has since left the configuration
/// used to make the thread unopenable; both now land on today's default with the
/// hop recorded and announced.
#[tokio::test]
async fn reopening_a_thread_starts_on_the_configured_default() {
    let dirs = test_dirs("resume-route-reopen");
    let configured = Arc::new(Mutex::new(vec!["a".to_string(), "b".to_string()]));
    let build = renaming_factory(dirs.offload.clone(), Arc::clone(&configured));

    let mut client = start_server(Arc::clone(&build), &dirs);
    let thread_id = client.init_and_start().await;
    client
        .request("turn/start", json!({"threadId": thread_id, "input": "q1"}))
        .await;
    client
        .recv_until(|message| message["method"] == "turn/completed")
        .await;
    let switch = client
        .request(
            "thread/provider/switch",
            json!({
                "threadId": thread_id,
                "providerId": "b",
                "expectedRouteRevision": 1,
            }),
        )
        .await;
    let switched = client.recv_until(|message| message["id"] == switch).await;
    let switched = switched
        .iter()
        .find(|message| message["id"] == switch)
        .unwrap();
    assert_eq!(switched["result"]["route"]["revision"], 2);
    client.shutdown().await;

    // `b` is still configured, but the default is `a`: the switch does not
    // outlive its session, and the hop off it is a recorded revision.
    let mut client = start_server(Arc::clone(&build), &dirs);
    client.initialize().await;
    let resume = client
        .request("thread/resume", json!({"threadId": thread_id}))
        .await;
    let messages = client.recv_until(|message| message["id"] == resume).await;
    let resumed = messages
        .iter()
        .find(|message| message["id"] == resume)
        .unwrap();
    assert_eq!(resumed["result"]["thread"]["route"]["providerId"], "a");
    assert_eq!(resumed["result"]["thread"]["route"]["revision"], 3);
    assert_eq!(resumed["result"]["messageCount"], 2);
    let note = messages
        .iter()
        .find(|message| message["method"] == "note")
        .unwrap_or_else(|| panic!("no reopen note: {messages:?}"));
    assert_eq!(
        note["params"]["text"],
        "session was written on b/shared; reopening on a/shared"
    );
    client.shutdown().await;

    // The same path carries a provider that has left the configuration entirely.
    *configured.lock().unwrap() = vec!["c".to_string()];
    let mut client = start_server(build, &dirs);
    client.initialize().await;
    let resume = client
        .request("thread/resume", json!({"threadId": thread_id}))
        .await;
    let messages = client.recv_until(|message| message["id"] == resume).await;
    let resumed = messages
        .iter()
        .find(|message| message["id"] == resume)
        .unwrap();
    assert_eq!(resumed["result"]["thread"]["route"]["providerId"], "c");
    assert_eq!(resumed["result"]["thread"]["route"]["revision"], 4);
    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn provider_switch_errors_have_stable_kinds() {
    let dirs = test_dirs("provider-switch-errors");
    let seen_a = Arc::new(Mutex::new(Vec::new()));
    let seen_b = Arc::new(Mutex::new(Vec::new()));
    let mut client = start_server(
        switch_factory(
            dirs.offload.clone(),
            Arc::clone(&seen_a),
            Arc::clone(&seen_b),
        ),
        &dirs,
    );
    let thread_id = client.init_and_start().await;

    let switch = client
        .request(
            "thread/provider/switch",
            json!({
                "threadId": thread_id,
                "providerId": "not-configured",
                "expectedRouteRevision": 1,
            }),
        )
        .await;
    let messages = client.recv_until(|message| message["id"] == switch).await;
    let response = messages
        .iter()
        .find(|message| message["id"] == switch)
        .unwrap();
    assert_eq!(response["error"]["data"]["kind"], "unknown_provider");

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn thread_read_list_resume_and_fork_preserve_runtime() {
    let dirs = test_dirs("native-history");
    let project = dirs.root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = std::fs::canonicalize(project).unwrap();
    let project_text = project.to_string_lossy().to_string();
    let seen = Arc::new(Mutex::new(Vec::new()));

    let mut client = start_server(
        recording_factory(
            vec![vec![text("first answer")]],
            dirs.offload.clone(),
            seen.clone(),
        ),
        &dirs,
    );
    client.initialize().await;
    let start = client
        .request("thread/start", json!({"cwd": project, "model": "model-a"}))
        .await;
    let response = client.recv().await;
    assert_eq!(response["id"], start);
    let thread_id = response["result"]["thread"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    client
        .request("turn/start", json!({"threadId": thread_id, "input": "q1"}))
        .await;
    client
        .recv_until(|message| message["method"] == "turn/completed")
        .await;

    client
        .request("thread/read", json!({"threadId": thread_id}))
        .await;
    let read = client.recv().await;
    assert_eq!(read["result"]["thread"]["cwd"], project_text.as_str());
    assert_eq!(read["result"]["thread"]["route"]["model"], "model-a");
    assert_eq!(read["result"]["thread"]["resumable"], true);
    assert_eq!(
        read["result"]["thread"]["messages"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        read["result"]["thread"]["terminals"][0]["status"],
        "completed"
    );

    client.request("thread/list", json!({"limit": 1})).await;
    let list = client.recv().await;
    assert_eq!(list["result"]["threads"][0]["id"], thread_id);
    assert_eq!(list["result"]["threads"][0]["cwd"], project_text.as_str());
    assert_eq!(list["result"]["threads"][0]["route"]["model"], "model-a");
    assert_eq!(list["result"]["threads"][0]["messages"], 2);
    client.shutdown().await;

    seen.lock().unwrap().clear();
    let mut client = start_server(
        recording_factory(
            vec![vec![text("after restart")]],
            dirs.offload.clone(),
            seen.clone(),
        ),
        &dirs,
    );
    client.initialize().await;
    let resume = client
        .request("thread/resume", json!({"threadId": thread_id}))
        .await;
    let messages = client.recv_until(|message| message["id"] == resume).await;
    let resumed = messages
        .iter()
        .find(|message| message["id"] == resume)
        .unwrap();
    assert_eq!(resumed["result"]["thread"]["cwd"], project_text.as_str());
    // Reopening starts on the default route rather than the recorded one, and
    // the options carry no provider/model for the factory to reproduce. This
    // fixture's factory declares one model per thread, so the session that ran
    // on `model-a` comes back on the default `mock` as a recorded hop.
    assert_eq!(resumed["result"]["thread"]["route"]["model"], "mock");
    assert_eq!(resumed["result"]["thread"]["route"]["revision"], 2);
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[ThreadStartOptions {
            cwd: project.clone(),
            provider_id: None,
            model: None,
        }]
    );

    client
        .request("thread/fork", json!({"threadId": thread_id}))
        .await;
    let forked = client.recv().await;
    let fork_id = forked["result"]["thread"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("fork response: {forked}"))
        .to_string();
    assert_eq!(forked["result"]["thread"]["cwd"], project_text.as_str());
    // The fork branches off what the resumed thread is already on, so its route
    // is adopted unchanged — no second hop, no new revision.
    assert_eq!(forked["result"]["thread"]["route"]["model"], "mock");
    assert_eq!(forked["result"]["thread"]["route"]["revision"], 2);
    client
        .request("thread/read", json!({"threadId": fork_id}))
        .await;
    let fork_read = client.recv().await;
    assert_eq!(
        fork_read["result"]["thread"]["forkedFrom"]["threadId"],
        thread_id
    );
    assert_eq!(fork_read["result"]["thread"]["cwd"], project_text.as_str());

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn thread_read_strips_reasoning_replay_secrets() {
    let dirs = test_dirs("reasoning-public-projection");
    std::fs::create_dir_all(&dirs.sessions).unwrap();
    let path = dirs.sessions.join("reasoning.jsonl");
    let mut rollout = Rollout::new(path);
    rollout
        .append_message(&Message::user_text("question"))
        .unwrap();
    rollout
        .append_message(&Message::assistant_from_provider(
            vec![
                ContentBlock::Thinking {
                    thinking: "display summary".into(),
                    signature: "opaque-signature".into(),
                },
                ContentBlock::RedactedThinking {
                    data: "opaque-redacted".into(),
                },
            ],
            kloop_protocol::ProviderResponseProvenance {
                route_revision: 1,
                origin_boundary: 3,
                provider_id: "test".into(),
                api_family: kloop_protocol::ProviderApiFamily::Mock,
                endpoint_fingerprint: Provider::mock(Vec::new()).endpoint_fingerprint(),
                model: "mock".into(),
            },
        ))
        .unwrap();
    drop(rollout);

    let mut client = start_server(
        factory(vec![vec![text("unused")]], dirs.offload.clone(), false),
        &dirs,
    );
    client.initialize().await;
    client
        .request("thread/read", json!({"threadId": "reasoning"}))
        .await;
    let read = client.recv().await;
    let assistant = &read["result"]["thread"]["messages"][1];
    assert!(assistant.get("provider_provenance").is_none());
    assert_eq!(
        assistant["content"],
        json!([{"type": "thinking", "thinking": "display summary"}])
    );
    let wire = serde_json::to_string(&read).unwrap();
    assert!(!wire.contains("opaque-signature"));
    assert!(!wire.contains("opaque-redacted"));

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn legacy_session_without_route_timeline_is_rejected_without_migration() {
    let dirs = test_dirs("legacy-history");
    std::fs::create_dir_all(&dirs.sessions).unwrap();
    let thread_id = "legacy";
    let path = dirs.sessions.join("legacy.jsonl");
    let mut rollout = Rollout::new(path.clone());
    rollout
        .append_message(&Message::user_text("old question"))
        .unwrap();
    drop(rollout);
    let legacy = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .skip(1)
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    std::fs::write(&path, legacy).unwrap();

    let mut client = start_server(
        factory(vec![vec![text("answer")]], dirs.offload.clone(), false),
        &dirs,
    );
    client.initialize().await;
    for request in [
        json!({"threadId": thread_id}),
        json!({"threadId": thread_id, "cwd": dirs.root}),
    ] {
        client.request("thread/resume", request).await;
        let error = client.recv().await;
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("provider route timeline"),
            "{error}"
        );
    }
    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn read_methods_do_not_repair_torn_tail() {
    let dirs = test_dirs("read-only-tail");
    std::fs::create_dir_all(&dirs.sessions).unwrap();
    let path = dirs.sessions.join("torn.jsonl");
    let cwd = std::fs::canonicalize(&dirs.root).unwrap();
    let mut rollout = Rollout::new_with_runtime_and_route(
        path.clone(),
        SessionRuntime {
            cwd: cwd.to_string_lossy().into_owned(),
        },
        &mock_route(),
    )
    .unwrap();
    rollout
        .append_message(&Message::user_text("intact"))
        .unwrap();
    let intact_len = std::fs::metadata(&path).unwrap().len();
    let mut raw = std::fs::read(&path).unwrap();
    raw.extend_from_slice(b"{\"type\":\"message\",\"role\":\"user\"");
    std::fs::write(&path, &raw).unwrap();

    let mut client = start_server(factory(Vec::new(), dirs.offload.clone(), false), &dirs);
    client.initialize().await;
    client
        .request("thread/read", json!({"threadId": "torn"}))
        .await;
    let read = client.recv().await;
    assert!(read["error"].is_null());
    assert_eq!(std::fs::read(&path).unwrap(), raw);
    client.shutdown().await;

    let mut client = start_server(factory(Vec::new(), dirs.offload.clone(), false), &dirs);
    client.initialize().await;
    client
        .request("thread/resume", json!({"threadId": "torn"}))
        .await;
    let resumed = client.recv().await;
    assert!(resumed["error"].is_null(), "resume failed: {resumed}");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), intact_len);
    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn client_thread_ids_cannot_escape_sessions_dir() {
    let dirs = test_dirs("safe-thread-id");
    std::fs::create_dir_all(&dirs.sessions).unwrap();
    let outside = dirs.root.join("outside.jsonl");
    std::fs::write(&outside, b"external bytes").unwrap();
    let absolute = outside.to_string_lossy().into_owned();
    let mut client = start_server(factory(Vec::new(), dirs.offload.clone(), false), &dirs);
    client.initialize().await;

    for (method, params) in [
        ("thread/read", json!({"threadId": "../outside"})),
        ("thread/resume", json!({"threadId": "../outside"})),
        ("thread/fork", json!({"threadId": "../outside"})),
        ("config/read", json!({"threadId": "../outside"})),
        ("thread/read", json!({"threadId": absolute})),
        ("thread/read", json!({"threadId": "a/b"})),
        ("thread/read", json!({"threadId": ".."})),
    ] {
        client.request(method, params).await;
        let response = client.recv().await;
        assert_eq!(response["error"]["code"], -32602, "{method}: {response}");
    }
    assert_eq!(std::fs::read(&outside).unwrap(), b"external bytes");
    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn recovery_repairs_pairing_once_and_preserves_public_shape() {
    let dirs = test_dirs("repair-server");
    std::fs::create_dir_all(&dirs.sessions).unwrap();
    let path = dirs.sessions.join("repair.jsonl");
    let cwd = std::fs::canonicalize(&dirs.root).unwrap();
    let mut rollout = Rollout::new_with_runtime_and_route(
        path.clone(),
        SessionRuntime {
            cwd: cwd.to_string_lossy().into_owned(),
        },
        &mock_route(),
    )
    .unwrap();
    rollout
        .append_message(&Message::assistant(vec![ContentBlock::ToolUse {
            id: "missing".into(),
            name: "bash".into(),
            input: json!({"command": "true"}),
        }]))
        .unwrap();
    let before = std::fs::read(&path).unwrap();

    let mut client = start_server(factory(Vec::new(), dirs.offload.clone(), false), &dirs);
    client.initialize().await;
    client
        .request("thread/read", json!({"threadId": "repair"}))
        .await;
    let read = client.recv().await;
    assert_eq!(
        read["result"]["thread"]["messages"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    client.shutdown().await;

    let mut client = start_server(factory(Vec::new(), dirs.offload.clone(), false), &dirs);
    client.initialize().await;
    client
        .request("thread/resume", json!({"threadId": "repair"}))
        .await;
    assert!(client.recv().await["error"].is_null());
    let marker_bytes = std::fs::read(&path).unwrap();
    assert!(String::from_utf8_lossy(&marker_bytes).contains("\"type\":\"repaired\""));
    client.shutdown().await;

    let mut client = start_server(factory(Vec::new(), dirs.offload.clone(), false), &dirs);
    client.initialize().await;
    client
        .request("thread/resume", json!({"threadId": "repair"}))
        .await;
    assert!(client.recv().await["error"].is_null());
    assert_eq!(std::fs::read(&path).unwrap(), marker_bytes);
    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn early_prefix_fork_restores_route_and_runtime_at_the_cut() {
    let dirs = test_dirs("fork-runtime-prefix");
    std::fs::create_dir_all(&dirs.sessions).unwrap();
    let source_id = "legacy-two-turns";
    let source_path = dirs.sessions.join(format!("{source_id}.jsonl"));
    let cwd = std::fs::canonicalize(&dirs.root).unwrap();
    let mut rollout = Rollout::new_with_runtime_and_route(
        source_path.clone(),
        SessionRuntime {
            cwd: cwd.to_string_lossy().into_owned(),
        },
        &mock_route(),
    )
    .unwrap();
    for message in [
        Message::user_text("q1"),
        Message::assistant(vec![ContentBlock::Text { text: "a1".into() }]),
        Message::user_text("q2"),
        Message::assistant(vec![ContentBlock::Text { text: "a2".into() }]),
    ] {
        rollout.append_message(&message).unwrap();
    }
    drop(rollout);

    let source = kloop_core::rollout::load_session_snapshot(&source_path).unwrap();
    assert_eq!(source.runtime.as_ref().unwrap().cwd, cwd.to_string_lossy());
    assert_eq!(source.provider_routes.last().unwrap().provider_id, "mock");
    let cut = kloop_core::rollout::fork_points(&source_path)
        .unwrap()
        .first()
        .expect("two-turn source has an early fork point")
        .seq;

    let mut client = start_server(factory(Vec::new(), dirs.offload.clone(), false), &dirs);
    client.initialize().await;
    let request = client
        .request("thread/fork", json!({"threadId": source_id, "cut": cut}))
        .await;
    let response = client.recv_until(|message| message["id"] == request).await;
    let fork_id = response
        .iter()
        .find(|message| message["id"] == request)
        .unwrap()["result"]["thread"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    client.shutdown().await;

    let fork =
        kloop_core::rollout::load_session_snapshot(&dirs.sessions.join(format!("{fork_id}.jsonl")))
            .unwrap();
    assert_eq!(
        fork.runtime.unwrap().cwd,
        std::fs::canonicalize(&dirs.root).unwrap().to_string_lossy()
    );
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
        vec![tool_use("t3", "exit_worktree", json!({"action": "keep"}))],
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
    assert_eq!(wt[0]["params"]["branch"], "kloop-worktree-srv");
    let entered_cwd = wt[0]["params"]["cwd"].as_str().unwrap().replace('\\', "/");
    assert!(entered_cwd.ends_with(".kloop/worktrees/srv"));
    assert_eq!(wt[1]["params"]["branch"], Value::Null);

    // The write landed in the worktree, not the main repo; the dirty tree is
    // kept (model exited with default keep).
    assert!(repo.join(".kloop/worktrees/srv/s.txt").exists());
    assert!(!repo.join("s.txt").exists());

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&repo);
}

/// A model that enters a worktree but never exits: shutdown retains the tree
/// because no explicit remove intent was supplied.
#[tokio::test]
async fn unexited_worktree_is_retained_on_shutdown() {
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
        repo.join(".kloop/worktrees/leak").exists(),
        "tree exists mid-session"
    );

    // Shutdown closes the turn channel; without explicit remove intent the
    // worker restores the base cwd and retains the clean tree.
    client.shutdown().await;
    assert!(
        repo.join(".kloop/worktrees/leak").exists(),
        "clean tree retained on shutdown"
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
    // by turn/started and turn/completed, with a token-usage update per round
    // and one more before the close.
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
            // Twice: the agent loop publishes the context size at every round
            // boundary, then the turn bracket repeats the post-turn total.
            "thread/tokenUsage/updated",
            "thread/tokenUsage/updated",
            "turn/completed",
        ]
    );
    let sequenced = log
        .iter()
        .filter(|message| message["params"]["threadId"] == thread_id)
        .collect::<Vec<_>>();
    let generation = sequenced[0]["params"]["eventGeneration"]
        .as_str()
        .expect("event generation")
        .to_string();
    assert!(!generation.is_empty());
    for (index, message) in sequenced.iter().enumerate() {
        assert_eq!(message["params"]["eventGeneration"], generation);
        assert_eq!(message["params"]["seq"], (index + 1).to_string());
    }
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

#[tokio::test]
async fn event_sync_replays_then_resume_changes_generation_and_snapshots_history() {
    let dirs = test_dirs("event-sync");
    let mut client = start_server(
        factory(
            vec![vec![text("first reply")], vec![text("second reply")]],
            dirs.offload.clone(),
            false,
        ),
        &dirs,
    );
    let thread_id = client.init_and_start().await;

    let sync_id = client
        .request("thread/events/sync", json!({"threadId": thread_id}))
        .await;
    let initial = client.recv().await;
    assert_eq!(initial["id"], sync_id);
    assert_eq!(initial["result"]["mode"], "snapshot");
    assert_eq!(initial["result"]["reason"], "initial");
    assert_eq!(initial["result"]["highWaterSeq"], "0");
    assert_eq!(initial["result"]["snapshot"]["schemaVersion"], 1);
    assert_eq!(
        initial["result"]["snapshot"]["recovery"],
        json!({"source": "fresh", "volatileState": "live"})
    );
    let initial_cursor = initial["result"]["eventCursor"].clone();
    let generation = initial_cursor["generation"].as_str().unwrap().to_string();

    client
        .request(
            "turn/start",
            json!({"threadId": thread_id, "input": "first input"}),
        )
        .await;
    let live = client
        .recv_until(|message| message["method"] == "turn/completed")
        .await;
    let live_events = live
        .iter()
        .filter(|message| message["params"]["threadId"] == thread_id)
        .map(|message| {
            json!({
                "method": message["method"].clone(),
                "params": message["params"].clone(),
            })
        })
        .collect::<Vec<_>>();
    assert!(!live_events.is_empty());

    let replay_id = client
        .request(
            "thread/events/sync",
            json!({"threadId": thread_id, "eventCursor": initial_cursor}),
        )
        .await;
    let replay = client.recv().await;
    assert_eq!(replay["id"], replay_id);
    assert_eq!(replay["result"]["mode"], "replay");
    assert_eq!(replay["result"]["generation"], generation);
    assert_eq!(replay["result"]["events"], json!(live_events));
    let high_water_cursor = replay["result"]["eventCursor"].clone();

    let current_id = client
        .request(
            "thread/events/sync",
            json!({"threadId": thread_id, "eventCursor": high_water_cursor.clone()}),
        )
        .await;
    let current = client.recv().await;
    assert_eq!(current["id"], current_id);
    assert_eq!(current["result"]["mode"], "replay");
    assert_eq!(current["result"]["events"], json!([]));

    let snapshot_id = client
        .request("thread/events/sync", json!({"threadId": thread_id}))
        .await;
    let snapshot = client.recv().await;
    assert_eq!(snapshot["id"], snapshot_id);
    assert_eq!(
        snapshot["result"]["snapshot"]["tail"]["turns"][0]["input"],
        json!([{"type": "text", "text": "first input"}])
    );
    assert_eq!(
        snapshot["result"]["snapshot"]["tail"]["turns"][0]["items"][0]["text"],
        "first reply"
    );

    client
        .request(
            "turn/start",
            json!({"threadId": thread_id, "input": "second input"}),
        )
        .await;
    let second_live = client
        .recv_until(|message| message["method"] == "turn/completed")
        .await;
    let first_second_seq = second_live
        .iter()
        .find_map(|message| message["params"]["seq"].as_str())
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let first_high_water = high_water_cursor["seq"]
        .as_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert_eq!(first_second_seq, first_high_water + 1);

    for bad_cursor in [
        json!({"threadId": thread_id, "generation": generation, "seq": 1}),
        json!({"threadId": "other", "generation": generation, "seq": "0"}),
        json!({"threadId": thread_id, "generation": generation, "seq": "18446744073709551616"}),
        json!({"threadId": thread_id, "generation": generation, "seq": "999999"}),
    ] {
        let id = client
            .request(
                "thread/events/sync",
                json!({"threadId": thread_id, "eventCursor": bad_cursor}),
            )
            .await;
        let error = client.recv().await;
        assert_eq!(error["id"], id);
        assert_eq!(error["error"]["code"], -32602);
    }

    client.shutdown().await;

    let mut resumed = start_server(
        factory(vec![vec![text("unused")]], dirs.offload.clone(), false),
        &dirs,
    );
    resumed.initialize().await;
    let resume_id = resumed
        .request("thread/resume", json!({"threadId": thread_id}))
        .await;
    assert_eq!(resumed.recv().await["id"], resume_id);
    let changed_id = resumed
        .request(
            "thread/events/sync",
            json!({"threadId": thread_id, "eventCursor": high_water_cursor}),
        )
        .await;
    let changed = resumed.recv().await;
    assert_eq!(changed["id"], changed_id);
    assert_eq!(changed["result"]["mode"], "snapshot");
    assert_eq!(changed["result"]["reason"], "generationChanged");
    assert_ne!(changed["result"]["generation"], generation);
    assert_eq!(
        changed["result"]["snapshot"]["recovery"],
        json!({"source": "resumed", "volatileState": "reset"})
    );
    assert_eq!(
        changed["result"]["snapshot"]["history"]["messages"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(changed["result"]["snapshot"]["tail"]["turns"], json!([]));

    resumed.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn scheduled_idle_delivery_allocates_the_next_turn_id() {
    let dirs = test_dirs("scheduled-delivery");
    let clock = kloop_core::scheduler::ManualClock::new(0);
    let turns = vec![
        vec![tool_use(
            "schedule",
            "schedule_wakeup",
            json!({
                "delay_seconds": 60,
                "reason": "deterministic test",
                "prompt": "timer work",
            }),
        )],
        vec![text("ordinary turn done")],
        vec![text("timer answer")],
    ];
    let mut client = start_server(
        clocked_scheduler_factory(turns, dirs.offload.clone(), clock.clone()),
        &dirs,
    );
    let thread_id = client.init_and_start().await;
    let request_id = client
        .request(
            "turn/start",
            json!({"threadId": thread_id, "input": "start timer"}),
        )
        .await;
    let first_log = client
        .recv_until(|message| message["method"] == "turn/completed")
        .await;
    let first_turn_id = first_log
        .iter()
        .find(|message| message["id"] == request_id)
        .unwrap()["result"]["turn"]["id"]
        .as_u64()
        .unwrap();
    assert!(first_log.iter().any(|message| {
        message["method"] == "thread/scheduler/updated"
            && message["params"]["task"]["origin"] == "loopWakeup"
            && message["params"]["task"]["status"] == "scheduled"
    }));

    clock.set(60_000);
    let delivery = client
        .recv_until(|message| message["method"] == "turn/completed")
        .await;
    let started = delivery
        .iter()
        .find(|message| message["method"] == "turn/started")
        .expect("scheduled turn/started");
    let delivery_turn_id = started["params"]["turn"]["id"].as_u64().unwrap();
    assert_eq!(delivery_turn_id, first_turn_id + 1);
    assert!(delivery.iter().any(|message| {
        message["method"] == "thread/scheduler/updated"
            && message["params"]["task"]["origin"] == "loopWakeup"
            && message["params"]["task"]["status"] == "fired"
            && message["params"].get("turnId").is_none()
    }));
    for message in &delivery {
        if matches!(
            message["method"].as_str(),
            Some("item/started" | "item/delta" | "item/completed")
        ) {
            assert_eq!(message["params"]["turnId"], delivery_turn_id);
        }
    }
    let completed = delivery.last().unwrap();
    assert_eq!(completed["params"]["turn"]["id"], delivery_turn_id);
    assert!(delivery.iter().any(|message| {
        message["method"] == "item/delta" && message["params"]["text"] == "timer answer"
    }));

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

/// Task tools use the ordinary toolCall lifecycle; the native wire has no
/// task-board or retired todo item type.
#[tokio::test]
async fn task_create_surfaces_as_an_ordinary_tool_call() {
    let dirs = test_dirs("task-create");
    let input = json!({"subject":"Parse","description":"Parse the input"});
    let turns = vec![
        vec![tool_use("t1", "task_create", input.clone())],
        vec![text("done")],
    ];
    let mut client = start_server(factory(turns, dirs.offload.clone(), false), &dirs);

    let thread_id = client.init_and_start().await;
    client
        .request("turn/start", json!({"threadId": thread_id, "input": "go"}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;

    assert!(
        !log.iter()
            .any(|message| message["params"]["item"]["type"] == "todo"),
        "the retired todo item type must be absent: {log:?}"
    );
    assert!(
        !log.iter().any(|message| {
            message["method"]
                .as_str()
                .is_some_and(|method| method.to_ascii_lowercase().contains("taskgraph"))
                || matches!(
                    message["params"]["item"]["type"].as_str(),
                    Some("task" | "taskGraph")
                )
        }),
        "the internal Task graph snapshot must not become public wire: {log:?}"
    );
    let calls = log
        .iter()
        .filter(|message| {
            message["params"]["item"]["type"] == "toolCall"
                && message["params"]["item"]["name"] == "task_create"
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2, "started and completed toolCall: {log:?}");
    assert_eq!(calls[0]["method"], "item/started");
    assert_eq!(calls[0]["params"]["item"]["input"], input);
    assert_eq!(calls[1]["method"], "item/completed");
    assert_eq!(calls[1]["params"]["item"]["status"], "completed");
    assert!(
        calls[1]["params"]["item"]["output"]
            .as_str()
            .is_some_and(|output| output.contains("\"id\":\"1\""))
    );

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn task_graph_is_isolated_per_server_thread() {
    let dirs = test_dirs("task-thread-isolation");
    let turns = vec![
        vec![tool_use(
            "t1",
            "task_create",
            json!({"subject":"Thread task","description":"must stay local"}),
        )],
        vec![text("done")],
    ];
    let mut client = start_server(factory(turns, dirs.offload.clone(), false), &dirs);
    client.initialize().await;
    client.request("thread/start", json!({})).await;
    let first = client.recv().await["result"]["thread"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    client.request("thread/start", json!({})).await;
    let second = client.recv().await["result"]["thread"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    client
        .request("turn/start", json!({"threadId":first,"input":"create"}))
        .await;
    client
        .request("turn/start", json!({"threadId":second,"input":"create"}))
        .await;
    let mut completed = 0;
    let log = client
        .recv_until(|message| {
            if message["method"] == "turn/completed" {
                completed += 1;
            }
            completed == 2
        })
        .await;
    for thread_id in [&first, &second] {
        let output = log
            .iter()
            .find(|message| {
                message["method"] == "item/completed"
                    && message["params"]["threadId"] == *thread_id
                    && message["params"]["item"]["name"] == "task_create"
            })
            .and_then(|message| message["params"]["item"]["output"].as_str())
            .unwrap_or_else(|| panic!("missing task_create completion for {thread_id}: {log:?}"));
        let output: Value = serde_json::from_str(output).unwrap();
        assert_eq!(
            output["task"]["id"], "1",
            "each server thread owns a fresh registry"
        );
    }

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn partial_stream_is_sealed_then_continued_and_stays_recoverable() {
    let dirs = test_dirs("partial-history");
    let mut client = start_server(partial_factory(dirs.offload.clone()), &dirs);
    let thread_id = client.init_and_start().await;

    client
        .request(
            "turn/start",
            json!({"threadId": thread_id, "input": "answer"}),
        )
        .await;
    let log = client
        .recv_until(|message| message["method"] == "turn/completed")
        .await;
    // The transient drop is resumed, so the turn ends normally — but the wire
    // contract this test guards is unchanged: one terminal, and the interrupted
    // item sealed exactly once rather than left open or re-opened.
    assert_eq!(log.last().unwrap()["params"]["turn"]["status"], "completed");
    assert_eq!(
        log.iter()
            .filter(|message| message["method"] == "turn/completed")
            .count(),
        1,
        "the turn has exactly one terminal notification"
    );
    let completed_items: Vec<&Value> = log
        .iter()
        .filter(|message| message["method"] == "item/completed")
        .collect();
    assert_eq!(
        completed_items.len(),
        2,
        "the sealed partial and the continuation are each completed once"
    );

    client
        .request("thread/read", json!({"threadId": thread_id}))
        .await;
    let read = client.recv().await;
    let thread = &read["result"]["thread"];
    // The partial survives a fresh read: it is a real assistant message in the
    // thread, followed by the nudge and the continuation.
    assert_eq!(thread["messages"].as_array().unwrap().len(), 4);
    assert_eq!(thread["messages"][1]["content"][0]["text"], "half answer");
    assert_eq!(thread["messages"][3]["content"][0]["text"], " and the rest");
    assert_eq!(
        thread["terminals"],
        json!([{
            "afterMessage": 4,
            "status": "completed",
        }])
    );

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

/// A run_agent call surfaces the sub-agent lifecycle on the wire: a `subAgent`
/// brackets it, and the sub-agent's own tool calls carry an "agent" field while
/// the main agent's calls stay unadorned.
#[tokio::test]
async fn subagent_items_carry_the_agent_label() {
    let dirs = test_dirs("subagent");
    let script = vec![
        // main: spawn the sub-agent
        vec![tool_use("t1", "run_agent", json!({"prompt": "sub work"}))],
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
            m["params"]["item"]["type"] == "toolCall" && m["params"]["item"]["name"] == "run_agent"
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
    assert!(request["params"].get("eventGeneration").is_none());
    assert!(request["params"].get("seq").is_none());
    assert_eq!(request["params"]["kind"], "fileChange");
    assert!(
        request["params"]["description"]
            .as_str()
            .unwrap()
            .contains("write_file")
    );
    assert_eq!(
        request["params"]["approvalScopes"],
        json!(["once", "workspaceSession"])
    );
    assert!(request["params"]["rememberRules"].is_array());
    // The change preview reaches the client (a fresh path is a new file).
    assert_eq!(request["params"]["preview"], "(new file)\n+1  x");

    client.request("thread/list", json!({})).await;
    let list = client.recv().await;
    let active = list["result"]["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|thread| thread["id"] == thread_id)
        .unwrap();
    assert_eq!(active["inProgress"], true);
    let files_before = std::fs::read_dir(&dirs.sessions).unwrap().count();
    client
        .request("thread/fork", json!({"threadId": thread_id}))
        .await;
    let fork_error = client.recv().await;
    assert!(
        fork_error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("while a turn is running")
    );
    assert_eq!(
        std::fs::read_dir(&dirs.sessions).unwrap().count(),
        files_before,
        "a rejected running fork must not create a rollout"
    );

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

#[tokio::test]
async fn negotiated_questions_round_trip_multiple_answers() {
    let dirs = test_dirs("questions");
    let ask = tool_use(
        "q1",
        "ask_user_question",
        json!({
            "questions": [
                {
                    "question": "Choose one",
                    "header": "Single",
                    "options": [
                        {"label": "A", "description": "first", "preview": "preview A"},
                        {"label": "B", "description": "second"}
                    ],
                    "multiSelect": false
                },
                {
                    "question": "Choose several",
                    "header": "Multi",
                    "options": [
                        {"label": "X", "description": "x"},
                        {"label": "Y", "description": "y"}
                    ],
                    "multiSelect": true
                }
            ]
        }),
    );
    let mut client = start_server(
        factory(
            vec![vec![ask], vec![text("answers received")]],
            dirs.offload.clone(),
            false,
        ),
        &dirs,
    );
    let thread_id = client.init_questions_and_start().await;
    client
        .request("turn/start", json!({"threadId": thread_id, "input": "ask"}))
        .await;

    let log = client
        .recv_until(|message| message["method"] == "question/request")
        .await;
    let first = log.last().unwrap();
    assert_eq!(first["params"]["threadId"], thread_id);
    assert_eq!(first["params"]["turnId"], 1);
    assert_eq!(first["params"]["questionIndex"], 0);
    assert_eq!(first["params"]["question"]["header"], "Single");
    assert_eq!(
        first["params"]["question"]["options"][0]["preview"],
        "preview A"
    );
    let first_id = first["id"].as_i64().unwrap();
    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": first_id,
            "result": {"outcome": "answered", "selected": [1], "notes": "prefer B"}
        }))
        .await;

    let second = client
        .recv_until(|message| message["method"] == "question/request")
        .await
        .pop()
        .unwrap();
    assert_eq!(second["params"]["questionIndex"], 1);
    assert_eq!(second["params"]["question"]["multiSelect"], true);
    let second_id = second["id"].as_i64().unwrap();
    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": second_id,
            "result": {"outcome": "answered", "selected": [0, 1], "other": "Z"}
        }))
        .await;

    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert!(log.iter().any(|m| {
        m["method"] == "item/completed"
            && m["params"]["item"]["name"] == "ask_user_question"
            && m["params"]["item"]["status"] == "completed"
    }));
    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}

#[tokio::test]
async fn missing_question_capability_fails_closed_without_reverse_request() {
    let dirs = test_dirs("questions-unsupported");
    let ask = tool_use(
        "q1",
        "ask_user_question",
        json!({
            "questions": [{
                "question": "Choose",
                "header": "Choice",
                "options": [
                    {"label": "A", "description": "first"},
                    {"label": "B", "description": "second"}
                ],
                "multiSelect": false
            }]
        }),
    );
    let mut client = start_server(
        factory(
            vec![vec![ask], vec![text("continued")]],
            dirs.offload.clone(),
            false,
        ),
        &dirs,
    );
    let thread_id = client.init_and_start().await;
    client
        .request("turn/start", json!({"threadId": thread_id, "input": "ask"}))
        .await;
    let log = client.recv_until(|m| m["method"] == "turn/completed").await;
    assert!(!log.iter().any(|m| m["method"] == "question/request"));
    assert!(log.iter().any(|m| {
        m["method"] == "item/completed"
            && m["params"]["item"]["name"] == "ask_user_question"
            && m["params"]["item"]["status"] == "failed"
    }));
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
    assert!(
        log.last().unwrap()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already running")
    );

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
    assert!(
        log.iter()
            .any(|m| m["id"] == next_id && m["result"]["turn"]["id"].is_u64())
    );

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
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no session")
    );

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
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already active")
    );

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
        Some(format!("{src}#5"))
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
        msg.contains("cannot fork") && msg.contains("#5"),
        "illegal-cut error must list the legal points: {msg}"
    );

    // Forking a session that does not exist is a clean error too.
    client
        .request("thread/fork", json!({"threadId": "ghost"}))
        .await;
    let err = client.recv().await;
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no session")
    );

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

    // Provider control commands use the idle switch transaction directly: they
    // emit one bounded system result, not a turn bracket or usage event.
    client
        .request("turn/start", json!({"threadId": tid, "input": "/provider"}))
        .await;
    let provider_log = client.recv_until(|m| m["method"] == "system").await;
    assert_eq!(methods_for_thread(&provider_log, &tid), vec!["system"]);
    let provider_message = provider_log
        .iter()
        .find(|message| message["method"] == "system")
        .unwrap();
    assert!(
        provider_message["params"]["text"]
            .as_str()
            .unwrap()
            .contains("active:")
    );

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
    // Nor a terminal. The terminal is written by run_turn, which a command line
    // never reaches — a command reads and rewrites History, but nothing is
    // sampled, so there is no turn to end.
    let terminals =
        kloop_core::rollout::inspect_session(&dirs.sessions.join(format!("{tid}.jsonl")))
            .unwrap()
            .snapshot()
            .terminals;
    assert!(
        terminals.is_empty(),
        "a slash command is not a model turn: {terminals:?}"
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

/// Plan 102 against a live endpoint. The unit tests only lock the request body
/// kloop builds; this is the one that learns which effort levels the model on
/// the other end actually takes — and it is what proved the levels are a
/// property of the **model**, not the rail — and that produced the vocabulary
/// itself: `minimal` was dropped after every model measured refused it.
///
/// It sweeps kloop's whole vocabulary, prints the accepted/refused table, and
/// asserts only what every reasoning model owes: `low`/`medium`/`high` sample
/// successfully, `unset` (no field at all) samples successfully, and a level
/// kloop does not spell is refused by kloop before any request. A completed turn
/// is the signal — the request body is not observable from here.
///
/// One rail per run, selected by KLOOP_PROVIDER (anthropic | openai |
/// openai-responses) with that rail's usual credential/model variables:
///
///   KLOOP_PROVIDER=openai-responses OPENAI_API_KEY=… OPENAI_BASE_URL=… \
///     OPENAI_MODEL=gpt-5.6-sol cargo test -p kloop-server --test server \
///     real_effort_sweep_contract -- --exact --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires real Anthropic, OpenAI Chat, or OpenAI Responses credentials"]
async fn real_effort_sweep_contract() {
    let rail = required_real_env("KLOOP_PROVIDER");
    let (api_family, key, base, model) = match rail.as_str() {
        "anthropic" => (
            kloop_protocol::ProviderApiFamily::AnthropicMessages,
            required_real_env("ANTHROPIC_API_KEY"),
            required_real_env("ANTHROPIC_BASE_URL"),
            required_real_env("ANTHROPIC_MODEL"),
        ),
        "openai" => (
            kloop_protocol::ProviderApiFamily::OpenAiChatCompletions,
            required_real_env("OPENAI_API_KEY"),
            required_real_env("OPENAI_BASE_URL"),
            required_real_env("OPENAI_MODEL"),
        ),
        "openai-responses" => (
            kloop_protocol::ProviderApiFamily::OpenAiResponses,
            required_real_env("OPENAI_API_KEY"),
            required_real_env("OPENAI_BASE_URL"),
            required_real_env("OPENAI_MODEL"),
        ),
        other => {
            panic!("KLOOP_PROVIDER '{other}' must be anthropic, openai, or openai-responses")
        }
    };
    let base = base.trim_end_matches('/').to_string();
    let provider_id = format!("{rail}-real");

    let factory_key = key.clone();
    let factory_base = base.clone();
    let catalog = Arc::new(
        kloop_core::provider_route::ProviderCatalog::new(vec![
            kloop_core::provider_route::ProviderCatalogEntry {
                id: provider_id.clone(),
                api_family,
                endpoint_fingerprint: Provider::endpoint_fingerprint_for(api_family, &base),
                default_model: model.clone(),
                models: vec![model.clone()],
                availability: kloop_protocol::ProviderAvailabilityCode::Ready,
                // The sweep sets every level explicitly; starting from "no field
                // sent" keeps the first turn a clean control.
                default_effort: None,
                factory: Arc::new(move || {
                    let key = factory_key.clone();
                    let base = factory_base.clone();
                    Ok(match api_family {
                        kloop_protocol::ProviderApiFamily::AnthropicMessages => {
                            Provider::Anthropic {
                                key,
                                base,
                                cache: true,
                                thinking: kloop_provider::ThinkingMode::Unset,
                            }
                        }
                        kloop_protocol::ProviderApiFamily::OpenAiChatCompletions => {
                            Provider::OpenAiCompat { key, base }
                        }
                        kloop_protocol::ProviderApiFamily::OpenAiResponses => {
                            Provider::OpenAiResponses { key, base }
                        }
                        kloop_protocol::ProviderApiFamily::Mock => {
                            unreachable!("a real rail is selected above")
                        }
                    })
                }),
            },
        ])
        .unwrap(),
    );

    let dirs = test_dirs("real-effort-sweep");
    std::fs::create_dir_all(&dirs.root).unwrap();
    let cwd = std::fs::canonicalize(&dirs.root).unwrap();
    let mut server = ServerConfig::new(
        real_switch_factory(dirs.offload.clone()),
        ServerPaths {
            store: dirs.store.clone(),
        },
    );
    server.provider_catalog = Arc::clone(&catalog);
    let mut client = start_server_with_config(server);
    client.initialize().await;

    let start_id = client
        .request(
            "thread/start",
            json!({"cwd": cwd, "providerId": provider_id, "model": model}),
        )
        .await;
    let started = client.recv_with_timeout(Duration::from_secs(60)).await;
    assert_eq!(started["id"], start_id);
    assert!(started.get("error").is_none(), "real thread start failed");
    let thread_id = started["result"]["thread"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    // Nothing configured, so the route publishes no effort and sends no field.
    assert!(started["result"]["thread"]["route"]["effort"].is_null());

    // The no-field control first. If this fails the endpoint is simply
    // unavailable and every effort row below would be noise — that control is
    // what identified a throttled rail as throttled rather than effort-refusing.
    let cleared = run_real_command(&mut client, &thread_id, "/effort unset").await;
    assert!(
        cleared.starts_with("effort: unset —"),
        "unexpected: {cleared}"
    );
    assert_eq!(
        probe_real_effort(
            &mut client,
            &thread_id,
            "Reply exactly EFFORT_UNSET and nothing else.",
        )
        .await,
        EffortOutcome::Accepted,
        "the no-field control must sample cleanly, or the rows below are noise"
    );

    println!("== {rail} / {model} ==");
    println!("  (no field) ACCEPTED");
    let mut outcomes = Vec::new();
    for effort in kloop_protocol::ReasoningEffort::ALL {
        let shown = run_real_command(&mut client, &thread_id, &format!("/effort {effort}")).await;
        assert!(
            shown.starts_with(&format!("effort: {effort} (provider {provider_id})")),
            "unexpected /effort output for {effort}: {shown}"
        );
        let outcome = probe_real_effort(
            &mut client,
            &thread_id,
            &format!("Reply exactly EFFORT_{effort} and nothing else."),
        )
        .await;
        match &outcome {
            EffortOutcome::Accepted => println!("  {effort:<8} ACCEPTED"),
            EffortOutcome::Refused(error) => {
                let error = error.replace('\n', " ");
                println!("  {effort:<8} REFUSED  {}", &error[..error.len().min(200)]);
            }
            EffortOutcome::RateLimited(_) => {
                println!("  {effort:<8} INCONCLUSIVE (endpoint rate-limited)")
            }
        }
        outcomes.push((*effort, outcome));
    }

    // The floor every reasoning model owes; the rest of the table is this
    // model's own contract and is reported, not asserted. A throttled row is
    // called out as such — it is not evidence either way.
    for required in [
        kloop_protocol::ReasoningEffort::Low,
        kloop_protocol::ReasoningEffort::Medium,
        kloop_protocol::ReasoningEffort::High,
    ] {
        match outcomes
            .iter()
            .find(|(effort, _)| *effort == required)
            .map(|(_, outcome)| outcome)
        {
            Some(EffortOutcome::Accepted) => {}
            Some(EffortOutcome::RateLimited(error)) => panic!(
                "endpoint stayed rate-limited on '{required}' after retries, \
                 so this run proves nothing about it: {error}"
            ),
            other => panic!(
                "{model} refused '{required}', which every reasoning model is \
                 expected to take: {other:?}"
            ),
        }
    }

    // kloop enforces its own spelling and nothing else — no request is made.
    let unknown = run_real_command(&mut client, &thread_id, "/effort hgih").await;
    assert!(
        unknown.starts_with("unknown effort 'hgih'"),
        "unexpected: {unknown}"
    );

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dirs.root);
}
