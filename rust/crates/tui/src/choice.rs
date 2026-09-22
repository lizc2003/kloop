//! The one inline choice panel every keyboard-capturing surface renders through
//! (plan 104): approvals, questions, the provider picker, rewind.
//!
//! It is live chrome sitting directly above the composer — never a centred popup
//! that `Clear`s the transcript out from under the user. Because the panel is
//! part of the transcript's height budget, [`Layout::lines`] must fit the row
//! count the caller hands it; the pieces that justify the panel's existence (the
//! options and the key hint) are allocated first and the scrollable body takes
//! what is left.

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::render::BRAND;
use crate::render::DIM;
use crate::text_layout::display_width;
use crate::text_layout::truncate;
use crate::text_layout::wrap;

/// A `detail` never grows past this many rows, so one verbose option cannot
/// push the rest of the list off screen.
const DETAIL_ROWS: usize = 2;

/// One selectable row: a label, plus an optional dim explanation under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    pub detail: Option<String>,
}

impl Item {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: None,
        }
    }

    pub fn with_detail(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: Some(detail.into()),
        }
    }
}

/// The render model for one panel. Each surface builds this and nothing else
/// decides layout, colour, or hint wording.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Panel {
    /// The accent label on the first row (`▌ Bash command`).
    pub header: String,
    /// What the answer is about — the command, the path, the warning. Pinned
    /// directly under the header: an option list floating over a scrolled-away
    /// subject is how people approve the wrong thing.
    pub subject: Vec<Line<'static>>,
    /// Scrollable supporting detail: a diff, a plan, a preview.
    pub body: Vec<Line<'static>>,
    /// The one-line question directly above the options.
    pub prompt: Option<String>,
    pub items: Vec<Item>,
    pub cursor: usize,
    /// Multi-select ticks, by item index. Ignored unless `multi_select`.
    pub checked: Vec<usize>,
    pub multi_select: bool,
    /// The dim key hint, the panel's only such line (the footer stays still).
    pub hint: String,
    /// An editor row shown in place of the options (question's Other/Notes
    /// phases): the prefix, then the live text. The caller places the cursor.
    pub editor: Option<Editor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Editor {
    pub prefix: String,
    pub text: String,
}

/// A laid-out panel: the rows to draw, the clamped body scroll offset (written
/// back so over-scrolling self-corrects), and whether body content lies out of
/// view in either direction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub lines: Vec<Line<'static>>,
    pub scroll: usize,
    pub more_above: bool,
    pub more_below: bool,
    /// Where the text cursor belongs when the panel is an editor: the row offset
    /// from the panel's first line and the column, both in display cells. None
    /// when the panel is a list (the terminal cursor stays hidden in the input).
    pub editor_cursor: Option<(usize, usize)>,
}

/// Lay `panel` out into at most `max_rows` rows of `width` columns.
///
/// Degradation order when the terminal is short: the body shrinks (and scrolls)
/// first, then the blank separators go, then the prompt, then the option list
/// windows around the cursor. The hint, the header, the first line of the
/// subject and the cursor's own option are the last things standing.
pub fn panel_lines(panel: &Panel, width: usize, max_rows: usize, scroll: usize) -> Layout {
    if width == 0 || max_rows == 0 {
        return Layout {
            lines: Vec::new(),
            scroll: 0,
            more_above: false,
            more_below: false,
            editor_cursor: None,
        };
    }
    let prompt: Vec<Line<'static>> = panel
        .prompt
        .iter()
        .flat_map(|text| wrap(text, width))
        .map(Line::from)
        .collect();

    // Allocate bottom-up, most load-bearing first: the hint, the options, the
    // header, the subject, the prompt. Whatever survives goes to the body,
    // which is the only part that scrolls.
    let mut budget = max_rows;
    let hint_rows = usize::from(budget > 0);
    budget -= hint_rows;
    // Hold two rows back before the list takes the rest: a wall of answers with
    // no header and nothing saying what they answer is worse than one answer
    // fewer. Everything held back that a block does not use returns to the body.
    let reserved = (usize::from(!panel.header.is_empty()) + usize::from(!panel.subject.is_empty()))
        .min(budget);
    let (choices, choices_clipped) =
        window_choices(&choice_rows(panel, width), panel.cursor, budget - reserved);
    let choice_height: usize = choices.iter().map(Vec::len).sum();
    budget -= choice_height;
    let header =
        (!panel.header.is_empty() && budget > 0).then(|| header_line(&panel.header, width));
    budget -= usize::from(header.is_some());
    let subject_rows = panel.subject.len().min(budget);
    budget -= subject_rows;
    let prompt_rows = prompt.len().min(budget);
    budget -= prompt_rows;

    // What is left pays for the body and the blank separators between blocks.
    // Separators are all-or-nothing: a cramped panel reads better packed tight
    // than with some gaps kept and others silently dropped.
    let blocks = usize::from(header.is_some())
        + usize::from(subject_rows > 0)
        + usize::from(prompt_rows > 0)
        + usize::from(choice_height > 0)
        + hint_rows;
    let gaps = blocks.saturating_sub(1);
    let body_rows = if panel.body.is_empty() {
        0
    } else {
        budget.saturating_sub(gaps + 1)
    };
    let separate = if body_rows > 0 { true } else { budget >= gaps };
    let (body, scroll, more_above, more_below) = window_lines(&panel.body, scroll, body_rows);

    let mut sections: Vec<Vec<Line<'static>>> = Vec::new();
    if let Some(header) = header {
        sections.push(vec![header]);
    }
    if subject_rows > 0 {
        sections.push(panel.subject[..subject_rows].to_vec());
    }
    if !body.is_empty() {
        sections.push(body);
    }
    if prompt_rows > 0 {
        sections.push(prompt.into_iter().take(prompt_rows).collect());
    }
    let editor_section = panel.editor.as_ref().map(|editor| {
        let column = display_width(&editor.prefix) + display_width(&editor.text);
        (sections.len(), column.min(width.saturating_sub(1)))
    });
    if choice_height > 0 {
        sections.push(choices.into_iter().flatten().collect());
    }
    if hint_rows == 1 {
        // The body's scroll affordance rides on the hint rather than a border:
        // the panel has none.
        let hint = if more_above || more_below {
            format!("{} · PgUp/PgDn scroll", panel.hint)
        } else {
            panel.hint.clone()
        };
        sections.push(vec![Line::from(Span::styled(truncate(&hint, width), DIM))]);
    }

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(max_rows);
    let mut editor_cursor = None;
    for (index, section) in sections.into_iter().enumerate() {
        if index > 0 && separate {
            lines.push(Line::default());
        }
        if let Some((section_index, column)) = editor_section
            && section_index == index
        {
            editor_cursor = Some((lines.len(), column));
        }
        lines.extend(section);
    }
    Layout {
        lines,
        scroll,
        more_above,
        more_below: more_below || choices_clipped,
        editor_cursor,
    }
}

/// `▌ label` in the brand accent — one row, so it never depends on box-drawing
/// characters lining up across a resize.
fn header_line(header: &str, width: usize) -> Line<'static> {
    let bar = "▌ ";
    let style = Style::new().fg(BRAND).add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled(bar.to_string(), style),
        Span::styled(
            truncate(header, width.saturating_sub(display_width(bar))),
            style,
        ),
    ])
}

/// The option list (or, in an editor phase, the single editor row) as one row
/// group per entry, so windowing never splits an option from its detail.
fn choice_rows(panel: &Panel, width: usize) -> Vec<Vec<Line<'static>>> {
    if let Some(editor) = &panel.editor {
        let prefix_w = display_width(&editor.prefix);
        return vec![vec![Line::from(vec![
            Span::styled(editor.prefix.clone(), Style::new().fg(BRAND)),
            Span::raw(truncate(&editor.text, width.saturating_sub(prefix_w))),
        ])]];
    }
    panel
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| item_rows(panel, index, item, width))
        .collect()
}

fn item_rows(panel: &Panel, index: usize, item: &Item, width: usize) -> Vec<Line<'static>> {
    let selected = index == panel.cursor;
    let marker = if selected { "> " } else { "  " };
    // Only the first nine get a number, because only those have a direct key.
    let number = if index < 9 {
        format!("{}. ", index + 1)
    } else {
        String::new()
    };
    let tick = if panel.multi_select {
        if panel.checked.contains(&index) {
            "[x] "
        } else {
            "[ ] "
        }
    } else {
        ""
    };
    let prefix = format!("{marker}{number}{tick}");
    let prefix_w = display_width(&prefix);
    let indent = " ".repeat(prefix_w);
    let text_w = width.saturating_sub(prefix_w).max(1);
    let label_style = if selected {
        Style::new().fg(BRAND)
    } else {
        Style::default()
    };
    let mut rows = Vec::new();
    for (line_index, fragment) in wrap(&item.label, text_w).into_iter().enumerate() {
        let lead = if line_index == 0 {
            prefix.clone()
        } else {
            indent.clone()
        };
        rows.push(Line::from(vec![
            Span::styled(lead, label_style),
            Span::styled(fragment, label_style),
        ]));
    }
    if let Some(detail) = &item.detail {
        for fragment in wrap(detail, text_w).into_iter().take(DETAIL_ROWS) {
            rows.push(Line::from(vec![
                Span::raw(indent.clone()),
                Span::styled(fragment, DIM),
            ]));
        }
    }
    rows
}

/// Window the option groups so the cursor's group is always fully visible.
/// Returns the visible groups and whether any were dropped.
fn window_choices(
    groups: &[Vec<Line<'static>>],
    cursor: usize,
    budget: usize,
) -> (Vec<Vec<Line<'static>>>, bool) {
    let total: usize = groups.iter().map(Vec::len).sum();
    if groups.is_empty() || budget == 0 {
        return (Vec::new(), !groups.is_empty());
    }
    if total <= budget {
        return (groups.to_vec(), false);
    }
    // Grow a window outward from the cursor, taking the next row below before
    // the one above so the list reads as scrolling down through the options.
    let cursor = cursor.min(groups.len() - 1);
    let mut start = cursor;
    let mut end = cursor + 1;
    // A single option taller than the whole budget is cut, not dropped: the
    // cursor must always be on screen.
    let mut used = groups[cursor].len().min(budget);
    loop {
        let grew_below = end < groups.len() && used + groups[end].len() <= budget;
        if grew_below {
            used += groups[end].len();
            end += 1;
        }
        let grew_above = start > 0 && used + groups[start - 1].len() <= budget;
        if grew_above {
            start -= 1;
            used += groups[start].len();
        }
        if !grew_below && !grew_above {
            break;
        }
    }
    let mut window: Vec<Vec<Line<'static>>> = groups[start..end].to_vec();
    if let Some(first) = window.first_mut()
        && start == cursor
        && first.len() > budget
    {
        first.truncate(budget);
    }
    (window, true)
}

/// Window `lines` to `height` rows at offset `scroll`, clamped to a valid range.
/// Returns the clamped offset (so over-scrolling self-corrects on the next
/// frame), the visible slice, and whether more lies above/below.
pub fn window_lines(
    lines: &[Line<'static>],
    scroll: usize,
    height: usize,
) -> (Vec<Line<'static>>, usize, bool, bool) {
    if height == 0 {
        return (Vec::new(), 0, false, !lines.is_empty());
    }
    let scroll = scroll.min(lines.len().saturating_sub(height));
    let end = (scroll + height).min(lines.len());
    (
        lines[scroll..end].to_vec(),
        scroll,
        scroll > 0,
        end < lines.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn texts(layout: &Layout) -> Vec<String> {
        layout.lines.iter().map(text).collect()
    }

    fn panel() -> Panel {
        Panel {
            header: "Bash command".into(),
            subject: vec![Line::from("go test ./upstream")],
            body: (1..=8).map(|i| Line::from(format!("line {i}"))).collect(),
            prompt: Some("Do you want to proceed?".into()),
            items: vec![
                Item::new("Yes"),
                Item::with_detail("Yes, always", "bash(go test *)"),
                Item::new("No"),
            ],
            cursor: 0,
            checked: Vec::new(),
            multi_select: false,
            hint: "Enter select".into(),
            editor: None,
        }
    }

    /// The full-height shape: header, pinned subject, scrollable body, prompt,
    /// numbered options, hint — one blank row between blocks.
    #[test]
    fn a_roomy_panel_lays_out_every_block_in_order() {
        let layout = panel_lines(&panel(), 40, 30, 0);
        assert_eq!(
            texts(&layout),
            vec![
                "▌ Bash command",
                "",
                "go test ./upstream",
                "",
                "line 1",
                "line 2",
                "line 3",
                "line 4",
                "line 5",
                "line 6",
                "line 7",
                "line 8",
                "",
                "Do you want to proceed?",
                "",
                "> 1. Yes",
                "  2. Yes, always",
                "     bash(go test *)",
                "  3. No",
                "",
                "Enter select",
            ]
        );
        assert_eq!((layout.more_above, layout.more_below), (false, false));
        assert_eq!(layout.editor_cursor, None);
    }

    /// A panel with no body at all — the shape every plan approval has since
    /// the plan moved into the transcript (plan 194), so it is the common case
    /// rather than a corner. The blocks keep their separators and the panel
    /// takes only the rows it needs, however much room it was offered.
    #[test]
    fn a_panel_with_no_body_keeps_its_separators_and_its_height() {
        let bodyless = Panel {
            header: "Exit plan mode".into(),
            subject: vec![Line::from("Exit plan mode and start on this plan?")],
            body: Vec::new(),
            items: vec![Item::new("Yes"), Item::new("No")],
            ..panel()
        };
        let layout = panel_lines(&bodyless, 40, 20, 0);
        assert_eq!(
            texts(&layout),
            vec![
                "▌ Exit plan mode",
                "",
                "Exit plan mode and start on this plan?",
                "",
                "Do you want to proceed?",
                "",
                "> 1. Yes",
                "  2. No",
                "",
                "Enter select",
            ]
        );
        // Nothing scrolls, so the hint carries no scroll affordance.
        assert_eq!((layout.more_above, layout.more_below), (false, false));
        // The same panel in a 10-row terminal is the same panel: it never
        // needed the other ten.
        assert_eq!(texts(&panel_lines(&bodyless, 40, 10, 0)), texts(&layout));
    }

    /// Squeezed, the body gives way first — the options, the hint, the header
    /// and the subject the answer is about all survive.
    #[test]
    fn a_cramped_panel_sheds_the_body_before_anything_else() {
        let layout = panel_lines(&panel(), 40, 8, 0);
        assert_eq!(layout.lines.len(), 8);
        let rows = texts(&layout);
        assert_eq!(rows[0], "▌ Bash command");
        assert_eq!(rows[1], "go test ./upstream");
        assert!(!rows.iter().any(|row| row.starts_with("line ")));
        assert!(rows.iter().any(|row| row == "> 1. Yes"));
        assert!(rows.iter().any(|row| row == "  3. No"));
        assert_eq!(rows.last().unwrap(), "Enter select · PgUp/PgDn scroll");
        // Nothing was shown of the body, so all of it is still below.
        assert!(layout.more_below);
    }

    /// The body scrolls under a fixed frame, and an offset past the end
    /// self-corrects to the last full window.
    #[test]
    fn the_body_scrolls_and_over_scroll_clamps() {
        let scrolled = panel_lines(&panel(), 40, 18, 2);
        let rows = texts(&scrolled);
        assert!(rows.contains(&"line 3".to_string()));
        assert!(!rows.contains(&"line 1".to_string()));
        assert_eq!((scrolled.more_above, scrolled.more_below), (true, true));

        let over = panel_lines(&panel(), 40, 18, 999);
        assert_eq!(
            over.scroll,
            scrolled.scroll + 1,
            "clamped to the last window"
        );
        assert!(texts(&over).contains(&"line 8".to_string()));
        assert!(!over.more_below);
    }

    /// A list longer than the panel windows around the cursor, keeping the
    /// cursor's row (and its detail) whole.
    #[test]
    fn a_long_option_list_windows_around_the_cursor() {
        let mut panel = panel();
        panel.body.clear();
        panel.items = (1..=12).map(|i| Item::new(format!("option {i}"))).collect();
        panel.cursor = 7;
        let layout = panel_lines(&panel, 40, 9, 0);
        let rows = texts(&layout);
        assert!(
            rows.iter().any(|row| row == "> 8. option 8"),
            "the cursor row is on screen: {rows:?}"
        );
        // The header and the subject held their rows; the list gave way.
        assert_eq!(rows[0], "▌ Bash command");
        assert_eq!(rows[1], "go test ./upstream");
        assert!(
            !rows.iter().any(|row| row == "  1. option 1"),
            "the far end of the list is windowed out: {rows:?}"
        );
        assert!(layout.more_below, "the panel says the list is clipped");
        assert!(layout.lines.len() <= 9);
    }

    /// Only the first nine rows get a number, because only those have a key.
    #[test]
    fn rows_past_the_ninth_carry_no_number() {
        let mut panel = panel();
        panel.body.clear();
        panel.items = (1..=11).map(|i| Item::new(format!("option {i}"))).collect();
        panel.cursor = 10;
        let rows = texts(&panel_lines(&panel, 40, 30, 0));
        assert!(rows.iter().any(|row| row == "  9. option 9"), "{rows:?}");
        assert!(rows.iter().any(|row| row == "  option 10"), "{rows:?}");
        assert!(rows.iter().any(|row| row == "> option 11"), "{rows:?}");
    }

    #[test]
    fn multi_select_rows_show_their_ticks() {
        let mut panel = panel();
        panel.body.clear();
        panel.multi_select = true;
        panel.checked = vec![2];
        let rows = texts(&panel_lines(&panel, 40, 30, 0));
        assert!(rows.contains(&"> 1. [ ] Yes".to_string()), "{rows:?}");
        assert!(rows.contains(&"  3. [x] No".to_string()), "{rows:?}");
    }

    /// An editor panel replaces the list with one input row and reports where
    /// the terminal cursor belongs.
    #[test]
    fn an_editor_panel_places_the_cursor_after_the_text() {
        let mut panel = panel();
        panel.body.clear();
        panel.editor = Some(Editor {
            prefix: "Your answer > ".into(),
            text: "abc".into(),
        });
        let layout = panel_lines(&panel, 40, 30, 0);
        let rows = texts(&layout);
        let (row, column) = layout.editor_cursor.expect("an editor takes the cursor");
        assert_eq!(rows[row], "Your answer > abc");
        assert_eq!(column, "Your answer > abc".len());
        // The options are gone: the panel is taking text, not a choice.
        assert!(!rows.iter().any(|row| row.contains("1. Yes")), "{rows:?}");
    }

    /// A one-row terminal still shows the one thing that can be acted on.
    #[test]
    fn a_degenerate_panel_keeps_the_hint() {
        let layout = panel_lines(&panel(), 40, 1, 0);
        assert_eq!(texts(&layout), vec!["Enter select · PgUp/PgDn scroll"]);
        assert!(panel_lines(&panel(), 0, 30, 0).lines.is_empty());
        assert!(panel_lines(&panel(), 40, 0, 0).lines.is_empty());
    }

    #[test]
    fn window_lines_slices_by_offset_and_flags_overflow() {
        let lines: Vec<Line<'static>> = (0..10).map(|i| Line::from(i.to_string())).collect();
        let seen = |ls: &[Line]| -> Vec<String> { ls.iter().map(text).collect() };

        // Everything fits: no clamp, no scroll needed, no hints.
        let (vis, scroll, up, down) = window_lines(&lines, 0, 10);
        assert_eq!((scroll, up, down), (0, false, false));
        assert_eq!(seen(&vis).len(), 10);

        // A window in the middle: both directions have more.
        let (vis, scroll, up, down) = window_lines(&lines, 3, 4);
        assert_eq!((scroll, up, down), (3, true, true));
        assert_eq!(seen(&vis), vec!["3", "4", "5", "6"]);

        // Over-scrolled: the offset self-corrects to the last full window.
        let (vis, scroll, up, down) = window_lines(&lines, 999, 4);
        assert_eq!((scroll, up, down), (6, true, false));
        assert_eq!(seen(&vis), vec!["6", "7", "8", "9"]);

        // No room at all: everything is still below.
        let (vis, _, up, down) = window_lines(&lines, 0, 0);
        assert!(vis.is_empty());
        assert_eq!((up, down), (false, true));
    }
}
