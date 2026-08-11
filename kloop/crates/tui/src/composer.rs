//! The multi-line input composer (plan 38 slice 3, hardened in plan 76).
//!
//! The document uses UTF-8 byte ranges, while editing moves only across extended
//! grapheme boundaries. Rendering and vertical navigation consume one canonical
//! visual-row layout. Large pastes are range-addressed atoms: their visible label
//! is only a projection, and submission expands the exact atom once.

use kloop_protocol::ContentBlock;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::text_layout::byte_offset_at_display_column;
use crate::text_layout::display_column;
use crate::text_layout::display_width;
use crate::text_layout::grapheme_ranges;
use crate::text_layout::is_grapheme_boundary;
use crate::text_layout::next_grapheme_boundary;
use crate::text_layout::previous_grapheme_boundary;
use crate::text_layout::snap_grapheme_boundary;
use crate::text_layout::ByteOffset;
use crate::text_layout::TextRange;

const PROMPT: &str = "› ";
const CONT: &str = "  ";
const GUTTER_W: usize = 2;
const MAX_ROWS: usize = 8;
const PASTE_CHARS: usize = 400;
const PASTE_LINES: usize = 5;

const DIM: Style = Style::new().add_modifier(Modifier::DIM);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PasteId(u64);

#[derive(Clone, Debug, PartialEq, Eq)]
struct PasteAtom {
    id: PasteId,
    range: TextRange,
    label: String,
    content: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ComposerDocument {
    display: String,
    atoms: Vec<PasteAtom>,
}

impl ComposerDocument {
    fn text(&self) -> &str {
        &self.display
    }

    fn expand(&self) -> String {
        let mut expanded = String::new();
        let mut copied = 0;
        for atom in &self.atoms {
            expanded.push_str(&self.display[copied..atom.range.start().get()]);
            expanded.push_str(&atom.content);
            copied = atom.range.end().get();
        }
        expanded.push_str(&self.display[copied..]);
        expanded
    }

    fn atom_ending_at(&self, offset: ByteOffset) -> Option<usize> {
        self.atoms
            .iter()
            .position(|atom| atom.range.end() == offset)
    }

    fn atom_starting_at(&self, offset: ByteOffset) -> Option<usize> {
        self.atoms
            .iter()
            .position(|atom| atom.range.start() == offset)
    }

    fn insert_atom(
        &mut self,
        at: ByteOffset,
        id: PasteId,
        label: String,
        content: String,
    ) -> ByteOffset {
        self.assert_boundary(at);
        debug_assert!(self
            .atoms
            .iter()
            .all(|atom| at <= atom.range.start() || at >= atom.range.end()));

        let mut projected = self.display.clone();
        projected.insert_str(at.get(), &label);
        let end = ByteOffset::new(at.get() + label.len());
        if !is_grapheme_boundary(&projected, at) || !is_grapheme_boundary(&projected, end) {
            return self.replace(TextRange::empty(at), &content);
        }

        let cursor = self.replace(TextRange::empty(at), &label);
        debug_assert_eq!(cursor, end);
        self.atoms.push(PasteAtom {
            id,
            range: TextRange::new(at, end),
            label,
            content,
        });
        self.atoms.sort_by_key(|atom| atom.range.start());
        self.assert_invariants();
        cursor
    }

    fn replace(&mut self, range: TextRange, replacement: &str) -> ByteOffset {
        self.assert_range(range);
        let range = self.materialize_intersections(range);
        let mut cursor = self.replace_raw(range, replacement);
        if !is_grapheme_boundary(&self.display, cursor) {
            let snapped = snap_grapheme_boundary(&self.display, cursor);
            cursor = if snapped < cursor {
                next_grapheme_boundary(&self.display, cursor)
            } else {
                snapped
            };
        }

        while let Some(index) = self.atoms.iter().position(|atom| {
            !is_grapheme_boundary(&self.display, atom.range.start())
                || !is_grapheme_boundary(&self.display, atom.range.end())
        }) {
            cursor = self.materialize_atom(index, cursor);
        }
        self.assert_invariants();
        cursor
    }

    fn delete_atom(&mut self, index: usize) -> ByteOffset {
        let atom = self.atoms.remove(index);
        let cursor = self.replace_raw(atom.range, "");
        self.assert_invariants();
        cursor
    }

    fn materialize_intersections(&mut self, mut range: TextRange) -> TextRange {
        if range.is_empty() {
            return range;
        }
        while let Some(index) = self
            .atoms
            .iter()
            .position(|atom| atom.range.intersects(range))
        {
            let atom = self.atoms.remove(index);
            let old = atom.range;
            let materialized_end = ByteOffset::new(old.start().get() + atom.content.len());
            let start = map_start(range.start(), old, materialized_end);
            let end = map_end(range.end(), old, materialized_end);
            self.replace_raw(old, &atom.content);
            range = TextRange::new(start, end);
        }
        range
    }

    fn materialize_atom(&mut self, index: usize, cursor: ByteOffset) -> ByteOffset {
        let atom = self.atoms.remove(index);
        let old = atom.range;
        let materialized_end = ByteOffset::new(old.start().get() + atom.content.len());
        let cursor = map_end(cursor, old, materialized_end);
        self.replace_raw(old, &atom.content);
        if is_grapheme_boundary(&self.display, cursor) {
            cursor
        } else {
            next_grapheme_boundary(&self.display, cursor)
        }
    }

    fn replace_raw(&mut self, range: TextRange, replacement: &str) -> ByteOffset {
        let old_len = range.len();
        self.display
            .replace_range(range.start().get()..range.end().get(), replacement);
        for atom in &mut self.atoms {
            if atom.range.start() >= range.end() {
                atom.range = shift_range(atom.range, old_len, replacement.len());
            } else {
                debug_assert!(atom.range.end() <= range.start());
            }
        }
        ByteOffset::new(range.start().get() + replacement.len())
    }

    fn assert_boundary(&self, offset: ByteOffset) {
        assert!(
            is_grapheme_boundary(&self.display, offset),
            "document offset must be a grapheme boundary"
        );
    }

    fn assert_range(&self, range: TextRange) {
        assert!(
            range.end().get() <= self.display.len(),
            "range past document end"
        );
        self.assert_boundary(range.start());
        self.assert_boundary(range.end());
    }

    fn assert_invariants(&self) {
        let mut previous_end = ByteOffset::ZERO;
        for atom in &self.atoms {
            debug_assert!(previous_end <= atom.range.start());
            debug_assert!(atom.range.end().get() <= self.display.len());
            debug_assert!(is_grapheme_boundary(&self.display, atom.range.start()));
            debug_assert!(is_grapheme_boundary(&self.display, atom.range.end()));
            debug_assert_eq!(
                &self.display[atom.range.start().get()..atom.range.end().get()],
                atom.label
            );
            previous_end = atom.range.end();
        }
    }
}

fn shift_range(range: TextRange, removed: usize, inserted: usize) -> TextRange {
    TextRange::new(
        shift_offset(range.start(), removed, inserted),
        shift_offset(range.end(), removed, inserted),
    )
}

fn shift_offset(offset: ByteOffset, removed: usize, inserted: usize) -> ByteOffset {
    if inserted >= removed {
        ByteOffset::new(offset.get() + inserted - removed)
    } else {
        ByteOffset::new(offset.get() - (removed - inserted))
    }
}

fn map_start(offset: ByteOffset, old: TextRange, materialized_end: ByteOffset) -> ByteOffset {
    if offset <= old.start() {
        offset
    } else if offset >= old.end() {
        shift_offset(
            offset,
            old.len(),
            materialized_end.get() - old.start().get(),
        )
    } else {
        old.start()
    }
}

fn map_end(offset: ByteOffset, old: TextRange, materialized_end: ByteOffset) -> ByteOffset {
    if offset <= old.start() {
        offset
    } else if offset >= old.end() {
        shift_offset(
            offset,
            old.len(),
            materialized_end.get() - old.start().get(),
        )
    } else {
        materialized_end
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HistoryEntry {
    document: ComposerDocument,
}

#[derive(Clone, Debug)]
pub struct Attachment {
    pub label: String,
    block: ContentBlock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowBreak {
    Soft,
    Hard,
    End,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct VisualRow {
    source: TextRange,
    break_kind: RowBreak,
    display_width: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ComposerLayout {
    rows: Vec<VisualRow>,
    cursor_row: usize,
    cursor_column: usize,
}

impl ComposerLayout {
    fn new(text: &str, cursor: ByteOffset, width: usize) -> Self {
        assert!(is_grapheme_boundary(text, cursor));
        let content_width = width.max(GUTTER_W + 1) - GUTTER_W;
        let mut rows = Vec::new();
        let mut row_start = ByteOffset::ZERO;
        let mut row_width = 0;

        for (range, grapheme) in grapheme_ranges(text) {
            if grapheme.ends_with('\n') {
                rows.push(VisualRow {
                    source: TextRange::new(row_start, range.start()),
                    break_kind: RowBreak::Hard,
                    display_width: row_width,
                });
                row_start = range.end();
                row_width = 0;
                continue;
            }

            let grapheme_width = display_width(grapheme);
            if row_width + grapheme_width > content_width && range.start() > row_start {
                rows.push(VisualRow {
                    source: TextRange::new(row_start, range.start()),
                    break_kind: RowBreak::Soft,
                    display_width: row_width,
                });
                row_start = range.start();
                row_width = 0;
            }
            row_width += grapheme_width;
        }
        if row_width == content_width && row_start.get() < text.len() {
            rows.push(VisualRow {
                source: TextRange::new(row_start, ByteOffset::new(text.len())),
                break_kind: RowBreak::Soft,
                display_width: row_width,
            });
            row_start = ByteOffset::new(text.len());
            row_width = 0;
        }
        rows.push(VisualRow {
            source: TextRange::new(row_start, ByteOffset::new(text.len())),
            break_kind: RowBreak::End,
            display_width: row_width,
        });

        let cursor_row = rows
            .iter()
            .position(|row| {
                row.source.contains(cursor)
                    || (row.break_kind != RowBreak::Soft && row.source.end() == cursor)
            })
            .unwrap_or(rows.len() - 1);
        let row = &rows[cursor_row];
        let cursor_column = display_column(
            &text[row.source.start().get()..row.source.end().get()],
            ByteOffset::new(cursor.get().saturating_sub(row.source.start().get())),
        );
        Self {
            rows,
            cursor_row,
            cursor_column,
        }
    }

    fn offset_at_column(&self, text: &str, row: usize, column: usize) -> ByteOffset {
        let source = self.rows[row].source;
        let relative =
            byte_offset_at_display_column(&text[source.start().get()..source.end().get()], column);
        ByteOffset::new(source.start().get() + relative.get())
    }
}

pub struct View {
    pub rows: Vec<Line<'static>>,
    pub cursor_row: u16,
    pub cursor_col: u16,
}

pub struct Submission {
    pub text: String,
    pub images: Vec<ContentBlock>,
}

pub struct Composer {
    document: ComposerDocument,
    cursor: ByteOffset,
    history: Vec<HistoryEntry>,
    hist: Option<usize>,
    draft: ComposerDocument,
    attachments: Vec<Attachment>,
    goal_column: Option<usize>,
    next_paste_id: u64,
}

impl Composer {
    pub fn new() -> Self {
        Self {
            document: ComposerDocument::default(),
            cursor: ByteOffset::ZERO,
            history: Vec::new(),
            hist: None,
            draft: ComposerDocument::default(),
            attachments: Vec::new(),
            goal_column: None,
            next_paste_id: 1,
        }
    }

    pub fn text(&self) -> &str {
        self.document.text()
    }

    pub fn is_blank(&self) -> bool {
        self.text().trim().is_empty() && self.attachments.is_empty()
    }

    pub fn cursor(&self) -> ByteOffset {
        self.cursor
    }

    pub fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }

    pub fn insert_char(&mut self, character: char) {
        self.begin_edit();
        self.cursor = self
            .document
            .replace(TextRange::empty(self.cursor), &character.to_string());
        self.goal_column = None;
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    pub fn paste(&mut self, text: &str) {
        self.begin_edit();
        let character_count = text.chars().count();
        let is_large = character_count >= PASTE_CHARS || text.split('\n').count() >= PASTE_LINES;
        if is_large {
            let id = PasteId(self.next_paste_id);
            self.next_paste_id = self
                .next_paste_id
                .checked_add(1)
                .expect("paste id high-water exhausted");
            let label = format!("[Pasted #{}: {character_count} chars]", id.0);
            self.cursor = self
                .document
                .insert_atom(self.cursor, id, label, text.to_string());
        } else {
            self.cursor = self.document.replace(TextRange::empty(self.cursor), text);
        }
        self.goal_column = None;
    }

    pub fn replace_range(
        &mut self,
        range: TextRange,
        expected_cursor: ByteOffset,
        replacement: &str,
    ) -> bool {
        if self.cursor != expected_cursor || range.end() != expected_cursor {
            return false;
        }
        if range.end().get() > self.text().len()
            || !is_grapheme_boundary(self.text(), range.start())
            || !is_grapheme_boundary(self.text(), range.end())
        {
            return false;
        }
        self.begin_edit();
        self.cursor = self.document.replace(range, &format!("{replacement} "));
        self.goal_column = None;
        true
    }

    pub fn attach_image(&mut self, label: String, block: ContentBlock) {
        self.attachments.push(Attachment { label, block });
    }

    pub fn backspace(&mut self) {
        self.begin_edit();
        if let Some(index) = self.document.atom_ending_at(self.cursor) {
            self.cursor = self.document.delete_atom(index);
        } else if self.cursor > ByteOffset::ZERO {
            let previous = previous_grapheme_boundary(self.text(), self.cursor);
            self.cursor = self
                .document
                .replace(TextRange::new(previous, self.cursor), "");
        }
        self.goal_column = None;
    }

    pub fn delete(&mut self) {
        self.begin_edit();
        if let Some(index) = self.document.atom_starting_at(self.cursor) {
            self.cursor = self.document.delete_atom(index);
        } else if self.cursor.get() < self.text().len() {
            let next = next_grapheme_boundary(self.text(), self.cursor);
            self.cursor = self.document.replace(TextRange::new(self.cursor, next), "");
        }
        self.goal_column = None;
    }

    pub fn left(&mut self) {
        if let Some(index) = self.document.atom_ending_at(self.cursor) {
            self.cursor = self.document.atoms[index].range.start();
        } else {
            self.cursor = previous_grapheme_boundary(self.text(), self.cursor);
        }
        self.goal_column = None;
    }

    pub fn right(&mut self) {
        if let Some(index) = self.document.atom_starting_at(self.cursor) {
            self.cursor = self.document.atoms[index].range.end();
        } else {
            self.cursor = next_grapheme_boundary(self.text(), self.cursor);
        }
        self.goal_column = None;
    }

    pub fn home(&mut self) {
        let prefix = &self.text()[..self.cursor.get()];
        self.cursor = prefix
            .rfind('\n')
            .map(|index| ByteOffset::new(index + 1))
            .unwrap_or(ByteOffset::ZERO);
        self.goal_column = None;
    }

    pub fn end(&mut self) {
        let suffix = &self.text()[self.cursor.get()..];
        let raw = suffix
            .find('\n')
            .map(|index| self.cursor.get() + index)
            .unwrap_or_else(|| self.text().len());
        self.cursor = if is_grapheme_boundary(self.text(), ByteOffset::new(raw)) {
            ByteOffset::new(raw)
        } else {
            previous_grapheme_boundary(self.text(), ByteOffset::new(raw + 1))
        };
        self.goal_column = None;
    }

    pub fn up(&mut self, width: usize) {
        let layout = ComposerLayout::new(self.text(), self.cursor, width);
        if layout.cursor_row == 0 {
            self.history_prev();
        } else {
            let goal = self.goal_column.unwrap_or(layout.cursor_column);
            self.cursor = layout.offset_at_column(self.text(), layout.cursor_row - 1, goal);
            self.goal_column = Some(goal);
        }
    }

    pub fn down(&mut self, width: usize) {
        let layout = ComposerLayout::new(self.text(), self.cursor, width);
        if layout.cursor_row + 1 == layout.rows.len() {
            self.history_next();
        } else {
            let goal = self.goal_column.unwrap_or(layout.cursor_column);
            self.cursor = layout.offset_at_column(self.text(), layout.cursor_row + 1, goal);
            self.goal_column = Some(goal);
        }
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.hist {
            None => {
                self.draft = self.document.clone();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(index) => index - 1,
        };
        self.load_history(index);
    }

    fn history_next(&mut self) {
        match self.hist {
            None => {}
            Some(index) if index + 1 < self.history.len() => self.load_history(index + 1),
            Some(_) => {
                self.hist = None;
                self.document = std::mem::take(&mut self.draft);
                self.cursor = ByteOffset::new(self.text().len());
                self.goal_column = None;
            }
        }
    }

    fn load_history(&mut self, index: usize) {
        self.hist = Some(index);
        self.document = self.history[index].document.clone();
        self.cursor = ByteOffset::new(self.text().len());
        self.goal_column = None;
    }

    fn begin_edit(&mut self) {
        self.hist = None;
    }

    pub fn submit(&mut self) -> Option<Submission> {
        if self.is_blank() {
            return None;
        }
        let text = self.take_text();
        let images = std::mem::take(&mut self.attachments)
            .into_iter()
            .map(|attachment| attachment.block)
            .collect();
        Some(Submission { text, images })
    }

    pub fn submit_text(&mut self) -> Option<String> {
        if self.text().trim().is_empty() {
            return None;
        }
        Some(self.take_text())
    }

    fn take_text(&mut self) -> String {
        let document = std::mem::take(&mut self.document);
        let text = document.expand();
        if !document.display.trim().is_empty()
            && self.history.last().map(|entry| &entry.document) != Some(&document)
        {
            self.history.push(HistoryEntry { document });
        }
        self.cursor = ByteOffset::ZERO;
        self.hist = None;
        self.draft = ComposerDocument::default();
        self.goal_column = None;
        text
    }

    pub fn clear(&mut self) {
        self.document = ComposerDocument::default();
        self.cursor = ByteOffset::ZERO;
        self.attachments.clear();
        self.hist = None;
        self.draft = ComposerDocument::default();
        self.goal_column = None;
    }

    pub fn view(&self, width: usize) -> View {
        let width = width.max(GUTTER_W + 1);
        if self.text().is_empty() {
            return View {
                rows: vec![Line::from(vec![
                    Span::styled(PROMPT.to_string(), Style::new().fg(Color::Cyan)),
                    Span::styled(
                        "Type a message…  (Shift+Enter for newline)".to_string(),
                        DIM,
                    ),
                ])],
                cursor_row: 0,
                cursor_col: GUTTER_W as u16,
            };
        }

        let layout = ComposerLayout::new(self.text(), self.cursor, width);
        let start = layout.cursor_row.saturating_sub(MAX_ROWS - 1);
        let end = (start + MAX_ROWS).min(layout.rows.len());
        let rows = layout.rows[start..end]
            .iter()
            .enumerate()
            .map(|(visible_index, row)| {
                let is_first = start + visible_index == 0;
                let (gutter, style) = if is_first {
                    (PROMPT, Style::new().fg(Color::Cyan))
                } else {
                    (CONT, Style::default())
                };
                Line::from(vec![
                    Span::styled(gutter.to_string(), style),
                    Span::raw(
                        self.text()[row.source.start().get()..row.source.end().get()].to_string(),
                    ),
                ])
            })
            .collect();
        let cursor_col = (layout.cursor_column + GUTTER_W).min(width - 1);
        View {
            rows,
            cursor_row: (layout.cursor_row - start) as u16,
            cursor_col: cursor_col as u16,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_segmentation::UnicodeSegmentation;

    const WIDTH: usize = 80;
    const GRAPHEMES: &[&str] = &["e\u{301}", "👨‍👩‍👧‍👦", "👩🏽‍💻", "👍🏽", "🇨🇳", "❤️", "1️⃣"];

    fn typed(text: &str) -> Composer {
        let mut composer = Composer::new();
        for character in text.chars() {
            composer.insert_char(character);
        }
        composer
    }

    fn row_texts(view: &View) -> Vec<String> {
        view.rows
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    fn image() -> ContentBlock {
        ContentBlock::Image {
            source: kloop_protocol::ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "aGk=".into(),
            },
        }
    }

    #[test]
    fn grapheme_navigation_and_deletion_keep_clusters_whole() {
        for grapheme in GRAPHEMES {
            let mut composer = typed(&format!("a{grapheme}z"));
            composer.left();
            let before_z = composer.cursor();
            composer.left();
            assert_eq!(composer.cursor(), ByteOffset::new(1), "{grapheme:?}");
            composer.right();
            assert_eq!(composer.cursor(), before_z, "{grapheme:?}");
            composer.backspace();
            assert_eq!(composer.text(), "az", "{grapheme:?}");

            let mut composer = typed(&format!("a{grapheme}z"));
            composer.home();
            composer.right();
            composer.delete();
            assert_eq!(composer.text(), "az", "{grapheme:?}");
            assert!(is_grapheme_boundary(composer.text(), composer.cursor()));
        }
    }

    #[test]
    fn insertion_that_joins_a_cluster_normalizes_cursor_to_its_end() {
        let mut composer = typed("\u{301}z");
        composer.home();
        composer.insert_char('e');
        assert_eq!(composer.text(), "e\u{301}z");
        assert_eq!(composer.cursor(), ByteOffset::new("e\u{301}".len()));
    }

    #[test]
    fn visual_navigation_wraps_and_preserves_display_goal() {
        let mut composer = typed("abcdef\n你x\nabcdefgh");
        let narrow = 6; // four content columns
        composer.left();
        let layout = ComposerLayout::new(composer.text(), composer.cursor(), narrow);
        assert_eq!((layout.cursor_row, layout.cursor_column), (4, 3));
        composer.up(narrow);
        let layout = ComposerLayout::new(composer.text(), composer.cursor(), narrow);
        assert_eq!((layout.cursor_row, layout.cursor_column), (3, 3));
        composer.up(narrow);
        let layout = ComposerLayout::new(composer.text(), composer.cursor(), narrow);
        assert_eq!((layout.cursor_row, layout.cursor_column), (2, 3));
        composer.up(narrow);
        let layout = ComposerLayout::new(composer.text(), composer.cursor(), narrow);
        assert_eq!((layout.cursor_row, layout.cursor_column), (1, 2));
        composer.up(narrow);
        let layout = ComposerLayout::new(composer.text(), composer.cursor(), narrow);
        assert_eq!((layout.cursor_row, layout.cursor_column), (0, 3));
    }

    #[test]
    fn visual_edges_recall_history_but_soft_rows_do_not() {
        let mut composer = typed("history");
        composer.submit();
        composer.paste("abcdefgh");
        composer.up(6);
        assert_eq!(composer.text(), "abcdefgh");
        assert_eq!(
            ComposerLayout::new(composer.text(), composer.cursor(), 6).cursor_row,
            1
        );
        composer.up(6);
        assert_eq!(
            ComposerLayout::new(composer.text(), composer.cursor(), 6).cursor_row,
            0
        );
        composer.up(6);
        assert_eq!(composer.text(), "history");
        composer.down(6);
        assert_eq!(composer.text(), "abcdefgh");
    }

    #[test]
    fn resize_recomputes_visual_rows_without_document_state() {
        let composer = typed("你好abcdef");
        let narrow = ComposerLayout::new(composer.text(), composer.cursor(), 6);
        let wide = ComposerLayout::new(composer.text(), composer.cursor(), 20);
        assert_eq!(narrow.rows.len(), 3);
        assert_eq!(wide.rows.len(), 1);
    }

    #[test]
    fn home_and_end_keep_hard_line_semantics() {
        let mut composer = typed("abcdef\nxy");
        composer.home();
        assert_eq!(composer.cursor(), ByteOffset::new(7));
        composer.home();
        assert_eq!(composer.cursor(), ByteOffset::new(7));
        composer.end();
        assert_eq!(composer.cursor(), ByteOffset::new(9));
        composer.left();
        composer.up(6);
        assert_eq!(composer.text(), "abcdef\nxy");
    }

    #[test]
    fn history_adopts_edits_and_restores_full_draft_document() {
        let mut composer = typed("first");
        composer.submit();
        let pasted = "x".repeat(500);
        composer.paste(&pasted);
        composer.up(WIDTH);
        assert_eq!(composer.text(), "first");
        composer.down(WIDTH);
        assert!(composer.text().starts_with("[Pasted #1:"));
        assert_eq!(composer.submit().unwrap().text, pasted);

        composer.up(WIDTH);
        composer.insert_char('!');
        composer.down(WIDTH);
        assert!(composer.text().ends_with('!'));
    }

    #[test]
    fn paste_atoms_expand_exactly_once_without_literal_collision() {
        let mut composer = typed("literal [Pasted #1: 500 chars] ");
        let first = "x".repeat(500);
        let second = format!("{} tail", "[Pasted #2: 500 chars]");
        composer.paste(&first);
        composer.paste(&second.repeat(20));
        let submission = composer.submit().unwrap().text;
        assert!(submission.starts_with("literal [Pasted #1: 500 chars] "));
        assert!(submission.contains(&first));
        assert!(submission.ends_with(&second.repeat(20)));
    }

    #[test]
    fn large_paste_at_an_atom_boundary_inserts_a_distinct_atom() {
        let first = "x".repeat(500);
        let second = "y".repeat(500);
        let mut composer = Composer::new();
        composer.paste(&first);
        composer.left();
        composer.paste(&second);

        assert_eq!(composer.document.atoms.len(), 2);
        assert_eq!(composer.submit().unwrap().text, format!("{second}{first}"));
    }

    #[test]
    fn adjacent_delete_removes_a_whole_paste_atom() {
        let big = "x".repeat(500);
        let mut composer = typed("a");
        composer.paste(&big);
        composer.backspace();
        assert_eq!(composer.text(), "a");

        composer.paste(&big);
        composer.home();
        composer.right();
        composer.delete();
        assert_eq!(composer.text(), "a");
    }

    #[test]
    fn cutting_an_atom_materializes_payload_before_editing() {
        let big = "abcdef".repeat(100);
        let mut document = ComposerDocument::default();
        let cursor = document.insert_atom(
            ByteOffset::ZERO,
            PasteId(1),
            "[Pasted #1: 600 chars]".into(),
            big.clone(),
        );
        let cut = TextRange::new(ByteOffset::ZERO, cursor);
        let cursor = document.replace(cut, "replacement");
        assert_eq!(document.text(), "replacement");
        assert_eq!(cursor, ByteOffset::new("replacement".len()));
        assert!(document.atoms.is_empty());
    }

    #[test]
    fn paste_id_high_water_survives_submit_and_clear() {
        let mut composer = Composer::new();
        composer.paste(&"x".repeat(500));
        assert!(composer.text().contains("#1"));
        composer.submit();
        composer.paste(&"y".repeat(500));
        assert!(composer.text().contains("#2"));
        composer.clear();
        composer.paste(&"z".repeat(500));
        assert!(composer.text().contains("#3"));
    }

    #[test]
    fn image_attachment_rides_submit_and_survives_steer() {
        let block = image();
        let mut composer = Composer::new();
        composer.attach_image("shot.png".into(), block.clone());
        assert_eq!(composer.attachments()[0].label, "shot.png");
        assert!(!composer.is_blank());
        assert!(composer.submit_text().is_none());
        composer.paste("keep going");
        assert_eq!(composer.submit_text().as_deref(), Some("keep going"));
        assert_eq!(composer.attachments()[0].label, "shot.png");
        assert_eq!(composer.submit().unwrap().images, vec![block]);
        assert!(composer.attachments().is_empty());
    }

    #[test]
    fn blank_submit_returns_none() {
        let mut composer = Composer::new();
        assert!(composer.submit().is_none());
        composer.paste("   ");
        assert!(composer.submit().is_none());
    }

    #[test]
    fn canonical_layout_drives_view_and_cursor_window() {
        let mut composer = typed("abcdef");
        let view = composer.view(6);
        assert_eq!(row_texts(&view), vec!["› abcd", "  ef"]);
        assert_eq!((view.cursor_row, view.cursor_col), (1, 4));
        composer.home();
        assert_eq!(
            (composer.view(6).cursor_row, composer.view(6).cursor_col),
            (0, 2)
        );

        let mut tall = Composer::new();
        for index in 0..20 {
            tall.paste(&format!("line{index}"));
            tall.insert_newline();
        }
        let view = tall.view(40);
        assert_eq!(view.rows.len(), MAX_ROWS);
        assert_eq!(view.cursor_row, (MAX_ROWS - 1) as u16);
    }

    #[test]
    fn exact_width_projects_cursor_to_the_next_visual_row() {
        let mut composer = typed("abcd");
        let exact = composer.view(6);
        assert_eq!(row_texts(&exact), vec!["› abcd", "  "]);
        assert_eq!((exact.cursor_row, exact.cursor_col), (1, 2));

        composer.left();
        assert_eq!(
            (composer.view(6).cursor_row, composer.view(6).cursor_col),
            (0, 5)
        );
        composer.right();
        assert_eq!(
            (composer.view(6).cursor_row, composer.view(6).cursor_col),
            (1, 2)
        );

        let narrow = typed("你").view(3);
        assert!(narrow.cursor_col < 3);
        assert_eq!(row_texts(&narrow), vec!["› 你"]);
    }

    #[test]
    fn layout_ranges_reconstruct_display_across_hard_and_soft_breaks() {
        let composer = typed("abcdef\n你好");
        let layout = ComposerLayout::new(composer.text(), composer.cursor(), 6);
        let mut reconstructed = String::new();
        for row in &layout.rows {
            reconstructed
                .push_str(&composer.text()[row.source.start().get()..row.source.end().get()]);
            if row.break_kind == RowBreak::Hard {
                let next = composer.text()[row.source.end().get()..]
                    .graphemes(true)
                    .next()
                    .expect("hard break has separator");
                reconstructed.push_str(next);
            }
        }
        assert_eq!(reconstructed, composer.text());
    }
}
