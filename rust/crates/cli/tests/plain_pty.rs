#![cfg(unix)]

mod tui_pty_support;

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Mutex;

use tui_pty_support::ChatFixture;
use tui_pty_support::PtyHarness;
use tui_pty_support::PtyOptions;
use tui_pty_support::contains_bytes;
use tui_pty_support::sse_text;

static PTY_TEST_LOCK: Mutex<()> = Mutex::const_new(());

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_switch_reaches_the_next_request_in_plain_and_tui() -> Result<()> {
    let _guard = PTY_TEST_LOCK.lock().await;
    for plain in [true, false] {
        let original = ChatFixture::start(vec![
            sse_text("ORIGINAL_ROUTE_ACK"),
            sse_text("RETURN_ROUTE_ACK"),
        ])
        .await;
        let next = ChatFixture::start(vec![sse_text("NEXT_ROUTE_ACK")]).await;
        let original_url = original.uri();
        let next_url = next.uri();
        let config = format!(
            "provider = \"openai-compat\"\n\n\
             [providers.openai-compat]\n\
             wire_api = \"chat\"\n\
             base_url = \"{original_url}\"\n\
             auth_header = {{ Authorization = \"Bearer synthetic-old-key\" }}\n\
             model = \"tui-pty-model\"\n\n\
             [providers.next]\n\
             wire_api = \"chat\"\n\
             base_url = \"{next_url}\"\n\
             auth_header = {{ Authorization = \"Bearer synthetic-new-key\" }}\n\
             model = \"next-model\"\n"
        );
        let mut harness = PtyHarness::spawn_with_options(
            Path::new(env!("CARGO_BIN_EXE_kloop")),
            &original_url,
            24,
            120,
            &PtyOptions {
                args: if plain { &["--plain"] } else { &[] },
                provider_config: Some(&config),
                ..PtyOptions::default()
            },
        )?;
        if plain {
            wait_for_boot(&mut harness)?;
        } else {
            harness.wait_for("TUI boot", Duration::from_secs(10), |frame| {
                frame.contains("Type a message") && frame.bracketed_paste
            })?;
        }
        harness.write(b"before switch\r")?;
        harness.wait_for_quiescent(
            "old route completed",
            Duration::from_secs(10),
            Duration::from_millis(100),
            |frame| frame.contains("ORIGINAL_ROUTE_ACK"),
        )?;
        assert_eq!(original.requests().len(), 1);
        assert!(next.requests().is_empty());
        harness.write(b"/provider next next-model\r")?;
        harness.wait_for_quiescent(
            "provider switched",
            Duration::from_secs(5),
            Duration::from_millis(100),
            |frame| frame.contains("provider switched: next next-model"),
        )?;
        assert_eq!(original.requests().len(), 1);
        assert!(next.requests().is_empty());
        harness.write(b"after switch\r")?;
        harness.wait_for_quiescent(
            "new route completed",
            Duration::from_secs(10),
            Duration::from_millis(100),
            |frame| frame.contains("NEXT_ROUTE_ACK"),
        )?;
        assert_eq!(original.requests().len(), 1);
        let requests = next.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].model, "next-model");
        assert_eq!(
            requests[0].user_texts.last().map(String::as_str),
            Some("after switch")
        );
        assert!(
            requests[0]
                .user_texts
                .iter()
                .any(|text| text == "before switch")
        );
        harness.write(b"/provider openai-compat\r")?;
        harness.wait_for_quiescent(
            "provider switched back",
            Duration::from_secs(5),
            Duration::from_millis(100),
            |frame| frame.contains("provider switched: openai-compat tui-pty-model"),
        )?;
        harness.write(b"after return\r")?;
        harness.wait_for_quiescent(
            "return route completed",
            Duration::from_secs(10),
            Duration::from_millis(100),
            |frame| frame.contains("RETURN_ROUTE_ACK"),
        )?;
        let requests = original.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].model, "tui-pty-model");
        assert_eq!(
            requests[1].user_texts.last().map(String::as_str),
            Some("after return")
        );
        assert_eq!(next.requests().len(), 1);
        harness.write(b"\x03")?;
        if !plain {
            harness.wait_for("exit armed", Duration::from_secs(3), |frame| {
                frame.contains("press Ctrl+C again to exit")
            })?;
            harness.write(b"\x03")?;
        }
        assert!(harness.wait_for_exit(Duration::from_secs(5))?.success());
        assert!(!harness.emergency_killed());
    }
    Ok(())
}

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
