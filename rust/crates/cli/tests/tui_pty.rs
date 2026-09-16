#![cfg(unix)]

mod tui_pty_support;

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use anyhow::bail;
use tokio::sync::Mutex;

use tui_pty_support::ChatFixture;
use tui_pty_support::PtyHarness;
use tui_pty_support::PtyOptions;
use tui_pty_support::contains_bytes;
use tui_pty_support::find_bytes;
use tui_pty_support::sse_text;
use tui_pty_support::sse_tool_call;

static PTY_TEST_LOCK: Mutex<()> = Mutex::const_new(());

/// How long the screen must stay untouched before a frame counts as final.
/// The predicate a test waits on is satisfied by a transitional frame — the
/// repaint behind it is still in flight — and that frame is the wrong one to
/// put in a baseline.
const QUIET: Duration = Duration::from_millis(250);

const CTRL_C: &[u8] = b"\x03";
const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";
const BEGIN_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026h";
const END_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026l";
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

/// Last occurrence of `needle`, for asking "which frame was this byte in?".
fn rfind_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
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
    // The spot checks above say the cursor and the badge survived the shrink.
    // The baseline says what the other 959 cells hold: where the banner wrapped,
    // how the 70-column input rewrapped, whether the footer kept its row.
    let settled = harness.wait_for_quiescent(
        "shrunk screen settles",
        Duration::from_secs(3),
        QUIET,
        |frame| frame.rows == 16 && frame.cols == 60,
    )?;
    insta::assert_snapshot!("resize_shrunk_16x60", settled.stable_text());

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
    let settled = harness.wait_for_quiescent(
        "grown screen settles",
        Duration::from_secs(3),
        QUIET,
        |frame| frame.rows == 24 && frame.cols == 100,
    )?;
    insta::assert_snapshot!("resize_grown_24x100", settled.stable_text());

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
    // Wait for the composer, not just for the answer. The turn's last output and
    // the repainted composer land in different frames, so a predicate that stops
    // at the text returns a transitional frame — one where "Working" has gone,
    // the prompt has not yet come back, and typing into it would go nowhere.
    // Waiting for the prompt costs nothing: if it never returns, this still
    // fails, just by timeout.
    harness.wait_for("first long turn", Duration::from_secs(8), |frame| {
        frame.contains("FIRST_OVERFLOW_TAIL")
            && !frame.contains("Working")
            && frame.contains("Type a message")
    })?;

    harness.write(b"second turn")?;
    harness.write(ENTER)?;
    let final_frame = harness.wait_for("second turn", Duration::from_secs(8), |frame| {
        frame.contains("SECOND_TAIL")
            && !frame.contains("Working")
            && frame.contains("Type a message")
    })?;
    assert_eq!(final_frame.count("Type a message"), 1);
    assert_eq!(final_frame.count("[manual]"), 1);
    // Two commits have scrolled the first turn away; what is left is the seam
    // between them. Counting "Type a message" cannot see a stray blank row or a
    // line of the previous turn left behind by the clear — the baseline can.
    let settled = harness.wait_for_quiescent(
        "second turn settles",
        Duration::from_secs(3),
        QUIET,
        |frame| frame.contains("SECOND_TAIL") && frame.contains("Type a message"),
    )?;
    insta::assert_snapshot!("two_turn_overflow_14x80", settled.stable_text());

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
    // Plan 103 (the black flash): that clear blanks the whole inline viewport,
    // so it must not reach the terminal on its own. Clear and repaint ride one
    // synchronized-update frame, and nothing in the commit may stop to ask the
    // terminal where the cursor is — that `ESC[6n` blocks on crossterm's reader
    // lock (held for up to one 200ms input poll) with the screen already blank.
    assert!(
        !contains_bytes(&raw, CURSOR_POSITION_QUERY),
        "a commit must not stall on a cursor-position query"
    );
    let frame_start =
        rfind_bytes(&raw[..clear], BEGIN_SYNCHRONIZED_UPDATE).expect("clear inside a frame");
    let frame_end =
        find_bytes(&raw, END_SYNCHRONIZED_UPDATE, clear).expect("frame end after the clear");
    assert!(
        rfind_bytes(&raw[..clear], END_SYNCHRONIZED_UPDATE).is_none_or(|end| end < frame_start),
        "the clear must sit inside an open synchronized frame, not between two"
    );
    assert!(
        repaint < frame_end,
        "the repaint must land in the same synchronized frame as the clear"
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

// ---------------------------------------------------------------------------
// Whole-screen layout baselines (plan 149).
//
// The tests above assert terminal-protocol facts: no alternate screen, no
// scroll region, a CPR that gets answered. These assert the other half — what
// the screen actually looks like once a turn has landed. They drive the real
// binary the same way, but every one of them ends on one `insta` baseline of
// the settled frame, so a regression in wrapping, indentation, alignment or
// spacing has somewhere to show up. `cargo insta review` accepts a new one.
// ---------------------------------------------------------------------------

fn spawn_with_files(
    fixture: &ChatFixture,
    rows: u16,
    cols: u16,
    files: &[(&str, &str)],
) -> Result<PtyHarness> {
    PtyHarness::spawn_with_options(
        Path::new(env!("CARGO_BIN_EXE_kloop")),
        &fixture.uri(),
        rows,
        cols,
        &PtyOptions {
            files,
            ..PtyOptions::default()
        },
    )
}

/// Send one message and return the screen once it has stopped moving.
fn one_turn(harness: &mut PtyHarness, message: &str, tail: &str) -> Result<String> {
    harness.write(message.as_bytes())?;
    harness.write(ENTER)?;
    harness.wait_for("turn completes", Duration::from_secs(8), |frame| {
        frame.contains(tail) && !frame.contains("Working") && frame.contains("Type a message")
    })?;
    let settled =
        harness.wait_for_quiescent("screen settles", Duration::from_secs(3), QUIET, |frame| {
            frame.contains(tail) && frame.contains("Type a message")
        })?;
    Ok(settled.stable_text())
}

/// Markdown structure: an ordered list, a nested bullet under one of its items,
/// a fenced code block, and prose around them. The `markdown` unit tests check
/// the lines this produces; only the whole screen shows how those lines sit
/// against each other once wrapped to 80 columns and drawn under the banner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn markdown_reply_with_a_list_and_a_code_block_fills_the_screen() -> Result<()> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let reply = concat!(
        "Two things to change, then a check:\n\n",
        "1. Move the guard above the early return.\n",
        "2. Keep the counter monotonic:\n",
        "   - never reset it on reconnect\n",
        "   - let it wrap at `u32::MAX`\n\n",
        "```rust\n",
        "fn bump(counter: &mut u32) {\n",
        "    *counter = counter.wrapping_add(1);\n",
        "}\n",
        "```\n\n",
        "That is MARKDOWN_TAIL.",
    );
    let fixture = ChatFixture::start(vec![sse_text(reply)]).await;
    let mut harness = spawn(&fixture, 24, 80)?;
    wait_for_boot(&mut harness)?;

    let screen = one_turn(&mut harness, "what should I change?", "MARKDOWN_TAIL")?;
    insta::assert_snapshot!("markdown_list_and_code_24x80", screen);

    graceful_exit(&mut harness)?;
    Ok(())
}

/// One tool call and its result. `read_file` is read-only, so the gate lets it
/// through without a prompt and the turn runs end to end: a tool row, its
/// preview, then the model's closing text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_tool_call_and_its_result_fill_the_screen() -> Result<()> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let fixture = ChatFixture::start(vec![
        sse_tool_call(
            "call-read-1",
            "read_file",
            &serde_json::json!({"path": "notes.txt"}),
        ),
        sse_text("The file says TOOLROW_TAIL."),
    ])
    .await;
    let mut harness = spawn_with_files(&fixture, 24, 80, &[("notes.txt", "alpha\nbeta\ngamma\n")])?;
    wait_for_boot(&mut harness)?;

    let screen = one_turn(&mut harness, "read notes.txt", "TOOLROW_TAIL")?;
    insta::assert_snapshot!("tool_call_and_result_24x80", screen);

    graceful_exit(&mut harness)?;
    Ok(())
}

/// A result longer than the preview cap. The transcript must show the first few
/// lines and say how much it dropped, rather than letting a long file push the
/// composer off the bottom — the "…more" hint is exactly the kind of detail a
/// `contains()` check never looks at.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_tool_result_is_truncated_on_screen() -> Result<()> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let long_file = (1..=20)
        .map(|line| format!("line {line:02} of the long file"))
        .collect::<Vec<_>>()
        .join("\n");
    let fixture = ChatFixture::start(vec![
        sse_tool_call(
            "call-read-2",
            "read_file",
            &serde_json::json!({"path": "long.txt"}),
        ),
        sse_text("Read it: TRUNCATED_TAIL."),
    ])
    .await;
    let mut harness = spawn_with_files(&fixture, 24, 80, &[("long.txt", &long_file)])?;
    wait_for_boot(&mut harness)?;

    let screen = one_turn(&mut harness, "read long.txt", "TRUNCATED_TAIL")?;
    insta::assert_snapshot!("long_tool_result_truncated_24x80", screen);

    graceful_exit(&mut harness)?;
    Ok(())
}
