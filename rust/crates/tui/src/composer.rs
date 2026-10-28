//! The multi-line input composer (plan 38 slice 3, hardened in plan 76).
//!
//! The document uses UTF-8 byte ranges, while editing moves only across extended
//! grapheme boundaries. Rendering and vertical navigation consume one canonical
//! visual-row layout. Large pastes are range-addressed atoms: their visible label
//! is only a projection, and submission expands the exact atom once. Images are
//! atoms too (plan 242): `[Image #N]` stays in the submitted text, and the
//! images ride after it in the order their placeholders appear.

use std::sync::Arc;

use kloop_protocol::ImageSource;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::text_layout::ByteOffset;
use crate::text_layout::TextRange;
use crate::text_layout::byte_offset_at_display_column;
use crate::text_layout::display_column;
use crate::text_layout::display_width;
use crate::text_layout::grapheme_ranges;
use crate::text_layout::is_grapheme_boundary;
use crate::text_layout::next_grapheme_boundary;
use crate::text_layout::previous_grapheme_boundary;
use crate::text_layout::snap_grapheme_boundary;
use crate::text_layout::truncate;

const PROMPT: &str = "› ";
const GUTTER_W: usize = 2;
const MAX_ROWS: usize = 8;
const PASTE_CHARS: usize = 400;
const PASTE_LINES: usize = 5;
/// Display columns of a large paste's first line shown in its label. The
/// character count alone did not show that a clipboard held only the tail of
/// what the user meant to copy; the opening words do, before it is sent.
const PASTE_PREVIEW_COLS: usize = 24;

const DIM: Style = Style::new().add_modifier(Modifier::DIM);
/// Atoms (images and large pastes) stand apart from typed text.
const ATOM: Style = Style::new().fg(Color::Cyan);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PasteId(u64);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Atom {
    range: TextRange,
    label: String,
    payload: AtomPayload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AtomPayload {
    Paste {
        id: PasteId,
        content: String,
    },
    /// 编号在一次输入里按粘贴先后递增，不复用也不重编：删掉一张，其余编号
    /// 不变，用户才认得出删的是哪张。图片数据用 `Arc` 共享，草稿进历史、翻
    /// 历史都不复制 base64。
    Image {
        number: usize,
        source: Arc<ImageSource>,
    },
}

impl Atom {
    /// What an edit cutting into the atom leaves behind. An image has no text
    /// form, so cutting its label drops the image with it.
    fn materialized(&self) -> &str {
        match &self.payload {
            AtomPayload::Paste { content, .. } => content,
            AtomPayload::Image { .. } => "",
        }
    }
}

fn image_label(number: usize) -> String {
    format!("[Image #{number}]")
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ComposerDocument {
    display: String,
    atoms: Vec<Atom>,
    /// The highest image number this input has handed out. It lives on the
    /// draft, so a new input starts again at #1 while history recall and a
    /// restored draft carry on from their own numbers.
    image_high_water: usize,
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
            match &atom.payload {
                AtomPayload::Paste { content, .. } => expanded.push_str(content),
                AtomPayload::Image { .. } => expanded.push_str(&atom.label),
            }
            copied = atom.range.end().get();
        }
        expanded.push_str(&self.display[copied..]);
        expanded
    }

    /// The images in the order their placeholders appear, each with its
    /// number: sent in this order, the k-th image is the k-th `[Image #N]` a
    /// reader meets in the text.
    fn images(&self) -> Vec<(usize, Arc<ImageSource>)> {
        self.atoms
            .iter()
            .filter_map(|atom| match &atom.payload {
                AtomPayload::Image { number, source } => Some((*number, Arc::clone(source))),
                AtomPayload::Paste { .. } => None,
            })
            .collect()
    }

    fn image_count(&self) -> usize {
        self.atoms
            .iter()
            .filter(|atom| matches!(atom.payload, AtomPayload::Image { .. }))
            .count()
    }

    /// The display split into word-scan cells: one per grapheme, but a paste
    /// atom is a single cell, so a scan never stops inside a label.
    fn cells(&self) -> Vec<Cell> {
        let mut cells = Vec::new();
        let mut copied = 0;
        for atom in &self.atoms {
            push_text_cells(
                &mut cells,
                &self.display[copied..atom.range.start().get()],
                copied,
            );
            cells.push(Cell {
                range: atom.range,
                class: CellClass::Atom,
            });
            copied = atom.range.end().get();
        }
        push_text_cells(&mut cells, &self.display[copied..], copied);
        cells
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

    fn insert_atom(&mut self, at: ByteOffset, label: String, payload: AtomPayload) -> ByteOffset {
        self.assert_boundary(at);
        debug_assert!(
            self.atoms
                .iter()
                .all(|atom| at <= atom.range.start() || at >= atom.range.end())
        );

        let mut projected = self.display.clone();
        projected.insert_str(at.get(), &label);
        let end = ByteOffset::new(at.get() + label.len());
        let fits_start = is_grapheme_boundary(&projected, at);
        let fits_end = is_grapheme_boundary(&projected, end);
        if !fits_start || !fits_end {
            match payload {
                AtomPayload::Paste { content, .. } => {
                    return self.replace(TextRange::empty(at), &content);
                }
                // 图片没有文字形态可退：在缺边界的一侧补一个空格，标签就落在
                // 字素边界上(前面是 Prepend 字符，或后面紧跟组合符时)。
                AtomPayload::Image { .. } => {
                    let mut at = at;
                    if !fits_start {
                        at = self.replace_raw(TextRange::empty(at), " ");
                    }
                    if !fits_end {
                        self.replace_raw(TextRange::empty(at), " ");
                    }
                    return self.insert_atom(at, label, payload);
                }
            }
        }

        let cursor = self.replace(TextRange::empty(at), &label);
        debug_assert_eq!(cursor, end);
        if let AtomPayload::Image { number, .. } = &payload {
            self.image_high_water = self.image_high_water.max(*number);
        }
        self.atoms.push(Atom {
            range: TextRange::new(at, end),
            label,
            payload,
        });
        self.atoms.sort_by_key(|atom| atom.range.start());
        self.assert_invariants();
        cursor
    }

    fn replace(&mut self, range: TextRange, replacement: &str) -> ByteOffset {
        self.assert_range(range);
        let range = self.materialize_intersections(range);
        let mut cursor = self.replace_raw(range, replacement);
        // 先隔开图片再对齐光标：否则光标会被对齐进与标签粘成一体的字素里。
        while let Some(index) = self.atoms.iter().position(|atom| {
            matches!(atom.payload, AtomPayload::Image { .. }) && self.breaks_boundary(atom)
        }) {
            cursor = self.separate_image(index, cursor);
        }
        if !is_grapheme_boundary(&self.display, cursor) {
            let snapped = snap_grapheme_boundary(&self.display, cursor);
            cursor = if snapped < cursor {
                next_grapheme_boundary(&self.display, cursor)
            } else {
                snapped
            };
        }

        while let Some(index) = self
            .atoms
            .iter()
            .position(|atom| self.breaks_boundary(atom))
        {
            cursor = match self.atoms[index].payload {
                AtomPayload::Paste { .. } => self.materialize_atom(index, cursor),
                AtomPayload::Image { .. } => self.separate_image(index, cursor),
            };
        }
        self.assert_invariants();
        cursor
    }

    fn breaks_boundary(&self, atom: &Atom) -> bool {
        !is_grapheme_boundary(&self.display, atom.range.start())
            || !is_grapheme_boundary(&self.display, atom.range.end())
    }

    /// An edit glued a neighbour onto an image label (a combining mark after
    /// it, a Prepend character before it). An image has no text to expand
    /// into, so a space goes on the glued side instead and the image stays.
    /// The cursor ends on a boundary: after the space when it was past the
    /// gap, and never between a Prepend character and its space.
    fn separate_image(&mut self, index: usize, mut cursor: ByteOffset) -> ByteOffset {
        let end = self.atoms[index].range.end();
        if !is_grapheme_boundary(&self.display, end) {
            self.replace_raw(TextRange::empty(end), " ");
            if cursor > end {
                cursor = ByteOffset::new(cursor.get() + 1);
            }
        }
        let start = self.atoms[index].range.start();
        if !is_grapheme_boundary(&self.display, start) {
            self.replace_raw(TextRange::empty(start), " ");
            if cursor >= start {
                cursor = ByteOffset::new(cursor.get() + 1);
            }
        }
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
            let materialized_end = ByteOffset::new(old.start().get() + atom.materialized().len());
            let start = map_start(range.start(), old, materialized_end);
            let end = map_end(range.end(), old, materialized_end);
            self.replace_raw(old, atom.materialized());
            range = TextRange::new(start, end);
        }
        range
    }

    fn materialize_atom(&mut self, index: usize, cursor: ByteOffset) -> ByteOffset {
        let atom = self.atoms.remove(index);
        let old = atom.range;
        let materialized_end = ByteOffset::new(old.start().get() + atom.materialized().len());
        let cursor = map_end(cursor, old, materialized_end);
        self.replace_raw(old, atom.materialized());
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
        let mut numbers: Vec<usize> = self
            .atoms
            .iter()
            .filter_map(|atom| match atom.payload {
                AtomPayload::Image { number, .. } => Some(number),
                AtomPayload::Paste { .. } => None,
            })
            .collect();
        numbers.sort_unstable();
        debug_assert!(numbers.windows(2).all(|pair| pair[0] < pair[1]));
        debug_assert!(
            numbers
                .last()
                .is_none_or(|&last| last <= self.image_high_water)
        );
    }
}

/// The indivisible units a word scan steps over: one cell per grapheme, and
/// one cell for a whole paste atom however long its label is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CellClass {
    Whitespace,
    Word,
    Punct,
    Atom,
}

#[derive(Clone, Copy, Debug)]
struct Cell {
    range: TextRange,
    class: CellClass,
}

/// A grapheme is a word when it starts with an alphanumeric or `_`, whitespace
/// when it starts with a whitespace character (so `\n` is a word-scan
/// separator, and word motion crosses hard lines), and punctuation otherwise.
/// Classifying by the first scalar keeps decomposed clusters like `e`+U+0301 in
/// the same class as their precomposed form.
fn grapheme_class(grapheme: &str) -> CellClass {
    match grapheme.chars().next() {
        Some(character) if character == '_' || character.is_alphanumeric() => CellClass::Word,
        Some(character) if character.is_whitespace() => CellClass::Whitespace,
        _ => CellClass::Punct,
    }
}

fn push_text_cells(cells: &mut Vec<Cell>, text: &str, base: usize) {
    for (range, grapheme) in grapheme_ranges(text) {
        cells.push(Cell {
            range: TextRange::new(
                ByteOffset::new(base + range.start().get()),
                ByteOffset::new(base + range.end().get()),
            ),
            class: grapheme_class(grapheme),
        });
    }
}

/// The index of the first cell starting at or after `offset`; `cells.len()` when
/// `offset` is at or past the end.
fn cell_boundary(cells: &[Cell], offset: ByteOffset) -> usize {
    cells
        .iter()
        .position(|cell| cell.range.start() >= offset)
        .unwrap_or(cells.len())
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

fn paste_label(id: PasteId, text: &str, character_count: usize) -> String {
    match paste_preview(text) {
        Some(preview) => format!("[Pasted #{}: {character_count} chars · {preview}]", id.0),
        None => format!("[Pasted #{}: {character_count} chars]", id.0),
    }
}

/// The first non-blank line on one row: whitespace runs become one space and
/// control characters are dropped, so the label never breaks or styles the row.
fn paste_preview(text: &str) -> Option<String> {
    text.lines()
        .map(|line| {
            line.split_whitespace()
                .map(|word| word.chars().filter(|c| !c.is_control()).collect::<String>())
                .filter(|word| !word.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .find(|line| !line.is_empty())
        .map(|line| {
            if display_width(&line) <= PASTE_PREVIEW_COLS {
                return line;
            }
            let cut = truncate(&line, PASTE_PREVIEW_COLS);
            let kept = cut.strip_suffix('…').unwrap_or(&cut);
            format!("{}…", kept.trim_end())
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HistoryEntry {
    document: ComposerDocument,
}

/// The draft exactly as it was submitted, images in place: what goes back into
/// the composer when the turn is interrupted before the model produced anything.
#[derive(Clone, Debug)]
pub struct Draft(ComposerDocument);

impl Draft {
    /// The text that was sent for this draft.
    pub fn text(&self) -> String {
        self.0.expand()
    }

    pub fn image_count(&self) -> usize {
        self.0.image_count()
    }
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub rows: Vec<Line<'static>>,
    pub cursor_row: u16,
    pub cursor_col: u16,
}

pub struct Submission {
    pub text: String,
    /// In placeholder order: the k-th is the k-th `[Image #N]` in `text`.
    pub images: Vec<ImageSource>,
    pub draft: Draft,
}

/// One composer edit reachable from a key. `app.rs` owns which key maps to which
/// action; `Composer` owns what the action does, so the key table can be
/// asserted exhaustively without a terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComposerAction {
    Insert(char),
    MoveLeft,
    MoveRight,
    MoveWordLeft,
    MoveWordRight,
    MoveLineStart,
    MoveLineEnd,
    MoveUp,
    MoveDown,
    DeleteBackward,
    DeleteForward,
}

#[derive(Debug)]
pub struct Composer {
    document: ComposerDocument,
    cursor: ByteOffset,
    history: Vec<HistoryEntry>,
    hist: Option<usize>,
    draft: ComposerDocument,
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
            goal_column: None,
            next_paste_id: 1,
        }
    }

    pub fn text(&self) -> &str {
        self.document.text()
    }

    pub fn is_blank(&self) -> bool {
        self.text().trim().is_empty()
    }

    pub fn cursor(&self) -> ByteOffset {
        self.cursor
    }

    pub fn insert_char(&mut self, character: char) {
        self.insert_text(&character.to_string());
    }

    pub(crate) fn insert_text(&mut self, text: &str) {
        self.begin_edit();
        self.cursor = self.document.replace(TextRange::empty(self.cursor), text);
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
            let label = paste_label(id, text, character_count);
            self.cursor = self.document.insert_atom(
                self.cursor,
                label,
                AtomPayload::Paste {
                    id,
                    content: text.to_string(),
                },
            );
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

    /// Insert `[Image #N]` at the cursor, N one past every number this input has
    /// handed out, so a deleted image's number never comes back within it.
    pub fn attach_image(&mut self, source: ImageSource) {
        let number = self.document.image_high_water + 1;
        self.insert_image(number, Arc::new(source));
    }

    fn insert_image(&mut self, number: usize, source: Arc<ImageSource>) {
        self.begin_edit();
        self.cursor = self.document.insert_atom(
            self.cursor,
            image_label(number),
            AtomPayload::Image { number, source },
        );
        self.goal_column = None;
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

    pub fn word_left(&mut self) {
        self.cursor = self.word_left_offset();
        self.goal_column = None;
    }

    pub fn word_right(&mut self) {
        self.cursor = self.word_right_offset();
        self.goal_column = None;
    }

    /// The offset a leftward word step lands on.
    fn word_left_offset(&self) -> ByteOffset {
        let cells = self.document.cells();
        let mut index = cell_boundary(&cells, self.cursor);
        while index > 0 && cells[index - 1].class == CellClass::Whitespace {
            index -= 1;
        }
        if index == 0 {
            return ByteOffset::ZERO;
        }
        let class = cells[index - 1].class;
        while index > 0 && cells[index - 1].class == class {
            index -= 1;
        }
        cells[index].range.start()
    }

    /// The offset a rightward word step lands on; symmetric to
    /// [`Composer::word_left_offset`].
    fn word_right_offset(&self) -> ByteOffset {
        let cells = self.document.cells();
        let mut index = cell_boundary(&cells, self.cursor);
        while index < cells.len() && cells[index].class == CellClass::Whitespace {
            index += 1;
        }
        if index == cells.len() {
            return ByteOffset::new(self.text().len());
        }
        let class = cells[index].class;
        while index < cells.len() && cells[index].class == class {
            index += 1;
        }
        cells[index - 1].range.end()
    }

    pub(crate) fn apply(&mut self, action: ComposerAction, width: usize) {
        match action {
            ComposerAction::Insert(character) => self.insert_char(character),
            ComposerAction::MoveLeft => self.left(),
            ComposerAction::MoveRight => self.right(),
            ComposerAction::MoveWordLeft => self.word_left(),
            ComposerAction::MoveWordRight => self.word_right(),
            ComposerAction::MoveLineStart => self.home(),
            ComposerAction::MoveLineEnd => self.end(),
            ComposerAction::MoveUp => self.up(width),
            ComposerAction::MoveDown => self.down(width),
            ComposerAction::DeleteBackward => self.backspace(),
            ComposerAction::DeleteForward => self.delete(),
        }
    }

    pub(crate) fn apply_with_prefix(&mut self, action: ComposerAction, width: usize, prefix: &str) {
        let prefix = truncate(prefix, width.saturating_sub(1));
        self.apply(
            action,
            width.saturating_sub(display_width(&prefix)) + GUTTER_W,
        );
    }

    pub fn up(&mut self, width: usize) {
        let layout = ComposerLayout::new(self.text(), self.cursor, width);
        if layout.cursor_row == 0 {
            self.history_prev();
        } else {
            let goal = self.goal_column.unwrap_or(layout.cursor_column);
            let landed = layout.offset_at_column(self.text(), layout.cursor_row - 1, goal);
            // The visual layout reads only the display text, so a row inside a
            // wrapped paste-atom label is a position it will happily draw. The
            // cursor must never rest there (it is not a byte any edit can use),
            // so stepping up snaps to the atom's start.
            self.cursor = self
                .atom_containing(landed)
                .map_or(landed, |atom| atom.start());
            self.goal_column = Some(goal);
        }
    }

    pub fn down(&mut self, width: usize) {
        let layout = ComposerLayout::new(self.text(), self.cursor, width);
        if layout.cursor_row + 1 == layout.rows.len() {
            self.history_next();
        } else {
            let goal = self.goal_column.unwrap_or(layout.cursor_column);
            let landed = layout.offset_at_column(self.text(), layout.cursor_row + 1, goal);
            self.cursor = self
                .atom_containing(landed)
                .map_or(landed, |atom| atom.end());
            self.goal_column = Some(goal);
        }
    }

    /// The range of the paste atom strictly containing `offset`, if any. The
    /// cursor is never left there: a label is one indivisible unit, and the
    /// label-only visual layout is the one place that can invent such a byte.
    fn atom_containing(&self, offset: ByteOffset) -> Option<TextRange> {
        self.document
            .atoms
            .iter()
            .map(|atom| atom.range)
            .find(|range| range.start() < offset && offset < range.end())
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
        let draft = Draft(self.document.clone());
        let images = self
            .document
            .images()
            .into_iter()
            .map(|(_, source)| ImageSource::clone(&source))
            .collect();
        let text = self.take_text();
        Some(Submission {
            text,
            images,
            draft,
        })
    }

    /// A slash command's line. Commands take text only, so the images are left
    /// out of it and stay behind as the next draft.
    pub fn submit_command(&mut self) -> Option<String> {
        if self.is_blank() {
            return None;
        }
        let images = self.document.images();
        let high_water = self.document.image_high_water;
        let mut command = self.document.clone();
        while let Some(index) = command
            .atoms
            .iter()
            .position(|atom| matches!(atom.payload, AtomPayload::Image { .. }))
        {
            command.delete_atom(index);
        }
        self.take_text();
        for (number, source) in images {
            self.insert_image(number, source);
        }
        // 留下的图仍是这次输入：已删掉的号也不能在它上面复用。
        self.document.image_high_water = high_water;
        Some(command.expand())
    }

    /// Put a submitted draft back, cursor at its end.
    pub fn restore(&mut self, draft: Draft) {
        self.document = draft.0;
        self.cursor = ByteOffset::new(self.text().len());
        self.hist = None;
        self.goal_column = None;
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

        self.view_with_prefix(width, PROMPT, Style::new().fg(Color::Cyan))
    }

    /// One visual row's text, atom pieces styled: a label wrapped across rows is
    /// styled on each row it reaches.
    fn row_spans(&self, source: TextRange) -> Vec<Span<'static>> {
        let text = self.text();
        let end = source.end().get();
        let mut at = source.start().get();
        let mut spans = Vec::new();
        for atom in &self.document.atoms {
            let start = atom.range.start().get().max(at);
            let stop = atom.range.end().get().min(end);
            if start >= stop {
                continue;
            }
            if at < start {
                spans.push(Span::raw(text[at..start].to_string()));
            }
            spans.push(Span::styled(text[start..stop].to_string(), ATOM));
            at = stop;
        }
        if at < end || spans.is_empty() {
            spans.push(Span::raw(text[at..end].to_string()));
        }
        spans
    }

    pub(crate) fn view_with_prefix(&self, width: usize, prefix: &str, style: Style) -> View {
        if width == 0 {
            return View {
                rows: Vec::new(),
                cursor_row: 0,
                cursor_col: 0,
            };
        }
        let prefix = truncate(prefix, width - 1);
        let gutter_width = display_width(&prefix);
        let continuation = " ".repeat(gutter_width);
        let layout = ComposerLayout::new(self.text(), self.cursor, width - gutter_width + GUTTER_W);
        let start = layout.cursor_row.saturating_sub(MAX_ROWS - 1);
        let end = (start + MAX_ROWS).min(layout.rows.len());
        let rows = layout.rows[start..end]
            .iter()
            .enumerate()
            .map(|(visible_index, row)| {
                let is_first = start + visible_index == 0;
                let (gutter, style) = if is_first {
                    (prefix.as_str(), style)
                } else {
                    (continuation.as_str(), Style::default())
                };
                let mut spans = vec![Span::styled(gutter.to_string(), style)];
                spans.extend(self.row_spans(row.source));
                Line::from(spans)
            })
            .collect();
        let cursor_col = (layout.cursor_column + gutter_width).min(width - 1);
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

    #[test]
    fn prefixed_editor_navigation_uses_the_same_rows_as_rendering() {
        let mut editor = typed("abcdef界Z");
        let prefix = "Your answer > ";
        let view = editor.view_with_prefix(20, prefix, Style::default());
        assert_eq!(
            row_texts(&view),
            vec!["Your answer > abcdef", "              界Z"]
        );
        assert_eq!((view.cursor_row, view.cursor_col), (1, 17));
        editor.apply_with_prefix(ComposerAction::MoveUp, 20, prefix);
        assert_eq!((editor.text(), editor.cursor().get()), ("abcdef界Z", 3));
        editor.apply_with_prefix(ComposerAction::MoveDown, 20, prefix);
        assert_eq!((editor.text(), editor.cursor().get()), ("abcdef界Z", 10));
        for width in 0..=20 {
            let view = editor.view_with_prefix(width, prefix, Style::default());
            if width == 0 {
                assert!(view.rows.is_empty());
            } else {
                assert!(usize::from(view.cursor_row) < view.rows.len());
                assert!(usize::from(view.cursor_col) < width);
            }
        }
    }

    fn image(data: &str) -> ImageSource {
        ImageSource::Base64 {
            media_type: "image/png".into(),
            data: data.into(),
        }
    }

    /// What a submit hands over: the text, and the images by their data.
    fn submitted(composer: &mut Composer) -> (String, Vec<String>) {
        let submission = composer.submit().expect("not blank");
        let images = submission
            .images
            .into_iter()
            .map(|ImageSource::Base64 { data, .. }| data)
            .collect();
        (submission.text, images)
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
        let first = "x".repeat(500);
        let first_label = format!("[Pasted #1: 500 chars · {}…]", "x".repeat(23));
        let mut composer = typed(&format!("literal {first_label} "));
        composer.paste(&first);
        assert_eq!(composer.document.atoms[0].label, first_label);
        let second = "[Pasted #2: 540 chars · [Pasted #2: 500 chars] t…] tail".repeat(20);
        composer.paste(&second);
        let submission = composer.submit().unwrap().text;
        assert_eq!(submission, format!("literal {first_label} {first}{second}"));
    }

    #[test]
    fn a_large_paste_label_previews_its_first_non_blank_line() {
        let more = "\nmore".repeat(5);
        let label_of = |text: &str| {
            let mut composer = Composer::new();
            composer.paste(text);
            composer.text().to_string()
        };
        let cases = [
            // Leading blank lines and indentation are skipped; a wide line is cut
            // to the preview width, with no space left before the ellipsis.
            (
                format!("\n\n  - key 名场景：切点落在 \"api_key\" 中间。{more}"),
                "- key 名场景：切点落在…",
            ),
            (format!("short{more}"), "short"),
            // A line of only control characters is blank; tabs and runs of
            // spaces fold to one space, and an escape byte never reaches the row.
            (
                format!("\u{7}\n\tfoo\t\tbar\u{1b}[31m  baz{more}"),
                "foo bar[31m baz",
            ),
        ];
        for (text, preview) in cases {
            let count = text.chars().count();
            assert_eq!(
                label_of(&text),
                format!("[Pasted #1: {count} chars · {preview}]")
            );
        }
        assert_eq!(label_of(&" ".repeat(500)), "[Pasted #1: 500 chars]");
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
            "[Pasted #1: 600 chars]".into(),
            AtomPayload::Paste {
                id: PasteId(1),
                content: big.clone(),
            },
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
    fn an_image_lands_at_the_cursor_and_submits_in_placeholder_order() {
        let mut composer = typed("ab");
        composer.left();
        composer.attach_image(image("one"));
        composer.end();
        composer.attach_image(image("two"));
        composer.home();
        composer.attach_image(image("three"));
        assert_eq!(
            (composer.text(), composer.cursor().get()),
            ("[Image #3]a[Image #1]b[Image #2]", "[Image #3]".len())
        );
        // 编号是粘贴先后;发送则按占位在正文里的先后。
        let mut copy = Composer::new();
        copy.document = composer.document.clone();
        assert_eq!(
            submitted(&mut copy),
            (
                "[Image #3]a[Image #1]b[Image #2]".into(),
                vec!["three".into(), "one".into(), "two".into()]
            )
        );
        // 相邻 Backspace 整块删掉一张，其余编号不动。
        composer.backspace();
        assert_eq!(
            submitted(&mut composer),
            (
                "a[Image #1]b[Image #2]".into(),
                vec!["one".into(), "two".into()]
            )
        );
    }

    #[test]
    fn image_numbers_stay_put_within_an_input() {
        let mut composer = Composer::new();
        for data in ["i1", "i2", "i3"] {
            composer.attach_image(image(data));
        }
        // 删掉中间的 #2:其余编号不动，删的是哪张一眼可见。
        composer.left();
        composer.backspace();
        assert_eq!(composer.text(), "[Image #1][Image #3]");
        // 删掉最大号 #3 后再粘，也不复用 #3。
        composer.end();
        composer.backspace();
        composer.attach_image(image("i4"));
        assert_eq!(
            submitted(&mut composer),
            (
                "[Image #1][Image #4]".into(),
                vec!["i1".into(), "i4".into()]
            )
        );
        // 提交或清空之后是新的一次输入，从 #1 重新开始。
        composer.attach_image(image("i5"));
        assert_eq!(composer.text(), "[Image #1]");
        composer.clear();
        composer.attach_image(image("i6"));
        assert_eq!(composer.text(), "[Image #1]");
        // 翻回历史里的那次输入，接着它自己的编号往上加。
        composer.clear();
        composer.up(WIDTH);
        composer.attach_image(image("i7"));
        assert_eq!(composer.text(), "[Image #1][Image #4][Image #5]");
    }

    #[test]
    fn delete_and_word_motion_treat_an_image_as_one_unit() {
        let mut composer = typed("go ");
        composer.attach_image(image("one"));
        composer.insert_text(" x");
        composer.word_left();
        composer.word_left();
        assert_eq!(composer.cursor().get(), "go ".len());
        composer.delete();
        assert_eq!(
            (composer.text(), composer.document.image_count()),
            ("go  x", 0)
        );
    }

    #[test]
    fn an_edit_cutting_an_image_label_drops_that_image() {
        let mut document = ComposerDocument::default();
        for (number, data) in [(1, "a"), (2, "b")] {
            let at = ByteOffset::new(document.text().len());
            document.insert_atom(
                at,
                image_label(number),
                AtomPayload::Image {
                    number,
                    source: Arc::new(image(data)),
                },
            );
        }
        let cursor = document.replace(TextRange::new(ByteOffset::ZERO, ByteOffset::new(3)), "x");
        assert_eq!(
            (document.text(), cursor.get(), document.images()),
            ("x[Image #2]", 1, vec![(2, Arc::new(image("b")))])
        );
    }

    #[test]
    fn an_image_label_never_breaks_a_grapheme() {
        // 换行后紧跟组合符：换行之后是字素边界，但标签的 `]` 会和组合符粘成一个字素。
        let mut composer = typed("\n\u{301}");
        composer.left();
        composer.attach_image(image("one"));
        assert_eq!(
            (composer.text(), composer.cursor().get()),
            ("\n[Image #1] \u{301}", "\n[Image #1]".len())
        );
        assert_eq!(composer.document.image_count(), 1);
    }

    #[test]
    fn typing_that_glues_onto_an_image_label_keeps_the_image() {
        // 标签后打组合符：`]` 会和它粘成一个字素，补空格隔开，图片不丢。
        let mut composer = Composer::new();
        composer.attach_image(image("one"));
        composer.insert_char('\u{301}');
        assert_eq!(
            (composer.text(), composer.cursor().get()),
            ("[Image #1] \u{301}", "[Image #1] \u{301}".len())
        );

        // 标签前打 Prepend 字符：它会和 `[` 粘连;光标落在空格之后、标签之前。
        let mut composer = Composer::new();
        composer.attach_image(image("one"));
        composer.home();
        composer.insert_char('\u{600}');
        assert_eq!(
            (composer.text(), composer.cursor().get()),
            ("\u{600} [Image #1]", "\u{600} ".len())
        );

        // 删掉中间的换行同样会让组合符粘上来;光标留在标签末尾。
        let mut composer = Composer::new();
        composer.attach_image(image("one"));
        composer.insert_text("\n\u{301}");
        composer.left();
        composer.left();
        composer.delete();
        assert_eq!(
            (composer.text(), composer.cursor().get()),
            ("[Image #1] \u{301}", "[Image #1]".len())
        );
        assert_eq!(
            submitted(&mut composer),
            ("[Image #1] \u{301}".into(), vec!["one".into()])
        );
    }

    #[test]
    fn history_recall_brings_the_images_back() {
        let mut composer = Composer::new();
        composer.attach_image(image("one"));
        composer.insert_text(" look");
        submitted(&mut composer);
        composer.up(WIDTH);
        assert_eq!(
            submitted(&mut composer),
            ("[Image #1] look".into(), vec!["one".into()])
        );
    }

    #[test]
    fn a_slash_command_leaves_its_images_as_the_next_draft() {
        let mut composer = typed("/review ");
        composer.attach_image(image("one"));
        composer.attach_image(image("two"));
        composer.attach_image(image("gone"));
        composer.backspace();
        composer.insert_text(" now");
        assert_eq!(composer.submit_command().as_deref(), Some("/review  now"));
        // 留下的图还属于这次输入：删掉的 #3 不复用。
        composer.attach_image(image("three"));
        assert_eq!(
            submitted(&mut composer),
            (
                "[Image #1][Image #2][Image #4]".into(),
                vec!["one".into(), "two".into(), "three".into()]
            )
        );
    }

    #[test]
    fn a_restored_draft_puts_images_back_where_they_were() {
        let mut composer = typed("a");
        composer.attach_image(image("one"));
        composer.insert_char('b');
        let draft = composer.submit().expect("not blank").draft;
        assert_eq!(
            (draft.text(), draft.image_count()),
            ("a[Image #1]b".into(), 1)
        );
        composer.restore(draft);
        assert_eq!(
            (composer.text(), composer.cursor().get()),
            ("a[Image #1]b", "a[Image #1]b".len())
        );
        assert_eq!(
            submitted(&mut composer),
            ("a[Image #1]b".into(), vec!["one".into()])
        );
    }

    #[test]
    fn atoms_are_styled_on_every_row_they_reach() {
        let mut composer = typed("a");
        composer.attach_image(image("one"));
        composer.insert_char('b');
        let prompt = Span::styled(PROMPT.to_string(), Style::new().fg(Color::Cyan));
        assert_eq!(
            composer.view(WIDTH).rows,
            vec![Line::from(vec![
                prompt.clone(),
                Span::raw("a"),
                Span::styled("[Image #1]", ATOM),
                Span::raw("b"),
            ])]
        );

        let mut wrapped = Composer::new();
        wrapped.attach_image(image("one"));
        assert_eq!(
            wrapped.view(GUTTER_W + 6).rows,
            vec![
                Line::from(vec![prompt, Span::styled("[Image", ATOM)]),
                Line::from(vec![Span::raw("  "), Span::styled(" #1]", ATOM)]),
            ]
        );
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

    #[test]
    fn word_motion_follows_character_classes() {
        let mut composer = typed("foo  bar.baz");
        // Words are alphanumeric runs, punctuation is its own class, and
        // whitespace is skipped: baz → . → bar → foo → start.
        for expected in [9, 8, 5, 0, 0] {
            composer.word_left();
            assert_eq!(composer.cursor(), ByteOffset::new(expected));
        }
        // Symmetric on the way forward: foo → bar → . → baz → end.
        for expected in [3, 8, 9, 12] {
            composer.word_right();
            assert_eq!(composer.cursor(), ByteOffset::new(expected));
        }
    }

    #[test]
    fn word_motion_crosses_hard_lines_as_whitespace() {
        let mut composer = typed("ab\ncd");
        composer.word_left();
        assert_eq!(composer.cursor(), ByteOffset::new(3));
        composer.word_left();
        assert_eq!(composer.cursor(), ByteOffset::ZERO);
        composer.word_right();
        assert_eq!(composer.cursor(), ByteOffset::new(2));
        composer.word_right();
        assert_eq!(composer.cursor(), ByteOffset::new(5));
    }

    #[test]
    fn word_motion_keeps_cjk_runs_and_emoji_graphemes_whole() {
        // A character-class word is every CJK run up to the next space or
        // punctuation, so one step crosses all four characters.
        let mut composer = typed("你好世界 abc");
        composer.word_left();
        assert_eq!(composer.cursor(), ByteOffset::new(13));
        composer.word_left();
        assert_eq!(composer.cursor(), ByteOffset::ZERO);

        let mut composer = typed("a 👩🏽‍💻");
        composer.word_left();
        assert_eq!(composer.cursor(), ByteOffset::new(2));
        assert!(is_grapheme_boundary(composer.text(), composer.cursor()));
    }

    #[test]
    fn word_motion_crosses_a_paste_atom_as_one_cell() {
        let big = "x".repeat(500);
        let mut composer = Composer::new();
        composer.paste("alpha ");
        composer.paste(&big);
        composer.paste(" beta");
        let atom = composer.document.atoms[0].range;

        // Backward: beta → (the space before "beta", then the atom as one cell).
        composer.word_left();
        assert_eq!(composer.cursor(), ByteOffset::new(atom.end().get() + 1));
        composer.word_left();
        assert_eq!(composer.cursor(), atom.start());
        composer.word_left();
        assert_eq!(composer.cursor(), ByteOffset::ZERO);

        // Forward walks back out the same way: end of "alpha", then the atom.
        composer.word_right();
        assert_eq!(composer.cursor(), ByteOffset::new(5));
        composer.word_right();
        assert_eq!(composer.cursor(), atom.end());

        // Two adjacent atoms are one run: a single step crosses both.
        let mut composer = Composer::new();
        composer.paste(&big);
        composer.paste(&big);
        assert_eq!(composer.document.atoms.len(), 2);
        composer.word_left();
        assert_eq!(composer.cursor(), ByteOffset::ZERO);
    }

    #[test]
    fn navigation_never_rests_inside_a_paste_atom() {
        let big = "x".repeat(500);
        let width = 12;
        let mut composer = Composer::new();
        composer.paste(&big);
        let atom = composer.document.atoms[0].range;

        // The label wraps over several rows; stepping up through it must land on
        // an atom edge, never on a byte the label-only layout drew.
        for _ in 0..4 {
            composer.up(width);
            let cursor = composer.cursor();
            assert!(
                cursor <= atom.start() || cursor >= atom.end(),
                "cursor rested inside the label at {}",
                cursor.get()
            );
            composer.document.assert_invariants();
        }
        assert_eq!(composer.cursor(), ByteOffset::ZERO);

        // Down from the start snaps forward to the end, not into the label.
        let mut composer = Composer::new();
        composer.paste(&big);
        composer.home();
        composer.down(width);
        assert_eq!(composer.cursor(), atom.end());

        // With the cursor on an edge a plain edit is safe: no payload splay.
        let mut composer = Composer::new();
        composer.paste(&big);
        composer.up(width);
        composer.insert_char('z');
        composer.document.assert_invariants();
        assert_eq!(composer.document.atoms.len(), 1);
        assert_eq!(composer.submit().unwrap().text, format!("z{big}"));
    }

    #[test]
    fn word_motion_is_a_no_op_on_empty_and_at_edges() {
        let mut composer = Composer::new();
        composer.word_left();
        composer.word_right();
        assert_eq!((composer.text(), composer.cursor().get()), ("", 0));

        let mut composer = typed("abc");
        composer.home();
        composer.word_left();
        assert_eq!((composer.text(), composer.cursor().get()), ("abc", 0));

        composer.end();
        composer.word_right();
        assert_eq!((composer.text(), composer.cursor().get()), ("abc", 3));
    }
}
