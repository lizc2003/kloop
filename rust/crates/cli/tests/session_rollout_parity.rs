#![cfg(unix)]

//! The same input sequence through all four front-ends has to leave the same
//! model-visible conversation behind. They differ in how a turn is presented —
//! TUI cells, plain text, headless NDJSON, server notifications — not in what a
//! turn does, because all four hand the session driver the same input.

mod tui_pty_support;

use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Result;
use anyhow::bail;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use tui_pty_support::ChatFixture;
use tui_pty_support::PtyHarness;
use tui_pty_support::PtyOptions;
use tui_pty_support::sse_text;

const PROMPT: &str = "the shared prompt";
const ANSWER: &str = "the shared answer";
const CTRL_C: &[u8] = b"\x03";

fn provider_config(base_url: &str) -> String {
    format!(
        "provider = \"openai-compat\"\n\n\
         [providers.openai-compat]\n\
         wire_api = \"chat\"\n\
         base_url = \"{base_url}\"\n\
         auth_header = {{ Authorization = \"Bearer synthetic-parity-key\" }}\n\
         model = \"tui-pty-model\"\n"
    )
}

/// What a model saw, in order: the text blocks of every message. Ids and
/// provenance are runtime bookkeeping and may differ per run; the conversation
/// is what must not.
fn transcript(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The sessions one run wrote. A run is one session; anything else means the
/// test drove more than it meant to.
fn only_session(home: &Path) -> Result<Vec<Message>> {
    let mut files = Vec::new();
    let projects = home.join(".kloop").join("projects");
    for version in std::fs::read_dir(&projects)
        .map_err(|error| anyhow::anyhow!("read {}: {error}", projects.display()))?
    {
        let version = version?.path();
        for project in std::fs::read_dir(&version)? {
            let sessions = project?.path().join("sessions");
            let Ok(entries) = std::fs::read_dir(&sessions) else {
                continue;
            };
            for file in entries {
                let file = file?.path();
                if file.extension().is_some_and(|ext| ext == "jsonl") {
                    files.push(file);
                }
            }
        }
    }
    match files.as_slice() {
        [file] => kloop_core::rollout::load_session(file)
            .map_err(|error| anyhow::anyhow!("load {}: {error}", file.display())),
        other => bail!("expected exactly one session file, found {other:?}"),
    }
}

fn write_home(home: &Path, base_url: &str) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;

    let kloop_home = home.join(".kloop");
    std::fs::create_dir_all(&kloop_home)?;
    std::fs::set_permissions(&kloop_home, std::fs::Permissions::from_mode(0o700))?;
    let config = kloop_home.join("config.toml");
    std::fs::write(&config, provider_config(base_url))?;
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))?;
    let workspace = home.join("workspace");
    std::fs::create_dir_all(&workspace)?;
    Ok(workspace)
}

fn child_command(home: &Path, workspace: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kloop"));
    command
        .args(args)
        .current_dir(workspace)
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("TERM", "xterm-256color")
        .env("NO_PROXY", "127.0.0.1,localhost,::1")
        .env("USER", "kloop-test");
    command
}

fn headless_messages(base_url: &str) -> Result<Vec<Message>> {
    let home = tempfile::tempdir()?;
    let workspace = write_home(home.path(), base_url)?;
    let output = child_command(home.path(), &workspace, &["--headless", PROMPT])
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        bail!(
            "headless failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    only_session(home.path())
}

fn send(stdin: &mut impl Write, value: serde_json::Value) -> Result<()> {
    writeln!(stdin, "{value}")?;
    stdin.flush()?;
    Ok(())
}

fn next_response(
    lines: &mut impl Iterator<Item = std::io::Result<String>>,
) -> Result<serde_json::Value> {
    for line in lines {
        let event: serde_json::Value = serde_json::from_str(&line?)?;
        if event.get("id").is_some() {
            return Ok(event);
        }
    }
    bail!("the server closed before answering")
}

fn server_messages(base_url: &str) -> Result<Vec<Message>> {
    let home = tempfile::tempdir()?;
    let workspace = write_home(home.path(), base_url)?;
    let mut child = child_command(home.path(), &workspace, &["--serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut lines = BufReader::new(stdout).lines();
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocol_version": "1.0", "capabilities": {}}
        }),
    )?;
    next_response(&mut lines)?;
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "thread/start",
            "params": {}
        }),
    )?;
    let started = next_response(&mut lines)?;
    let thread_id = started["result"]["thread"]["id"]
        .as_str()
        .expect("thread/start names the thread")
        .to_string();
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "turn/start",
            "params": {"thread_id": thread_id, "input": PROMPT}
        }),
    )?;
    loop {
        let Some(line) = lines.next() else {
            bail!("the server closed before turn/completed");
        };
        let event: serde_json::Value = serde_json::from_str(&line?)?;
        if event["method"] == "turn/completed" {
            break;
        }
    }
    drop(stdin);
    let status = child.wait()?;
    if !status.success() {
        bail!("server exited with {status}");
    }
    only_session(home.path())
}

fn plain_messages(base_url: &str) -> Result<Vec<Message>> {
    let mut harness = PtyHarness::spawn_with_options(
        Path::new(env!("CARGO_BIN_EXE_kloop")),
        base_url,
        24,
        100,
        &PtyOptions {
            args: &["--plain"],
            ..PtyOptions::default()
        },
    )?;
    harness.wait_for("plain boot", Duration::from_secs(10), |frame| {
        frame.contains("type a task")
    })?;
    harness.write(format!("{PROMPT}\n").as_bytes())?;
    harness.wait_for("plain answer", Duration::from_secs(10), |frame| {
        frame.contains(ANSWER)
    })?;
    harness.write(b"exit\n")?;
    let status = harness.wait_for_exit(Duration::from_secs(5))?;
    if !status.success() {
        bail!("plain exited with {}", status.exit_code());
    }
    only_session(harness.home())
}

fn tui_messages(base_url: &str) -> Result<Vec<Message>> {
    let mut harness = PtyHarness::spawn(Path::new(env!("CARGO_BIN_EXE_kloop")), base_url, 30, 100)?;
    harness.wait_for("TUI boot", Duration::from_secs(10), |frame| {
        frame.contains("Type a message")
    })?;
    harness.write(format!("{PROMPT}\r").as_bytes())?;
    harness.wait_for("TUI answer", Duration::from_secs(10), |frame| {
        frame.contains(ANSWER)
    })?;
    harness.write(CTRL_C)?;
    harness.wait_for("first Ctrl+C arms exit", Duration::from_secs(2), |frame| {
        frame.contains("press Ctrl+C again to exit")
    })?;
    harness.write(CTRL_C)?;
    let status = harness.wait_for_exit(Duration::from_secs(5))?;
    if !status.success() {
        bail!("TUI exited with {}", status.exit_code());
    }
    only_session(harness.home())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_front_end_leaves_the_same_conversation() -> Result<()> {
    // One scripted reply per run: every front-end sends one turn.
    let tui_fixture = ChatFixture::start(vec![sse_text(ANSWER)]).await;
    let plain_fixture = ChatFixture::start(vec![sse_text(ANSWER)]).await;
    let headless_fixture = ChatFixture::start(vec![sse_text(ANSWER)]).await;
    let server_fixture = ChatFixture::start(vec![sse_text(ANSWER)]).await;

    let tui = transcript(&tui_messages(&tui_fixture.uri())?);
    let plain = transcript(&plain_messages(&plain_fixture.uri())?);
    let headless = transcript(&headless_messages(&headless_fixture.uri())?);
    let server = transcript(&server_messages(&server_fixture.uri())?);

    let expected = vec![PROMPT.to_string(), ANSWER.to_string()];
    assert_eq!(tui, expected, "TUI conversation");
    assert_eq!(plain, tui, "plain conversation");
    assert_eq!(headless, tui, "headless conversation");
    assert_eq!(server, tui, "server conversation");
    Ok(())
}
