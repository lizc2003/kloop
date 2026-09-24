mod closest;

pub(crate) use closest::ClosestShape;
pub(crate) use closest::LineDifference;
pub(crate) use closest::LineDrift;
pub(crate) use closest::closest_match;

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct TextEditOutcome {
    pub updated: Option<String>,
    pub match_count: usize,
    pub replacement_count: usize,
    /// The layer that matched, or the last one tried when none did.
    pub layer: MatchLayer,
    /// 1-based line of the first match, when there is one.
    pub first_line: Option<usize>,
}

/// Each layer runs only when every layer before it found nothing, so a looser
/// layer can never outvote an exact match elsewhere in the file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MatchLayer {
    Exact,
    /// LF `old_string` against CRLF text.
    NewlineFallback,
    /// Prose punctuation folded (curly quotes, dashes, full-width CJK) and one
    /// typesetting space after a separator made optional. Nothing else about
    /// whitespace is forgiven: indentation is content.
    PunctuationTolerant,
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
            layer: MatchLayer::Exact,
            first_line: None,
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
            layer: MatchLayer::Exact,
            first_line: current.find(old).map(|index| line_at(current, index)),
        };
    }

    let (logical, raw_boundaries) = logical_lf_view(current);
    let logical_old = normalize_newlines(old);
    if logical_old.is_empty() {
        return TextEditOutcome {
            updated: None,
            match_count: 0,
            replacement_count: 0,
            layer: MatchLayer::NewlineFallback,
            first_line: None,
        };
    }
    let matches: Vec<(usize, usize)> = logical
        .match_indices(&logical_old)
        .map(|(start, value)| (start, start + value.len()))
        .collect();
    if matches.is_empty() {
        return punctuation_tolerant_edit(
            current,
            &logical,
            &raw_boundaries,
            &logical_old,
            new,
            replace_all,
        );
    }
    let match_count = matches.len();
    let first_line = Some(line_at(&logical, matches[0].0));
    if match_count > 1 && !replace_all {
        return TextEditOutcome {
            updated: None,
            match_count,
            replacement_count: 0,
            layer: MatchLayer::NewlineFallback,
            first_line,
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
        layer: MatchLayer::NewlineFallback,
        first_line,
    }
}

/// The third layer, run over the logical LF view so that the raw boundaries
/// the newline fallback already computed carry a match back to raw bytes, and
/// a CRLF file keeps its line endings.
///
/// Only the part of `new` that actually differs from `old` is written; the
/// shared prefix and suffix keep the file's own bytes. Otherwise an edit meant
/// to rename one variable would also straighten every curly quote around it —
/// a change the model never asked for.
fn punctuation_tolerant_edit(
    current: &str,
    logical: &str,
    raw_boundaries: &[usize],
    logical_old: &str,
    new: &str,
    replace_all: bool,
) -> TextEditOutcome {
    let haystack = fold(logical);
    let needle = fold(logical_old);
    let starts = tolerant_match_starts(&haystack, &needle);
    let match_count = starts.len();
    let first_line = starts
        .first()
        .map(|&start| line_at(logical, haystack.spans[start].start));
    if match_count == 0 || (match_count > 1 && !replace_all) {
        return TextEditOutcome {
            updated: None,
            match_count,
            replacement_count: 0,
            layer: MatchLayer::PunctuationTolerant,
            first_line,
        };
    }

    let logical_new = normalize_newlines(new);
    let folded_new = fold(&logical_new);
    let (prefix, suffix) = shared_ends(logical_old, &needle, &logical_new, &folded_new);
    let delta = (prefix < folded_new.chars.len() - suffix).then(|| {
        let start = folded_new.spans[prefix].start;
        let end = folded_new.spans[folded_new.chars.len() - suffix - 1].end;
        &logical_new[start..end]
    });

    let length = needle.chars.len();
    let last = needle.spans[length - 1];
    let needle_ends_with_space = last.end > last.core_end;
    let file_eol = dominant_eol(current).unwrap_or(Eol::Lf);
    let selected = if replace_all {
        &starts[..]
    } else {
        &starts[..1]
    };
    let mut updated = String::with_capacity(current.len().saturating_add(logical_new.len()));
    let mut copied_until = 0usize;
    for &start in selected {
        // A space the file's last matched character absorbed is not part of
        // the match unless the needle absorbed one too: `old` ending in "，"
        // must not swallow the space after the file's ", ".
        let final_span = haystack.spans[start + length - 1];
        let logical_start = haystack.spans[start].start;
        let logical_end = if needle_ends_with_space {
            final_span.end
        } else {
            final_span.core_end
        };
        let prefix_end = match prefix {
            0 => logical_start,
            n => haystack.spans[start + n - 1].end.min(logical_end),
        };
        let suffix_start = match suffix {
            0 => logical_end,
            n => haystack.spans[start + length - n].start,
        };
        let raw_start = raw_boundaries[logical_start];
        let raw_end = raw_boundaries[logical_end];
        updated.push_str(&current[copied_until..raw_start]);
        updated.push_str(&current[raw_start..raw_boundaries[prefix_end]]);
        if let Some(delta) = delta {
            let replacement_eol = dominant_eol(&current[raw_start..raw_end]).unwrap_or(file_eol);
            push_with_eol(&mut updated, delta, replacement_eol);
        }
        updated.push_str(&current[raw_boundaries[suffix_start]..raw_end]);
        copied_until = raw_end;
    }
    updated.push_str(&current[copied_until..]);

    TextEditOutcome {
        updated: Some(updated),
        match_count,
        replacement_count: selected.len(),
        layer: MatchLayer::PunctuationTolerant,
        first_line,
    }
}

/// How many folded characters `old` and `new` share at each end, counting a
/// character as shared only when its source bytes are identical too. Where
/// they fold alike but are spelled differently ("：" in `old`, ": " in
/// `new`), that difference is the edit, and it has to come from `new`.
fn shared_ends(old: &str, folded_old: &Folded, new: &str, folded_new: &Folded) -> (usize, usize) {
    let same = |old_index: usize, new_index: usize| {
        let old_span = folded_old.spans[old_index];
        let new_span = folded_new.spans[new_index];
        folded_old.chars[old_index] == folded_new.chars[new_index]
            && old[old_span.start..old_span.end] == new[new_span.start..new_span.end]
    };
    let old_len = folded_old.chars.len();
    let new_len = folded_new.chars.len();
    let mut prefix = 0usize;
    while prefix < old_len && prefix < new_len && same(prefix, prefix) {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < old_len - prefix
        && suffix < new_len - prefix
        && same(old_len - 1 - suffix, new_len - 1 - suffix)
    {
        suffix += 1;
    }
    (prefix, suffix)
}

/// One character of a folded view, and the source bytes it stands for.
/// `core_end` ends the character itself; `end` also covers the typesetting
/// space it absorbed, if any. Spans tile the source: each one starts where
/// the previous one ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Span {
    start: usize,
    core_end: usize,
    end: usize,
}

struct Folded {
    chars: Vec<char>,
    spans: Vec<Span>,
}

fn fold(text: &str) -> Folded {
    let mut chars = Vec::with_capacity(text.len());
    let mut spans = Vec::with_capacity(text.len());
    let mut iter = text.char_indices().peekable();
    while let Some((start, character)) = iter.next() {
        let folded = fold_char(character);
        let core_end = start + character.len_utf8();
        let mut end = core_end;
        if absorbs_following_space(folded) && iter.next_if(|&(_, next)| next == ' ').is_some() {
            end += 1;
        }
        chars.push(folded);
        spans.push(Span {
            start,
            core_end,
            end,
        });
    }
    Folded { chars, spans }
}

/// One character in, one character out, so folding never moves a boundary.
fn fold_char(character: char) -> char {
    match character {
        '“' | '”' | '„' | '‟' => '"',
        '‘' | '’' | '‚' | '‛' => '\'',
        '–' | '—' | '−' => '-',
        '，' => ',',
        '；' => ';',
        '：' => ':',
        '。' | '．' => '.',
        '！' => '!',
        '？' => '?',
        '（' => '(',
        '）' => ')',
        other => other,
    }
}

/// Quotes and the hyphen are deliberately absent: `" foo"` is not `"foo"`,
/// and `- foo` is a list item. After a separator the space is typesetting.
fn absorbs_following_space(folded: char) -> bool {
    matches!(folded, ',' | ';' | ':' | '.' | '!' | '?' | '(' | ')')
}

/// Non-overlapping, left to right — the same occurrences `str::replace` would
/// take, so a `replace_all` never splices two matches over one range.
fn tolerant_match_starts(haystack: &Folded, needle: &Folded) -> Vec<usize> {
    let length = needle.chars.len();
    let mut starts = Vec::new();
    if length == 0 || length > haystack.chars.len() {
        return starts;
    }
    let mut index = 0usize;
    while index + length <= haystack.chars.len() {
        if haystack.chars[index..index + length] == needle.chars[..] {
            starts.push(index);
            index += length;
        } else {
            index += 1;
        }
    }
    starts
}

fn line_at(text: &str, index: usize) -> usize {
    text[..index].matches('\n').count() + 1
}

/// The 1-based line the first match of `old` starts on, under the same
/// three layers [`apply_text_edit`] itself uses, in the same order.
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
        return Some(line_at(current, index));
    }
    let (logical, _) = logical_lf_view(current);
    let logical_old = normalize_newlines(old);
    if logical_old.is_empty() {
        return None;
    }
    if let Some(index) = logical.find(&logical_old) {
        return Some(line_at(&logical, index));
    }
    let haystack = fold(&logical);
    let start = *tolerant_match_starts(&haystack, &fold(&logical_old)).first()?;
    Some(line_at(&logical, haystack.spans[start].start))
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
                layer: MatchLayer::Exact,
                first_line: Some(1),
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
        assert_eq!(outcome.layer, MatchLayer::NewlineFallback);
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

    fn tolerant(current: &str, old: &str, new: &str) -> TextEditOutcome {
        apply_text_edit(current, old, new, false)
    }

    /// One entry of the fold table per assertion: the model typed the ASCII
    /// form, the file holds the typographic one, and the edit still lands.
    #[test]
    fn each_folded_punctuation_mark_matches_its_ascii_form() {
        let cases = [
            ("say “hi”", "say \"hi\""),
            ("say „hi‟", "say \"hi\""),
            ("it’s ‘x’ ‚y‛", "it's 'x' 'y'"),
            ("a–b—c−d", "a-b-c-d"),
            ("甲，乙；丙：丁。戊．", "甲,乙;丙:丁.戊."),
            ("真！假？（注）", "真!假?(注)"),
        ];
        for (file, old) in cases {
            let current = format!("before\n{file}\nafter\n");
            assert_eq!(
                tolerant(&current, old, "X"),
                TextEditOutcome {
                    updated: Some("before\nX\nafter\n".into()),
                    match_count: 1,
                    replacement_count: 1,
                    layer: MatchLayer::PunctuationTolerant,
                    first_line: Some(2),
                },
                "{file:?} vs {old:?}"
            );
        }
    }

    /// One typesetting space after a separator is optional in both
    /// directions: the model added one the file lacks, or dropped one it has.
    #[test]
    fn one_space_after_a_separator_is_optional() {
        for (file, old) in [
            ("f(a,b); g()", "f(a, b);g()"),
            ("key: value. Next", "key:value.Next"),
            ("甲，乙", "甲, 乙"),
            ("call( x )", "call(x )"),
        ] {
            let outcome = tolerant(file, old, "Z");
            assert_eq!(outcome.updated.as_deref(), Some("Z"), "{file:?} vs {old:?}");
            assert_eq!(outcome.layer, MatchLayer::PunctuationTolerant);
        }
    }

    /// A space after a quote or a hyphen means something (`" foo"`, a `- foo`
    /// list item), and indentation is content — none of it is forgiven.
    #[test]
    fn spaces_after_quotes_and_hyphens_and_indentation_are_not_forgiven() {
        for (file, old) in [
            ("- foo", "-foo"),
            ("\" foo\"", "\"foo\""),
            ("x = 1", "x  = 1"),
            ("  body();", "    body();"),
            ("\tbody();", "    body();"),
            ("a,  b", "a,b"),
        ] {
            let outcome = tolerant(file, old, "Z");
            assert_eq!(
                (outcome.updated, outcome.match_count),
                (None, 0),
                "{file:?} vs {old:?}"
            );
        }
    }

    /// Only the part of `new` that differs from `old` is written. Everything
    /// the two share keeps the file's own bytes, so renaming one word does not
    /// also straighten every curly quote around it.
    #[test]
    fn a_tolerant_edit_keeps_the_file_bytes_around_the_change() {
        let current = "line one\n“Hello”, said the “old” fox — twice。\nline three\n";
        let outcome = tolerant(
            current,
            "\"Hello\", said the \"old\" fox - twice.",
            "\"Hello\", said the \"new\" fox - twice.",
        );
        assert_eq!(
            outcome,
            TextEditOutcome {
                updated: Some(
                    "line one\n“Hello”, said the “new” fox — twice。\nline three\n".into()
                ),
                match_count: 1,
                replacement_count: 1,
                layer: MatchLayer::PunctuationTolerant,
                first_line: Some(2),
            }
        );
    }

    /// Where `old` and `new` fold alike but are spelled differently, that
    /// spelling is the edit — it comes from `new`, not from the file.
    #[test]
    fn a_respelled_mark_in_new_is_the_edit() {
        let outcome = tolerant("名称：值\n", "名称:值", "名称: 值");
        assert_eq!(outcome.updated.as_deref(), Some("名称: 值\n"));
    }

    /// `old` ending on a full-width comma must not take the space after the
    /// file's ASCII one with it; the shared comma keeps the file's spelling.
    #[test]
    fn a_match_does_not_swallow_a_space_the_needle_never_had() {
        let outcome = tolerant("alpha, beta\n", "alpha，", "gamma，");
        assert_eq!(outcome.updated.as_deref(), Some("gamma, beta\n"));
    }

    #[test]
    fn tolerant_duplicates_refuse_unless_replace_all() {
        let current = "“a” then “a”\n";
        assert_eq!(
            tolerant(current, "\"a\"", "b"),
            TextEditOutcome {
                updated: None,
                match_count: 2,
                replacement_count: 0,
                layer: MatchLayer::PunctuationTolerant,
                first_line: Some(1),
            }
        );
        assert_eq!(
            apply_text_edit(current, "\"a\"", "b", true),
            TextEditOutcome {
                updated: Some("b then b\n".into()),
                match_count: 2,
                replacement_count: 2,
                layer: MatchLayer::PunctuationTolerant,
                first_line: Some(1),
            }
        );
    }

    #[test]
    fn a_tolerant_edit_keeps_crlf_line_endings() {
        let current = "keep\r\n“one”\r\n“two”\r\nkeep\r\n";
        let outcome = tolerant(current, "\"one\"\n\"two\"", "\"one\"\n\"2\"");
        assert_eq!(
            outcome.updated.as_deref(),
            Some("keep\r\n“one”\r\n“2”\r\nkeep\r\n")
        );
        assert_eq!(outcome.first_line, Some(2));
    }

    #[test]
    fn first_match_line_reaches_the_tolerant_layer() {
        assert_eq!(first_match_line("a\nb\n“c”\n", "\"c\""), Some(3));
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
