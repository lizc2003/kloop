//! The multi-line input composer (plan 38 slice 3).
//!
//! Replaces the old single-line input: text may contain newlines (Shift+Enter /
//! Ctrl+J insert one), the cursor moves across logical lines, and Up/Down on the
//! first/last line step through the input history instead of moving. Large
//! pastes collapse to a `[Pasted N chars]` placeholder that expands on submit;
//! pasted image files attach as blocks that ride the turn. Pure with respect to
//! the terminal — editing and the wrapped view are unit-tested without a TTY.

use kloop_protocol::ContentBlock;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

/// Left prompt on the first visual row; continuation rows align under it.
const PROMPT: &str = "› ";
const CONT: &str = "  ";
/// The prompt/continuation column width (both are two columns).
const GUTTER_W: usize = 2;
/// The composer never grows past this many rows on screen; taller input scrolls
/// to keep the cursor visible.
const MAX_ROWS: usize = 8;
/// A paste at least this many characters (or this many lines) collapses to a
/// placeholder rather than flooding the composer.
const PASTE_CHARS: usize = 400;
const PASTE_LINES: usize = 5;

const DIM: Style = Style::new().add_modifier(Modifier::DIM);

/// A large paste held out of the visible text: its placeholder shows in the
/// composer, and submit expands the placeholder back to `content`.
struct Paste {
    placeholder: String,
    content: String,
}

/// The composer's rendered layout for one width: the visible rows (already
/// prefixed and windowed to [`MAX_ROWS`]) and the cursor's position within them.
pub struct View {
    pub rows: Vec<Line<'static>>,
    pub cursor_row: u16,
    pub cursor_col: u16,
}

/// The result of a submit: the expanded text and any attached images.
pub struct Submission {
    pub text: String,
    pub images: Vec<ContentBlock>,
}

pub struct Composer {
    /// The editable text (may contain newlines and paste/`nothing`
    /// placeholders); the cursor is a char index into it.
    text: String,
    cursor: usize,
    /// Submitted entries, oldest first, for Up/Down recall.
    history: Vec<String>,
    /// Which history entry is being viewed (None = editing the live draft).
    hist: Option<usize>,
    /// The live draft saved while browsing history, restored on the way back.
    draft: String,
    /// Stashed large pastes, expanded into the text at submit.
    pastes: Vec<Paste>,
    /// Attached images (built by the event loop from pasted paths) and their
    /// display labels.
    images: Vec<ContentBlock>,
    labels: Vec<String>,
    /// The column vertical movement aims for, so Up/Down over short lines don't
    /// lose the horizontal position. Cleared by any horizontal edit.
    goal_col: Option<usize>,
}

impl Composer {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            history: Vec::new(),
            hist: None,
            draft: String::new(),
            pastes: Vec::new(),
            images: Vec::new(),
            labels: Vec::new(),
            goal_col: None,
        }
    }

    // --- queries -----------------------------------------------------------

    /// The current visible text (with placeholders, unexpanded). Used for the
    /// empty check and slash-command detection.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_blank(&self) -> bool {
        self.text.trim().is_empty() && self.images.is_empty()
    }

    /// The cursor's char index into [`text`](Self::text), for the completion
    /// menu's trigger detection ([`crate::menu::detect_trigger`]).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Display labels of attached images, for the attachment line.
    pub fn attachments(&self) -> &[String] {
        &self.labels
    }

    fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    // --- editing -----------------------------------------------------------

    pub fn insert_char(&mut self, c: char) {
        self.begin_edit();
        let at = byte_index(&self.text, self.cursor);
        self.text.insert(at, c);
        self.cursor += 1;
        self.goal_col = None;
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Insert a paste: small text goes in verbatim, a large one collapses to a
    /// placeholder that submit expands.
    pub fn paste(&mut self, s: &str) {
        self.begin_edit();
        let big = s.chars().count() >= PASTE_CHARS || s.split('\n').count() >= PASTE_LINES;
        if big {
            let n = s.chars().count();
            let placeholder = format!("[Pasted #{}: {n} chars]", self.pastes.len() + 1);
            self.insert_str(&placeholder);
            self.pastes.push(Paste {
                placeholder,
                content: s.to_string(),
            });
        } else {
            self.insert_str(s);
        }
    }

    fn insert_str(&mut self, s: &str) {
        let at = byte_index(&self.text, self.cursor);
        self.text.insert_str(at, s);
        self.cursor += s.chars().count();
        self.goal_col = None;
    }

    /// Replace the current whitespace-delimited token (the run of non-space
    /// chars ending at the cursor) with `replacement` plus a trailing space, and
    /// put the cursor after it. Used by the completion menu to insert a chosen
    /// command or file path; the replacement includes its `/`/`@` prefix, so the
    /// typed trigger is overwritten in place. A completed token is ordinary text,
    /// so pastes and history are left untouched.
    pub fn replace_token(&mut self, replacement: &str) {
        self.begin_edit();
        let chars: Vec<char> = self.text.chars().collect();
        let cursor = self.cursor.min(chars.len());
        let mut start = cursor;
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let start_b = byte_index(&self.text, start);
        let end_b = byte_index(&self.text, cursor);
        let insert = format!("{replacement} ");
        self.text.replace_range(start_b..end_b, &insert);
        self.cursor = start + insert.chars().count();
        self.goal_col = None;
    }

    /// Attach an image (block already built by the loop) with a display label.
    pub fn attach_image(&mut self, label: String, block: ContentBlock) {
        self.images.push(block);
        self.labels.push(label);
    }

    pub fn backspace(&mut self) {
        self.begin_edit();
        if self.cursor > 0 {
            self.cursor -= 1;
            let at = byte_index(&self.text, self.cursor);
            self.text.remove(at);
        }
        self.goal_col = None;
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.goal_col = None;
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_count());
        self.goal_col = None;
    }

    pub fn home(&mut self) {
        let (row, _) = self.row_col();
        self.cursor = self.line_starts()[row];
        self.goal_col = None;
    }

    pub fn end(&mut self) {
        let (row, _) = self.row_col();
        let starts = self.line_starts();
        self.cursor = self.line_end(&starts, row);
        self.goal_col = None;
    }

    /// Up: move to the previous line (keeping the goal column), or recall the
    /// previous history entry when already on the first line. Returns true if it
    /// moved the cursor rather than recalling history — the caller does not care,
    /// but tests do.
    pub fn up(&mut self) {
        let (row, col) = self.row_col();
        if row == 0 {
            self.history_prev();
        } else {
            self.move_to_row(row - 1, col);
        }
    }

    pub fn down(&mut self) {
        let (row, col) = self.row_col();
        let last = self.line_starts().len() - 1;
        if row == last {
            self.history_next();
        } else {
            self.move_to_row(row + 1, col);
        }
    }

    fn move_to_row(&mut self, target: usize, col: usize) {
        let goal = self.goal_col.unwrap_or(col);
        let starts = self.line_starts();
        let start = starts[target];
        let len = self.line_end(&starts, target) - start;
        self.cursor = start + goal.min(len);
        self.goal_col = Some(goal);
    }

    // --- history -----------------------------------------------------------

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.hist {
            None => {
                self.draft = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.load_history(idx);
    }

    fn history_next(&mut self) {
        match self.hist {
            None => {}
            Some(i) if i + 1 < self.history.len() => self.load_history(i + 1),
            Some(_) => {
                // Past the newest entry: back to the live draft.
                self.hist = None;
                self.text = std::mem::take(&mut self.draft);
                self.cursor = self.char_count();
                self.goal_col = None;
            }
        }
    }

    fn load_history(&mut self, idx: usize) {
        self.hist = Some(idx);
        self.text = self.history[idx].clone();
        self.cursor = self.char_count();
        self.goal_col = None;
    }

    /// The first edit after recalling a history entry adopts it as the new draft.
    fn begin_edit(&mut self) {
        self.hist = None;
    }

    // --- submit ------------------------------------------------------------

    /// Expand pastes, take the images, push the entry to history, and clear the
    /// composer. Returns None when there is nothing to send.
    pub fn submit(&mut self) -> Option<Submission> {
        if self.is_blank() {
            return None;
        }
        let display = std::mem::take(&mut self.text);
        let mut text = display.clone();
        for p in self.pastes.drain(..) {
            text = text.replace(&p.placeholder, &p.content);
        }
        // History keeps the compact display form (placeholders), like the user
        // saw it; a blank line (image-only submit) is not worth recalling.
        if !display.trim().is_empty() && self.history.last() != Some(&display) {
            self.history.push(display);
        }
        let images = std::mem::take(&mut self.images);
        self.labels.clear();
        self.cursor = 0;
        self.hist = None;
        self.draft.clear();
        self.goal_col = None;
        Some(Submission { text, images })
    }

    /// Clear the composer (Esc when idle) without touching history.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.pastes.clear();
        self.images.clear();
        self.labels.clear();
        self.hist = None;
        self.goal_col = None;
    }

    // --- line geometry -----------------------------------------------------

    /// Char index where each logical line starts (line 0 at 0, each subsequent
    /// line just after a '\n').
    fn line_starts(&self) -> Vec<usize> {
        let mut starts = vec![0];
        for (i, c) in self.text.chars().enumerate() {
            if c == '\n' {
                starts.push(i + 1);
            }
        }
        starts
    }

    /// Char index at the end of logical line `row` (before its '\n', or the text
    /// end for the last line).
    fn line_end(&self, starts: &[usize], row: usize) -> usize {
        if row + 1 < starts.len() {
            starts[row + 1] - 1
        } else {
            self.char_count()
        }
    }

    /// The cursor's (logical row, column-in-chars).
    fn row_col(&self) -> (usize, usize) {
        let mut row = 0;
        let mut col = 0;
        for (i, c) in self.text.chars().enumerate() {
            if i == self.cursor {
                return (row, col);
            }
            if c == '\n' {
                row += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (row, col)
    }

    // --- rendering ---------------------------------------------------------

    /// Wrap the text to `width`, prefix the prompt/continuation gutter, window to
    /// [`MAX_ROWS`] around the cursor, and report the cursor's on-screen position.
    pub fn view(&self, width: usize) -> View {
        let width = width.max(GUTTER_W + 1);
        let content_w = width - GUTTER_W;

        // Empty draft: prompt + a dim placeholder, cursor just after the prompt.
        if self.text.is_empty() {
            let row = Line::from(vec![
                Span::styled(PROMPT.to_string(), Style::new().fg(Color::Cyan)),
                Span::styled(
                    "Type a message…  (Shift+Enter for newline)".to_string(),
                    DIM,
                ),
            ]);
            return View {
                rows: vec![row],
                cursor_row: 0,
                cursor_col: GUTTER_W as u16,
            };
        }

        // Wrap every logical line to content columns, remembering the cursor's
        // visual cell as we lay chars down.
        let mut rows: Vec<String> = Vec::new();
        let mut cur_row = 0usize;
        let mut cur_col = 0usize;
        let mut line = String::new();
        let mut col = 0usize;
        let mut idx = 0usize;
        let logical: Vec<&str> = self.text.split('\n').collect();
        for (li, seg) in logical.iter().enumerate() {
            for c in seg.chars() {
                if idx == self.cursor {
                    cur_row = rows.len();
                    cur_col = col;
                }
                let w = c.width().unwrap_or(0);
                if col + w > content_w && !line.is_empty() {
                    rows.push(std::mem::take(&mut line));
                    col = 0;
                }
                line.push(c);
                col += w;
                idx += 1;
            }
            // Cursor at the end of this logical line (before its '\n').
            if idx == self.cursor {
                cur_row = rows.len();
                cur_col = col;
            }
            rows.push(std::mem::take(&mut line));
            col = 0;
            if li + 1 < logical.len() {
                idx += 1; // the '\n' between logical lines
            }
        }

        // Window to MAX_ROWS keeping the cursor visible.
        let start = cur_row.saturating_sub(MAX_ROWS - 1);
        let end = (start + MAX_ROWS).min(rows.len());
        let visible = &rows[start..end];
        let lines: Vec<Line<'static>> = visible
            .iter()
            .enumerate()
            .map(|(i, content)| {
                let is_first = start + i == 0;
                let (gutter, style) = if is_first {
                    (PROMPT, Style::new().fg(Color::Cyan))
                } else {
                    (CONT, Style::default())
                };
                Line::from(vec![
                    Span::styled(gutter.to_string(), style),
                    Span::raw(content.clone()),
                ])
            })
            .collect();

        View {
            rows: lines,
            cursor_row: (cur_row - start) as u16,
            cursor_col: (cur_col + GUTTER_W) as u16,
        }
    }
}

/// Byte offset of the `n`-th char (or the end), for splitting `String` at a char
/// boundary.
fn byte_index(s: &str, char_index: usize) -> usize {
    s.char_indices()
        .nth(char_index)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> Composer {
        let mut c = Composer::new();
        for ch in s.chars() {
            c.insert_char(ch);
        }
        c
    }

    fn row_texts(v: &View) -> Vec<String> {
        v.rows
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn typing_newlines_and_cursor_movement() {
        let mut c = typed("ab");
        c.insert_newline();
        for ch in "cd".chars() {
            c.insert_char(ch);
        }
        assert_eq!(c.text(), "ab\ncd");
        // Cursor at end (row 1, col 2).
        assert_eq!(c.row_col(), (1, 2));
        // Up keeps the column, landing on row 0 col 2 (end of "ab").
        c.up();
        assert_eq!(c.row_col(), (0, 2));
        // Home/End move within the logical line.
        c.home();
        assert_eq!(c.row_col(), (0, 0));
        c.end();
        assert_eq!(c.row_col(), (0, 2));
        // Down returns to row 1 at the goal column.
        c.down();
        assert_eq!(c.row_col(), (1, 2));
    }

    #[test]
    fn goal_column_survives_a_short_line() {
        // Column 4 on a long line, then up over a short line, then up again:
        // the goal column is preserved, not clamped to the short line.
        let mut c = Composer::new();
        for ch in "long line\nhi\nlonger line".chars() {
            c.insert_char(ch);
        }
        // Cursor at end of "longer line" (row 2, col 11). Put it at col 4.
        c.home();
        for _ in 0..4 {
            c.right();
        }
        assert_eq!(c.row_col(), (2, 4));
        c.up(); // onto "hi" (len 2) — clamps to col 2 but remembers goal 4
        assert_eq!(c.row_col(), (1, 2));
        c.up(); // onto "long line" — restores goal col 4
        assert_eq!(c.row_col(), (0, 4));
    }

    #[test]
    fn up_down_at_edges_recall_history() {
        let mut c = Composer::new();
        // Two submitted entries.
        for ch in "first".chars() {
            c.insert_char(ch);
        }
        c.submit();
        for ch in "second".chars() {
            c.insert_char(ch);
        }
        c.submit();
        assert_eq!(c.text(), "");

        // A partial draft, then Up recalls newest-first.
        for ch in "dra".chars() {
            c.insert_char(ch);
        }
        c.up();
        assert_eq!(c.text(), "second");
        c.up();
        assert_eq!(c.text(), "first");
        c.up(); // already oldest: stays
        assert_eq!(c.text(), "first");
        // Down walks back to the saved draft.
        c.down();
        assert_eq!(c.text(), "second");
        c.down();
        assert_eq!(c.text(), "dra", "returns to the live draft");
    }

    #[test]
    fn editing_a_recalled_entry_adopts_it_as_the_draft() {
        let mut c = typed("hello");
        c.submit();
        c.up();
        assert_eq!(c.text(), "hello");
        c.insert_char('!');
        assert_eq!(c.text(), "hello!");
        // Down no longer walks history (we left it by editing).
        c.down();
        assert_eq!(c.text(), "hello!");
    }

    #[test]
    fn large_paste_collapses_to_a_placeholder_and_expands_on_submit() {
        let mut c = typed("see: ");
        let big = "x".repeat(500);
        c.paste(&big);
        assert_eq!(c.text(), "see: [Pasted #1: 500 chars]");
        let sub = c.submit().unwrap();
        assert_eq!(sub.text, format!("see: {big}"));
        assert!(sub.images.is_empty());
        // History keeps the compact form.
        c.up();
        assert_eq!(c.text(), "see: [Pasted #1: 500 chars]");
    }

    #[test]
    fn small_paste_inserts_verbatim() {
        let mut c = typed("a");
        c.paste("bc");
        assert_eq!(c.text(), "abc");
        assert!(c.submit().unwrap().text == "abc");
    }

    #[test]
    fn image_attachment_rides_the_submission() {
        let mut c = Composer::new();
        let block = ContentBlock::Image {
            source: kloop_protocol::ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "aGk=".into(),
            },
        };
        c.attach_image("shot.png".into(), block.clone());
        assert_eq!(c.attachments(), &["shot.png".to_string()]);
        // Image-only submit is allowed even with no text.
        assert!(!c.is_blank());
        let sub = c.submit().unwrap();
        assert_eq!(sub.text, "");
        assert_eq!(sub.images, vec![block]);
        assert!(c.attachments().is_empty(), "cleared after submit");
    }

    #[test]
    fn blank_submit_returns_none() {
        let mut c = Composer::new();
        assert!(c.submit().is_none());
        for ch in "   ".chars() {
            c.insert_char(ch);
        }
        assert!(c.submit().is_none());
    }

    #[test]
    fn view_wraps_and_places_the_cursor() {
        // Width 6 → content width 4 (after the 2-col gutter).
        let mut c = typed("abcdef");
        let v = c.view(6);
        assert_eq!(row_texts(&v), vec!["› abcd", "  ef"]);
        // Cursor at end: row 1, col 2 (gutter) + 2 = 4.
        assert_eq!((v.cursor_row, v.cursor_col), (1, 4));

        // Move home: cursor back to the first row just after the prompt.
        c.home();
        let v = c.view(6);
        assert_eq!((v.cursor_row, v.cursor_col), (0, 2));
    }

    #[test]
    fn view_shows_placeholder_when_empty() {
        let c = Composer::new();
        let v = c.view(40);
        assert_eq!(v.rows.len(), 1);
        let text: String = v.rows[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(text.starts_with("› Type a message"));
        assert_eq!((v.cursor_row, v.cursor_col), (0, 2));
    }

    #[test]
    fn view_windows_tall_input_to_keep_cursor_visible() {
        let mut c = Composer::new();
        for i in 0..20 {
            for ch in format!("line{i}").chars() {
                c.insert_char(ch);
            }
            c.insert_newline();
        }
        // Cursor is on the last (empty) line; the window shows the last MAX_ROWS.
        let v = c.view(40);
        assert_eq!(v.rows.len(), MAX_ROWS);
        assert_eq!(v.cursor_row, (MAX_ROWS - 1) as u16);
    }

    #[test]
    fn replace_token_swaps_the_current_word_and_trails_a_space() {
        // Slash completion: the whole `/co` token becomes `/compact `.
        let mut c = typed("/co");
        c.replace_token("/compact");
        assert_eq!(c.text(), "/compact ");
        assert_eq!(c.cursor(), 9);

        // File completion mid-line: only the `@` token is replaced.
        let mut c = typed("review @src/ma");
        c.replace_token("@src/main.rs");
        assert_eq!(c.text(), "review @src/main.rs ");

        // Cursor mid-token replaces only up to the cursor (the suffix stays).
        let mut c = typed("@src/main");
        c.left(); // cursor before the trailing 'n'... actually after "@src/mai"
        c.replace_token("@src/lib");
        assert_eq!(c.text(), "@src/lib n");
    }

    #[test]
    fn cjk_width_in_view() {
        // Two double-width chars fill content width 4 exactly, one wraps.
        let c = typed("你好世");
        let v = c.view(6);
        assert_eq!(row_texts(&v), vec!["› 你好", "  世"]);
    }
}
