use std::path::PathBuf;
use std::process::Command;

struct TestRoot {
    root: PathBuf,
}

impl TestRoot {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "kloop-headless-contract-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home/.kloop")).unwrap();
        Self { root }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kloop"));
        command
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("PATH", "/usr/bin:/bin")
            .env("KLOOP_PROVIDER", "invalid-provider")
            .env("KLOOP_CACHE", "not-a-boolean")
            .env("ANTHROPIC_API_KEY", "")
            .env("OPENAI_API_KEY", "")
            .env("KLOOP_ALLOW", "not a valid rule (");
        command
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn mock_headless_text_keeps_answer_on_stdout() {
    let root = TestRoot::new("text");
    let output = root
        .command(&["--mock", "--headless"])
        .output()
        .expect("start kloop");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stdout,
        "Demo complete: root-owned tasks, parallel batch, offload + read-back, and a result-only sub-agent all worked.\n"
    );
    assert!(!stderr.contains("Demo complete:"));
    assert!(!stdout.contains("item/"));
    assert!(!stdout.contains("task_create"));
}

#[test]
fn mock_headless_json_is_ndjson_only() {
    let root = TestRoot::new("json");
    let output = root
        .command(&["--mock", "--headless", "--json"])
        .output()
        .expect("start kloop");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let events: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("stdout must be NDJSON"))
        .collect();
    assert!(!events.is_empty());
    assert_eq!(events.first().unwrap()["method"], "turn/started");
    assert_eq!(events.last().unwrap()["method"], "turn/completed");
    let thread_id = events[0]["params"]["threadId"].as_str().unwrap();
    assert!(!thread_id.is_empty());
    assert!(
        events
            .iter()
            .all(|event| { event["params"]["threadId"].as_str() == Some(thread_id) })
    );
    assert!(
        stderr.is_empty(),
        "JSON diagnostics belong on stdout events"
    );
}

#[test]
fn help_and_list_sessions_ignore_invalid_provider_environment() {
    let root = TestRoot::new("fast-path");
    std::fs::write(root.root.join("home/.kloop/config.toml"), "invalid = [\n").unwrap();

    for args in [["--help"].as_slice(), ["--list-sessions"].as_slice()] {
        let output = root.command(args).output().expect("start kloop");
        assert!(output.status.success(), "{args:?}: {:?}", output.status);
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stdout.contains("credentials missing"));
        assert!(!stderr.contains("credentials missing"));
        assert!(!stderr.contains("cannot parse"));
        if args == ["--help"] {
            assert!(stdout.contains("kloop — a Rust coding agent"));
        } else {
            assert!(stdout.contains("no saved sessions"));
        }
    }
}
