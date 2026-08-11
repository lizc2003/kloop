use std::collections::HashSet;
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
const BACKGROUND_AGENT_SENTINEL: &str = "BACKGROUND_AGENT_69";
const BACKGROUND_AGENT_DESCRIPTION: &str = "verify background Agent delivery";
const BACKGROUND_PROGRAM_SENTINEL: &str = "BACKGROUND_PROGRAM_69";
const BACKGROUND_PROGRAM_DESCRIPTION: &str = "verify background Program delivery";
const BACKGROUND_PROGRAM_SOURCE: &str = "return 'BACKGROUND_PROGRAM_69:' + 'x'.repeat(20000);";
const PROGRAM_CHILD_SENTINEL: &str = "PROGRAM_CHILD_68";
const PROGRAM_FAILURE_SENTINEL: &str = "PROGRAM_EXPECTED_FAILURE_68";
const WORKFLOW_SENTINEL: &str = "WORKFLOW_RESULT_68";
const WORKFLOW_LEFT: &str = "WORKFLOW_LEFT_68";
const WORKFLOW_RIGHT: &str = "WORKFLOW_RIGHT_68";
const PROGRAM_SOURCE: &str = "const child = await agent(\"Reply exactly PROGRAM_CHILD_68\");\nthrow new Error(\"PROGRAM_EXPECTED_FAILURE_68:\" + child);";
const MAILBOX_AGENT_A_DESCRIPTION: &str = "mailbox Agent A";
const MAILBOX_AGENT_B_DESCRIPTION: &str = "mailbox Agent B";
const MAILBOX_MAIN_TO_A: &str = "MAILBOX_MAIN_TO_A_70";
const MAILBOX_A_TO_B: &str = "MAILBOX_A_TO_B_70";
const MAILBOX_B_TO_MAIN: &str = "MAILBOX_B_TO_MAIN_70";
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
        Self::spawn_with_permission_mode(root, "accept-edits")
    }

    fn spawn_with_permission_mode(root: &TestRoot, permission_mode: &str) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kloop"));
        command
            .args(["app-server", "--permission-mode", permission_mode])
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
            "KLOOP_EFFORT",
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

    fn collect_background_delivery(&self, mut messages: Vec<Value>, kind: &str) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let terminal_index = messages.iter().position(|message| {
                message["method"] == "thread/backgroundTask/updated"
                    && message["params"]["task"]["kind"] == kind
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
    assert_eq!(
        starts.len(),
        expected,
        "unexpected tool start count; inputs={:?}",
        starts
            .iter()
            .map(|message| &message["params"]["item"]["input"])
            .collect::<Vec<_>>()
    );
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

fn assert_background_lifecycle(
    messages: &[Value],
    kind: &str,
    description: &str,
    run_prefix: Option<&str>,
) -> (String, Option<String>) {
    let updates: Vec<&Value> = messages
        .iter()
        .filter(|message| {
            message["method"] == "thread/backgroundTask/updated"
                && message["params"]["task"]["kind"] == kind
        })
        .collect();
    assert!(!updates.is_empty(), "missing {kind} lifecycle events");
    assert!(updates.iter().all(|message| {
        message["params"].get("turnId").is_none()
            && message["params"]["task"]["description"] == description
    }));

    let execution_id = updates[0]["params"]["task"]["id"]
        .as_str()
        .expect("background update omitted execution id")
        .to_string();
    assert!(updates
        .iter()
        .all(|message| message["params"]["task"]["id"] == execution_id));
    assert!(updates
        .iter()
        .any(|message| message["params"]["task"]["status"] == "running"));
    let terminals: Vec<&Value> = updates
        .iter()
        .copied()
        .filter(|message| {
            matches!(
                message["params"]["task"]["status"].as_str(),
                Some("completed" | "failed" | "cancelled")
            )
        })
        .collect();
    assert_eq!(terminals.len(), 1, "{kind} must publish one terminal state");
    assert_eq!(
        terminals[0]["params"]["task"]["status"], "completed",
        "{kind} did not complete successfully"
    );

    let run_id = match run_prefix {
        Some(prefix) => {
            let run_id = updates[0]["params"]["task"]["runId"]
                .as_str()
                .expect("durable background work omitted runId")
                .to_string();
            assert!(run_id.starts_with(prefix), "unexpected run id shape");
            assert!(updates
                .iter()
                .all(|message| message["params"]["task"]["runId"] == run_id));
            Some(run_id)
        }
        None => {
            assert!(updates
                .iter()
                .all(|message| message["params"]["task"].get("runId").is_none()));
            None
        }
    };
    (execution_id, run_id)
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
#[ignore = "requires real Anthropic, OpenAI Chat, or OpenAI Responses credentials"]
fn real_agent_program_workflow_contract() {
    let provider = std::env::var("KLOOP_PROVIDER").expect("set KLOOP_PROVIDER");
    assert!(
        matches!(
            provider.as_str(),
            "anthropic" | "openai" | "openai-responses"
        ),
        "real evaluator supports Anthropic, OpenAI Chat, or OpenAI Responses"
    );
    if provider == "openai-responses" {
        std::env::var("KLOOP_EFFORT")
            .ok()
            .filter(|effort| !effort.is_empty())
            .expect("set KLOOP_EFFORT so the Responses evaluator exercises reasoning items");
    }
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
            "Acceptance case Agent. Call run_agent exactly once in foreground with prompt `Reply exactly {AGENT_SENTINEL}`. Omit agent_type, isolation, max_rounds, background, and description. Do not use Program or Workflow. After its successful tool result, answer `AGENT_CASE_DONE_68`."
        ),
    );
    let agent_outputs = assert_tool_pairs(&agent_messages, "run_agent", 1);
    let agent_input =
        &tool_items(&agent_messages, "item/started", "run_agent")[0]["params"]["item"]["input"];
    assert!(
        agent_outputs[0].contains(AGENT_SENTINEL),
        "direct Agent sentinel missing; input={agent_input}; output={}",
        agent_outputs[0]
    );

    let program_prompt = format!(
        "Acceptance case Program. Call run_program exactly twice and do not call run_agent or Workflow directly. First call it in foreground with the exact JavaScript source below and omit description, background, and resume_from_run_id. It is expected to fail after one child Agent. Read the reported durable run-* id, then call run_program a second time with the byte-identical source and that resume_from_run_id, again omitting description and background. The second failure is expected; do not retry again. Finish with `PROGRAM_CASE_DONE_68`.\n\n```js\n{PROGRAM_SOURCE}\n```"
    );
    let program_messages = client.run_turn(&thread_id, &program_prompt);
    let program_outputs = assert_tool_pairs(&program_messages, "run_program", 2);
    assert!(
        program_outputs
            .iter()
            .all(|output| output.contains(PROGRAM_FAILURE_SENTINEL)),
        "Program expected failure sentinel missing; inputs={:?}; outputs={program_outputs:?}",
        tool_items(&program_messages, "item/started", "run_program")
            .iter()
            .map(|message| &message["params"]["item"]["input"])
            .collect::<Vec<_>>()
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
    let invoked_program_sources: Vec<&str> =
        tool_items(&program_messages, "item/started", "run_program")
            .into_iter()
            .map(|message| {
                message["params"]["item"]["input"]["source"]
                    .as_str()
                    .expect("run_program omitted source")
            })
            .collect();
    assert_eq!(invoked_program_sources.len(), 2);
    assert_eq!(
        invoked_program_sources[0], invoked_program_sources[1],
        "resume source was not byte-identical"
    );
    assert_eq!(
        invoked_program_sources[0].trim_end_matches(['\r', '\n']),
        PROGRAM_SOURCE,
        "model changed the requested Program source"
    );
    assert_eq!(
        std::fs::read_to_string(stored_source).expect("missing Program source artifact"),
        invoked_program_sources[0]
    );

    let background_agent_prompt = format!(
        "Acceptance case background Agent. Call run_agent exactly once with background=true, description `{BACKGROUND_AGENT_DESCRIPTION}`, and prompt `Reply exactly {BACKGROUND_AGENT_SENTINEL}`. Omit agent_type, isolation, and max_rounds. Do not call wait_for_activity. After launch, keep working normally; when its automatically delivered result arrives, acknowledge it without launching any more tools."
    );
    let initial_background_agent = client.run_turn(&thread_id, &background_agent_prompt);
    let background_agent_messages =
        client.collect_background_delivery(initial_background_agent, "agent");
    let background_agent_outputs = assert_tool_pairs(&background_agent_messages, "run_agent", 1);
    assert!(
        background_agent_outputs[0].contains(&format!("Agent({BACKGROUND_AGENT_DESCRIPTION})"))
            && background_agent_outputs[0].contains("Agent ID: agent-"),
        "background Agent launch omitted its display description or execution id"
    );
    assert!(
        tool_items(
            &background_agent_messages,
            "item/started",
            "wait_for_activity"
        )
        .is_empty(),
        "background delivery must not require wait_for_activity"
    );
    let (background_agent_id, no_agent_run_id) = assert_background_lifecycle(
        &background_agent_messages,
        "agent",
        BACKGROUND_AGENT_DESCRIPTION,
        None,
    );
    assert!(background_agent_id.starts_with("agent-"));
    assert!(no_agent_run_id.is_none());

    let background_program_prompt = format!(
        "Acceptance case background Program. Call run_program exactly once with background=true, description `{BACKGROUND_PROGRAM_DESCRIPTION}`, and the exact JavaScript source below; omit resume_from_run_id. Do not call wait_for_activity. After launch, keep working normally; when its automatically delivered result arrives, acknowledge it without launching another Program.\n\n```js\n{BACKGROUND_PROGRAM_SOURCE}\n```"
    );
    let initial_background_program = client.run_turn(&thread_id, &background_program_prompt);
    let background_program_messages =
        client.collect_background_delivery(initial_background_program, "program");
    let background_program_outputs =
        assert_tool_pairs(&background_program_messages, "run_program", 1);
    assert!(
        background_program_outputs[0]
            .contains(&format!("Program({BACKGROUND_PROGRAM_DESCRIPTION})"))
            && background_program_outputs[0].contains("Program ID: program-")
            && background_program_outputs[0].contains("Run ID: run-"),
        "background Program launch omitted display, execution, or durable identity"
    );
    assert!(
        tool_items(
            &background_program_messages,
            "item/started",
            "wait_for_activity"
        )
        .is_empty(),
        "background delivery must not require wait_for_activity"
    );
    let (background_program_id, background_program_run_id) = assert_background_lifecycle(
        &background_program_messages,
        "program",
        BACKGROUND_PROGRAM_DESCRIPTION,
        Some("run-"),
    );
    assert!(background_program_id.starts_with("program-"));
    let background_program_run_id = background_program_run_id.unwrap();
    assert!(background_program_outputs[0].contains(&background_program_run_id));
    assert_eq!(
        std::fs::read_to_string(
            root.workspace()
                .join(".kloop/program-runs")
                .join(&background_program_run_id)
                .join("source.js")
        )
        .expect("missing background Program source artifact"),
        BACKGROUND_PROGRAM_SOURCE
    );
    let offloaded_program_result = std::fs::read_dir(root.workspace().join(".kloop/offload"))
        .expect("background Program result was not offloaded")
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .any(|content| content.contains(BACKGROUND_PROGRAM_SENTINEL));
    assert!(
        offloaded_program_result,
        "large background Program result missing from offload artifacts"
    );

    let workflow_prompt = format!(
        "Acceptance case Workflow. I explicitly authorize multi-agent Workflow orchestration. Call workflow exactly once with the exact script below and omit args, script_path, resume_from_run_id, name, description, and title. Do not call wait_for_activity, run_agent, or run_program directly; the Workflow result will be delivered automatically. Finish after the delivered result is folded in.\n\n```js\n{WORKFLOW_SCRIPT}\n```"
    );
    let initial_workflow = client.run_turn(&thread_id, &workflow_prompt);
    let workflow_messages = client.collect_background_delivery(initial_workflow, "workflow");
    let workflow_outputs = assert_tool_pairs(&workflow_messages, "workflow", 1);
    assert!(
        workflow_outputs[0].contains("Workflow ID: workflow-")
            && workflow_outputs[0].contains("Run ID: wf_"),
        "Workflow launch omitted transient or durable identity"
    );
    assert!(
        tool_items(&workflow_messages, "item/started", "wait_for_activity").is_empty(),
        "Workflow delivery must not require wait_for_activity"
    );
    let (workflow_id, workflow_run_id) = assert_background_lifecycle(
        &workflow_messages,
        "workflow",
        "Exercise scoped Workflow agents",
        Some("wf_"),
    );
    assert!(workflow_outputs[0].contains(&workflow_id));
    assert!(workflow_outputs[0].contains(&workflow_run_id.unwrap()));
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
            .matches("A background sub-agent you dispatched has finished")
            .count(),
        1,
        "background Agent result was not delivered exactly once"
    );
    assert_eq!(
        rollout
            .matches("A background program you launched has finished")
            .count(),
        1,
        "background Program result was not delivered exactly once"
    );
    assert_eq!(
        rollout
            .matches("A background Workflow you launched has finished")
            .count(),
        1,
        "Workflow result was not delivered exactly once"
    );

    println!(
        "real primitive acceptance passed: provider={provider} model={model} foreground_agent_calls=1 foreground_program_calls=2 program_child_spawns=1 background_agent_calls=1 background_program_calls=1 workflow_calls=1 terminals=3 automatic_deliveries=3"
    );
}

#[test]
#[ignore = "requires real Anthropic or OpenAI Chat credentials"]
fn real_local_agent_mailbox_contract() {
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
    let mut client = NativeClient::spawn_with_permission_mode(&root, "bypass");
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

    let agent_a_prompt = format!(
        "You are mailbox Agent A. First call bash exactly once with command `sleep 8`. After it completes, consume the local peer message from main. Then call list_agents exactly once, select the other open Agent whose description is exactly `{MAILBOX_AGENT_B_DESCRIPTION}`, and call send_message exactly once to its exact agent-N id with summary `A to B` and message `{MAILBOX_A_TO_B}`. Do not use wait_for_activity or any status polling. After the queued result, finish with exactly `MAILBOX_A_DONE_70`."
    );
    let agent_b_prompt = format!(
        "You are mailbox Agent B. First call bash exactly once with command `sleep 25`. At the next safe boundary, consume Agent A's peer message `{MAILBOX_A_TO_B}`. Then call send_message exactly once to `main` with summary `B to main` and message `{MAILBOX_B_TO_MAIN}`. Do not use wait_for_activity, list_agents, or any status polling. After the queued result, finish with exactly `MAILBOX_B_DONE_70`."
    );
    let main_prompt = format!(
        "Exercise the local Agent mailbox contract. In one assistant response call run_agent exactly twice with background=true: description `{MAILBOX_AGENT_A_DESCRIPTION}` and prompt `{agent_a_prompt}`; description `{MAILBOX_AGENT_B_DESCRIPTION}` and prompt `{agent_b_prompt}`. After both launch tool results return, identify Agent A's exact agent-N id from its description and call send_message exactly once to it with summary `main to A` and message `{MAILBOX_MAIN_TO_A}`. Do not call wait_for_activity, list_agents, stop tools, or poll status. After the queued result, end the current turn with `MAILBOX_MAIN_LAUNCHED_70`; background terminals and results will arrive automatically."
    );
    let mut messages = client.run_turn(&thread_id, &main_prompt);
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let terminal_agents = messages
            .iter()
            .filter(|message| {
                message["method"] == "thread/backgroundTask/updated"
                    && message["params"]["task"]["kind"] == "agent"
                    && matches!(
                        message["params"]["task"]["status"].as_str(),
                        Some("completed" | "failed" | "cancelled")
                    )
            })
            .count();
        let delivered = messages
            .iter()
            .filter(|message| {
                message["method"] == "thread/agentMessage/updated"
                    && message["params"]["status"] == "delivered"
            })
            .count();
        let last_terminal = messages.iter().rposition(|message| {
            message["method"] == "thread/backgroundTask/updated"
                && message["params"]["task"]["kind"] == "agent"
                && matches!(
                    message["params"]["task"]["status"].as_str(),
                    Some("completed" | "failed" | "cancelled")
                )
        });
        let completion_after_terminals = last_terminal.is_some_and(|index| {
            messages[index + 1..]
                .iter()
                .any(|message| message["method"] == "turn/completed")
        });
        if terminal_agents == 2 && delivered == 3 && completion_after_terminals {
            break;
        }
        messages.push(client.receive(deadline));
    }

    assert_tool_pairs(&messages, "run_agent", 2);
    assert_tool_pairs(&messages, "send_message", 3);
    assert_tool_pairs(&messages, "list_agents", 1);
    assert!(
        tool_items(&messages, "item/started", "wait_for_activity").is_empty(),
        "mailbox coordination must not poll wait_for_activity"
    );

    let agent_id = |description: &str| {
        messages
            .iter()
            .find(|message| {
                message["method"] == "thread/backgroundTask/updated"
                    && message["params"]["task"]["kind"] == "agent"
                    && message["params"]["task"]["description"] == description
            })
            .and_then(|message| message["params"]["task"]["id"].as_str())
            .expect("missing described Agent lifecycle")
            .to_string()
    };
    let agent_a = agent_id(MAILBOX_AGENT_A_DESCRIPTION);
    let agent_b = agent_id(MAILBOX_AGENT_B_DESCRIPTION);
    assert_ne!(agent_a, agent_b);

    for id in [&agent_a, &agent_b] {
        let updates = messages
            .iter()
            .filter(|message| {
                message["method"] == "thread/backgroundTask/updated"
                    && message["params"]["task"]["id"] == *id
            })
            .collect::<Vec<_>>();
        assert!(updates
            .iter()
            .any(|message| message["params"]["task"]["status"] == "running"));
        let terminals = updates
            .iter()
            .filter(|message| message["params"]["task"]["status"] != "running")
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 1, "Agent {id} emitted duplicate terminal");
        assert_eq!(terminals[0]["params"]["task"]["status"], "completed");
    }

    let expected_routes = [
        ("main", agent_a.as_str()),
        (agent_a.as_str(), agent_b.as_str()),
        (agent_b.as_str(), "main"),
    ];
    let updates = messages
        .iter()
        .filter(|message| message["method"] == "thread/agentMessage/updated")
        .collect::<Vec<_>>();
    assert_eq!(updates.len(), 6, "each of three messages needs two states");
    assert!(updates.iter().all(|message| {
        let params = &message["params"];
        params.get("turnId").is_none()
            && params.get("message").is_none()
            && params.get("body").is_none()
            && params.get("contextId").is_none()
            && params.get("taskId").is_none()
            && params.get("threadId").is_some()
    }));
    assert!(updates
        .iter()
        .all(|message| message["params"]["status"] != "undeliverable"));

    let mut route_ids = Vec::new();
    for (from, to) in expected_routes {
        let route = updates
            .iter()
            .filter(|message| message["params"]["from"] == from && message["params"]["to"] == to)
            .collect::<Vec<_>>();
        assert_eq!(route.len(), 2, "route {from}->{to} did not have two states");
        assert_eq!(route[0]["params"]["status"], "queued");
        assert_eq!(route[1]["params"]["status"], "delivered");
        let id = route[0]["params"]["messageId"]
            .as_str()
            .expect("message update omitted id");
        assert_eq!(route[1]["params"]["messageId"], id);
        route_ids.push(id.to_string());
    }
    assert_eq!(route_ids.iter().collect::<HashSet<_>>().len(), 3);
    let sequences = route_ids
        .iter()
        .map(|id| id.strip_prefix("message-").unwrap().parse::<u64>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(sequences, [1, 2, 3]);

    let send_starts = tool_items(&messages, "item/started", "send_message");
    assert!(send_starts.iter().all(|message| {
        let input = &message["params"]["item"]["input"];
        input.get("to").is_some()
            && input.get("summary").is_some()
            && input.get("messageBytes").is_some()
            && input.get("message").is_none()
    }));
    let senders = send_starts
        .iter()
        .map(|message| {
            message["params"]["item"]["agent"]
                .as_str()
                .unwrap_or("main")
                .to_string()
        })
        .collect::<HashSet<_>>();
    assert_eq!(
        senders,
        HashSet::from(["main".to_string(), agent_a.clone(), agent_b.clone()])
    );
    let list_start = tool_items(&messages, "item/started", "list_agents");
    assert_eq!(list_start[0]["params"]["item"]["agent"], agent_a);

    client.shutdown();
    let sessions = root.workspace().join(".kloop/sessions");
    let main_rollout = std::fs::read_to_string(sessions.join(format!("{thread_id}.jsonl")))
        .expect("missing main mailbox rollout");
    let agent_a_rollout =
        std::fs::read_to_string(sessions.join(format!("{thread_id}-{agent_a}.jsonl")))
            .expect("missing Agent A rollout");
    let agent_b_rollout =
        std::fs::read_to_string(sessions.join(format!("{thread_id}-{agent_b}.jsonl")))
            .expect("missing Agent B rollout");
    assert!(agent_a_rollout.contains(MAILBOX_MAIN_TO_A));
    assert!(agent_a_rollout.contains(&format!("[{} from main]", route_ids[0])));
    assert!(agent_b_rollout.contains(MAILBOX_A_TO_B));
    assert!(agent_b_rollout.contains(&format!("[{} from {agent_a}]", route_ids[1])));
    assert!(main_rollout.contains(MAILBOX_B_TO_MAIN));
    assert!(main_rollout.contains(&format!("[{} from {agent_b}]", route_ids[2])));
    assert_eq!(
        main_rollout
            .matches("A background sub-agent you dispatched has finished")
            .count(),
        2,
        "final Agent results must keep the existing completion path"
    );

    println!(
        "real local mailbox acceptance passed: provider={provider} model={model} messages=3 delivered=3 agent_terminals=2 completion_deliveries=2"
    );
}
