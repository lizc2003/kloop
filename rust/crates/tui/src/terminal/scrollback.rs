//! Committing the overflow backlog into native scrollback in O(batches), not
//! O(cells) (plan 100). See the [module catalogue](super) for why the per-cell
//! path was slow.

use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget as _;

use super::PinnedBackend;

/// Per-`insert_before` cost is fixed regardless of the block's height: a
/// full-height inline viewport pays one full-screen scroll + `clear_region` +
/// flush every call (ratatui-core `insert_before_no_scrolling_regions`). So a
/// `-c` resume that commits its whole backlog one cell at a time did O(cells)
/// full-screen repaints — the "history scrolls for ages" dogfood report (plan
/// 100). Coalescing the backlog into a few tall batches makes it O(batches).
/// The cap bounds the transient `width × height` buffer each `insert_before`
/// allocates to a few MB while still collapsing hundreds of cells into a
/// handful of clears; `insert_before` chunks a batch taller than the screen on
/// its own, so an oversized single cell is fine as its own batch.
const SCROLLBACK_BATCH_ROWS: usize = 512;

/// Flatten per-cell blocks into batches no taller than `max_rows`, preserving
/// line order and content. A block is never split across batches (a cell's
/// lines stay contiguous); a block that alone exceeds `max_rows` becomes its
/// own oversized batch. Pure so the O(cells) → O(batches) collapse is unit
/// testable without a terminal.
fn coalesce_scrollback_batches(
    blocks: Vec<Vec<Line<'static>>>,
    max_rows: usize,
) -> Vec<Vec<Line<'static>>> {
    let max_rows = max_rows.max(1);
    let mut batches: Vec<Vec<Line<'static>>> = Vec::new();
    let mut current: Vec<Line<'static>> = Vec::new();
    for block in blocks {
        if block.is_empty() {
            continue;
        }
        if !current.is_empty() && current.len() + block.len() > max_rows {
            batches.push(std::mem::take(&mut current));
        }
        current.extend(block);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// Insert the overflow backlog into native scrollback. With `scrolling-regions`
/// off, Ratatui's full-height `insert_before` scrolls each batch off the bottom
/// with `append_lines` (a real LF-at-bottom scroll every terminal handles),
/// instead of the DECSTBM one-row scroll that iTerm2 smears (plan 99). Blocks
/// are coalesced into a few tall batches first so a long resume commits in
/// O(batches) full-screen repaints, not one per cell (plan 100).
///
/// No clear of our own: `insert_before` already ends with `Terminal::clear`,
/// which both blanks the inline viewport (committed scrollback stays intact —
/// an inline clear starts at the viewport) and resets the diff buffer so the
/// next draw repaints the tail and bottom chrome. The second clear this used to
/// do was not free — `Terminal::clear` opens with an `ESC[6n` cursor query, and
/// that one landed *after* the viewport had gone blank, so the screen stayed
/// black for as long as the reply waited on crossterm's reader lock: 205-335ms,
/// measured (plan 103). The clear ratatui does itself is free of that stall
/// because this function brackets the whole commit in
/// [`PinnedBackend::begin_commit`] — hence the [`PinnedBackend`] in the
/// signature: opening that window is part of committing, not something a caller
/// can forget.
pub(crate) fn insert_scrollback_blocks<B>(
    terminal: &mut ratatui::Terminal<PinnedBackend<B>>,
    blocks: Vec<Vec<Line<'static>>>,
) -> std::result::Result<(), B::Error>
where
    B: ratatui::backend::Backend,
{
    terminal.backend_mut().begin_commit();
    let result = insert_batches(terminal, blocks);
    terminal.backend_mut().end_commit();
    result
}

fn insert_batches<B>(
    terminal: &mut ratatui::Terminal<PinnedBackend<B>>,
    blocks: Vec<Vec<Line<'static>>>,
) -> std::result::Result<(), B::Error>
where
    B: ratatui::backend::Backend,
{
    for lines in coalesce_scrollback_batches(blocks, SCROLLBACK_BATCH_ROWS) {
        let height = lines.len() as u16;
        if height == 0 {
            continue;
        }
        terminal.insert_before(height, |buf| {
            let area = buf.area;
            Paragraph::new(lines).render(area, buf);
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell as StateCell;
    use std::cell::RefCell;

    use ratatui::TerminalOptions;
    use ratatui::Viewport;
    use ratatui::backend::Backend;
    use ratatui::backend::ClearType;
    use ratatui::backend::TestBackend;
    use ratatui::backend::WindowSize;
    use ratatui::buffer::Cell as BufferCell;
    use ratatui::layout::Position;
    use ratatui::layout::Size;

    use super::*;
    use crate::terminal::PinnedBackend;

    #[test]
    fn scrollback_insert_clears_viewport_without_clearing_history() {
        const WIDTH: u16 = 24;
        const HEIGHT: u16 = 6;
        let backend = PinnedBackend::new(TestBackend::new(WIDTH, HEIGHT));
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(HEIGHT),
            },
        )
        .unwrap();
        let initial = vec![
            Line::from("live tail"),
            Line::from(""),
            Line::from("────────────────────────"),
            Line::from("› draft"),
            Line::from("────────────────────────"),
            Line::from("[manual]"),
        ];
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(Paragraph::new(initial.clone()), area);
            })
            .unwrap();

        insert_scrollback_blocks(
            &mut terminal,
            vec![
                vec![Line::from("committed one"), Line::from("committed two")],
                vec![Line::from("committed three")],
            ],
        )
        .unwrap();

        terminal.backend().inner.assert_scrollback_lines([
            "committed one           ",
            "committed two           ",
            "committed three         ",
        ]);
        // TestBackend's AfterCursor keeps the cursor cell itself; every other
        // visible cell proves that the inline viewport was cleared. The real
        // Crossterm ED sequence clears from the cursor inclusively.
        assert!(
            terminal
                .backend()
                .inner
                .buffer()
                .content
                .iter()
                .skip(1)
                .all(|cell| cell.symbol() == " ")
        );

        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(Paragraph::new(initial), area);
            })
            .unwrap();
        terminal.backend().inner.assert_buffer_lines([
            "live tail               ",
            "                        ",
            "────────────────────────",
            "› draft                 ",
            "────────────────────────",
            "[manual]                ",
        ]);
    }

    #[test]
    fn coalesce_batches_collapses_many_cells_and_preserves_content() {
        // 100 one-line cells with a cap of 10 → 10 batches of 10 lines each,
        // in original order, nothing lost.
        let blocks: Vec<Vec<Line<'static>>> = (0..100)
            .map(|i| vec![Line::from(format!("cell {i:03}"))])
            .collect();
        let batches = coalesce_scrollback_batches(blocks, 10);
        assert_eq!(batches.len(), 10);
        assert!(batches.iter().all(|b| b.len() == 10));
        let flat: Vec<String> = batches
            .iter()
            .flatten()
            .map(|line| line.to_string())
            .collect();
        assert_eq!(flat.len(), 100);
        assert_eq!(flat[0], "cell 000");
        assert_eq!(flat[99], "cell 099");
    }

    #[test]
    fn coalesce_batches_never_splits_a_cell_and_lets_a_tall_cell_stand_alone() {
        // A cell taller than the cap is its own oversized batch; a cell is
        // never split across batches, so a 3-line cell that would overflow the
        // current batch starts a fresh one instead of straddling.
        let blocks = vec![
            vec![Line::from("a1"), Line::from("a2")], // 2
            vec![Line::from("b1"), Line::from("b2"), Line::from("b3")], // 3
            vec![
                Line::from("c1"),
                Line::from("c2"),
                Line::from("c3"),
                Line::from("c4"),
            ], // 4 > cap
            vec![Line::from("d1")],                   // 1
        ];
        let batches = coalesce_scrollback_batches(blocks, 3);
        let shapes: Vec<usize> = batches.iter().map(Vec::len).collect();
        // [a1,a2] | [b1,b2,b3] | [c1..c4] alone | [d1]
        assert_eq!(shapes, vec![2, 3, 4, 1]);
        assert_eq!(batches[2][0].to_string(), "c1");
        assert_eq!(batches[3][0].to_string(), "d1");
    }

    #[test]
    fn coalesce_batches_drops_empty_blocks() {
        let blocks = vec![vec![], vec![Line::from("x")], vec![], vec![Line::from("y")]];
        let batches = coalesce_scrollback_batches(blocks, 512);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 2);
    }

    /// Wraps `TestBackend` and counts `clear`/`clear_region` plus cursor
    /// queries. Each `insert_before` on an inline viewport ends with exactly one
    /// `clear_region`, so the clear count equals the number of `insert_before`
    /// calls — the O(cells) → O(batches) regression signal (plan 100). The
    /// cursor-query count is the flicker signal (plan 103): on the real backend
    /// every one of them is a blocking `ESC[6n` round trip, and ratatui asks for
    /// it right after blanking the viewport.
    struct ClearCountingBackend {
        inner: TestBackend,
        clears: StateCell<usize>,
        cursor_queries: StateCell<usize>,
    }

    impl ClearCountingBackend {
        fn new(width: u16, height: u16) -> Self {
            Self {
                inner: TestBackend::new(width, height),
                clears: StateCell::new(0),
                cursor_queries: StateCell::new(0),
            }
        }

        fn clear_calls(&self) -> usize {
            self.clears.get()
        }

        fn cursor_queries(&self) -> usize {
            self.cursor_queries.get()
        }
    }

    impl Backend for ClearCountingBackend {
        type Error = <TestBackend as Backend>::Error;

        fn draw<'a, I>(&mut self, content: I) -> std::result::Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a BufferCell)>,
        {
            self.inner.draw(content)
        }

        fn get_cursor_position(&mut self) -> std::result::Result<Position, Self::Error> {
            self.cursor_queries.set(self.cursor_queries.get() + 1);
            self.inner.get_cursor_position()
        }

        fn append_lines(&mut self, lines: u16) -> std::result::Result<(), Self::Error> {
            self.inner.append_lines(lines)
        }

        fn hide_cursor(&mut self) -> std::result::Result<(), Self::Error> {
            self.inner.hide_cursor()
        }

        fn show_cursor(&mut self) -> std::result::Result<(), Self::Error> {
            self.inner.show_cursor()
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> std::result::Result<(), Self::Error> {
            self.inner.set_cursor_position(position)
        }

        fn clear(&mut self) -> std::result::Result<(), Self::Error> {
            self.clears.set(self.clears.get() + 1);
            self.inner.clear()
        }

        fn clear_region(&mut self, clear_type: ClearType) -> std::result::Result<(), Self::Error> {
            self.clears.set(self.clears.get() + 1);
            self.inner.clear_region(clear_type)
        }

        fn size(&self) -> std::result::Result<Size, Self::Error> {
            self.inner.size()
        }

        fn window_size(&mut self) -> std::result::Result<WindowSize, Self::Error> {
            self.inner.window_size()
        }

        fn flush(&mut self) -> std::result::Result<(), Self::Error> {
            self.inner.flush()
        }
    }

    #[test]
    fn resume_backlog_commits_in_batches_not_one_clear_per_cell() {
        // A full-height inline viewport plus a long backlog of short cells: the
        // exact `-c` resume shape. Committing per cell would drive one
        // full-screen scroll + clear_region per cell (O(cells)); coalescing
        // makes it O(batches). 300 one-line cells fit one 512-row batch, so the
        // whole commit is one insert_before — one clear_region, not ~300.
        //
        // The same run pins the flicker fix (plan 103): the commit must ask the
        // terminal for the cursor position zero times. Each query is a blocking
        // `ESC[6n` on the real backend, and ratatui issues it right after the
        // clear has blanked the viewport — measured at 205-335ms of black screen
        // per commit before this was suppressed.
        const WIDTH: u16 = 40;
        const HEIGHT: u16 = 10;
        let backend = PinnedBackend::new(ClearCountingBackend::new(WIDTH, HEIGHT));
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(HEIGHT),
            },
        )
        .unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(Paragraph::new(vec![Line::from("live tail")]), area);
            })
            .unwrap();

        let cells = 300;
        let blocks: Vec<Vec<Line<'static>>> = (0..cells)
            .map(|i| vec![Line::from(format!("committed {i}"))])
            .collect();
        let before = terminal.backend().inner.clear_calls();
        let queries_before = terminal.backend().inner.cursor_queries();
        insert_scrollback_blocks(&mut terminal, blocks).unwrap();
        let clears = terminal.backend().inner.clear_calls() - before;
        let queries = terminal.backend().inner.cursor_queries() - queries_before;

        // One 512-row batch → one insert_before → one clear_region. The point is
        // it does NOT scale with cells.
        assert!(
            clears < cells / 10,
            "commit did {clears} clears for {cells} cells — expected batched, not per-cell"
        );
        assert_eq!(clears, 1, "expected a single batch and no clear of our own");
        assert_eq!(queries, 0, "a commit must not stall on a cursor query");
    }

    /// Records the `(x, y, symbol)` cells handed to `Backend::draw`, so a test
    /// can assert exactly what the terminal is asked to print.
    struct RecordingBackend {
        inner: TestBackend,
        drawn: RefCell<Vec<(u16, u16, String)>>,
    }

    impl RecordingBackend {
        fn new(width: u16, height: u16) -> Self {
            Self {
                inner: TestBackend::new(width, height),
                drawn: RefCell::new(Vec::new()),
            }
        }
    }

    impl Backend for RecordingBackend {
        type Error = <TestBackend as Backend>::Error;

        fn draw<'a, I>(&mut self, content: I) -> std::result::Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a BufferCell)>,
        {
            let cells: Vec<(u16, u16, BufferCell)> =
                content.map(|(x, y, c)| (x, y, c.clone())).collect();
            self.drawn.borrow_mut().extend(
                cells
                    .iter()
                    .map(|(x, y, c)| (*x, *y, c.symbol().to_string())),
            );
            self.inner.draw(cells.iter().map(|(x, y, c)| (*x, *y, c)))
        }

        fn append_lines(&mut self, lines: u16) -> std::result::Result<(), Self::Error> {
            self.inner.append_lines(lines)
        }

        fn hide_cursor(&mut self) -> std::result::Result<(), Self::Error> {
            self.inner.hide_cursor()
        }

        fn show_cursor(&mut self) -> std::result::Result<(), Self::Error> {
            self.inner.show_cursor()
        }

        fn get_cursor_position(&mut self) -> std::result::Result<Position, Self::Error> {
            self.inner.get_cursor_position()
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> std::result::Result<(), Self::Error> {
            self.inner.set_cursor_position(position)
        }

        fn clear(&mut self) -> std::result::Result<(), Self::Error> {
            self.inner.clear()
        }

        fn clear_region(&mut self, clear_type: ClearType) -> std::result::Result<(), Self::Error> {
            self.inner.clear_region(clear_type)
        }

        fn size(&self) -> std::result::Result<Size, Self::Error> {
            self.inner.size()
        }

        fn window_size(&mut self) -> std::result::Result<WindowSize, Self::Error> {
            self.inner.window_size()
        }

        fn flush(&mut self) -> std::result::Result<(), Self::Error> {
            self.inner.flush()
        }
    }

    /// ratatui's no-scrolling-regions `insert_before` blits every cell of the
    /// committed block, including the " " placeholder that trails each wide
    /// grapheme. `PinnedBackend::draw` drops those continuation cells (mirroring
    /// the live-draw diff), so committed CJK reads "关键" and not "关 键".
    #[test]
    fn scrollback_commit_drops_wide_char_continuation_cells() {
        const WIDTH: u16 = 8;
        const HEIGHT: u16 = 4;
        let backend = PinnedBackend::new(RecordingBackend::new(WIDTH, HEIGHT));
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(HEIGHT),
            },
        )
        .unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new(vec![Line::from("live")]), frame.area());
            })
            .unwrap();

        terminal.backend().inner.drawn.borrow_mut().clear();
        insert_scrollback_blocks(&mut terminal, vec![vec![Line::from("关键Ab")]]).unwrap();

        // The committed row: wide chars sit two columns apart with no interstitial
        // placeholder (x=1 and x=3 are dropped); ASCII stays width-1; the tail is
        // real padding, not a continuation cell.
        let row: Vec<(u16, String)> = terminal
            .backend()
            .inner
            .drawn
            .borrow()
            .iter()
            .filter(|(_, y, _)| *y == 0)
            .map(|(x, _, s)| (*x, s.clone()))
            .collect();
        assert_eq!(
            row,
            vec![
                (0, "关".to_string()),
                (2, "键".to_string()),
                (4, "A".to_string()),
                (5, "b".to_string()),
                (6, " ".to_string()),
                (7, " ".to_string()),
            ],
            "wide-char continuation cells must not reach the terminal"
        );
    }
}
