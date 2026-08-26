#![cfg(unix)]

mod tui_pty_support;

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use anyhow::bail;
use tokio::sync::Mutex;

use tui_pty_support::ChatFixture;
use tui_pty_support::PtyHarness;
use tui_pty_support::contains_bytes;
use tui_pty_support::find_bytes;
use tui_pty_support::sse_text;

static PTY_TEST_LOCK: Mutex<()> = Mutex::const_new(());

const CTRL_C: &[u8] = b"\x03";
const LEFT: &[u8] = b"\x1b[D";
const RIGHT: &[u8] = b"\x1b[C";
const BACKSPACE: &[u8] = b"\x7f";
const DELETE: &[u8] = b"\x1b[3~";
const ENTER: &[u8] = b"\r";
const SHIFT_ENTER_CSI_U: &[u8] = b"\x1b[13;2u";
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";
const KEYBOARD_ENHANCEMENT_PUSH: &[u8] = b"\x1b[>1u";
const KEYBOARD_ENHANCEMENT_POP: &[u8] = b"\x1b[<1u";
const ALTERNATE_SCREEN_SEQUENCES: &[&[u8]] = &[
    b"\x1b[?47h",
    b"\x1b[?47l",
    b"\x1b[?1047h",
    b"\x1b[?1047l",
    b"\x1b[?1049h",
    b"\x1b[?1049l",
];

fn spawn(fixture: &ChatFixture, rows: u16, cols: u16) -> Result<PtyHarness> {
    PtyHarness::spawn(
        Path::new(env!("CARGO_BIN_EXE_kloop")),
        &fixture.uri(),
        rows,
        cols,
    )
}

fn wait_for_boot(harness: &mut PtyHarness) -> Result<()> {
    harness.wait_for("TUI boot", Duration::from_secs(10), |frame| {
        frame.contains(">_ kloop")
            && frame.contains("tui-pty-model")
            && frame.contains("Type a message")
            && frame.contains("[manual]")
            && frame.bracketed_paste
            && frame.cpr_count >= 1
    })?;
    Ok(())
}

fn graceful_exit(harness: &mut PtyHarness) -> Result<()> {
    harness.write(CTRL_C)?;
    harness.wait_for("first Ctrl+C arms exit", Duration::from_secs(2), |frame| {
        frame.contains("press Ctrl+C again to exit")
    })?;
    harness.write(CTRL_C)?;
    let status = harness.wait_for_exit(Duration::from_secs(3))?;
    if !status.success() {
        bail!("kloop exited with {}", status.exit_code());
    }
    Ok(())
}

fn assert_no_alternate_screen(raw: &[u8]) {
    for sequence in ALTERNATE_SCREEN_SEQUENCES {
        assert!(
            !contains_bytes(raw, sequence),
            "alternate-screen sequence appeared: {sequence:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_answers_cpr_without_alternate_screen() -> Result<()> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let fixture = ChatFixture::start(vec![sse_text("unused")]).await;
    let mut harness = spawn(&fixture, 30, 100)?;
    wait_for_boot(&mut harness)?;

    graceful_exit(&mut harness)?;
    let raw = harness.raw();
    assert!(contains_bytes(&raw, b"\x1b[6n"));
    assert!(contains_bytes(&raw, b"\x1b[?2004h"));
    assert!(contains_bytes(&raw, KEYBOARD_ENHANCEMENT_PUSH));
    assert!(contains_bytes(&raw, KEYBOARD_ENHANCEMENT_POP));
    assert_no_alternate_screen(&raw);
    assert!(!harness.emergency_killed());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resize_keeps_cpr_and_current_viewport_in_sync() -> Result<()> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let fixture = ChatFixture::start(vec![sse_text("unused")]).await;
    let mut harness = spawn(&fixture, 30, 100)?;
    wait_for_boot(&mut harness)?;

    let input = "r".repeat(70);
    harness.write(input.as_bytes())?;
    harness.wait_for(
        "wide composer layout",
        Duration::from_millis(1500),
        |frame| frame.cols == 100 && frame.cursor.1 == 72,
    )?;

    let initial_cpr = harness.snapshot().cpr_count;
    harness.resize(16, 60)?;
    let shrunk = harness.wait_for("shrink redraw", Duration::from_millis(1500), |frame| {
        frame.rows == 16
            && frame.cols == 60
            && frame.cpr_count > initial_cpr
            && frame.cursor.1 == 14
            && frame.contains("[manual]")
    })?;
    assert_eq!(harness.pty_size()?, (16, 60));
    assert!(shrunk.cursor.0 < shrunk.rows && shrunk.cursor.1 < shrunk.cols);
    assert_eq!(shrunk.count("[manual]"), 1);

    let shrunk_cpr = shrunk.cpr_count;
    harness.resize(24, 100)?;
    let grown = harness.wait_for("grow redraw", Duration::from_millis(1500), |frame| {
        frame.rows == 24
            && frame.cols == 100
            && frame.cpr_count > shrunk_cpr
            && frame.cursor.1 == 72
            && frame.contains("[manual]")
    })?;
    assert_eq!(harness.pty_size()?, (24, 100));
    assert!(grown.cursor.0 < grown.rows && grown.cursor.1 < grown.cols);
    assert_eq!(grown.count("[manual]"), 1);

    graceful_exit(&mut harness)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_turn_overflow_commits_without_scroll_regions_then_repaints() -> Result<()> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let mut first = (0..36)
        .map(|index| format!("FIRST-OVERFLOW-{index:02}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    first.push_str("\n\nFIRST_OVERFLOW_TAIL");
    let fixture = ChatFixture::start(vec![sse_text(&first), sse_text("SECOND_TAIL")]).await;
    let mut harness = spawn(&fixture, 14, 80)?;
    wait_for_boot(&mut harness)?;
    let mark = harness.raw_mark();

    harness.write(b"first turn")?;
    harness.write(ENTER)?;
    harness.wait_for("first long turn", Duration::from_secs(8), |frame| {
        frame.contains("FIRST_OVERFLOW_TAIL") && !frame.contains("Working")
    })?;

    harness.write(b"second turn")?;
    harness.write(ENTER)?;
    let final_frame = harness.wait_for("second turn", Duration::from_secs(8), |frame| {
        frame.contains("SECOND_TAIL") && !frame.contains("Working")
    })?;
    assert_eq!(final_frame.count("Type a message"), 1);
    assert_eq!(final_frame.count("[manual]"), 1);

    let raw = harness.raw_since(mark);
    // Bug #1 (plan 99): a full-height inline commit must NOT drive DECSTBM scroll
    // regions — that per-row path smears committed lines in iTerm2. Overflow now
    // scrolls into scrollback with plain line feeds (append_lines) instead, so
    // none of the scroll-region control sequences may appear.
    assert!(
        find_bytes(&raw, b"\x1b[1;1r", 0).is_none(),
        "commit must not set a one-row scroll region"
    );
    assert!(
        find_bytes(&raw, b"\x1b[1S", 0).is_none(),
        "commit must not emit a scroll-region scroll-up"
    );
    // The overflow still commits, clears the inline viewport, then repaints the
    // first turn's tail followed by the second turn's — in that order.
    let clear = find_bytes(&raw, b"\x1b[J", 0).expect("inline clear");
    let repaint =
        find_bytes(&raw, b"FIRST_OVERFLOW_TAIL", clear).expect("first tail repaint after clear");
    let second = find_bytes(&raw, b"SECOND_TAIL", repaint).expect("second tail after first");
    assert!(
        clear < repaint && repaint < second,
        "commit ordering in raw output"
    );

    graceful_exit(&mut harness)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn double_ctrl_c_restores_terminal_modes_without_emergency_kill() -> Result<()> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let fixture = ChatFixture::start(vec![sse_text("unused")]).await;
    let mut harness = spawn(&fixture, 24, 90)?;
    wait_for_boot(&mut harness)?;
    let mark = harness.raw_mark();

    harness.write(CTRL_C)?;
    harness.wait_for("exit arm", Duration::from_secs(2), |frame| {
        frame.contains("press Ctrl+C again to exit")
    })?;
    harness.write(CTRL_C)?;
    let status = harness.wait_for_exit(Duration::from_secs(3))?;
    assert!(status.success());
    assert!(!harness.emergency_killed());

    let raw = harness.raw();
    assert_no_alternate_screen(&raw);
    let restore = &raw[mark.min(raw.len())..];
    let keyboard_pop =
        find_bytes(restore, KEYBOARD_ENHANCEMENT_POP, 0).expect("keyboard enhancement pop");
    let paste_disable =
        find_bytes(restore, b"\x1b[?2004l", keyboard_pop).expect("bracketed paste disable");
    assert!(keyboard_pop < paste_disable);
    assert!(contains_bytes(restore, b"\x1b[?25h"));
    assert!(restore.ends_with(b"\r\n"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bracketed_crlf_paste_submits_a_logical_multiline_message() -> Result<()> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let fixture = ChatFixture::start(vec![sse_text("PASTE_ACK")]).await;
    let mut harness = spawn(&fixture, 24, 100)?;
    wait_for_boot(&mut harness)?;

    harness.write(BRACKETED_PASTE_START)?;
    harness.write(b"abc\r\ndef")?;
    harness.write(BRACKETED_PASTE_END)?;
    harness.write(ENTER)?;
    harness.wait_for(
        "pasted multiline response",
        Duration::from_secs(8),
        |frame| frame.contains("PASTE_ACK") && !frame.contains("Working"),
    )?;

    let requests = fixture.requests();
    let request = requests.last().expect("one OpenAI request");
    assert_eq!(
        request.user_texts.last().map(String::as_str),
        Some("abc\ndef")
    );

    graceful_exit(&mut harness)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn csi_u_shift_enter_submits_a_multiline_message() -> Result<()> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let fixture = ChatFixture::start(vec![sse_text("MULTILINE_ACK")]).await;
    let mut harness = spawn(&fixture, 24, 100)?;
    wait_for_boot(&mut harness)?;

    harness.write(b"first")?;
    harness.write(SHIFT_ENTER_CSI_U)?;
    harness.write(b"second")?;
    harness.write(ENTER)?;
    harness.wait_for("multiline response", Duration::from_secs(8), |frame| {
        frame.contains("MULTILINE_ACK") && !frame.contains("Working")
    })?;

    let requests = fixture.requests();
    let request = requests.last().expect("one OpenAI request");
    assert_eq!(request.model, "tui-pty-model");
    assert_eq!(
        request.user_texts.last().map(String::as_str),
        Some("first\nsecond")
    );

    graceful_exit(&mut harness)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unicode_input_round_trips_through_real_binary_editing() -> Result<()> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let fixture = ChatFixture::start(vec![sse_text("UNICODE_ACK")]).await;
    let mut harness = spawn(&fixture, 24, 100)?;
    wait_for_boot(&mut harness)?;

    let expected = "Ae\u{301}B界C👩🏽‍💻D";
    harness.write(expected.as_bytes())?;

    // Forward-delete the complete ZWJ cluster, then restore it in place.
    harness.write(LEFT)?;
    harness.write(LEFT)?;
    harness.write(DELETE)?;
    harness.write("👩🏽‍💻".as_bytes())?;

    // Backspace the complete CJK grapheme, then restore it in place.
    harness.write(LEFT)?;
    harness.write(LEFT)?;
    harness.write(BACKSPACE)?;
    harness.write("界".as_bytes())?;

    // Backspace must remove the base plus combining mark as one grapheme.
    harness.write(LEFT)?;
    harness.write(LEFT)?;
    harness.write(BACKSPACE)?;
    harness.write("e\u{301}".as_bytes())?;

    // Traverse B, CJK, C, ZWJ, and D back to the document end.
    for _ in 0..5 {
        harness.write(RIGHT)?;
    }
    harness.write(ENTER)?;
    harness.wait_for("Unicode response", Duration::from_secs(8), |frame| {
        frame.contains("UNICODE_ACK") && !frame.contains("Working")
    })?;

    let requests = fixture.requests();
    let request = requests.last().expect("one OpenAI request");
    assert_eq!(request.model, "tui-pty-model");
    assert_eq!(
        request.user_texts.last().map(String::as_str),
        Some(expected)
    );

    graceful_exit(&mut harness)?;
    Ok(())
}
