#![cfg(unix)]

mod tui_pty_support;

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Mutex;

use tui_pty_support::ChatFixture;
use tui_pty_support::PtyHarness;
use tui_pty_support::contains_bytes;
use tui_pty_support::sse_text;

static PTY_TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn spawn(fixture: &ChatFixture) -> Result<PtyHarness> {
    PtyHarness::spawn_with_args(
        Path::new(env!("CARGO_BIN_EXE_kloop")),
        &fixture.uri(),
        24,
        100,
        &["--plain"],
    )
}

fn wait_for_boot(harness: &mut PtyHarness) -> Result<()> {
    harness.wait_for("plain boot", Duration::from_secs(10), |frame| {
        frame.contains("type a task, /help for commands, 'exit' or Ctrl+C to quit")
            && frame.contains("> ")
    })?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_ctrl_c_exits_once_without_advertising_ctrl_d() -> Result<()> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let fixture = ChatFixture::start(vec![sse_text("unused")]).await;
    let mut harness = spawn(&fixture)?;
    wait_for_boot(&mut harness)?;
    assert!(!harness.snapshot().contains("Ctrl+D"));

    harness.write(b"\r")?;
    harness.wait_for("idle loop advanced", Duration::from_secs(2), |frame| {
        frame.text.matches("> ").count() >= 2
    })?;
    std::thread::sleep(Duration::from_millis(100));
    harness.write(b"\x03")?;
    let status = harness.wait_for_exit(Duration::from_secs(3))?;
    assert!(
        status.success(),
        "exit code {}; raw={}",
        status.exit_code(),
        String::from_utf8_lossy(&harness.raw())
    );
    let raw = harness.raw();
    assert!(
        raw.ends_with(b"\r\n"),
        "raw={}",
        String::from_utf8_lossy(&raw)
    );
    assert!(!harness.emergency_killed());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_ctrl_c_before_any_output_discards_the_turn() -> Result<()> {
    let _guard = PTY_TEST_LOCK.lock().await;
    // Delay a valid response long enough that the turn is definitely active but
    // has produced nothing. Ctrl+C must cancel it and leave cleanly without
    // waiting for the delayed body — and because not one token arrived, the
    // turn never happened: the message is not recorded, so there is no history
    // to patch and the plain REPL says as much.
    let fixture =
        ChatFixture::start_delayed(vec![sse_text("too late")], Duration::from_secs(30)).await;
    let mut harness = spawn(&fixture)?;
    wait_for_boot(&mut harness)?;

    harness.write(b"wait forever\r")?;
    harness.wait_for("provider request", Duration::from_secs(5), |_| {
        !fixture.requests().is_empty()
    })?;
    harness.write(b"\x03")?;
    let status = harness.wait_for_exit(Duration::from_secs(5))?;
    assert!(status.success());
    assert!(!harness.emergency_killed());
    assert!(contains_bytes(
        &harness.raw(),
        "[interrupted before the model replied — that message was not recorded]".as_bytes()
    ));
    Ok(())
}
