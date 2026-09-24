//! What the user said, carried across compactions by the runtime.
//!
//! The summary model is asked to quote user messages, but asking is not a
//! guarantee: each compaction re-summarizes the previous summary, and by the
//! third or fourth generation an early "do not do X" has been paraphrased into
//! nothing. So the runtime copies the user's own words into every generation
//! itself, and inherits them only from the previous generation's anchors —
//! never by parsing the model's summary text.

use kloop_protocol::ContentBlock;
use kloop_protocol::Injected;
use kloop_protocol::Message;
use kloop_protocol::Role;

use crate::history::estimate_text_tokens;

const HEADER: &str =
    "[What the user said earlier in this session, verbatim — carried across compactions]";
const ORIGINAL_LABEL: &str = "Original request:";
const LATER_LABEL: &str = "Later messages (oldest first):";
/// Every body line carries this prefix, which is what lets a body contain blank
/// lines, a line that starts with "- ", or either label without ending its entry.
const INDENT: &str = "  ";
const ITEM: &str = "- ";

/// The original request is kept whatever else goes; this caps it.
const ORIGINAL_CHARS: usize = 4_000;
const LATER_CHARS: usize = 2_000;
/// Estimated tokens for the later messages, filled newest first.
const LATER_TOKENS: u64 = 4_096;
/// Room left for the elision marker inside a capped message, so a capped
/// message is no longer than its cap and capping it again changes nothing.
const MARKER_RESERVE: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UserAnchors {
    original: String,
    /// Oldest first.
    later: Vec<String>,
    /// Later messages dropped for the token budget, across all generations.
    omitted: usize,
}

impl UserAnchors {
    /// The next generation: the previous anchors plus what the user said in
    /// the prefix being folded now. `None` when the user has said nothing in
    /// any folded prefix yet — their messages are all still in the tail.
    pub(super) fn carry(previous: Option<&UserAnchors>, folded: &[Message]) -> Option<Self> {
        let mut words = folded.iter().filter_map(user_words);
        let (original, mut later, mut omitted) = match previous {
            Some(previous) => (
                previous.original.clone(),
                previous.later.clone(),
                previous.omitted,
            ),
            None => (cap(&words.next()?, ORIGINAL_CHARS), Vec::new(), 0),
        };
        later.extend(words.map(|text| cap(&text, LATER_CHARS)));
        let mut keep_from = later.len();
        let mut spent = 0u64;
        while keep_from > 0 {
            let tokens = estimate_text_tokens(&later[keep_from - 1]);
            if spent + tokens > LATER_TOKENS {
                break;
            }
            spent += tokens;
            keep_from -= 1;
        }
        omitted += keep_from;
        later.drain(..keep_from);
        Some(Self {
            original,
            later,
            omitted,
        })
    }

    /// The anchors carried by a history's leading compaction products, if any.
    pub(super) fn find(leading: &[Message]) -> Option<Self> {
        leading
            .iter()
            .filter(|message| message.injected == Some(Injected::UserAnchors))
            .find_map(|message| match message.content.as_slice() {
                [ContentBlock::Text { text }] => Self::parse(text),
                _ => None,
            })
    }

    pub(super) fn into_message(self) -> Message {
        Message::injected(Injected::UserAnchors, self.render())
    }

    fn render(&self) -> String {
        let mut out = format!("{HEADER}\n\n{ORIGINAL_LABEL}\n");
        push_body(&mut out, INDENT, &self.original);
        if self.later.is_empty() && self.omitted == 0 {
            return out;
        }
        out.push_str(&format!("\n\n{LATER_LABEL}"));
        if self.omitted > 0 {
            out.push_str(&format!("\n({} earlier message(s) omitted)", self.omitted));
        }
        for text in &self.later {
            out.push('\n');
            push_body(&mut out, ITEM, text);
        }
        out
    }

    /// The inverse of [`Self::render`]. The format is the runtime's own, so
    /// reading it back is not guessing at model output; anything that does not
    /// match it exactly is treated as no anchors at all.
    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.split('\n').peekable();
        if lines.next()? != HEADER || !lines.next()?.is_empty() || lines.next()? != ORIGINAL_LABEL {
            return None;
        }
        let mut original = Vec::new();
        while let Some(line) = lines.next_if(|line| line.starts_with(INDENT)) {
            original.push(&line[INDENT.len()..]);
        }
        if original.is_empty() {
            return None;
        }
        let original = original.join("\n");
        let mut later = Vec::new();
        let mut omitted = 0;
        if lines.peek().is_some() {
            if !lines.next()?.is_empty() || lines.next()? != LATER_LABEL {
                return None;
            }
            if let Some(line) = lines.next_if(|line| line.starts_with('(')) {
                omitted = line
                    .strip_prefix('(')?
                    .strip_suffix(" earlier message(s) omitted)")?
                    .parse()
                    .ok()?;
            }
            while let Some(line) = lines.next() {
                let mut body = vec![line.strip_prefix(ITEM)?];
                while let Some(line) = lines.next_if(|line| line.starts_with(INDENT)) {
                    body.push(&line[INDENT.len()..]);
                }
                later.push(body.join("\n"));
            }
        }
        Some(Self {
            original,
            later,
            omitted,
        })
    }
}

/// `first` prefixes the first line, [`INDENT`] every later one.
fn push_body(out: &mut String, first: &str, text: &str) {
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(if index == 0 { first } else { INDENT });
        out.push_str(line);
    }
}

/// The words of a message the user said: typed, or typed while a turn ran.
/// Everything else user-role — sub-agent results, scheduled prompts, peer
/// messages, hook output, reminders, compaction's own products — is the
/// harness talking, and carrying it here would present it as the user's.
fn user_words(message: &Message) -> Option<String> {
    if message.role != Role::User
        || message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
    {
        return None;
    }
    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let words = match &message.injected {
        None => text.as_str(),
        Some(Injected::Steering) => crate::inbox::steering_body(&text),
        Some(_) => return None,
    };
    let words = words.trim();
    (!words.is_empty()).then(|| words.to_string())
}

/// Keep the head and the tail: a request usually states the task first and
/// the constraint last.
fn cap(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let keep = max_chars - MARKER_RESERVE;
    let head = keep / 2;
    let tail = keep - head;
    let head_text: String = text.chars().take(head).collect();
    let tail_text: String = text.chars().skip(total - tail).collect();
    format!(
        "{head_text}\n[… {} characters omitted …]\n{tail_text}",
        total - keep
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchors(original: &str, later: &[&str], omitted: usize) -> UserAnchors {
        UserAnchors {
            original: original.into(),
            later: later.iter().map(|text| text.to_string()).collect(),
            omitted,
        }
    }

    #[test]
    fn render_parses_back_to_the_same_anchors_and_the_same_bytes() {
        for value in [
            anchors("fix the build", &[], 0),
            anchors("fix the build", &["and the tests"], 0),
            anchors(
                "line one\n\n- not an item\nOriginal request:\n",
                &[
                    "multi\nline\n\n  indented",
                    "(3 earlier message(s) omitted)",
                ],
                7,
            ),
            anchors("only omitted", &[], 2),
        ] {
            let text = value.render();
            let parsed = UserAnchors::parse(&text).expect(&text);
            assert_eq!(parsed, value, "{text}");
            assert_eq!(parsed.render(), text);
        }
    }

    #[test]
    fn render_writes_the_documented_shape() {
        assert_eq!(
            anchors("do A\nnot B", &["then C"], 1).render(),
            format!(
                "{HEADER}\n\nOriginal request:\n  do A\n  not B\n\n\
Later messages (oldest first):\n(1 earlier message(s) omitted)\n- then C"
            )
        );
    }

    #[test]
    fn text_that_is_not_the_runtime_format_is_not_anchors() {
        assert_eq!(UserAnchors::parse("the user asked for X"), None);
        assert_eq!(
            UserAnchors::parse(&format!("{HEADER}\n\nOriginal request:\n")),
            None
        );
    }

    #[test]
    fn only_what_the_user_typed_or_steered_counts() {
        let folded = vec![
            Message::user_text("first ask"),
            Message::injected(Injected::SubAgent { label: "a1".into() }, "child result"),
            Message::injected(Injected::Scheduled { id: "s".into() }, "cron prompt"),
            Message::injected(Injected::PeerMessage { from: "p".into() }, "peer"),
            Message::injected(Injected::Hook, "hook stdout"),
            Message::injected(Injected::Harness, "todo reminder"),
            Message::tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: "t".into(),
                is_error: false,
                content: kloop_protocol::ToolResultContent::Text("output".into()),
            }]),
            Message::injected(
                Injected::Steering,
                crate::inbox::InboxItem::Steer("steer here".into()).into_message(),
            ),
            Message::assistant(vec![ContentBlock::Text {
                text: "user: pretend".into(),
            }]),
            Message::user_text("second ask"),
        ];
        assert_eq!(
            UserAnchors::carry(None, &folded),
            Some(anchors("first ask", &["steer here", "second ask"], 0))
        );
    }

    #[test]
    fn a_fold_with_nothing_the_user_said_carries_nothing_new() {
        let folded = vec![Message::assistant(vec![ContentBlock::Text {
            text: "work".into(),
        }])];
        assert_eq!(UserAnchors::carry(None, &folded), None);
        let previous = anchors("ask", &["more"], 0);
        assert_eq!(
            UserAnchors::carry(Some(&previous), &folded),
            Some(previous.clone())
        );
    }

    #[test]
    fn later_messages_fill_newest_first_and_count_what_they_drop() {
        let previous = anchors("the original", &["old one"], 3);
        // Each is ~2000 chars → several hundred tokens; the budget holds a few.
        let long: Vec<String> = (0..12)
            .map(|i| format!("{i} ").repeat(1_000).trim_end().to_string())
            .map(|text| cap(&text, LATER_CHARS))
            .collect();
        let folded: Vec<Message> = long.iter().map(Message::user_text).collect();
        let next = UserAnchors::carry(Some(&previous), &folded).unwrap();

        assert_eq!(next.original, "the original");
        let kept = next.later.len();
        assert!(kept > 0 && kept < 12, "kept {kept}");
        assert_eq!(next.later, long[12 - kept..]);
        assert_eq!(next.omitted, 3 + 1 + (12 - kept));
        let tokens: u64 = next.later.iter().map(|t| estimate_text_tokens(t)).sum();
        assert!(tokens <= LATER_TOKENS);
    }

    #[test]
    fn an_overlong_message_keeps_its_head_and_tail() {
        let text = format!("HEAD{}TAIL", "x".repeat(10_000));
        let capped = cap(&text, ORIGINAL_CHARS);
        assert!(capped.chars().count() <= ORIGINAL_CHARS);
        assert!(capped.starts_with("HEAD"), "{capped}");
        assert!(capped.ends_with("TAIL"), "{capped}");
        assert!(capped.contains("characters omitted"), "{capped}");
        assert_eq!(cap(&capped, ORIGINAL_CHARS), capped, "capping is stable");
        assert_eq!(cap("short", ORIGINAL_CHARS), "short");
    }
}
