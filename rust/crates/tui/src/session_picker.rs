//! The `--resume` session picker (plan 162): a list you read, not a numbered
//! dump you count through.
//!
//! It runs before the agent starts, so it owns the terminal for the length of
//! one blocking loop and hands it back cleared. State and layout are pure —
//! [`Picker`] folds keys into a selection and [`Picker::lines`] turns it into
//! rows — so everything but the twenty lines of terminal plumbing is unit
//! tested.

use std::io::Write as _;
use std::time::Duration;

use anyhow::Result;
use ratatui::TerminalOptions;
use ratatui::Viewport;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::render::BRAND;
use crate::render::DIM;
use crate::text_layout::display_width;
use crate::text_layout::truncate;

/// One row's worth of session, already free of anything that needs the disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEntry {
    /// Matched on, but not shown: an id is a timestamp with a suffix, and
    /// reading fifteen of them is the job this screen exists to remove.
    pub id: String,
    pub title: String,
    /// Age at the moment the list was built, not a timestamp: the rendered
    /// rows are then a pure function of the entries.
    pub age: Duration,
    pub bytes: u64,
    /// `forked` / `sub-agent of …` — lineage, when there is any.
    pub badge: Option<String>,
}

/// What a key did to the picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Stay,
    /// Index into the entries handed to [`Picker::new`].
    Chose(usize),
    Cancel,
}

/// Chrome rows that are never given to the list: title, blank, the three-row
/// search box, blank, the project label, blank, and one row of key hint. A
/// narrow terminal wraps the hint onto more rows, which costs entry slots.
const CHROME_ROWS: usize = 9;
/// The key hint, in the pieces it is allowed to break between: wrapping it by
/// column would cut `cancel` in half.
const HINT: [&str; 4] = [
    "↑↓ to select",
    "Enter to resume",
    "Type to search",
    "Esc to cancel",
];

/// Greedy fill, one separator per join.
fn hint_rows(width: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    for part in HINT {
        match rows.last_mut() {
            Some(row)
                if display_width(row) + display_width(" · ") + display_width(part) <= width =>
            {
                row.push_str(" · ");
                row.push_str(part);
            }
            _ => rows.push(part.to_string()),
        }
    }
    rows
}
/// Title, subtitle, and the blank that separates one entry from the next.
const ROWS_PER_ENTRY: usize = 3;

pub struct Picker {
    entries: Vec<SessionEntry>,
    /// The project these sessions belong to, shown once above the list.
    project: String,
    query: String,
    /// Position within [`Picker::matches`], not within `entries`.
    cursor: usize,
    /// First visible match. Only [`Picker::lines`] moves it, because only a
    /// render knows how many rows fit.
    top: usize,
}

impl Picker {
    pub fn new(entries: Vec<SessionEntry>, project: impl Into<String>) -> Self {
        Self {
            entries,
            project: project.into(),
            query: String::new(),
            cursor: 0,
            top: 0,
        }
    }

    /// Indices into `entries`, in list order, that the query keeps. Matching is
    /// case-insensitive substring over the title and the id — the id is there
    /// so a session someone half-remembers by its timestamp is still findable.
    pub fn matches(&self) -> Vec<usize> {
        if self.query.is_empty() {
            return (0..self.entries.len()).collect();
        }
        let needle = self.query.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.title.to_lowercase().contains(&needle)
                    || entry.id.to_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect()
    }

    pub fn on_key(&mut self, key: crossterm::event::KeyEvent) -> Outcome {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyModifiers;

        let matches = self.matches();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => Outcome::Cancel,
            KeyCode::Char('c' | 'd') if ctrl => Outcome::Cancel,
            KeyCode::Enter => match matches.get(self.cursor) {
                Some(&index) => Outcome::Chose(index),
                // Nothing matches the query: Enter has nothing to open, and
                // must not fall through to "the first one".
                None => Outcome::Stay,
            },
            KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                Outcome::Stay
            }
            KeyCode::Char('p') if ctrl => {
                self.cursor = self.cursor.saturating_sub(1);
                Outcome::Stay
            }
            KeyCode::Down => {
                self.cursor = (self.cursor + 1).min(matches.len().saturating_sub(1));
                Outcome::Stay
            }
            KeyCode::Char('n') if ctrl => {
                self.cursor = (self.cursor + 1).min(matches.len().saturating_sub(1));
                Outcome::Stay
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.clamp_cursor();
                Outcome::Stay
            }
            KeyCode::Char(c) if !ctrl => {
                self.query.push(c);
                self.clamp_cursor();
                Outcome::Stay
            }
            _ => Outcome::Stay,
        }
    }

    /// A shrinking match list must not leave the cursor pointing past its end.
    fn clamp_cursor(&mut self) {
        let len = self.matches().len();
        self.cursor = self.cursor.min(len.saturating_sub(1));
    }

    /// How many entry slots fit, at least one.
    fn visible_rows(width: usize, height: usize) -> usize {
        let chrome = CHROME_ROWS + hint_rows(width).len().saturating_sub(1);
        (height.saturating_sub(chrome) / ROWS_PER_ENTRY).max(1)
    }

    /// The viewport this picker asks for: as tall as the list needs, capped by
    /// the terminal. A three-session list should not blank a 50-row screen.
    pub fn height(&self, terminal_cols: usize, terminal_rows: usize) -> usize {
        let chrome = CHROME_ROWS + hint_rows(terminal_cols).len().saturating_sub(1);
        let wanted = chrome + ROWS_PER_ENTRY * self.entries.len().max(1);
        wanted
            .max(chrome + ROWS_PER_ENTRY)
            .min(terminal_rows.max(1))
    }

    pub fn lines(&mut self, width: usize, height: usize) -> Vec<Line<'static>> {
        let width = width.max(20);
        let matches = self.matches();
        let visible = Self::visible_rows(width, height);
        let top = self.scroll_to_cursor(matches.len(), visible);
        let cursor = self.cursor;

        let mut lines = Vec::new();
        lines.push(Line::from(vec![
            Span::styled("Resume session", Style::new().fg(BRAND)),
            Span::styled(
                format!(
                    " ({} of {})",
                    if matches.is_empty() {
                        0
                    } else {
                        self.cursor + 1
                    },
                    matches.len()
                ),
                DIM,
            ),
        ]));
        lines.push(Line::default());
        lines.extend(self.search_box(width));
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!("  {}", truncate(&self.project, width.saturating_sub(2))),
            DIM,
        )));
        lines.push(Line::default());

        if matches.is_empty() {
            lines.push(Line::from(Span::styled(
                "  no session matches that search".to_string(),
                DIM,
            )));
        }
        for (row, &index) in matches.iter().enumerate().skip(top).take(visible) {
            let entry = &self.entries[index];
            let selected = row == cursor;
            // The gutter carries both the selection and the fact that the list
            // continues past this row, which is why it is one column wide and
            // not a scrollbar.
            let gutter = match (selected, row) {
                (true, _) => "❯",
                (_, r) if r == top && top > 0 => "↑",
                (_, r) if r + 1 == top + visible && r + 1 < matches.len() => "↓",
                _ => " ",
            };
            let title_style = if selected {
                Style::new().fg(BRAND)
            } else {
                Style::new()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{gutter} "), Style::new().fg(BRAND)),
                Span::styled(truncate(&entry.title, width.saturating_sub(2)), title_style),
            ]));
            lines.push(Line::from(Span::styled(
                format!("  {}", truncate(&subtitle(entry), width.saturating_sub(2))),
                DIM,
            )));
            lines.push(Line::default());
        }

        let hint = hint_rows(width);
        while lines.len() + hint.len() < height {
            lines.push(Line::default());
        }
        for row in hint {
            lines.push(Line::from(Span::styled(row, DIM)));
        }
        lines.truncate(height);
        lines
    }

    /// Keep the cursor inside the window, scrolling by whole entries.
    fn scroll_to_cursor(&mut self, len: usize, visible: usize) -> usize {
        let mut top = self.top.min(len.saturating_sub(1));
        if self.cursor < top {
            top = self.cursor;
        } else if self.cursor >= top + visible {
            top = self.cursor + 1 - visible;
        }
        self.top = top;
        top
    }

    fn search_box(&self, width: usize) -> Vec<Line<'static>> {
        let inner = width.saturating_sub(4);
        let field = inner.saturating_sub(display_width("⌕ "));
        let shown = if self.query.is_empty() {
            Span::styled("Search…".to_string(), DIM)
        } else {
            Span::raw(truncate(&self.query, field))
        };
        let pad = field.saturating_sub(display_width(shown.content.as_ref()));
        vec![
            Line::from(Span::styled(
                format!("╭{}╮", "─".repeat(inner + 2)),
                Style::new().fg(BRAND),
            )),
            Line::from(vec![
                Span::styled("│ ".to_string(), Style::new().fg(BRAND)),
                Span::styled("⌕ ".to_string(), DIM),
                shown,
                Span::raw(" ".repeat(pad)),
                Span::styled(" │".to_string(), Style::new().fg(BRAND)),
            ]),
            Line::from(Span::styled(
                format!("╰{}╯", "─".repeat(inner + 2)),
                Style::new().fg(BRAND),
            )),
        ]
    }
}

/// `19 hours ago · 1.2MB`, plus the lineage badge when there is one.
fn subtitle(entry: &SessionEntry) -> String {
    let mut text = format!("{} · {}", relative_age(entry.age), human_bytes(entry.bytes));
    if let Some(badge) = &entry.badge {
        text.push_str(" · ");
        text.push_str(badge);
    }
    text
}

/// Coarse on purpose: the list is sorted by recency, so the age only has to
/// separate "this morning" from "last week".
fn relative_age(age: Duration) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    let seconds = age.as_secs();
    let (count, unit) = match seconds {
        0..MINUTE => (seconds, "second"),
        MINUTE..HOUR => (seconds / MINUTE, "minute"),
        HOUR..DAY => (seconds / HOUR, "hour"),
        _ => (seconds / DAY, "day"),
    };
    let count = count.max(1);
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {unit}{plural} ago")
}

fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let bytes = bytes as f64;
    if bytes < KB {
        return format!("{bytes:.0}B");
    }
    let (value, unit) = if bytes < MB {
        (bytes / KB, "KB")
    } else {
        (bytes / MB, "MB")
    };
    let text = format!("{value:.1}");
    let text = text.strip_suffix(".0").unwrap_or(&text).to_string();
    format!("{text}{unit}")
}

/// Run the picker on the real terminal. `Ok(None)` is a cancel — the caller
/// exits without starting a session.
pub fn pick_session(entries: Vec<SessionEntry>, project: &str) -> Result<Option<usize>> {
    pick(Picker::new(entries, project))
}

fn pick(mut picker: Picker) -> Result<Option<usize>> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let height = picker.height(cols as usize, rows as usize) as u16;
    crossterm::terminal::enable_raw_mode()?;
    let terminal = ratatui::Terminal::with_options(
        ratatui::backend::CrosstermBackend::new(std::io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(height.max(1)),
        },
    );
    let mut terminal = match terminal {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(error.into());
        }
    };
    let outcome = loop {
        // `draw` autoresizes first, so a resized terminal re-lays out here
        // rather than needing its own branch below.
        terminal.draw(|frame| {
            let area = frame.area();
            let lines = picker.lines(area.width as usize, area.height as usize);
            frame.render_widget(Paragraph::new(lines), area);
        })?;
        if let crossterm::event::Event::Key(key) = crossterm::event::read()?
            && key.kind == crossterm::event::KeyEventKind::Press
        {
            match picker.on_key(key) {
                Outcome::Stay => {}
                other => break other,
            }
        }
    };
    // Hand the screen back the way it was found: the picker's rows are chrome,
    // not transcript, and whatever runs next starts where they began. Dropping
    // the terminal parks the cursor at the bottom of the inline viewport, so
    // the rewind to its top row happens after that, not before — otherwise the
    // session banner prints under a screenful of blanks.
    let viewport_top = terminal.get_frame().area().y;
    let _ = terminal.clear();
    drop(terminal);
    let mut out = std::io::stdout();
    let _ = crossterm::execute!(
        out,
        crossterm::cursor::MoveTo(0, viewport_top),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::FromCursorDown),
        crossterm::cursor::Show,
    );
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = out.flush();
    Ok(match outcome {
        Outcome::Chose(index) => Some(index),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;

    fn entry(id: &str, title: &str, age: Duration, bytes: u64) -> SessionEntry {
        SessionEntry {
            id: id.to_string(),
            title: title.to_string(),
            age,
            bytes,
            badge: None,
        }
    }

    fn sample() -> Vec<SessionEntry> {
        vec![
            entry(
                "20260918-01",
                "review the picker",
                Duration::from_secs(16),
                1_048_576,
            ),
            entry(
                "20260917-02",
                "fix the banner width",
                Duration::from_secs(19 * 3600),
                1_258_291,
            ),
            entry(
                "20260916-03",
                "审查 243484df",
                Duration::from_secs(26 * 3600),
                587_878,
            ),
        ]
    }

    fn screen(picker: &mut Picker, width: usize, height: usize) -> String {
        picker
            .lines(width, height)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_whole_screen_reads_as_a_list() {
        let mut picker = Picker::new(sample(), "kloop");
        insta::assert_snapshot!("session_picker_24x80", screen(&mut picker, 80, 24));
    }

    #[test]
    fn typing_filters_and_the_count_follows() {
        let mut picker = Picker::new(sample(), "kloop");
        for c in "banner".chars() {
            picker.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(picker.matches(), vec![1]);
        let screen = screen(&mut picker, 80, 24);
        assert!(screen.contains("(1 of 1)"), "{screen}");
        assert!(screen.contains("fix the banner width"), "{screen}");
        assert!(!screen.contains("review the picker"), "{screen}");

        // Backspacing back to nothing restores the full list.
        for _ in 0..6 {
            picker.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        }
        assert_eq!(picker.matches(), vec![0, 1, 2]);
    }

    #[test]
    fn a_filtered_enter_opens_the_match_not_the_row_it_sits_on() {
        let mut picker = Picker::new(sample(), "kloop");
        for c in "审查".chars() {
            picker.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(
            picker.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Outcome::Chose(2)
        );
    }

    #[test]
    fn enter_on_an_empty_result_opens_nothing() {
        let mut picker = Picker::new(sample(), "kloop");
        for c in "zzz".chars() {
            picker.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert!(picker.matches().is_empty());
        assert_eq!(
            picker.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Outcome::Stay
        );
        assert!(screen(&mut picker, 80, 24).contains("no session matches"));
    }

    #[test]
    fn the_cursor_stays_inside_a_list_that_shrank_under_it() {
        let mut picker = Picker::new(sample(), "kloop");
        picker.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        picker.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(picker.cursor, 2);
        for c in "banner".chars() {
            picker.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(picker.cursor, 0);
        assert_eq!(
            picker.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Outcome::Chose(1)
        );
    }

    #[test]
    fn arrows_stop_at_both_ends() {
        let mut picker = Picker::new(sample(), "kloop");
        picker.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(picker.cursor, 0);
        for _ in 0..10 {
            picker.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert_eq!(picker.cursor, 2);
    }

    #[test]
    fn esc_and_ctrl_c_cancel() {
        let mut picker = Picker::new(sample(), "kloop");
        assert_eq!(
            picker.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Outcome::Cancel
        );
        assert_eq!(
            picker.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Outcome::Cancel
        );
    }

    #[test]
    fn a_long_list_scrolls_by_whole_entries_and_marks_the_overflow() {
        let entries: Vec<SessionEntry> = (0..20)
            .map(|i| {
                entry(
                    &format!("2026091{i:02}"),
                    &format!("session {i}"),
                    Duration::from_secs(60 * (i as u64 + 1)),
                    4096,
                )
            })
            .collect();
        let mut picker = Picker::new(entries, "kloop");
        // Five entry slots fit in 24 rows; the sixth Down scrolls by one.
        for _ in 0..5 {
            picker.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let screen = screen(&mut picker, 80, 24);
        assert!(screen.contains("❯ session 5"), "{screen}");
        assert!(!screen.contains("session 0"), "{screen}");
        assert!(screen.contains("↑ session 1"), "{screen}");
    }

    #[test]
    fn a_short_list_does_not_ask_for_the_whole_screen() {
        let picker = Picker::new(sample(), "kloop");
        assert_eq!(picker.height(80, 50), CHROME_ROWS + 3 * ROWS_PER_ENTRY);
        assert_eq!(picker.height(80, 14), 14);
    }

    #[test]
    fn ages_and_sizes_read_the_way_a_person_says_them() {
        assert_eq!(relative_age(Duration::from_secs(0)), "1 second ago");
        assert_eq!(relative_age(Duration::from_secs(16)), "16 seconds ago");
        assert_eq!(relative_age(Duration::from_secs(60)), "1 minute ago");
        assert_eq!(relative_age(Duration::from_secs(19 * 3600)), "19 hours ago");
        assert_eq!(relative_age(Duration::from_secs(26 * 3600)), "1 day ago");
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(587_878), "574.1KB");
        assert_eq!(human_bytes(1_048_576), "1MB");
        assert_eq!(human_bytes(1_258_291), "1.2MB");
    }
}
