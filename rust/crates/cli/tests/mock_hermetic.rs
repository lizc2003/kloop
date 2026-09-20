use std::io::Write as _;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "kloop-mock-hermetic-integration-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("home/.kloop")).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn mock_ignores_home_config_and_runtime_environment() {
    let root = TestDir::new();
    std::fs::write(
        root.0.join("home/.kloop/config.toml"),
        "this is intentionally invalid = [\n",
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_kloop"))
        .args(["app-server", "--mock"])
        .current_dir(&root.0)
        .env_clear()
        .env("HOME", root.0.join("home"))
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("KLOOP_PROVIDER", "invalid-provider")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            concat!(
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2.0\",\"capabilities\":{}}}\n",
                "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"config/read\",\"params\":{}}\n",
                "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"thread/start\",\"params\":{}}\n",
            )
            .as_bytes(),
        )
        .unwrap();

    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "mock startup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let responses: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for id in 1..=3 {
        let response = responses
            .iter()
            .find(|response| response["id"] == id)
            .unwrap_or_else(|| panic!("missing response {id}: {stdout}"));
        assert!(response.get("error").is_none(), "request {id} failed");
    }
    assert!(!stdout.contains("SENTINEL-FALLBACK"));
}
