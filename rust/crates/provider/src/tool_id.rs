//! Tool-call ids as each rail will take them.
//!
//! The canonical history keeps every id exactly as the provider that minted it
//! sent it. That is right on the rail that minted it and can be wrong on
//! another: a chat-compat service's `functions.read_file:0` is a 400 on
//! Anthropic, and since that history does not go away, so is every later
//! request of the session. So an id is translated on its way out, in the
//! request projection — never in the history, the same boundary as
//! `cache_control`.
//!
//! Only an id the rail would refuse is rewritten; one it accepts goes out
//! verbatim, so a session that never changes rail sends the bytes it always
//! did. The rewrite is a pure function of the id: a `tool_use` and its
//! `tool_result` reach the same wire id without looking at each other, and
//! every request renders an old id the same way, which keeps the cached prefix
//! stable after a switch.
//!
//! An id repeated across messages is left alone. Services that number calls
//! per turn repeat them by design, and no rail we measured refuses it.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use sha2::Digest as _;
use sha2::Sha256;

use super::ProviderFailure;

/// Measured on Responses: an upstream refuses a 65-byte call_id and names 1–64
/// as the range. On Anthropic it is pi's figure and unmeasured — the routes we
/// reached took far longer ids — kept because a rewrite costs nothing.
const MAX_LEN: usize = 64;
const HASH_BYTES: usize = 8;
/// What is left of `MAX_LEN` for the readable part: `{prefix}_{16 hex}`.
const PREFIX_LEN: usize = MAX_LEN - 1 - 2 * HASH_BYTES;

/// A rail that constrains tool-call ids. Chat/completions is not one: the
/// services measured on it took any id, and those whose ids break the other
/// rails are chat services themselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolIdRule {
    /// `^[A-Za-z0-9_-]{1,64}$`. The character set is measured: an upstream
    /// refuses `.`, `:` and `|`.
    Anthropic,
    /// 1–64 bytes of anything; no upstream refused a character.
    Responses,
}

impl ToolIdRule {
    fn accepts(self, id: &str) -> bool {
        let fits = (1..=MAX_LEN).contains(&id.len());
        match self {
            Self::Anthropic => fits && id.chars().all(is_plain),
            Self::Responses => fits,
        }
    }

    fn rail(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Responses => "openai-responses",
        }
    }
}

fn is_plain(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}

/// `{prefix}_{hex}`, within both rules. The prefix is the id with anything
/// outside `[A-Za-z0-9_-]` turned into `_` and cut to 47, so a reader can still
/// tell where it came from; the hex is the head of sha256(id), which keeps
/// `a.b` and `a:b` apart once both prefixes read `a_b`.
fn rewrite(id: &str) -> String {
    let prefix: String = id
        .chars()
        .take(PREFIX_LEN)
        .map(|c| if is_plain(c) { c } else { '_' })
        .collect();
    let hash: String = Sha256::digest(id.as_bytes())[..HASH_BYTES]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{prefix}_{hash}")
}

/// One request's translation. It remembers which canonical id each wire id
/// stands for, so two different ids that would go out as one fail the request
/// before it is sent rather than pair a result with the wrong call. That takes
/// a 64-bit hash collision, or a history that already holds, verbatim, what
/// another of its ids rewrites to.
pub(crate) struct WireToolIds {
    rule: ToolIdRule,
    sources: HashMap<String, String>,
}

impl WireToolIds {
    pub(crate) fn new(rule: ToolIdRule) -> Self {
        Self {
            rule,
            sources: HashMap::new(),
        }
    }

    pub(crate) fn wire(&mut self, id: &str) -> Result<String, ProviderFailure> {
        let wire = if self.rule.accepts(id) {
            id.to_string()
        } else {
            rewrite(id)
        };
        match self.sources.entry(wire.clone()) {
            Entry::Occupied(source) if source.get() != id => {
                let rail = self.rule.rail();
                let earlier = source.get();
                Err(ProviderFailure::protocol(format!(
                    "{rail} tool call ids {earlier:?} and {id:?} would both go out as {wire:?}"
                )))
            }
            Entry::Occupied(_) => Ok(wire),
            Entry::Vacant(slot) => {
                slot.insert(id.to_string());
                Ok(wire)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(rule: ToolIdRule, id: &str) -> String {
        WireToolIds::new(rule).wire(id).unwrap()
    }

    #[test]
    fn only_ids_the_rail_refuses_are_rewritten() {
        let long = "c".repeat(70);
        let ids = [
            "toolu_01ABCdef",
            "call_abc123",
            "functions.read_file:0",
            long.as_str(),
        ];
        let rendered: Vec<(String, String)> = ids
            .iter()
            .map(|id| {
                (
                    wire(ToolIdRule::Anthropic, id),
                    wire(ToolIdRule::Responses, id),
                )
            })
            .collect();
        let long_rewritten = format!("{}_6fe5981a314622bb", "c".repeat(47));
        assert_eq!(
            rendered,
            vec![
                ("toolu_01ABCdef".into(), "toolu_01ABCdef".into()),
                ("call_abc123".into(), "call_abc123".into()),
                (
                    "functions_read_file_0_f9cc2c7822d54220".into(),
                    "functions.read_file:0".into(),
                ),
                (long_rewritten.clone(), long_rewritten),
            ]
        );
    }

    #[test]
    fn the_length_limit_is_exactly_64() {
        let at = "x".repeat(64);
        let over = "x".repeat(65);
        let rewritten = wire(ToolIdRule::Responses, &over);
        assert_eq!(
            (wire(ToolIdRule::Responses, &at), rewritten.len()),
            (at.clone(), 64)
        );
        assert_eq!(rewritten, format!("{}_9537c5fdf120482f", "x".repeat(47)));
    }

    /// Replacing characters alone would render both as `a_b`.
    #[test]
    fn ids_differing_only_in_replaced_characters_stay_apart() {
        let mut ids = WireToolIds::new(ToolIdRule::Anthropic);
        assert_eq!(
            (ids.wire("a.b").unwrap(), ids.wire("a:b").unwrap()),
            ("a_b_2e7336dc8eba87ef".into(), "a_b_6783a31eabf68ccc".into())
        );
    }

    #[test]
    fn a_repeated_id_goes_out_the_same_way_every_time() {
        let mut ids = WireToolIds::new(ToolIdRule::Anthropic);
        let first = ids.wire("functions.read_file:0").unwrap();
        assert_eq!(ids.wire("functions.read_file:0").unwrap(), first);
    }

    #[test]
    fn two_ids_that_would_go_out_as_one_fail_the_request() {
        let mut ids = WireToolIds::new(ToolIdRule::Anthropic);
        ids.wire("a.b").unwrap();
        assert_eq!(
            ids.wire("a_b_2e7336dc8eba87ef"),
            Err(ProviderFailure::protocol(
                "anthropic tool call ids \"a.b\" and \"a_b_2e7336dc8eba87ef\" would both go out as \"a_b_2e7336dc8eba87ef\""
            ))
        );
    }
}
