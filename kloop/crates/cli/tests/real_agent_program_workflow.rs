use std::io::BufRead as _;
use std::io::BufReader;
use std::io::Read as _;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStdin;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

use serde_json::json;
use serde_json::Value;

const AGENT_SENTINEL: &str = "AGENT_DIRECT_68";
const PROGRAM_CHILD_SENTINEL: &str = "PROGRAM_CHILD_68";
const PROGRAM_FAILURE_SENTINEL: &str = "PROGRAM_EXPECTED_FAILURE_68";
const WORKFLOW_SENTINEL: &str = "WORKFLOW_RESULT_68";
const WORKFLOW_LEFT: &str = "WORKFLOW_LEFT_68";
const WORKFLOW_RIGHT: &str = "WORKFLOW_RIGHT_68";
const PROGRAM_SOURCE: &str = "const child = await agent(\"Reply exactly PROGRAM_CHILD_68\");\nthrow new Error(\"PROGRAM_EXPECTED_FAILURE_68:\" + child);";
const WORKFLOW_SCRIPT: &str = r#"export const meta = {
  name: 'real-primitives-68',
  description: 'Exercise scoped Workflow agents',
  phases: [{ title: 'Agents', detail: 'Run two sentinel agents' }],
};
phase('Agents');
const out = await parallel([
  (scope) => scope.agent('Reply exactly WORKFLOW_LEFT_68'),
  (scope) => scope.agent('Reply exactly WORKFLOW_RIGHT_68'),
]);
return { sentinel: 'WORKFLOW_RESULT_68', out };
"#;

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("kloop-real-primitives-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("home/.kloop")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path.join("home"), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            std::fs::set_permissions(
                path.join("home/.kloop"),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        std::fs::create_dir_all(path.join("workspace")).unwrap();
        Self(path)
    }

    fn workspace(&self) -> PathBuf {
        self.0.join("workspace")
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct NativeClient {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: mpsc::Receiver<Result<Value, &'static str>>,
    next_id: u64,
}

impl NativeClient {
    fn spawn(root: &TestRoot) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kloop"));
        command
            .args(["app-server", "--permission-mode", "accept-edits"])
            .current_dir(root.workspace())
            .env_clear()
            .env("HOME", root.0.join("home"))
            .env(
                "PATH",
                std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in [
            "KLOOP_PROVIDER",
            "KLOOP_MODEL",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_MODEL",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "OPENAI_MODEL",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command.spawn().expect("cannot start kloop app-server");
        let stdout = child.stdout.take().expect("missing app-server stdout");
        let mut stderr = child.stderr.take().expect("missing app-server stderr");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let message = match line {
                    Ok(line) => serde_json::from_str(&line).map_err(|_| "invalid server JSON"),
                    Err(_) => Err("cannot read server stdout"),
                };
                if tx.send(message).is_err() {
                    break;
                }
            }
        });
        // Drain diagnostics so the child cannot block on a full pipe. Never
        // surface the raw bytes: provider errors may include private endpoints.
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            let _ = stderr.read_to_end(&mut sink);
        });
        Self {
            stdin: child.stdin.take(),
            child,
            messages: rx,
            next_id: 1,
        }
    }

    fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let stdin = self.stdin.as_mut().expect("server stdin is closed");
        serde_json::to_writer(&mut *stdin, &request).expect("cannot encode request");
        stdin.write_all(b"\n").expect("cannot write request");
        stdin.flush().expect("cannot flush request");
        id
    }

    fn receive(&self, deadline: Instant) -> Value {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("real primitive evaluator timed out");
        match self.messages.recv_timeout(remaining) {
            Ok(Ok(message)) => message,
            Ok(Err(reason)) => panic!("app-server protocol failure: {reason}"),
            Err(_) => panic!("app-server stopped or timed out"),
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let message = self.receive(deadline);
            if message["id"] == id {
                assert!(message.get("error").is_none(), "app-server request failed");
                return message["result"].clone();
            }
        }
    }

    fn run_turn(&mut self, thread_id: &str, prompt: &str) -> Vec<Value> {
        let request_id = self.send(
            "turn/start",
            json!({"threadId": thread_id, "input": prompt}),
        );
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut messages = Vec::new();
        let mut turn_id = None;
        let mut completed = Vec::new();
        loop {
            let message = self.receive(deadline);
            if message["id"] == request_id {
                assert!(message.get("error").is_none(), "turn/start failed");
                turn_id = message["result"]["turn"]["id"].as_u64();
                assert!(turn_id.is_some(), "turn/start omitted turn id");
            }
            if message["method"] == "turn/completed" {
                if let Some(id) = message["params"]["turn"]["id"].as_u64() {
                    completed.push(id);
                }
            }
            messages.push(message);
            if turn_id.is_some_and(|id| completed.contains(&id)) {
                return messages;
            }
        }
    }

    fn collect_workflow_delivery(&self, mut messages: Vec<Value>) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let terminal_index = messages.iter().position(|message| {
                message["method"] == "thread/backgroundTask/updated"
                    && message["params"]["task"]["kind"] == "workflow"
                    && matches!(
                        message["params"]["task"]["status"].as_str(),
                        Some("completed" | "failed" | "cancelled")
                    )
            });
            if terminal_index.is_some_and(|index| {
                messages[index + 1..]
                    .iter()
                    .any(|message| message["method"] == "turn/completed")
            }) {
                return messages;
            }
            messages.push(self.receive(deadline));
        }
    }

    fn shutdown(mut self) {
        drop(self.stdin.take());
        let status = self.child.wait().expect("cannot wait for app-server");
        assert!(status.success(), "app-server exited unsuccessfully");
    }
}

fn tool_items<'a>(messages: &'a [Value], method: &str, name: &str) -> Vec<&'a Value> {
    messages
        .iter()
        .filter(|message| {
            message["method"] == method
                && message["params"]["item"]["type"] == "toolCall"
                && message["params"]["item"]["name"] == name
        })
        .collect()
}

fn assert_tool_pairs(messages: &[Value], name: &str, expected: usize) -> Vec<String> {
    let starts = tool_items(messages, "item/started", name);
    let completed = tool_items(messages, "item/completed", name);
    assert_eq!(starts.len(), expected, "unexpected tool start count");
    assert_eq!(
        completed.len(),
        expected,
        "unexpected tool completion count"
    );
    let mut start_ids: Vec<&str> = starts
        .iter()
        .filter_map(|message| message["params"]["item"]["id"].as_str())
        .collect();
    let mut completed_ids: Vec<&str> = completed
        .iter()
        .filter_map(|message| message["params"]["item"]["id"].as_str())
        .collect();
    start_ids.sort_unstable();
    completed_ids.sort_unstable();
    assert_eq!(start_ids, completed_ids, "tool lifecycle IDs did not pair");
    completed
        .into_iter()
        .map(|message| {
            message["params"]["item"]["output"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

fn extract_run_id(text: &str) -> Option<String> {
    let marker = "resume_from_run_id: \"";
    let rest = text.split_once(marker)?.1;
    let id = rest.split_once('"')?.0;
    id.starts_with("run-").then(|| id.to_string())
}

fn assert_path_within(path: &Path, root: &Path) -> PathBuf {
    let path = std::fs::canonicalize(path).expect("cannot canonicalize artifact");
    let root = std::fs::canonicalize(root).expect("cannot canonicalize workspace");
    assert!(
        path.starts_with(&root),
        "artifact escaped isolated workspace"
    );
    path
}

#[test]
#[ignore = "requires real Anthropic or OpenAI Chat credentials"]
fn real_agent_program_workflow_contract() {
    let provider = std::env::var("KLOOP_PROVIDER").expect("set KLOOP_PROVIDER");
    assert!(
        matches!(provider.as_str(), "anthropic" | "openai"),
        "real evaluator supports anthropic or OpenAI Chat only"
    );
    let model_var = if provider == "anthropic" {
        "ANTHROPIC_MODEL"
    } else {
        "OPENAI_MODEL"
    };
    let model = std::env::var(model_var)
        .or_else(|_| std::env::var("KLOOP_MODEL"))
        .expect("set the selected provider model");
    let root = TestRoot::new();
    let mut client = NativeClient::spawn(&root);
    let initialized = client.request(
        "initialize",
        json!({"protocolVersion": "1.0", "capabilities": {}}),
    );
    assert_eq!(initialized["protocolVersion"], "1.0");
    let started = client.request(
        "thread/start",
        json!({"cwd": root.workspace(), "model": model}),
    );
    let thread_id = started["thread"]["id"]
        .as_str()
        .expect("thread/start omitted id")
        .to_string();

    let agent_messages = client.run_turn(
        &thread_id,
        &format!(
            "Acceptance case Agent. Call run_agent exactly once in foreground with prompt `Reply exactly {AGENT_SENTINEL}`. Do not use Program or Workflow. After its successful tool result, answer `AGENT_CASE_DONE_68`."
        ),
    );
    let agent_outputs = assert_tool_pairs(&agent_messages, "run_agent", 1);
    assert!(
        agent_outputs[0].contains(AGENT_SENTINEL),
        "direct Agent sentinel missing"
    );

    let program_prompt = format!(
        "Acceptance case Program. Call run_program exactly twice and do not call run_agent or Workflow directly. First call it in foreground with the exact JavaScript source below and no resume id. It is expected to fail after one child Agent. Read the reported durable run-* id, then call run_program a second time with the byte-identical source and that resume_from_run_id. The second failure is expected; do not retry again. Finish with `PROGRAM_CASE_DONE_68`.\n\n```js\n{PROGRAM_SOURCE}\n```"
    );
    let program_messages = client.run_turn(&thread_id, &program_prompt);
    let program_outputs = assert_tool_pairs(&program_messages, "run_program", 2);
    assert!(
        program_outputs
            .iter()
            .all(|output| output.contains(PROGRAM_FAILURE_SENTINEL)),
        "Program expected failure sentinel missing"
    );
    let run_id = program_outputs
        .iter()
        .find_map(|output| extract_run_id(output))
        .expect("Program did not report a resumable run id");
    assert!(
        program_outputs
            .iter()
            .all(|output| output.contains(&run_id)),
        "Program resume did not use one durable run id"
    );
    let program_child_starts = program_messages
        .iter()
        .filter(|message| {
            message["method"] == "item/started"
                && message["params"]["item"]["type"] == "subAgent"
                && message["params"]["item"]["task"]
                    .as_str()
                    .is_some_and(|task| task.contains(PROGRAM_CHILD_SENTINEL))
        })
        .count();
    assert_eq!(
        program_child_starts, 1,
        "Program resume re-spawned a journaled child Agent"
    );
    let stored_source = root
        .workspace()
        .join(".kloop/program-runs")
        .join(&run_id)
        .join("source.js");
    assert_eq!(
        std::fs::read_to_string(stored_source).expect("missing Program source artifact"),
        PROGRAM_SOURCE
    );

    let workflow_prompt = format!(
        "Acceptance case Workflow. I explicitly authorize multi-agent Workflow orchestration. Call workflow exactly once with the exact script below and no args. Then call wait_for_activity as needed until its result is delivered; do not call run_agent or run_program directly. Finish with `WORKFLOW_CASE_DONE_68`.\n\n```js\n{WORKFLOW_SCRIPT}\n```"
    );
    let initial_workflow = client.run_turn(&thread_id, &workflow_prompt);
    let workflow_messages = client.collect_workflow_delivery(initial_workflow);
    let workflow_outputs = assert_tool_pairs(&workflow_messages, "workflow", 1);
    assert!(
        workflow_outputs[0].contains("Workflow ID: workflow-")
            && workflow_outputs[0].contains("Run ID: wf_"),
        "Workflow launch omitted transient or durable identity"
    );
    let terminals: Vec<&Value> = workflow_messages
        .iter()
        .filter(|message| {
            message["method"] == "thread/backgroundTask/updated"
                && message["params"]["task"]["kind"] == "workflow"
                && message["params"]["task"]["status"] != "running"
        })
        .collect();
    assert_eq!(
        terminals.len(),
        1,
        "Workflow emitted multiple terminal states"
    );
    assert_eq!(
        terminals[0]["params"]["task"]["status"], "completed",
        "Workflow did not complete"
    );
    let output_path = terminals[0]["params"]["task"]["outputPath"]
        .as_str()
        .expect("Workflow terminal omitted result path");
    let output_path = assert_path_within(Path::new(output_path), &root.workspace());
    let workflow_result = std::fs::read_to_string(output_path).expect("cannot read result.json");
    assert!(workflow_result.contains(WORKFLOW_SENTINEL));
    assert!(workflow_result.contains(WORKFLOW_LEFT));
    assert!(workflow_result.contains(WORKFLOW_RIGHT));

    client.shutdown();
    let rollout = std::fs::read_to_string(
        root.workspace()
            .join(".kloop/sessions")
            .join(format!("{thread_id}.jsonl")),
    )
    .expect("cannot read acceptance rollout");
    assert_eq!(
        rollout
            .matches("A background Workflow you launched has finished")
            .count(),
        1,
        "Workflow result was not delivered exactly once"
    );

    println!(
        "real primitive acceptance passed: provider={provider} model={model} agent_calls=1 program_calls=2 program_child_spawns=1 workflow_calls=1 workflow_terminals=1"
    );
}
