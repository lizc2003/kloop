//! One reading of a model-supplied tool input.
//!
//! Every tool used to carry its own copy of the same few helpers, and they had
//! already drifted apart: three spellings of "unknown field", two spellings of
//! "must be a string", explicit `null` meant "omitted" in some tools and
//! "wrong type" in others, and a key echoed back into an error was bounded by
//! bytes here and by characters there. A model sees those differences as noise
//! in its feedback loop, so the contract lives here once.

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use serde_json::Map;
use serde_json::Value;

/// How much of a model-supplied name an error echoes back.
pub(super) const MAX_DIAGNOSTIC_CHARS: usize = 80;

pub(crate) fn is_unicode_line_separator(ch: char) -> bool {
    matches!(ch, '\u{2028}' | '\u{2029}')
}

/// A display-safe, bounded copy of a model-supplied value.
///
/// Control characters and Unicode line separators become spaces so an error
/// stays one line, and the bound is counted in characters so a multi-byte
/// name is not cut mid-symbol (a `…` marks the cut).
pub(crate) fn bounded_diagnostic(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    let mut truncated = false;
    for (used, ch) in value.chars().enumerate() {
        if used == max_chars {
            truncated = true;
            break;
        }
        let ch = if ch.is_control() || is_unicode_line_separator(ch) {
            ' '
        } else {
            ch
        };
        output.push(ch);
    }
    if truncated {
        output.push('…');
    }
    output
}

pub(super) fn object<'a>(input: &'a Value, tool: &str) -> Result<&'a Map<String, Value>> {
    input
        .as_object()
        .ok_or_else(|| anyhow!("{tool}: input must be an object"))
}

pub(super) fn reject_unknown(
    object: &Map<String, Value>,
    allowed: &[&str],
    tool: &str,
) -> Result<()> {
    if let Some(unexpected) = object
        .keys()
        .find(|candidate| !allowed.contains(&candidate.as_str()))
    {
        let unexpected = bounded_diagnostic(unexpected, MAX_DIAGNOSTIC_CHARS);
        bail!("{tool}: unknown field `{unexpected}`");
    }
    Ok(())
}

pub(super) fn strict_object<'a>(
    input: &'a Value,
    allowed: &[&str],
    tool: &str,
) -> Result<&'a Map<String, Value>> {
    let object = object(input, tool)?;
    reject_unknown(object, allowed, tool)?;
    Ok(object)
}

/// How many other keys a missing-argument refusal lists, and how much of each.
const MAX_LISTED_KEYS: usize = 8;
const MAX_LISTED_KEY_CHARS: usize = 40;

/// What a missing-argument refusal can add: the other keys the call did carry.
///
/// The canonical name alone leaves a model that reached for another harness's
/// spelling — `file_path` for `path`, `old_str` for `old_string` — to work out
/// on its own that the key it sent went nowhere. Naming both sides turns a
/// blind retry into an informed one. The only renames kloop performs are the
/// exact, non-conflicting ones in `ARGUMENT_SYNONYMS`; anything else keeps its
/// spelling and is reported here.
fn provided_keys(input: &Value, key: &str) -> String {
    let Some(object) = input.as_object() else {
        return String::new();
    };
    let mut listed: Vec<String> = object
        .keys()
        .filter(|name| name.as_str() != key)
        .take(MAX_LISTED_KEYS + 1)
        .map(|name| bounded_diagnostic(name, MAX_LISTED_KEY_CHARS))
        .collect();
    if listed.is_empty() {
        return String::new();
    }
    let elided = listed.len() > MAX_LISTED_KEYS;
    listed.truncate(MAX_LISTED_KEYS);
    let names = listed.join(", ");
    let ellipsis = if elided { ", …" } else { "" };
    format!(" (got: {names}{ellipsis})")
}

pub(crate) fn required_str<'a>(input: &'a Value, key: &str, tool: &str) -> Result<&'a str> {
    input[key].as_str().ok_or_else(|| {
        let got = provided_keys(input, key);
        anyhow!("{tool}: missing required string argument '{key}'{got}")
    })
}

/// A single-key tool: any other key is unknown, and the one key is required.
pub(crate) fn strict_str_arg<'a>(input: &'a Value, key: &str, tool: &str) -> Result<&'a str> {
    strict_object(input, &[key], tool)?;
    required_str(input, key, tool)
}

/// Optional string with explicit `null` treated as omitted. A model uses
/// `null` for "not filled in"; refusing it made the same call fail in one tool
/// and succeed in another.
pub(super) fn optional_str<'a>(input: &'a Value, key: &str, tool: &str) -> Result<Option<&'a str>> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => bail!("{tool}: {key} must be a string"),
    }
}

pub(super) fn optional_bool(input: &Value, key: &str, tool: &str) -> Result<Option<bool>> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => bail!("{tool}: {key} must be a boolean"),
    }
}

pub(super) fn bool_or(input: &Value, key: &str, default: bool, tool: &str) -> Result<bool> {
    Ok(optional_bool(input, key, tool)?.unwrap_or(default))
}

/// Optional non-negative whole number. JSON numbers arrive as `u64`, `i64` or
/// `f64`; a numeric string is accepted because models occasionally quote one.
pub(super) fn integer_arg(input: &Value, key: &str, tool: &str) -> Result<Option<usize>> {
    let value = &input[key];
    if value.is_null() {
        return Ok(None);
    }
    let parsed = if let Some(value) = value.as_u64() {
        value
    } else if let Some(value) = value.as_i64() {
        if value < 0 {
            bail!("{tool}: {key} must be a whole number of 0 or more, got {value}");
        }
        value as u64
    } else if let Some(value) = value.as_f64() {
        if value < 0.0 || value.fract() != 0.0 {
            bail!("{tool}: {key} must be a whole number of 0 or more, got {value}");
        }
        value as u64
    } else if let Some(value) = value.as_str() {
        value.trim().parse::<u64>().map_err(|_| {
            anyhow!("{tool}: {key} must be a whole number of 0 or more, got {value:?}")
        })?
    } else {
        bail!("{tool}: {key} must be a whole number of 0 or more");
    };
    usize::try_from(parsed)
        .map(Some)
        .map_err(|_| anyhow!("{tool}: {key} is too large"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn explicit_null_is_omission() {
        let input = json!({"text": null, "flag": null, "count": null});
        assert_eq!(optional_str(&input, "text", "t").unwrap(), None);
        assert_eq!(optional_bool(&input, "flag", "t").unwrap(), None);
        assert_eq!(integer_arg(&input, "count", "t").unwrap(), None);
        assert!(bool_or(&input, "flag", true, "t").unwrap());
        assert!(required_str(&input, "text", "t").is_err());
    }

    #[test]
    fn unknown_fields_are_one_bounded_shape() {
        let long = format!("bad\n\u{2028}{}", "x".repeat(200));
        let input = json!({long: true});
        let error = strict_object(&input, &[], "tool").unwrap_err().to_string();
        assert!(error.starts_with("tool: unknown field `bad  "), "{error}");
        assert!(error.contains('…'), "{error}");
        assert!(!error.contains('\n'), "{error}");
        assert!(error.chars().count() < 120, "{error}");
    }

    #[test]
    fn integer_forms_and_bounds_agree() {
        for value in [json!(3), json!(3.0), json!("3"), json!(" 3 ")] {
            assert_eq!(
                integer_arg(&json!({"n": value}), "n", "t").unwrap(),
                Some(3)
            );
        }
        for value in [json!(-1), json!(1.5), json!("x"), json!(true)] {
            assert!(
                integer_arg(&json!({"n": value}), "n", "t").is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn missing_argument_lists_bounded_neighbours() {
        let input = json!({"filepath": "a", "new_string": 5});
        let error = required_str(&input, "path", "edit_file")
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "edit_file: missing required string argument 'path' (got: filepath, new_string)"
        );
    }
}
