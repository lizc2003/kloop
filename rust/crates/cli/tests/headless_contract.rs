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
            .env("ANTHROPIC_API_KEY", "")
            .env("OPENAI_API_KEY", "");
        command
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The global config this test binary would read is never the developer's:
/// every run gets a private HOME, and on Unix the file must be mode 0600 to be
/// read at all.
fn write_config(root: &TestRoot, contents: &str) {
    let directory = root.root.join("home/.kloop");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = directory.join("config.toml");
    std::fs::write(&path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// A writer killed between its runtime line and its first turn leaves a
/// session file with a preamble and nothing else.
fn seed_preamble_only_session(root: &TestRoot, id: &str) -> PathBuf {
    let path = root
        .root
        .join(".kloop/sessions")
        .join(format!("{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let line = serde_json::json!({
        "type": "session",
        "id": format!("{id}#1"),
        "ts": 0,
        "format_version": 1,
        "runtime": { "cwd": root.root.to_string_lossy() },
    });
    std::fs::write(&path, format!("{line}\n")).unwrap();
    path
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
        "Demo complete: root-owned todos, parallel batch, offload + query-in-place, and a result-only sub-agent all worked.\n"
    );
    assert!(!stderr.contains("Demo complete:"));
    assert!(!stdout.contains("item/"));
    assert!(!stdout.contains("todo_write"));
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
    let thread_id = events[0]["params"]["thread_id"].as_str().unwrap();
    assert!(!thread_id.is_empty());
    assert!(
        events
            .iter()
            .all(|event| { event["params"]["thread_id"].as_str() == Some(thread_id) })
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

#[cfg(unix)]
#[test]
fn permission_report_can_select_a_project_from_another_directory() {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};

    use kloop_core::project::WorkspaceIdentity;

    let root = TestRoot::new("permission-report");
    write_config(&root, "invalid = [\n");
    let workspace = root.root.join("work tree");
    std::fs::create_dir(&workspace).unwrap();
    let identity = WorkspaceIdentity::resolve(&workspace);
    let project_id = identity.project_id().unwrap().as_str();
    let policy_dir = root.root.join("home/.kloop/projects/v1").join(project_id);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&policy_dir)
        .unwrap();
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(policy_dir.join("permissions.json"))
        .unwrap()
        .write_all(br#"{"private_marker": true}"#)
        .unwrap();

    // 当前目录的项目还没记过授权：说明没有记录，正常退出，而不是报“有效”。
    let local = root.command(&["--check-permissions"]).output().unwrap();
    assert!(local.status.success(), "{local:?}");
    let local = String::from_utf8(local.stdout).unwrap();
    assert!(
        local.starts_with("No permission policy is recorded for project p1_"),
        "{local}"
    );

    // 显式给出、却没有权限文件的 ID（例如抄错了）：同样说明没有记录，但不能算通过。
    let missing = format!("p1_{}", "0".repeat(25));
    let absent = root
        .command(&["--check-permissions", &missing])
        .output()
        .unwrap();
    assert_eq!(absent.status.code(), Some(1), "{absent:?}");
    assert_eq!(
        String::from_utf8(absent.stdout).unwrap(),
        format!("No permission policy is recorded for project {missing}.\n")
    );

    let selected = root
        .command(&["--check-permissions", project_id])
        .output()
        .unwrap();
    assert_eq!(selected.status.code(), Some(1));
    let report = String::from_utf8(selected.stdout).unwrap();
    assert!(report.contains("unknown field (line "), "{report}");
    assert!(report.contains(project_id), "{report}");
    assert!(!report.contains("private_marker"), "{report}");

    // 手写的 revision 0：规则都合法也不能报成“没有记录”，按无效处理。
    std::fs::write(
        policy_dir.join("permissions.json"),
        format!(r#"{{"version": 1, "project_id": "{project_id}", "revision": 0, "allow": []}}"#),
    )
    .unwrap();
    let zero = root
        .command(&["--check-permissions", project_id])
        .output()
        .unwrap();
    assert_eq!(zero.status.code(), Some(1), "{zero:?}");
    let report = String::from_utf8(zero.stdout).unwrap();
    assert!(
        report.contains("project permission policy revision must be at least 1"),
        "{report}"
    );
    assert!(
        !report.contains("No permission policy is recorded"),
        "{report}"
    );

    // 修好之后同一条命令报有效。
    std::fs::write(
        policy_dir.join("permissions.json"),
        format!(r#"{{"version": 1, "project_id": "{project_id}", "revision": 1, "allow": []}}"#),
    )
    .unwrap();
    let fixed = root
        .command(&["--check-permissions", project_id])
        .output()
        .unwrap();
    assert!(fixed.status.success(), "{fixed:?}");
    assert_eq!(
        String::from_utf8(fixed.stdout).unwrap(),
        format!("Permission policy for project {project_id} is valid.\n")
    );
}

/// A writer killed between its runtime line and its first turn leaves a
/// preamble-only session. Resuming it must open the initial route like a fresh
/// session instead of failing to adopt a timeline that does not exist.
#[test]
fn mock_headless_resumes_a_preamble_only_session() {
    let root = TestRoot::new("preamble-resume");
    let id = "20260101-000001";
    let path = seed_preamble_only_session(&root, id);

    let output = root
        .command(&["--mock", "--headless", "--resume", id])
        .output()
        .expect("start kloop");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let transcript = std::fs::read_to_string(&path).unwrap();
    assert!(
        transcript.contains("\"type\":\"provider_route_initial\""),
        "the resumed session never opened its route: {transcript}"
    );
    assert!(
        transcript.contains("\"type\":\"message\""),
        "the mock turn did not run on the resumed session: {transcript}"
    );
}

/// Session notices are diagnostics: `--headless --json` promises stdout is
/// only NDJSON, so "[resumed session …]" must not lead the stream.
#[test]
fn mock_headless_json_continue_keeps_stdout_ndjson() {
    let root = TestRoot::new("json-continue");
    // The first run writes a real transcript (route + message); the second
    // continues it, which is the path that used to print "[resumed …]" first.
    let first = root
        .command(&["--mock", "--headless", "first"])
        .output()
        .expect("start kloop");
    assert!(first.status.success());

    let output = root
        .command(&["--mock", "--headless", "--json", "-c", "go"])
        .output()
        .expect("start kloop");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stdout.is_empty());
    for line in stdout.lines() {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|error| panic!("stdout line is not NDJSON ({error}): {line:?}"));
    }
    assert!(
        stderr.contains("[resumed session "),
        "the resume notice belongs on stderr: {stderr}"
    );
}

/// `[env]` is applied before the runtime exists; a name or value `set_var`
/// would panic on must be a normal startup error, and the error must not quote
/// a value that could be a credential.
#[test]
fn an_unsettable_env_entry_fails_without_quoting_the_value() {
    let root = TestRoot::new("env-unsettable");
    write_config(
        &root,
        "provider = \"openai-compat\"\n\n\
         [providers.openai-compat]\n\
         wire_api = \"chat\"\n\
         base_url = \"http://127.0.0.1:1\"\n\
         auth_header = { Authorization = \"Bearer synthetic-key\" }\n\
         model = \"m\"\n\n\
         [env]\n\
         \"A=B\" = \"s3cret\"\n",
    );

    let output = root
        .command(&["--headless", "hi"])
        .output()
        .expect("start kloop");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("[env]"), "stderr: {stderr}");
    assert!(
        !stderr.contains("s3cret"),
        "stderr quoted the value: {stderr}"
    );
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
}

#[cfg(unix)]
#[test]
fn a_non_utf8_argument_is_an_error_not_a_panic() {
    use std::os::unix::ffi::OsStringExt as _;

    let root = TestRoot::new("argv-non-utf8");
    let output = root
        .command(&[])
        .arg(std::ffi::OsString::from_vec(b"--prompt-\xff".to_vec()))
        .output()
        .expect("start kloop");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("valid UTF-8"), "stderr: {stderr}");
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
}
