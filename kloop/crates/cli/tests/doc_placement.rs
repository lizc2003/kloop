//! A doc comment that stops *before* the item it describes, held down by the
//! item someone inserted underneath it. The syntax is legal, so neither the
//! compiler nor any test says a word — `cargo doc` simply publishes the wrong
//! prose on the wrong item, and the item that should have had it has none.
//!
//! Lesson 111(c) recorded the shape ("拼接式编辑要么用锚点、要么自带尾巴,不能
//! 两者都有") but only the one instance it had just tripped over was fixed.
//! Plan 136's full read found the same shape in four more files across two
//! crates — which is why this guard lives in the top-level binary's integration
//! tests rather than in either crate: the invariant is repository-wide.
//!
//! Anchored on a distinctive phrase rather than the whole block, so the prose
//! stays free to be rewritten; only its placement is nailed down.

/// Read one workspace source file. `CARGO_MANIFEST_DIR` is this crate, so
/// sibling crates are reached through it.
fn source(krate: &str, file: &str) -> String {
    let path = format!("{}/../{krate}/src/{file}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("cannot read {path}: {error}"))
}

/// The doc comment attached to the declaration starting with `item`, flattened
/// to one line. Attributes sitting between the block and the declaration are
/// stepped over; anything else ends the block.
fn doc_above(source: &str, item: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let index = lines
        .iter()
        .position(|line| line.trim_start().starts_with(item))
        .unwrap_or_else(|| panic!("no declaration starting with `{item}`"));
    let mut doc: Vec<&str> = Vec::new();
    for line in lines[..index].iter().rev() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("///") {
            doc.push(rest.trim());
        } else if !trimmed.starts_with("#[") {
            break;
        }
    }
    doc.reverse();
    doc.join(" ")
}

#[track_caller]
fn assert_documented_by(source: &str, item: &str, phrase: &str) {
    let doc = doc_above(source, item);
    assert!(
        doc.contains(phrase),
        "`{item}` should carry the documentation about \"{phrase}\", but has: {doc}"
    );
}

#[track_caller]
fn assert_not_documented_by(source: &str, item: &str, phrase: &str) {
    let doc = doc_above(source, item);
    assert!(
        !doc.contains(phrase),
        "\"{phrase}\" documents another item, yet sits on `{item}`: {doc}"
    );
}

#[test]
fn the_stream_resume_cap_keeps_its_own_documentation() {
    let agent = source("core", "agent.rs");
    assert_documented_by(&agent, "const STREAM_RESUME_LIMIT", "resuming a turn");
    assert_not_documented_by(&agent, "struct Ending {", "resuming a turn");
    assert_documented_by(
        &agent,
        "struct Ending {",
        "How one round decides the turn ends",
    );
}

#[test]
fn the_context_estimate_keeps_its_own_documentation() {
    let history = source("core", "history.rs");
    assert_documented_by(&history, "pub fn estimated_tokens", "Current context size");
    assert_not_documented_by(&history, "pub fn effective_window", "Current context size");
    assert_documented_by(
        &history,
        "pub fn effective_window",
        "window to plan against",
    );
}

#[test]
fn the_sensitive_path_list_keeps_its_own_documentation() {
    let permissions = source("core", "permissions.rs");
    assert_documented_by(&permissions, "fn path_is_sensitive", "privilege escalation");
    assert_not_documented_by(
        &permissions,
        "fn path_tail_is_spill",
        "privilege escalation",
    );
    assert_documented_by(
        &permissions,
        "fn path_tail_is_spill",
        "offload/<spill file>",
    );
}

#[test]
fn the_transcript_replay_keeps_its_own_documentation() {
    let app = source("tui", "app.rs");
    assert_documented_by(
        &app,
        "pub fn cells_from_history",
        "Replay a resumed session",
    );
    assert_not_documented_by(&app, "fn injected_label", "Replay a resumed session");
    assert_documented_by(
        &app,
        "fn injected_label",
        "One line for an injected message",
    );
}
