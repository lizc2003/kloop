use std::collections::HashSet;
use std::io;

use crate::provider_route::{
    ProvenanceMismatch, ReasoningShape, validate_provenance, validate_timeline,
};
use crate::request_reduction::FrozenStub;
use crate::usage::UsageLedger;
use kloop_protocol::{Message, ProviderRouteReceipt, ProviderRouteSource, Role};

use super::line::{LineMeta, RolloutLine};
use super::{SESSION_FORMAT_VERSION, SessionRuntime, SnapshotTerminal};

/// One parse of a session file: the effective replayed history plus the
/// chain state needed to keep appending, and where the intact content ends.
pub(super) struct ParsedSession {
    pub(super) items: Vec<Message>,
    pub(super) provider_usage: UsageLedger,
    pub(super) request_stubs: Vec<FrozenStub>,
    pub(super) runtime: Option<SessionRuntime>,
    pub(super) terminals: Vec<SnapshotTerminal>,
    pub(super) route_timeline: Vec<ProviderRouteReceipt>,
    /// Every call recorded as started, across the whole file: tool_use ids
    /// are unique, so a compaction in between does not have to void them.
    pub(super) started: HashSet<String>,
    pub(super) last_id: Option<String>,
    pub(super) max_seq: u64,
    /// Byte offset just past the last intact line; anything after is a
    /// malformed or unterminated tail (crash mid-append).
    pub(super) intact_end: usize,
}

/// Every intact line in file order, plus the byte offset just past the last
/// one; anything after that offset is a malformed or unterminated tail.
pub(super) fn intact_lines(raw: &[u8]) -> io::Result<(Vec<RolloutLine>, usize)> {
    let mut lines = Vec::new();
    let mut intact_end = 0;
    for line in raw.split_inclusive(|byte| *byte == b'\n') {
        if !line.ends_with(b"\n") {
            break;
        }
        let Ok(line) = std::str::from_utf8(line) else {
            break;
        };
        let content = line.trim();
        if !content.is_empty() {
            let value: serde_json::Value = match serde_json::from_str(content) {
                Ok(value) => value,
                Err(_) => break,
            };
            let parsed = serde_json::from_value(value).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid complete session line: {error}"),
                )
            })?;
            lines.push(parsed);
        }
        intact_end += line.len();
    }
    Ok((lines, intact_end))
}

fn checked_seq_of(meta: &LineMeta) -> io::Result<u64> {
    meta.id
        .rsplit_once('#')
        .and_then(|(_, seq)| seq.parse().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid session line id '{}'", meta.id),
            )
        })
}

pub(super) fn seq_of(meta: &LineMeta) -> u64 {
    checked_seq_of(meta).unwrap_or(0)
}

/// A file says which format it is written in before it says anything else.
/// An unknown version is refused here rather than parsed under this kloop's
/// meaning of the fields, and the line may not appear anywhere but first —
/// one file is written in one format.
fn validate_format(lines: &[RolloutLine]) -> io::Result<()> {
    let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidData, message);
    let Some(first) = lines.first() else {
        return Ok(());
    };
    let Some(version) = first.meta().format_version else {
        return Err(invalid(
            "session file does not open with its format version".into(),
        ));
    };
    if version != SESSION_FORMAT_VERSION {
        return Err(invalid(format!(
            "unsupported session format version {version} (this kloop reads {SESSION_FORMAT_VERSION})"
        )));
    }
    if lines[1..]
        .iter()
        .any(|line| line.meta().format_version.is_some())
    {
        return Err(invalid(
            "session format version belongs on the first line only".into(),
        ));
    }
    Ok(())
}

fn validate_envelope(lines: &[RolloutLine]) -> io::Result<()> {
    let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidData, message);
    let mut previous: Option<(&str, u64)> = None;
    for line in lines {
        let meta = line.meta();
        let seq = checked_seq_of(meta)?;
        if let Some((previous_id, previous_seq)) = previous {
            if previous_seq
                .checked_add(1)
                .is_none_or(|expected| seq != expected)
            {
                return Err(invalid(
                    "session line id sequences must strictly increase by one".into(),
                ));
            }
            if meta.parent.as_deref() != Some(previous_id) {
                return Err(invalid(
                    "session line parent must reference the preceding line".into(),
                ));
            }
        } else if let Some(parent) = meta.parent.as_deref() {
            let current_prefix = meta.id.rsplit_once('#').map(|(prefix, _)| prefix);
            let valid_fork_parent = parent.rsplit_once('#').is_some_and(|(prefix, seq)| {
                !prefix.is_empty()
                    && prefix != current_prefix.unwrap_or_default()
                    && seq.parse::<u64>().is_ok()
            });
            if meta.subagent_of.is_some() || !valid_fork_parent {
                return Err(invalid(
                    "the first session line parent must be a valid cross-file fork origin".into(),
                ));
            }
        }
        previous = Some((&meta.id, seq));
    }
    Ok(())
}

fn validate_provider_routes(lines: &[RolloutLine]) -> io::Result<()> {
    let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidData, message);
    let mut routes: Vec<ProviderRouteReceipt> = Vec::new();
    for line in lines {
        let (meta, receipt, source_matches_line) = match line {
            RolloutLine::ProviderRouteInitial { meta, receipt } => (
                meta,
                receipt,
                receipt.source == ProviderRouteSource::Initial,
            ),
            // Both change kinds ride the same line; which one it was lives in
            // the receipt's own `source`, and `validate_timeline` decides
            // whether that value is legal at this revision.
            RolloutLine::ProviderRouteChanged { meta, receipt } => (
                meta,
                receipt,
                receipt.source != ProviderRouteSource::Initial,
            ),
            _ => continue,
        };
        let boundary = checked_seq_of(meta)?;
        if receipt.route_boundary != boundary || !source_matches_line {
            return Err(invalid(
                "provider route receipt boundary/source does not match its line".into(),
            ));
        }
        routes.push(receipt.clone());
    }
    validate_timeline(&routes).map_err(|_| invalid("provider route timeline is invalid".into()))?;

    let first_message = lines.iter().find_map(|line| match line {
        RolloutLine::Message { meta, .. } => Some(seq_of(meta)),
        _ => None,
    });
    if first_message.is_some_and(|boundary| routes[0].route_boundary >= boundary) {
        return Err(invalid(
            "initial provider route must precede the first history message".into(),
        ));
    }

    for line in lines {
        if let RolloutLine::ProviderUsage { meta, record } = line {
            let boundary = checked_seq_of(meta)?;
            let route = routes
                .iter()
                .rev()
                .find(|route| route.route_boundary < boundary)
                .ok_or_else(|| invalid("provider usage precedes the initial route".into()))?;
            let model_matches = record.model == route.model;
            if record.route_revision != route.route_revision
                || record.provider_id != route.provider_id
                || record.api_family != route.api_family
                || !model_matches
            {
                return Err(invalid(
                    "provider usage identity does not match the active route".into(),
                ));
            }
        }
    }

    let mut original_origins = HashSet::new();
    for line in lines {
        if let RolloutLine::Message { meta, message } = line {
            let boundary = checked_seq_of(meta)?;
            validate_provider_message(message, boundary, &routes, true)?;
            if let Some(source) = &message.provider_provenance {
                original_origins.insert((source.route_boundary, source.route_revision));
            }
        }
    }
    for line in lines {
        let replacement = match line {
            RolloutLine::Compacted { replacement, .. }
            | RolloutLine::Repaired { replacement, .. } => replacement,
            _ => continue,
        };
        for message in replacement {
            validate_provider_message(message, 0, &routes, false)?;
            if let Some(source) = &message.provider_provenance
                && !original_origins.contains(&(source.route_boundary, source.route_revision))
            {
                return Err(invalid(
                    "replacement history contains provider provenance with no original message"
                        .into(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_provider_message(
    message: &Message,
    line_boundary: u64,
    routes: &[ProviderRouteReceipt],
    original_line: bool,
) -> io::Result<()> {
    let invalid = |message: &'static str| io::Error::new(io::ErrorKind::InvalidData, message);
    let Some(source) = message.provider_provenance.as_ref() else {
        return if message.has_reasoning() {
            Err(invalid(
                "reasoning history is missing provider route provenance",
            ))
        } else {
            Ok(())
        };
    };
    if message.role != Role::Assistant {
        return Err(invalid(
            "only provider assistant messages may carry provider provenance",
        ));
    }
    validate_provenance(
        source,
        routes,
        ReasoningShape::of(message),
        original_line.then_some(line_boundary),
    )
    .map(|_| ())
    .map_err(|mismatch| {
        invalid(match mismatch {
            ProvenanceMismatch::UnknownRevision => {
                "provider provenance references an unknown route revision"
            }
            ProvenanceMismatch::OriginOutsideInterval => {
                "provider provenance origin lies outside its route interval"
            }
            ProvenanceMismatch::OriginNotOnItsLine => {
                "provider provenance origin does not match its message line boundary"
            }
            ProvenanceMismatch::IdentityMismatch => {
                "provider provenance identity does not match its producing route"
            }
            ProvenanceMismatch::BlockShapeMismatch => {
                "provider reasoning block shape does not match its producing API family"
            }
        })
    })
}

pub(super) fn parse_session(raw: &[u8]) -> io::Result<ParsedSession> {
    let (lines, intact_end) = intact_lines(raw)?;
    validate_format(&lines)?;
    validate_provider_routes(&lines)?;
    validate_envelope(&lines)?;
    let mut parsed = ParsedSession {
        items: Vec::new(),
        provider_usage: UsageLedger::default(),
        request_stubs: Vec::new(),
        runtime: None,
        terminals: Vec::new(),
        route_timeline: Vec::new(),
        started: HashSet::new(),
        last_id: None,
        max_seq: 0,
        intact_end,
    };
    for line in lines {
        let meta = match line {
            RolloutLine::Session { meta, runtime } => {
                parsed.runtime = Some(runtime);
                meta
            }
            RolloutLine::ProviderRouteInitial { meta, receipt } => {
                parsed.route_timeline.push(receipt);
                meta
            }
            RolloutLine::ProviderRouteChanged { meta, receipt } => {
                parsed.route_timeline.push(receipt);
                meta
            }
            RolloutLine::Message { meta, message } => {
                parsed.items.push(message);
                meta
            }
            RolloutLine::ProviderUsage { meta, record } => {
                parsed.provider_usage.push(record);
                meta
            }
            RolloutLine::Compacted { meta, replacement } => {
                parsed.items = replacement;
                parsed.terminals.clear();
                parsed.request_stubs.clear();
                meta
            }
            RolloutLine::RequestStub { meta, stub } => {
                parsed.request_stubs.push(stub);
                meta
            }
            RolloutLine::ToolStarted { meta, tool_use_id } => {
                parsed.started.insert(tool_use_id);
                meta
            }
            RolloutLine::TurnTerminal { meta, terminal } => {
                parsed.terminals.push(SnapshotTerminal {
                    after_message: parsed.items.len(),
                    status: terminal.status,
                    error: terminal.error,
                });
                meta
            }
            RolloutLine::Repaired {
                meta,
                kind: _,
                replacement,
                terminals,
                stats: _,
            } => {
                parsed.items = replacement;
                parsed.terminals = terminals;
                meta
            }
        };
        let seq = checked_seq_of(&meta)?;
        parsed.max_seq = parsed.max_seq.max(seq);
        parsed.last_id = Some(meta.id);
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use super::super::*;
    use super::*;
    use kloop_protocol::ContentBlock;
    use serde_json::{Value, json};

    /// One way a file can misstate its format: a name for the case, the
    /// corruption to apply, and the refusal it has to produce.
    type FormatCase = (&'static str, fn(&mut Vec<Value>), &'static str);

    #[test]
    fn a_file_whose_format_version_is_missing_wrong_or_misplaced_is_refused() {
        let path = temp_file("format-version-source");
        let mut rollout = Rollout::new(path.clone());
        rollout.append_message(&Message::user_text("one")).unwrap();
        rollout.append_message(&Message::user_text("two")).unwrap();
        drop(rollout);
        let original = raw_lines(&path);

        let cases: [FormatCase; 4] = [
            (
                "missing",
                |lines: &mut Vec<Value>| {
                    lines[0].as_object_mut().unwrap().remove("format_version");
                },
                "does not open with its format version",
            ),
            (
                "unknown-version",
                |lines: &mut Vec<Value>| {
                    lines[0]["format_version"] = json!(SESSION_FORMAT_VERSION + 1);
                },
                "unsupported session format version",
            ),
            (
                "misplaced",
                |lines: &mut Vec<Value>| {
                    lines[1]["format_version"] = json!(SESSION_FORMAT_VERSION);
                },
                "belongs on the first line only",
            ),
            (
                "on-a-later-line-instead",
                |lines: &mut Vec<Value>| {
                    lines[0].as_object_mut().unwrap().remove("format_version");
                    lines[1]["format_version"] = json!(SESSION_FORMAT_VERSION);
                },
                "does not open with its format version",
            ),
        ];
        for (tag, mutate, expected) in cases {
            let case_path = temp_file(tag);
            let mut lines = original.clone();
            mutate(&mut lines);
            let raw = lines
                .iter()
                .map(|line| format!("{line}\n"))
                .collect::<String>();
            std::fs::create_dir_all(case_path.parent().unwrap()).unwrap();
            std::fs::write(&case_path, raw).unwrap();
            let before = std::fs::read(&case_path).unwrap();
            let Err(error) = inspect_session(&case_path) else {
                panic!("{tag}: a file that misstates its format must be refused");
            };
            assert!(error.to_string().contains(expected), "{tag}: {error}");
            // A refused file is never rewritten on the way out.
            assert_eq!(std::fs::read(&case_path).unwrap(), before, "{tag}");
            cleanup(&case_path);
        }
        cleanup(&path);
    }

    #[test]
    fn unknown_fields_are_ignored_forward_compat() {
        let path = temp_file("compat");
        let mut rollout = Rollout::new(path.clone());
        rollout.append_message(&Message::user_text("old")).unwrap();
        // A line written by a future version with an extra field.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            r#"{{"type":"message","id":"session#3","parent":"session#2","ts":1,"future_field":42,"role":"user","content":[{{"type":"text","text":"new"}}]}}"#
        )
        .unwrap();

        assert_eq!(
            load_session(&path).unwrap(),
            vec![Message::user_text("old"), Message::user_text("new")]
        );
        cleanup(&path);
    }

    #[test]
    fn compacted_marker_preserves_provider_usage_ledger() {
        let path = temp_file("compacted-usage");
        let mut rollout = Rollout::new(path.clone());
        rollout
            .append_provider_usage(&usage_record("before", 10))
            .unwrap();
        rollout
            .append_compacted(&[Message::user_text("[summary]")])
            .unwrap();
        rollout
            .append_provider_usage(&usage_record("after", 20))
            .unwrap();

        drop(rollout);
        let resumed = resume_session(&path).unwrap();
        assert_eq!(resumed.messages, vec![Message::user_text("[summary]")]);
        assert_eq!(
            resumed.provider_usage.records(),
            &[usage_record("before", 10), usage_record("after", 20)]
        );
        cleanup(&path);
    }

    #[test]
    fn compacted_marker_replaces_prior_lines_on_replay() {
        let path = temp_file("compacted");
        let mut rollout = Rollout::new(path.clone());
        rollout
            .append_message(&Message::user_text("old 1"))
            .unwrap();
        rollout
            .append_message(&Message::user_text("old 2"))
            .unwrap();
        let replacement = vec![
            Message::user_text("[summary]"),
            Message::user_text("kept tail"),
        ];
        rollout.append_compacted(&replacement).unwrap();
        let after = Message::user_text("after compaction");
        rollout.append_message(&after).unwrap();

        let mut expected = replacement;
        expected.push(after);
        assert_eq!(load_session(&path).unwrap(), expected);
        cleanup(&path);
    }

    #[test]
    fn malformed_tail_line_truncates_instead_of_failing() {
        let path = temp_file("corrupt");
        let mut rollout = Rollout::new(path.clone());
        let good = Message::user_text("intact");
        rollout.append_message(&good).unwrap();
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"type\":\"message\",\"role\":\"us"); // crash mid-append
        std::fs::write(&path, &raw).unwrap();

        // Read-only load ignores the tail but leaves the file alone.
        assert_eq!(load_session(&path).unwrap(), vec![good.clone()]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        cleanup(&path);
    }

    #[test]
    fn runtime_and_terminals_are_snapshot_only() {
        let path = temp_file("snapshot");
        let runtime = SessionRuntime {
            cwd: "/tmp/project".into(),
        };
        let mut rollout = Rollout::new_with_runtime(path.clone(), runtime.clone()).unwrap();
        rollout
            .append_message(&Message::user_text("hello"))
            .unwrap();
        rollout
            .append_message(&Message::assistant(vec![ContentBlock::Text {
                text: "half".into(),
            }]))
            .unwrap();
        rollout
            .append_turn_terminal(&TurnTerminal {
                status: "error".into(),
                error: Some("stream dropped".into()),
                typed_error: None,
            })
            .unwrap();

        assert_eq!(
            load_session(&path).unwrap(),
            vec![
                Message::user_text("hello"),
                Message::assistant(vec![ContentBlock::Text {
                    text: "half".into(),
                }]),
            ],
            "runtime and terminal records must never enter provider history"
        );
        assert_eq!(
            load_session_snapshot(&path).unwrap(),
            SessionSnapshot {
                messages: vec![
                    Message::user_text("hello"),
                    Message::assistant(vec![ContentBlock::Text {
                        text: "half".into(),
                    }]),
                ],
                runtime: Some(runtime),
                terminals: vec![SnapshotTerminal {
                    after_message: 2,
                    status: "error".into(),
                    error: Some("stream dropped".into()),
                }],
                provider_routes: vec![fixture_route(2)],
            }
        );
        cleanup(&path);
    }

    #[test]
    fn chat_text_provenance_survives_read_without_reasoning() {
        let path = temp_file("chat-text-provenance");
        let provider = kloop_provider::Provider::OpenAiCompat {
            cred: kloop_provider::Credential::bearer("test-key"),
            base: "https://chat.invalid".into(),
        };
        let (catalog, _) = crate::provider_route::ProviderCatalog::from_provider(
            "chat",
            provider,
            "chat-model",
            vec!["chat-model".into()],
        )
        .unwrap();
        let route = catalog.initial_route("chat", Some("chat-model")).unwrap();
        let mut rollout = Rollout::new_with_initial_route(path.clone(), &route).unwrap();
        let assistant = Message::assistant_from_provider(
            vec![ContentBlock::Text {
                text: "chat answer".into(),
            }],
            route.primary_attempt().provenance(2),
        );
        rollout.append_message(&assistant).unwrap();
        assert_eq!(
            load_session_snapshot(&path).unwrap().messages,
            vec![assistant]
        );
        cleanup(&path);
    }

    /// And the same route with reasoning in it: a chat model that streams
    /// `reasoning_content` writes signature-less thinking under chat
    /// provenance, so refusing that shape on read would make the session it
    /// wrote unopenable. Stripping reasoning is the request projection's job,
    /// not the validator's.
    #[test]
    fn chat_reasoning_provenance_survives_read() {
        let path = temp_file("chat-reasoning-provenance");
        let provider = kloop_provider::Provider::OpenAiCompat {
            cred: kloop_provider::Credential::bearer("test-key"),
            base: "https://chat.invalid".into(),
        };
        let (catalog, _) = crate::provider_route::ProviderCatalog::from_provider(
            "chat",
            provider,
            "chat-model",
            vec!["chat-model".into()],
        )
        .unwrap();
        let route = catalog.initial_route("chat", Some("chat-model")).unwrap();
        let mut rollout = Rollout::new_with_initial_route(path.clone(), &route).unwrap();
        let assistant = Message::assistant_from_provider(
            vec![
                ContentBlock::Thinking {
                    thinking: "streamed as reasoning_content".into(),
                    signature: String::new(),
                },
                ContentBlock::Text {
                    text: "chat answer".into(),
                },
            ],
            route.primary_attempt().provenance(2),
        );
        rollout.append_message(&assistant).unwrap();
        assert_eq!(
            load_session_snapshot(&path).unwrap().messages,
            vec![assistant]
        );
        cleanup(&path);
    }

    #[test]
    fn legacy_and_semantically_invalid_route_lines_fail_without_repair() {
        let legacy_path = temp_file("legacy-no-route");
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::write(
            &legacy_path,
            b"{\"type\":\"message\",\"id\":\"session#1\",\"ts\":1,\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"old\"}]}\n",
        )
        .unwrap();
        let legacy_before = std::fs::read(&legacy_path).unwrap();
        assert!(inspect_session(&legacy_path).is_err());
        assert_eq!(std::fs::read(&legacy_path).unwrap(), legacy_before);

        let invalid_path = temp_file("invalid-route-line");
        let mut rollout = Rollout::new(invalid_path.clone());
        rollout.append_message(&Message::user_text("new")).unwrap();
        drop(rollout);
        let mut lines = raw_lines(&invalid_path);
        let mut duplicate = lines[0].clone();
        duplicate["type"] = json!("provider_route_changed");
        duplicate["id"] = json!("session#2");
        duplicate["parent"] = json!("session#1");
        duplicate["route_boundary"] = json!(2);
        duplicate["source"] = json!("explicit_switch");
        lines.insert(1, duplicate);
        let raw = lines
            .iter()
            .map(|line| format!("{}\n", serde_json::to_string(line).unwrap()))
            .collect::<String>();
        std::fs::write(&invalid_path, raw.as_bytes()).unwrap();
        let before = std::fs::read(&invalid_path).unwrap();
        assert!(inspect_session(&invalid_path).is_err());
        assert_eq!(std::fs::read(&invalid_path).unwrap(), before);
        cleanup(&legacy_path);
        cleanup(&invalid_path);
    }

    #[test]
    fn malformed_envelopes_and_route_timelines_fail_closed_without_repair() {
        let path = temp_file("malformed-envelope");
        let mut rollout = Rollout::new(path.clone());
        rollout.append_message(&Message::user_text("one")).unwrap();
        rollout.append_message(&Message::user_text("two")).unwrap();
        drop(rollout);

        let original = raw_lines(&path);
        let cases = [
            ("duplicate-id", "id", json!("session#2")),
            ("regressed-id", "id", json!("session#1")),
            ("bad-parent", "parent", json!("other#1")),
        ];
        for (tag, field, value) in cases {
            let case_path = temp_file(tag);
            let mut lines = original.clone();
            lines[2][field] = value;
            let raw = lines
                .iter()
                .map(|line| format!("{}\n", serde_json::to_string(line).unwrap()))
                .collect::<String>();
            std::fs::create_dir_all(case_path.parent().unwrap()).unwrap();
            std::fs::write(&case_path, raw).unwrap();
            let before = std::fs::read(&case_path).unwrap();
            assert!(inspect_session(&case_path).is_err(), "{tag}");
            assert_eq!(std::fs::read(&case_path).unwrap(), before, "{tag}");
            cleanup(&case_path);
        }
        cleanup(&path);
    }

    #[test]
    fn duplicate_or_regressed_boundary_and_revision_gap_fail_closed_without_repair() {
        let path = temp_file("malformed-route-timeline");
        let mut rollout = Rollout::new(path.clone());
        rollout.append_message(&Message::user_text("one")).unwrap();
        let mut first_switch = fixture_route(3);
        first_switch.route_revision = 2;
        first_switch.source = ProviderRouteSource::ExplicitSwitch;
        rollout
            .append_line(RolloutLine::ProviderRouteChanged {
                meta: rollout.next_meta(),
                receipt: first_switch,
            })
            .unwrap();
        rollout.append_message(&Message::user_text("two")).unwrap();
        let mut second_switch = fixture_route(5);
        second_switch.route_revision = 3;
        second_switch.source = ProviderRouteSource::ExplicitSwitch;
        rollout
            .append_line(RolloutLine::ProviderRouteChanged {
                meta: rollout.next_meta(),
                receipt: second_switch,
            })
            .unwrap();
        rollout
            .append_message(&Message::user_text("three"))
            .unwrap();
        drop(rollout);

        let original = raw_lines(&path);
        let cases = [
            ("duplicate-boundary", "route_boundary", json!(3)),
            ("regressed-boundary", "route_boundary", json!(2)),
            ("revision-gap", "route_revision", json!(4)),
        ];
        for (tag, field, value) in cases {
            let case_path = temp_file(tag);
            let mut lines = original.clone();
            lines[4][field] = value;
            let raw = lines
                .iter()
                .map(|line| format!("{}\n", serde_json::to_string(line).unwrap()))
                .collect::<String>();
            std::fs::create_dir_all(case_path.parent().unwrap()).unwrap();
            std::fs::write(&case_path, raw).unwrap();
            let before = std::fs::read(&case_path).unwrap();
            assert!(inspect_session(&case_path).is_err(), "{tag}");
            assert_eq!(std::fs::read(&case_path).unwrap(), before, "{tag}");
            cleanup(&case_path);
        }
        cleanup(&path);
    }

    #[test]
    fn future_provider_revision_in_message_provenance_fails_closed() {
        let path = temp_file("future-route-provenance");
        let mut rollout = Rollout::new(path.clone());
        rollout
            .append_message(&Message::user_text("question"))
            .unwrap();
        let provenance = kloop_protocol::ProviderResponseProvenance {
            route_revision: 2,
            route_boundary: 3,
            provider_id: "test".into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            endpoint_fingerprint: kloop_provider::Provider::endpoint_fingerprint_for(
                kloop_protocol::ProviderApiFamily::Mock,
                "mock",
            ),
            model: "mock".into(),
        };
        rollout
            .append_message(&Message::assistant_from_provider(
                vec![ContentBlock::Thinking {
                    thinking: "summary".into(),
                    signature: "opaque".into(),
                }],
                provenance,
            ))
            .unwrap();
        assert!(inspect_session(&path).is_err());

        // A revision the timeline does have, pinned to a boundary it never
        // had, is refused just the same. The rewrite is asserted to have
        // matched: a renamed field would otherwise turn this into a second
        // run of the assertion above, silently and still green.
        let original = std::fs::read_to_string(&path).unwrap();
        let raw = original.replace(
            "\"route_revision\":2,\"route_boundary\":3",
            "\"route_revision\":1,\"route_boundary\":99",
        );
        assert_ne!(raw, original, "provenance rewrite matched nothing");
        std::fs::write(&path, raw).unwrap();
        assert!(inspect_session(&path).is_err());
        cleanup(&path);
    }
}
