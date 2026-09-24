use std::time::UNIX_EPOCH;

use serde_json::json;

use super::*;

#[derive(Default)]
struct MemSink {
    saved: Vec<String>,
    broken: bool,
}

impl OffloadSink for MemSink {
    fn save(&mut self, content: &str) -> std::io::Result<PathBuf> {
        if self.broken {
            return Err(std::io::Error::other("disk full"));
        }
        self.saved.push(content.to_string());
        Ok(PathBuf::from(format!(
            "/offload/off-{:04}.txt",
            self.saved.len()
        )))
    }
}

fn identity(family: ProviderApiFamily, model: &str) -> ProviderAttemptIdentity {
    ProviderAttemptIdentity {
        route_revision: 1,
        provider_id: "p".into(),
        api_family: family,
        endpoint_fingerprint: "e".into(),
        model: model.into(),
    }
}

fn anthropic() -> ProviderAttemptIdentity {
    identity(ProviderApiFamily::AnthropicMessages, "m")
}

fn at(minutes: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_000_000 + minutes * 60)
}

/// One request at `minutes`, returning what it would send.
fn send(
    view: &[Message],
    state: &mut ReductionState,
    sink: &mut MemSink,
    identity: &ProviderAttemptIdentity,
    minutes: u64,
) -> Vec<Message> {
    let mut sent = view.to_vec();
    reduce(
        &mut sent,
        state,
        &RequestReduction {
            cwd: Path::new("/w"),
            now: at(minutes),
            identity,
        },
        sink,
    );
    sent
}

fn call(id: &str, name: &str, input: Value) -> Message {
    Message::assistant(vec![ContentBlock::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }])
}

fn result(id: &str, text: impl Into<String>, is_error: bool) -> Message {
    Message::tool_results(vec![ContentBlock::ToolResult {
        tool_use_id: id.into(),
        content: ToolResultContent::Text(text.into()),
        is_error,
    }])
}

fn reply() -> Message {
    Message::assistant(vec![ContentBlock::Text { text: "ok".into() }])
}

/// A tool call with its result, aged by `age` later assistant messages.
fn aged(
    id: &str,
    name: &str,
    input: Value,
    text: &str,
    is_error: bool,
    age: usize,
) -> Vec<Message> {
    let mut view = vec![call(id, name, input), result(id, text, is_error)];
    view.extend(std::iter::repeat_with(reply).take(age));
    view
}

fn text_of<'a>(view: &'a [Message], id: &str) -> &'a str {
    view.iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content: ToolResultContent::Text(text),
                ..
            } if tool_use_id == id => Some(text.as_str()),
            _ => None,
        })
        .unwrap()
}

fn big(chars: usize) -> String {
    "x".repeat(chars)
}

const ADVICE: &str = "query it in place (bash with grep or `python3 -c '...'` over that path, \
                      printing only what you need) instead of reading it back";

#[test]
fn a_read_made_stale_by_a_later_edit_is_stubbed_with_path_range_and_reason() {
    let original = big(5000);
    let mut view = vec![Message::user_text("go")];
    view.push(call(
        "r1",
        "read_file",
        json!({"path": "src/a.rs", "offset": 10, "limit": 40}),
    ));
    view.push(result("r1", original.clone(), false));
    view.push(call("e1", "edit_file", json!({"path": "/w/src/./a.rs"})));
    view.push(result("e1", "edited", false));
    view.push(reply());
    let mut sink = MemSink::default();

    let sent = send(
        &view,
        &mut ReductionState::default(),
        &mut sink,
        &anthropic(),
        0,
    );

    assert_eq!(
        text_of(&sent, "r1"),
        format!(
            "[read_file src/a.rs (lines 10-49, 5000 chars) removed from this request: stale, \
             changed by a later edit_file. The original result is saved to \
             /offload/off-0001.txt; {ADVICE}. For the file as it is now, read_file it again.]"
        )
    );
    assert_eq!(sink.saved, vec![original]);
}

#[test]
fn a_read_is_judged_only_by_the_calls_after_it() {
    let original = big(5000);
    let read = |id: &str, input: Value| {
        vec![
            call(id, "read_file", input),
            result(id, original.clone(), false),
        ]
    };
    let mut view = Vec::new();
    // Superseded: a later full read covers lines 10-29.
    view.extend(read(
        "covered",
        json!({"path": "/w/a.rs", "offset": 10, "limit": 20}),
    ));
    // A failed edit changed nothing; a later partial read does not cover it.
    view.extend(read("kept", json!({"path": "/w/b.rs"})));
    view.push(call("bad-edit", "write_file", json!({"path": "/w/b.rs"})));
    view.push(result("bad-edit", "denied", true));
    view.extend(read(
        "partial",
        json!({"path": "/w/b.rs", "offset": 1, "limit": 5}),
    ));
    view.extend(read("full", json!({"path": "/w/a.rs"})));
    view.extend(std::iter::repeat_with(reply).take(10));

    let sent = send(
        &view,
        &mut ReductionState::default(),
        &mut MemSink::default(),
        &anthropic(),
        0,
    );

    assert_eq!(
        text_of(&sent, "covered"),
        format!(
            "[read_file /w/a.rs (lines 10-29, 5000 chars) removed from this request: superseded \
             by a later read_file of the same lines. The original result is saved to \
             /offload/off-0001.txt; {ADVICE}. For the file as it is now, read_file it again.]"
        )
    );
    // Still the model's current view of their lines, however old.
    for id in ["kept", "partial", "full"] {
        assert_eq!(text_of(&sent, id), original, "{id}");
    }
}

#[test]
fn a_search_keeps_the_files_it_named() {
    let mut output = String::new();
    for file in 0..25 {
        for line in 0..3 {
            output.push_str(&format!(
                "src/f{file:02}.rs:{line}:{}\n",
                "needle ".repeat(8)
            ));
        }
    }
    output.push_str("--\nFound 75 total occurrences across 25 files.");
    let view = aged(
        "g1",
        "grep",
        json!({"pattern": "needle", "output_mode": "content"}),
        &output,
        false,
        2,
    );

    let sent = send(
        &view,
        &mut ReductionState::default(),
        &mut MemSink::default(),
        &anthropic(),
        0,
    );

    let files: Vec<String> = (0..20).map(|file| format!("src/f{file:02}.rs")).collect();
    assert_eq!(
        text_of(&sent, "g1"),
        format!(
            "[grep result removed from this request: 77 lines, {} chars.\nFiles it named \
             (first 20):\n{}\nThe full result is saved to /offload/off-0001.txt; {ADVICE}.]",
            output.chars().count(),
            files.join("\n")
        )
    );
}

#[test]
fn a_glob_listing_is_its_own_file_list() {
    let paths: Vec<String> = (0..300)
        .map(|n| format!("/w/src/module_{n:03}.rs"))
        .collect();
    let view = aged(
        "g1",
        "glob",
        json!({"pattern": "**/*.rs"}),
        &paths.join("\n"),
        false,
        2,
    );

    let sent = send(
        &view,
        &mut ReductionState::default(),
        &mut MemSink::default(),
        &anthropic(),
        0,
    );

    assert!(
        text_of(&sent, "g1").contains(&format!("(first 20):\n{}\n", paths[..20].join("\n"))),
        "{}",
        text_of(&sent, "g1")
    );
}

#[test]
fn a_shell_result_keeps_its_last_lines() {
    let lines: Vec<String> = (1..=40).map(|n| format!("out {n:02}")).collect();
    let output = format!("{}\n{}", big(3000), lines.join("\n"));
    let view = aged("b1", "bash", json!({"command": "make"}), &output, false, 2);

    let sent = send(
        &view,
        &mut ReductionState::default(),
        &mut MemSink::default(),
        &anthropic(),
        0,
    );

    assert_eq!(
        text_of(&sent, "b1"),
        format!(
            "[bash output trimmed from this request: {} chars, only its end is kept below. The \
             full output is saved to /offload/off-0001.txt; {ADVICE}.]\n{}",
            output.chars().count(),
            lines[20..].join("\n")
        )
    );
}

#[test]
fn a_shell_tail_of_long_lines_is_cut_to_its_char_budget() {
    let output = (0..30).map(|_| big(200)).collect::<Vec<_>>().join("\n");
    assert_eq!(shell_tail(&output).chars().count(), SHELL_STUB_CHARS);
}

#[test]
fn an_external_result_keeps_its_start_from_a_lower_size_and_a_later_age() {
    let output = format!("{}{}", "h".repeat(500), big(1500));
    let stub = format!(
        "{}\n…[docs__search result trimmed from this request: 2000 chars, only its start is kept \
         above. The full result is saved to /offload/off-0001.txt; {ADVICE}.]",
        "h".repeat(500)
    );
    let input = json!({"q": "x"});
    for (age, is_error, stubbed) in [
        (2, false, false),
        (3, false, true),
        (3, true, false),
        (4, true, true),
    ] {
        let view = aged("m1", "docs__search", input.clone(), &output, is_error, age);
        let sent = send(
            &view,
            &mut ReductionState::default(),
            &mut MemSink::default(),
            &anthropic(),
            0,
        );
        let expected = if stubbed {
            stub.as_str()
        } else {
            output.as_str()
        };
        assert_eq!(
            text_of(&sent, "m1"),
            expected,
            "age {age}, error {is_error}"
        );
    }
}

#[test]
fn shell_errors_wait_until_age_four() {
    let output = big(4000);
    for (age, stubbed) in [(3, false), (4, true)] {
        let view = aged("b1", "bash", json!({"command": "make"}), &output, true, age);
        let sent = send(
            &view,
            &mut ReductionState::default(),
            &mut MemSink::default(),
            &anthropic(),
            0,
        );
        assert_eq!(text_of(&sent, "b1") != output, stubbed, "age {age}");
    }
}

#[test]
fn results_outside_the_list_small_ones_and_previews_are_left_alone() {
    let pointer = format!(
        "{}\n[full output saved to /o/off-0009.txt (40000 chars). Query it in place instead of \
         reading it back: bash with `python3 -c '...'` over that path, printing only the fields \
         you need, so only what you extract enters the context]",
        big(2000)
    );
    let mut view = Vec::new();
    view.extend(aged(
        "s1",
        "skill",
        json!({"name": "x"}),
        &big(9000),
        false,
        5,
    ));
    view.extend(aged(
        "a1",
        "run_agent",
        json!({"prompt": "x"}),
        &big(9000),
        false,
        5,
    ));
    view.extend(aged(
        "q1",
        "ask_user_question",
        json!({}),
        &big(9000),
        false,
        5,
    ));
    view.extend(aged(
        "w1",
        "edit_file",
        json!({"path": "/w/a"}),
        &big(9000),
        false,
        5,
    ));
    view.extend(aged(
        "small",
        "bash",
        json!({"command": "ls"}),
        &big(3000),
        false,
        5,
    ));
    view.extend(aged("off", "docs__search", json!({}), &pointer, false, 5));
    view.push(call("img", "docs__screenshot", json!({})));
    view.push(Message::tool_results(vec![ContentBlock::ToolResult {
        tool_use_id: "img".into(),
        content: ToolResultContent::Blocks(vec![ContentBlock::Text { text: big(9000) }]),
        is_error: false,
    }]));
    view.extend(std::iter::repeat_with(reply).take(5));
    view.extend(aged(
        "young",
        "bash",
        json!({"command": "make"}),
        &big(9000),
        false,
        1,
    ));

    let sent = send(
        &view,
        &mut ReductionState::default(),
        &mut MemSink::default(),
        &anthropic(),
        0,
    );

    assert_eq!(sent, view);
}

#[test]
fn parallel_calls_answered_in_one_message_share_an_age() {
    let output = big(4000);
    let pair = || {
        vec![
            Message::assistant(vec![
                ContentBlock::ToolUse {
                    id: "b1".into(),
                    name: "bash".into(),
                    input: json!({"command": "a"}),
                },
                ContentBlock::ToolUse {
                    id: "b2".into(),
                    name: "bash".into(),
                    input: json!({"command": "b"}),
                },
            ]),
            Message::tool_results(
                ["b1", "b2"]
                    .map(|id| ContentBlock::ToolResult {
                        tool_use_id: id.into(),
                        content: ToolResultContent::Text(output.clone()),
                        is_error: false,
                    })
                    .to_vec(),
            ),
        ]
    };
    for (replies, stubbed) in [(1, false), (2, true)] {
        let mut view = pair();
        view.extend(std::iter::repeat_with(reply).take(replies));
        let sent = send(
            &view,
            &mut ReductionState::default(),
            &mut MemSink::default(),
            &anthropic(),
            0,
        );
        for id in ["b1", "b2"] {
            assert_eq!(
                text_of(&sent, id) != output,
                stubbed,
                "{id} after {replies}"
            );
        }
    }
}

#[test]
fn the_same_history_reduces_to_the_same_bytes() {
    let mut view = aged(
        "b1",
        "bash",
        json!({"command": "make"}),
        &big(4000),
        false,
        2,
    );
    view.extend(aged(
        "g1",
        "grep",
        json!({"pattern": "p"}),
        &big(4000),
        false,
        3,
    ));

    let first = send(
        &view,
        &mut ReductionState::default(),
        &mut MemSink::default(),
        &anthropic(),
        0,
    );
    let second = send(
        &view,
        &mut ReductionState::default(),
        &mut MemSink::default(),
        &anthropic(),
        0,
    );

    assert_eq!(first, second);
    assert_ne!(first, view);
}

#[test]
fn a_warm_cache_is_never_given_up_and_a_sent_stub_never_changes() {
    let output = big(4000);
    let mut view = aged("b1", "bash", json!({"command": "make"}), &output, false, 0);
    let mut state = ReductionState::default();
    let mut sink = MemSink::default();
    send(&view, &mut state, &mut sink, &anthropic(), 0);

    // Old enough now, but the cache from a minute ago is still warm.
    view.extend([reply(), reply()]);
    assert_eq!(
        text_of(&send(&view, &mut state, &mut sink, &anthropic(), 1), "b1"),
        output
    );
    // Four minutes on from *that* request: still inside the TTL.
    assert_eq!(
        text_of(&send(&view, &mut state, &mut sink, &anthropic(), 5), "b1"),
        output
    );

    // Idle past the TTL: nothing cached is left to protect.
    let stubbed = send(&view, &mut state, &mut sink, &anthropic(), 11);
    assert_ne!(text_of(&stubbed, "b1"), output);

    // From here on the same stub, byte for byte, in a request that has grown.
    view.extend(aged(
        "b2",
        "bash",
        json!({"command": "make"}),
        "ok",
        false,
        1,
    ));
    let later = send(&view, &mut state, &mut sink, &anthropic(), 12);
    assert_eq!(later[..stubbed.len()], stubbed[..]);
    assert_eq!(sink.saved.len(), 1);
}

#[test]
fn an_openai_cache_is_trusted_for_an_hour() {
    let output = big(4000);
    let responses = identity(ProviderApiFamily::OpenAiResponses, "m");
    let mut state = ReductionState::default();
    let mut sink = MemSink::default();
    send(
        &[Message::user_text("go")],
        &mut state,
        &mut sink,
        &responses,
        0,
    );
    let view = aged("b1", "bash", json!({"command": "make"}), &output, false, 2);

    assert_eq!(
        text_of(&send(&view, &mut state, &mut sink, &responses, 30), "b1"),
        output
    );
    assert_ne!(
        text_of(&send(&view, &mut state, &mut sink, &responses, 91), "b1"),
        output
    );
}

#[test]
fn a_model_never_asked_here_has_no_cache_to_protect() {
    let output = big(4000);
    let mut state = ReductionState::default();
    let mut sink = MemSink::default();
    send(
        &[Message::user_text("go")],
        &mut state,
        &mut sink,
        &anthropic(),
        0,
    );
    let view = aged("b1", "bash", json!({"command": "make"}), &output, false, 2);

    let other = identity(ProviderApiFamily::AnthropicMessages, "other");
    assert_ne!(
        text_of(&send(&view, &mut state, &mut sink, &other, 1), "b1"),
        output
    );
}

#[test]
fn a_rewritten_history_starts_cold() {
    let output = big(4000);
    let mut state = ReductionState::default();
    let mut sink = MemSink::default();
    send(
        &[Message::user_text("go")],
        &mut state,
        &mut sink,
        &anthropic(),
        0,
    );
    state.reset();
    let view = aged("b1", "bash", json!({"command": "make"}), &output, false, 2);

    assert_ne!(
        text_of(&send(&view, &mut state, &mut sink, &anthropic(), 1), "b1"),
        output
    );
}

#[test]
fn a_resumed_session_is_warm_until_its_file_has_been_quiet_past_the_ttl() {
    let output = big(4000);
    let view = aged("b1", "bash", json!({"command": "make"}), &output, false, 2);
    let mut sink = MemSink::default();

    let mut recent = ReductionState::resumed(Some(at(0)));
    assert_eq!(
        text_of(&send(&view, &mut recent, &mut sink, &anthropic(), 3), "b1"),
        output
    );

    let mut quiet = ReductionState::resumed(Some(at(0)));
    assert_ne!(
        text_of(&send(&view, &mut quiet, &mut sink, &anthropic(), 6), "b1"),
        output
    );
}

#[test]
fn asking_again_for_a_stubbed_search_protects_the_new_result() {
    let output = big(4000);
    let grep = json!({"pattern": "needle"});
    let make = json!({"command": "make"});
    let mut view = aged("g1", "grep", grep.clone(), &output, false, 0);
    view.extend(aged("b1", "bash", make.clone(), &output, false, 2));
    let mut state = ReductionState::default();
    let mut sink = MemSink::default();
    let first = send(&view, &mut state, &mut sink, &anthropic(), 0);
    let first_grep = text_of(&first, "g1").to_string();
    assert_ne!(first_grep, output);

    // The model runs both again, and they age into range; the cache goes cold.
    view.extend(aged("g2", "grep", grep, &output, false, 0));
    view.extend(aged("b2", "bash", make, &output, false, 4));
    send(&view, &mut state, &mut sink, &anthropic(), 1);
    let later = send(&view, &mut state, &mut sink, &anthropic(), 30);

    assert_eq!(text_of(&later, "g1"), first_grep);
    assert_eq!(text_of(&later, "g2"), output);
    // Re-running a command is ordinary, not a sign the stub cut too deep.
    assert_ne!(text_of(&later, "b2"), output);
}

#[test]
fn a_result_whose_original_cannot_be_saved_goes_out_whole() {
    let output = big(4000);
    let view = aged("b1", "bash", json!({"command": "make"}), &output, false, 2);
    let mut state = ReductionState::default();
    let mut broken = MemSink {
        broken: true,
        ..MemSink::default()
    };

    assert_eq!(send(&view, &mut state, &mut broken, &anthropic(), 0), view);
    // Not frozen: the next cold request tries again.
    let mut sink = MemSink::default();
    assert_ne!(
        text_of(&send(&view, &mut state, &mut sink, &anthropic(), 30), "b1"),
        output
    );
}

#[test]
fn stats_count_the_stubs_a_request_carries_and_what_they_saved() {
    let mut view = aged(
        "b1",
        "bash",
        json!({"command": "make"}),
        &big(4000),
        false,
        2,
    );
    view.extend(aged(
        "b2",
        "bash",
        json!({"command": "test"}),
        &big(8000),
        false,
        2,
    ));
    let mut state = ReductionState::default();
    let mut sent = view.clone();

    let stats = reduce(
        &mut sent,
        &mut state,
        &RequestReduction {
            cwd: Path::new("/w"),
            now: at(0),
            identity: &anthropic(),
        },
        &mut MemSink::default(),
    );

    let saved = |id: &str, original: usize| {
        (original as u64).div_ceil(4) - estimate_text_tokens(text_of(&sent, id))
    };
    assert_eq!(
        stats,
        ReductionStats {
            stubbed: 2,
            saved_tokens: saved("b1", 4000) + saved("b2", 8000),
        }
    );
    let mut replayed = view;
    assert_eq!(apply_frozen(&mut replayed, &state), stats);
    assert_eq!(replayed, sent);
}
