#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use kloop_core::context::BASE_SYSTEM;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::time::timeout;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Fixture {
    _root: TempDir,
    home: PathBuf,
    workspace: PathBuf,
    server: MockServer,
}

impl Fixture {
    async fn new() -> Self {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        let private = home.join(".kloop");
        std::fs::create_dir_all(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("AGENTS.md"), "PROJECT_RULES_SENTINEL").unwrap();

        let server = MockServer::start().await;
        let delta = json!({"choices": [{"index": 0, "delta": {"content": "ACK"}}]});
        let finish = json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]});
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                format!("data: {delta}\n\ndata: {finish}\n\ndata: [DONE]\n\n"),
                "text/event-stream",
            ))
            .mount(&server)
            .await;
        Self {
            _root: root,
            home,
            workspace,
            server,
        }
    }

    fn write_config(&self, prompt: Option<&str>) {
        use std::os::unix::fs::PermissionsExt as _;

        let system = prompt.map_or_else(String::new, |prompt| {
            std::fs::write(self.prompt_path(), prompt).unwrap();
            "system_prompt_file = 'system-prompt.md'\n".to_string()
        });
        let url = self.server.uri();
        let config = format!(
            "provider = 'local'\n\
             {system}\n\
             [providers.local]\n\
             wire_api = 'chat'\n\
             base_url = '{url}'\n\
             auth_header = {{ Authorization = 'Bearer synthetic-key' }}\n\
             model = 'fixture'\n\
             [shells]\n\
             login_env = false\n"
        );
        let path = self.home.join(".kloop/config.toml");
        std::fs::write(&path, config).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn prompt_path(&self) -> PathBuf {
        self.home.join(".kloop/system-prompt.md")
    }

    fn set_prompt_file(&self, path: &Path) {
        let config_path = self.home.join(".kloop/config.toml");
        let mut table: toml::Table = std::fs::read_to_string(&config_path)
            .unwrap()
            .parse()
            .unwrap();
        table.insert(
            "system_prompt_file".into(),
            toml::Value::String(path.to_str().unwrap().to_string()),
        );
        std::fs::write(config_path, toml::to_string(&table).unwrap()).unwrap();
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kloop"));
        command
            .current_dir(&self.workspace)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", "/usr/bin:/bin")
            .kill_on_drop(true);
        command
    }

    async fn headless(&self, args: &[&str]) -> String {
        let output = timeout(
            Duration::from_secs(15),
            self.command()
                .args(["--headless", "--json"])
                .args(args)
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .expect("headless timed out")
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let events: Vec<Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            // 恢复入口现有一行普通提示；本用例核验请求正文，不锁定 stdout 格式。
            .filter(|line| !line.starts_with("[resumed session "))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.last().unwrap()["method"], "turn/completed");
        events[0]["params"]["thread_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn requests(&self) -> Vec<Value> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }

    async fn assert_startup_error(&self, expected: &str) {
        let output = timeout(
            Duration::from_secs(15),
            self.command()
                .args(["--headless", "go"])
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .expect("invalid prompt file blocked startup")
        .unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!output.status.success(), "{stderr}");
        assert!(stderr.contains(expected), "{stderr}");
    }
}

fn assert_context(request: &Value, base: &str, cwd: &Path) {
    let messages = request["messages"].as_array().unwrap();
    let systems: Vec<&str> = messages
        .iter()
        .filter(|message| message["role"] == "system")
        .map(|message| message["content"].as_str().unwrap())
        .collect();
    assert_eq!(systems.len(), 1);
    let (actual_base, environment) = systems[0].split_once("\n\n# Environment\n").unwrap();
    assert_eq!(actual_base, base);
    let cwd = std::fs::canonicalize(cwd).unwrap();
    assert!(environment.contains(&format!("- Working directory: {}\n", cwd.display())));
    assert!(messages.iter().any(|message| {
        message["role"] == "user"
            && message["content"]
                .as_str()
                .is_some_and(|text| text.contains("PROJECT_RULES_SENTINEL"))
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_replaces_the_base_and_resume_uses_the_latest_config() {
    let fixture = Fixture::new().await;
    fixture.write_config(None);
    fixture.headless(&["default prompt"]).await;

    let custom = "  自定义编码助手。\n保留 C:\\work 和 \"引号\"，以及正文空白。\n";
    fixture.write_config(Some(custom));
    let id = fixture.headless(&["first custom turn"]).await;

    let updated = "UPDATED_SYSTEM_SENTINEL";
    fixture.write_config(Some(updated));
    fixture.set_prompt_file(&fixture.prompt_path());
    fixture
        .headless(&["--resume", &id, "second custom turn"])
        .await;

    let requests = fixture.requests().await;
    assert_eq!(requests.len(), 3);
    for (request, base) in requests.iter().zip([BASE_SYSTEM, custom, updated]) {
        assert_context(request, base, &fixture.workspace);
    }
    assert!(
        requests[2]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["role"] == "user"
                    && message["content"]
                        .as_str()
                        .is_some_and(|text| text.contains("first custom turn"))
            })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unusable_prompt_files_fail_before_sampling_without_blocking_on_a_fifo() {
    let fixture = Fixture::new().await;
    fixture.write_config(Some("initial"));
    for content in [b"".as_slice(), b" \t\n", "　\n".as_bytes()] {
        std::fs::write(fixture.prompt_path(), content).unwrap();
        fixture
            .assert_startup_error("must contain non-blank text")
            .await;
    }
    std::fs::write(fixture.prompt_path(), [0xff]).unwrap();
    fixture.assert_startup_error("as UTF-8 text").await;
    fixture.set_prompt_file(&fixture.home.join("missing.md"));
    fixture
        .assert_startup_error("cannot open system_prompt_file")
        .await;
    fixture.set_prompt_file(&fixture.workspace);
    fixture.assert_startup_error("must be a regular file").await;

    let fifo = fixture.home.join("prompt.fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    fixture.set_prompt_file(&fifo);
    fixture.assert_startup_error("must be a regular file").await;
    assert!(fixture.requests().await.is_empty());
}

async fn receive(stdout: &mut BufReader<ChildStdout>) -> Value {
    let mut line = String::new();
    let count = timeout(Duration::from_secs(15), stdout.read_line(&mut line))
        .await
        .expect("server response timed out")
        .unwrap();
    assert_ne!(count, 0, "server exited before replying");
    serde_json::from_str(&line).unwrap()
}

async fn rpc(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    stdin
        .write_all(format!("{message}\n").as_bytes())
        .await
        .unwrap();
    loop {
        let message = receive(stdout).await;
        if message["id"] == id {
            assert!(message.get("error").is_none(), "{message}");
            return message["result"].clone();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_threads_share_the_configured_base_with_their_own_environment() {
    let fixture = Fixture::new().await;
    let base = "SERVER_SYSTEM_SENTINEL";
    fixture.write_config(Some(base));
    let nested = fixture.workspace.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("AGENTS.md"), "PROJECT_RULES_SENTINEL nested").unwrap();
    let mut child = fixture
        .command()
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    rpc(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocol_version": kloop_server::PROTOCOL_VERSION, "capabilities": {}}),
    )
    .await;
    fixture.write_config(Some("NEXT_PROCESS_ONLY"));
    for (index, cwd) in [&fixture.workspace, &nested].into_iter().enumerate() {
        let started = rpc(
            &mut stdin,
            &mut stdout,
            2 + index as u64 * 2,
            "thread/start",
            json!({"cwd": cwd}),
        )
        .await;
        rpc(
            &mut stdin,
            &mut stdout,
            3 + index as u64 * 2,
            "turn/start",
            json!({"thread_id": started["thread"]["id"], "input": "go"}),
        )
        .await;
        loop {
            if receive(&mut stdout).await["method"] == "turn/completed" {
                break;
            }
        }
    }
    drop(stdin);
    assert!(
        timeout(Duration::from_secs(15), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let requests = fixture.requests().await;
    assert_eq!(requests.len(), 2);
    for (request, cwd) in requests.iter().zip([&fixture.workspace, &nested]) {
        assert_context(request, base, cwd);
    }
}
