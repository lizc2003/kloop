//! Repairing the JSON a model *meant* to write for a tool call.
//!
//! Every rail hands tool arguments over as a string the model generated token
//! by token, and models get it wrong at a low but real rate — a value left
//! unquoted, a raw newline inside a string, a trailing comma. The whole turn
//! used to die on it, which costs a full sampling round to recover something
//! the model already said correctly everywhere else.
//!
//! Repair is deliberately a **whitelist**: each rule rewrites one mistake with
//! one unambiguous reading, anything else gives up. The rewritten text is
//! still parsed by `serde_json` — this module never decides that something is
//! valid, only that it knows what the model meant. That matters because these
//! arguments become shell commands: a repair that guesses is a repair that can
//! run a command the model never wrote.

use serde_json::Value;

/// Structural characters a bare (unquoted) value may not contain. Their
/// presence means the text is not one run of plain prose — it could be a
/// truncated object, a second field, or a quoted fragment — and a guess about
/// where the value ends is exactly the guess this module refuses to make.
const FORBIDDEN_IN_BARE_VALUE: &[char] = &['"', '{', '}', '[', ']', ',', ':', '\\'];

/// Rewrite the three mistakes below, then let `serde_json` rule on the result.
/// `None` means "not one of these", never "close enough".
///
/// 1. an unquoted string value (`"description": 查看提交` → `"查看提交"`)
/// 2. a raw control character inside a string (a real newline in a heredoc)
/// 3. a trailing comma before `}` or `]`
pub(crate) fn repair(raw: &str) -> Option<Value> {
    let mut out = String::with_capacity(raw.len() + 16);
    let mut chars = raw.char_indices().peekable();
    let mut in_string = false;
    let mut escaped = false;
    let mut repaired = false;

    while let Some((index, ch)) = chars.next() {
        if in_string {
            if escaped {
                escaped = false;
                out.push(ch);
                continue;
            }
            match ch {
                '\\' => {
                    escaped = true;
                    out.push(ch);
                }
                '"' => {
                    in_string = false;
                    out.push(ch);
                }
                // Rule 2: a literal control character is illegal inside a JSON
                // string, and a model writing a multi-line command is the way
                // it gets there. Escaping it preserves the text exactly.
                '\n' | '\r' | '\t' => {
                    repaired = true;
                    out.push_str(match ch {
                        '\n' => "\\n",
                        '\r' => "\\r",
                        _ => "\\t",
                    });
                }
                _ => out.push(ch),
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            ':' => {
                out.push(ch);
                // Rule 1: what follows a key must start a value. If it does
                // not, read the run of text up to this object's next `,` or
                // `}` and quote it — but only when that run is unambiguous.
                let mut lookahead = chars.clone();
                let mut value_start = None;
                while let Some((next_index, next_ch)) = lookahead.peek().copied() {
                    if next_ch.is_whitespace() {
                        lookahead.next();
                        continue;
                    }
                    value_start = Some((next_index, next_ch));
                    break;
                }
                let Some((start, first)) = value_start else {
                    continue;
                };
                if starts_a_value(first) && literal_at(&raw[start..]).is_some() {
                    continue;
                }
                let end = raw[start..]
                    .find([',', '}'])
                    .map(|offset| start + offset)
                    .unwrap_or(raw.len());
                let bare = raw[start..end].trim_end();
                if bare.is_empty() || bare.contains(FORBIDDEN_IN_BARE_VALUE) {
                    return None;
                }
                // Whitespace between `:` and the value was consumed by the
                // lookahead, not by `out`; re-emit one space so the output
                // stays readable when a test prints it.
                out.push(' ');
                out.push_str(&serde_json::to_string(bare).ok()?);
                repaired = true;
                while let Some((next_index, _)) = chars.peek().copied() {
                    if next_index >= end {
                        break;
                    }
                    chars.next();
                }
            }
            ',' => {
                // Rule 3: a comma whose next non-space character closes the
                // container has nothing to separate.
                let rest = raw[index + ch.len_utf8()..].trim_start();
                if rest.starts_with('}') || rest.starts_with(']') {
                    repaired = true;
                    continue;
                }
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    if !repaired || in_string {
        return None;
    }
    serde_json::from_str(&out).ok()
}

/// Whether this character could begin a JSON value at all.
fn starts_a_value(ch: char) -> bool {
    matches!(ch, '"' | '{' | '[' | '-' | 't' | 'f' | 'n') || ch.is_ascii_digit()
}

/// The length of a genuine JSON literal at the head of `text`, if there is one.
/// `t`/`f`/`n` also begin ordinary words, so "starts like a value" is not
/// enough: `"description": true` is already valid, `"description": today` is a
/// bare string that only looks like one.
fn literal_at(text: &str) -> Option<usize> {
    for literal in ["true", "false", "null"] {
        if let Some(rest) = text.strip_prefix(literal)
            && rest
                .chars()
                .next()
                .is_none_or(|ch| ch.is_whitespace() || matches!(ch, ',' | '}' | ']'))
        {
            return Some(literal.len());
        }
    }
    if text.starts_with(['"', '{', '[']) {
        return Some(1);
    }
    let number: String = text
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || matches!(ch, '-' | '+' | '.' | 'e' | 'E'))
        .collect();
    (!number.is_empty() && number.parse::<f64>().is_ok()).then_some(number.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The real failure that started this: glm-5.3-flash left the second value
    /// unquoted, 96 columns in, and killed the turn.
    #[test]
    fn unquoted_value_is_quoted() {
        let raw = r#"{
 "command": "git -C <repo> show a8e6543d --stat", "description": 查看提交概要与文件列表}"#;
        assert_eq!(
            repair(raw),
            Some(json!({
                "command": "git -C <repo> show a8e6543d --stat",
                "description": "查看提交概要与文件列表",
            }))
        );
    }

    #[test]
    fn unquoted_value_before_another_field_is_quoted() {
        assert_eq!(
            repair(r#"{"description": 查看提交, "command": "ls"}"#),
            Some(json!({"description": "查看提交", "command": "ls"}))
        );
    }

    /// A bare run containing structure could end anywhere; guessing would hand
    /// a truncated command to the shell.
    #[test]
    fn ambiguous_bare_values_are_refused() {
        assert_eq!(repair(r#"{"command": rm -rf {a,b}/tmp}"#), None);
        assert_eq!(repair(r#"{"command": echo "hi"}"#), None);
        assert_eq!(repair(r#"{"command": a: b}"#), None);
        assert_eq!(repair(r#"{"command": }"#), None);
    }

    #[test]
    fn literal_values_are_left_alone() {
        // Already valid: nothing to repair, so `repair` declines rather than
        // returning a value its caller would prefer to have parsed directly.
        assert_eq!(repair(r#"{"timeout": 30, "quiet": true}"#), None);
        // Valid literals next to a real mistake stay literals.
        assert_eq!(
            repair(r#"{"timeout": 30, "quiet": true, "note": ok,}"#),
            Some(json!({"timeout": 30, "quiet": true, "note": "ok"}))
        );
        assert_eq!(
            repair(r#"{"note": today, "quiet": false}"#),
            Some(json!({"note": "today", "quiet": false}))
        );
    }

    #[test]
    fn raw_control_characters_inside_strings_are_escaped() {
        let raw = "{\"command\": \"cat <<'EOF'\nhello\nEOF\"}";
        assert_eq!(
            repair(raw),
            Some(json!({"command": "cat <<'EOF'\nhello\nEOF"}))
        );
    }

    #[test]
    fn trailing_commas_are_dropped() {
        assert_eq!(
            repair(r#"{"command": "ls", "paths": ["a", "b",],}"#),
            Some(json!({"command": "ls", "paths": ["a", "b"]}))
        );
    }

    /// Repair rewrites; it never ratifies. Anything still malformed after the
    /// whitelist ran is refused by serde, not waved through.
    #[test]
    fn a_repair_that_does_not_parse_is_still_refused() {
        assert_eq!(repair(r#"{"command": "unterminated}"#), None);
        assert_eq!(repair(r#"{"command" "ls",}"#), None);
        assert_eq!(repair("not json at all"), None);
    }
}
