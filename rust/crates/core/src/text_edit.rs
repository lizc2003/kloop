#[derive(Debug, Eq, PartialEq)]
pub(crate) struct TextEditOutcome {
    pub updated: Option<String>,
    pub match_count: usize,
    pub replacement_count: usize,
    pub used_newline_fallback: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Eol {
    Lf,
    Crlf,
}

impl Eol {
    fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }
}

pub(crate) fn apply_text_edit(
    current: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> TextEditOutcome {
    if old.is_empty() || old == new {
        return TextEditOutcome {
            updated: None,
            match_count: 0,
            replacement_count: 0,
            used_newline_fallback: false,
        };
    }

    let raw_matches = current.match_indices(old).count();
    if raw_matches > 0 {
        let executable = raw_matches == 1 || replace_all;
        let updated = executable.then(|| {
            if replace_all {
                current.replace(old, new)
            } else {
                current.replacen(old, new, 1)
            }
        });
        return TextEditOutcome {
            updated,
            match_count: raw_matches,
            replacement_count: usize::from(executable) * if replace_all { raw_matches } else { 1 },
            used_newline_fallback: false,
        };
    }

    let (logical, raw_boundaries) = logical_lf_view(current);
    let logical_old = normalize_newlines(old);
    if logical_old.is_empty() {
        return TextEditOutcome {
            updated: None,
            match_count: 0,
            replacement_count: 0,
            used_newline_fallback: true,
        };
    }
    let matches: Vec<(usize, usize)> = logical
        .match_indices(&logical_old)
        .map(|(start, value)| (start, start + value.len()))
        .collect();
    let match_count = matches.len();
    if match_count == 0 || (match_count > 1 && !replace_all) {
        return TextEditOutcome {
            updated: None,
            match_count,
            replacement_count: 0,
            used_newline_fallback: true,
        };
    }

    let logical_new = normalize_newlines(new);
    let file_eol = dominant_eol(current).unwrap_or(Eol::Lf);
    let selected = if replace_all {
        matches.as_slice()
    } else {
        &matches[..1]
    };
    let mut updated = String::with_capacity(current.len().saturating_add(logical_new.len()));
    let mut copied_until = 0usize;
    for &(logical_start, logical_end) in selected {
        let raw_start = raw_boundaries[logical_start];
        let raw_end = raw_boundaries[logical_end];
        updated.push_str(&current[copied_until..raw_start]);
        let replacement_eol = dominant_eol(&current[raw_start..raw_end]).unwrap_or(file_eol);
        push_with_eol(&mut updated, &logical_new, replacement_eol);
        copied_until = raw_end;
    }
    updated.push_str(&current[copied_until..]);

    TextEditOutcome {
        updated: Some(updated),
        match_count,
        replacement_count: selected.len(),
        used_newline_fallback: true,
    }
}

/// The 1-based line the first match of `old` starts on, under the same
/// exact-first-then-logical-LF matching [`apply_text_edit`] itself uses.
/// `None` when nothing matches — that is a different failure with its own
/// message, and this is only ever an addition to someone else's.
///
/// Counting newlines in the logical view is counting them in the raw text: the
/// view replaces each CRLF with one LF and touches nothing else.
pub(crate) fn first_match_line(current: &str, old: &str) -> Option<usize> {
    if old.is_empty() {
        return None;
    }
    if let Some(index) = current.find(old) {
        return Some(current[..index].matches('\n').count() + 1);
    }
    let (logical, _) = logical_lf_view(current);
    let logical_old = normalize_newlines(old);
    if logical_old.is_empty() {
        return None;
    }
    let index = logical.find(&logical_old)?;
    Some(logical[..index].matches('\n').count() + 1)
}

fn logical_lf_view(raw: &str) -> (String, Vec<usize>) {
    let bytes = raw.as_bytes();
    let mut logical = String::with_capacity(raw.len());
    let mut boundaries = vec![usize::MAX; raw.len() + 1];
    boundaries[0] = 0;
    let mut raw_offset = 0usize;
    while raw_offset < bytes.len() {
        if bytes[raw_offset] == b'\r'
            && bytes.get(raw_offset + 1).is_some_and(|byte| *byte == b'\n')
        {
            logical.push('\n');
            raw_offset += 2;
        } else {
            let character = raw[raw_offset..]
                .chars()
                .next()
                .expect("raw offset is a UTF-8 boundary");
            logical.push(character);
            raw_offset += character.len_utf8();
        }
        boundaries[logical.len()] = raw_offset;
    }
    boundaries.truncate(logical.len() + 1);
    (logical, boundaries)
}

fn normalize_newlines(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut normalized = String::with_capacity(text.len());
    let mut offset = 0usize;
    while offset < bytes.len() {
        if bytes[offset] == b'\r' && bytes.get(offset + 1).is_some_and(|byte| *byte == b'\n') {
            normalized.push('\n');
            offset += 2;
        } else {
            let character = text[offset..]
                .chars()
                .next()
                .expect("offset is a UTF-8 boundary");
            normalized.push(character);
            offset += character.len_utf8();
        }
    }
    normalized
}

fn dominant_eol(text: &str) -> Option<Eol> {
    let bytes = text.as_bytes();
    let mut crlf = 0usize;
    let mut lf = 0usize;
    let mut offset = 0usize;
    while offset < bytes.len() {
        match bytes[offset] {
            b'\r' if bytes.get(offset + 1).is_some_and(|byte| *byte == b'\n') => {
                crlf += 1;
                offset += 2;
            }
            b'\n' => {
                lf += 1;
                offset += 1;
            }
            _ => offset += 1,
        }
    }
    match (crlf, lf) {
        (0, 0) => None,
        (crlf, lf) if crlf > lf => Some(Eol::Crlf),
        _ => Some(Eol::Lf),
    }
}

fn push_with_eol(output: &mut String, logical: &str, eol: Eol) {
    let mut pieces = logical.split('\n').peekable();
    while let Some(piece) = pieces.next() {
        output.push_str(piece);
        if pieces.peek().is_some() {
            output.push_str(eol.as_str());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_exact_match_wins_and_preserves_raw_replacement() {
        let outcome = apply_text_edit("a\r\nb\r\n", "a\r\nb", "x\ny", false);
        assert_eq!(
            outcome,
            TextEditOutcome {
                updated: Some("x\ny\r\n".into()),
                match_count: 1,
                replacement_count: 1,
                used_newline_fallback: false,
            }
        );
    }

    #[test]
    fn lf_old_string_edits_crlf_without_normalizing_the_file() {
        let outcome = apply_text_edit(
            "prefix\r\none\r\ntwo\r\nsuffix\r\n",
            "one\ntwo",
            "ONE\nTWO",
            false,
        );
        assert_eq!(
            outcome.updated.as_deref(),
            Some("prefix\r\nONE\r\nTWO\r\nsuffix\r\n")
        );
        assert!(outcome.used_newline_fallback);
    }

    #[test]
    fn mixed_eol_replace_all_uses_each_occurrence_local_style() {
        let outcome = apply_text_edit("a\r\nb\r\nc|a\nb\nc", "a\r\nb\nc", "x\ny\nz", true);
        assert_eq!(outcome.updated.as_deref(), Some("x\r\ny\r\nz|x\ny\nz"));
        assert_eq!(outcome.match_count, 2);
        assert_eq!(outcome.replacement_count, 2);
    }

    #[test]
    fn unmatched_bytes_and_lone_carriage_returns_are_preserved() {
        let outcome = apply_text_edit("left\ra\r\nb\nright", "a\nb", "A\nB", false);
        assert_eq!(outcome.updated.as_deref(), Some("left\rA\r\nB\nright"));
    }

    /// The line a refusal reports follows the same matching the edit would
    /// have done, so a CRLF file does not report a line the model cannot find.
    #[test]
    fn first_match_line_follows_exact_then_logical_matching() {
        assert_eq!(first_match_line("a\nb\nc\n", "a"), Some(1));
        assert_eq!(first_match_line("a\nb\nc\n", "c"), Some(3));
        // LF needle over CRLF text: matched through the logical view, and the
        // line counted there is the line in the raw text.
        assert_eq!(first_match_line("a\r\nb\r\nc\r\n", "b\nc"), Some(2));
        // First match wins, which is what replace_all reports.
        assert_eq!(first_match_line("x\ny\nx\n", "x"), Some(1));
        assert_eq!(first_match_line("a\nb\n", "missing"), None);
        assert_eq!(first_match_line("a\nb\n", ""), None);
    }

    #[test]
    fn dominant_eol_uses_strict_crlf_majority_and_tie_is_lf() {
        assert_eq!(dominant_eol("a\r\nb\r\nc\n"), Some(Eol::Crlf));
        assert_eq!(dominant_eol("a\r\nb\n"), Some(Eol::Lf));
        assert_eq!(dominant_eol("a\nb\n"), Some(Eol::Lf));
        assert_eq!(dominant_eol("a\rb"), None);
    }

    #[test]
    fn fallback_keeps_missing_and_duplicate_protection() {
        let missing = apply_text_edit("a\r\nb", "missing\nvalue", "x", false);
        assert_eq!(missing.match_count, 0);
        assert!(missing.updated.is_none());

        let duplicate = apply_text_edit("a\r\nb\r\nc|a\nb\nc", "a\r\nb\nc", "x", false);
        assert_eq!(duplicate.match_count, 2);
        assert!(duplicate.updated.is_none());
    }
}
