use std::fmt;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ByteOffset(usize);

impl ByteOffset {
    pub(crate) const ZERO: Self = Self(0);

    pub(crate) const fn new(value: usize) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> usize {
        self.0
    }
}

impl fmt::Debug for ByteOffset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ByteOffset({})", self.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TextRange {
    start: ByteOffset,
    end: ByteOffset,
}

impl TextRange {
    pub(crate) fn new(start: ByteOffset, end: ByteOffset) -> Self {
        assert!(start <= end, "text range start must not exceed end");
        Self { start, end }
    }

    pub(crate) const fn empty(at: ByteOffset) -> Self {
        Self { start: at, end: at }
    }

    pub(crate) const fn start(self) -> ByteOffset {
        self.start
    }

    pub(crate) const fn end(self) -> ByteOffset {
        self.end
    }

    pub(crate) const fn is_empty(self) -> bool {
        self.start.0 == self.end.0
    }

    pub(crate) const fn len(self) -> usize {
        self.end.0 - self.start.0
    }

    pub(crate) const fn contains(self, offset: ByteOffset) -> bool {
        self.start.0 <= offset.0 && offset.0 < self.end.0
    }

    pub(crate) const fn intersects(self, other: Self) -> bool {
        self.start.0 < other.end.0 && other.start.0 < self.end.0
    }
}

impl fmt::Debug for TextRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TextRange")
            .field(&self.start.0)
            .field(&self.end.0)
            .finish()
    }
}

pub(crate) fn is_grapheme_boundary(text: &str, offset: ByteOffset) -> bool {
    let offset = offset.get();
    if offset > text.len() || !text.is_char_boundary(offset) {
        return false;
    }
    offset == text.len()
        || text
            .grapheme_indices(true)
            .any(|(index, _)| index == offset)
}

pub(crate) fn previous_grapheme_boundary(text: &str, offset: ByteOffset) -> ByteOffset {
    let limit = offset.get().min(text.len());
    ByteOffset::new(
        text.grapheme_indices(true)
            .map(|(index, _)| index)
            .take_while(|index| *index < limit)
            .last()
            .unwrap_or(0),
    )
}

pub(crate) fn next_grapheme_boundary(text: &str, offset: ByteOffset) -> ByteOffset {
    let start = offset.get().min(text.len());
    text.grapheme_indices(true)
        .map(|(index, grapheme)| index + grapheme.len())
        .find(|end| *end > start)
        .map(ByteOffset::new)
        .unwrap_or_else(|| ByteOffset::new(text.len()))
}

pub(crate) fn snap_grapheme_boundary(text: &str, offset: ByteOffset) -> ByteOffset {
    let raw = offset.get().min(text.len());
    let offset = ByteOffset::new(raw);
    if is_grapheme_boundary(text, offset) {
        return offset;
    }
    let previous = previous_grapheme_boundary(text, offset);
    let next = next_grapheme_boundary(text, offset);
    if raw - previous.get() <= next.get() - raw {
        previous
    } else {
        next
    }
}

pub(crate) fn grapheme_ranges(text: &str) -> impl Iterator<Item = (TextRange, &str)> {
    text.grapheme_indices(true).map(|(start, grapheme)| {
        (
            TextRange::new(
                ByteOffset::new(start),
                ByteOffset::new(start + grapheme.len()),
            ),
            grapheme,
        )
    })
}

pub(crate) fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub(crate) fn display_column(text: &str, offset: ByteOffset) -> usize {
    assert!(
        is_grapheme_boundary(text, offset),
        "display-column offset must be a grapheme boundary"
    );
    display_width(&text[..offset.get()])
}

pub(crate) fn byte_offset_at_display_column(text: &str, column: usize) -> ByteOffset {
    let mut previous = (ByteOffset::ZERO, 0usize);
    for (range, grapheme) in grapheme_ranges(text) {
        let next = (range.end(), previous.1 + display_width(grapheme));
        if column <= next.1 {
            return if column - previous.1 <= next.1 - column {
                previous.0
            } else {
                next.0
            };
        }
        previous = next;
    }
    ByteOffset::new(text.len())
}

/// Hard-wrap text at grapheme boundaries. A grapheme wider than the requested
/// width occupies a row by itself rather than being split.
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let mut line = String::new();
        let mut columns = 0;
        for grapheme in raw.graphemes(true) {
            let grapheme_width = display_width(grapheme);
            if columns + grapheme_width > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                columns = 0;
            }
            line.push_str(grapheme);
            columns += grapheme_width;
        }
        lines.push(line);
    }
    lines
}

/// Truncate to display columns and append an ellipsis when any grapheme was cut.
pub(crate) fn truncate(text: &str, width: usize) -> String {
    let width = width.max(1);
    if display_width(text) <= width {
        return text.to_string();
    }

    let content_width = width.saturating_sub(display_width("…"));
    let mut output = String::new();
    let mut columns = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = display_width(grapheme);
        if columns + grapheme_width > content_width {
            break;
        }
        output.push_str(grapheme);
        columns += grapheme_width;
    }
    output.push('…');
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRAPHEMES: &[&str] = &["e\u{301}", "👨‍👩‍👧‍👦", "👩🏽‍💻", "👍🏽", "🇨🇳", "❤️", "1️⃣", "你"];

    #[test]
    fn boundaries_keep_extended_graphemes_whole() {
        for grapheme in GRAPHEMES {
            let text = format!("a{grapheme}z");
            let start = ByteOffset::new(1);
            let end = ByteOffset::new(1 + grapheme.len());
            assert!(is_grapheme_boundary(&text, start), "{grapheme:?}");
            assert!(is_grapheme_boundary(&text, end), "{grapheme:?}");
            assert_eq!(next_grapheme_boundary(&text, start), end, "{grapheme:?}");
            assert_eq!(
                previous_grapheme_boundary(&text, end),
                start,
                "{grapheme:?}"
            );
            for byte in start.get() + 1..end.get() {
                assert!(!is_grapheme_boundary(&text, ByteOffset::new(byte)));
            }
        }
    }

    #[test]
    fn snap_chooses_nearest_boundary_and_clamps() {
        let text = "a👩🏽‍💻z";
        let start = ByteOffset::new(1);
        let end = ByteOffset::new(1 + "👩🏽‍💻".len());
        assert_eq!(snap_grapheme_boundary(text, ByteOffset::new(2)), start);
        assert_eq!(
            snap_grapheme_boundary(text, ByteOffset::new(end.get() - 1)),
            end
        );
        assert_eq!(
            snap_grapheme_boundary(text, ByteOffset::new(usize::MAX)),
            ByteOffset::new(text.len())
        );
    }

    #[test]
    fn display_columns_map_only_to_grapheme_boundaries() {
        let text = "a你👩🏽‍💻z";
        let after_a = ByteOffset::new(1);
        let after_cjk = ByteOffset::new(4);
        let after_emoji = ByteOffset::new(4 + "👩🏽‍💻".len());
        assert_eq!(display_column(text, after_a), 1);
        assert_eq!(display_column(text, after_cjk), 3);
        assert_eq!(display_column(text, after_emoji), 5);
        assert_eq!(byte_offset_at_display_column(text, 0), ByteOffset::ZERO);
        assert_eq!(byte_offset_at_display_column(text, 1), after_a);
        assert_eq!(byte_offset_at_display_column(text, 2), after_a);
        assert_eq!(byte_offset_at_display_column(text, 3), after_cjk);
        assert_eq!(byte_offset_at_display_column(text, 4), after_cjk);
        assert_eq!(byte_offset_at_display_column(text, 5), after_emoji);
        assert_eq!(
            byte_offset_at_display_column(text, 99),
            ByteOffset::new(text.len())
        );
    }

    #[test]
    fn wrap_never_leaves_orphan_codepoints() {
        for grapheme in GRAPHEMES {
            assert_eq!(wrap(&format!("a{grapheme}b"), 1), vec!["a", *grapheme, "b"]);
        }
        assert_eq!(wrap("你好世界", 4), vec!["你好", "世界"]);
        assert_eq!(wrap("ab\ncd\n", 10), vec!["ab", "cd", ""]);
        assert_eq!(wrap("", 10), vec![""]);
    }

    #[test]
    fn truncate_never_leaves_orphan_codepoints() {
        for grapheme in GRAPHEMES {
            assert_eq!(truncate(&format!("{grapheme}x"), 1), "…", "{grapheme:?}");
            assert_eq!(truncate(grapheme, display_width(grapheme)), *grapheme);
        }
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello!", 5), "hell…");
        assert_eq!(truncate("你好世界", 5), "你好…");
        assert_eq!(truncate("x", 0), "x");
    }

    #[test]
    fn text_range_reports_half_open_relationships() {
        let left = TextRange::new(ByteOffset::new(1), ByteOffset::new(4));
        let right = TextRange::new(ByteOffset::new(4), ByteOffset::new(8));
        assert_eq!(left.len(), 3);
        assert!(!left.is_empty());
        assert!(left.contains(ByteOffset::new(1)));
        assert!(!left.contains(ByteOffset::new(4)));
        assert!(!left.intersects(right));
        assert!(left.intersects(TextRange::new(ByteOffset::new(3), ByteOffset::new(5))));
    }
}
