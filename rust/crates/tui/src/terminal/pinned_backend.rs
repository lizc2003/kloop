//! `PinnedBackend`: the single backend wrapper every draw funnels through. Four
//! jobs, all serving a correct — and flicker-free — scrollback commit (see the
//! [module catalogue](super)):
//!
//! - **Wide-char continuation skip (plan 101).** [`PinnedBackend`]'s `draw`
//!   drops the " " placeholder that trails each wide grapheme, mirroring the skip
//!   `Terminal::draw`'s diff already does — so `insert_before` (which blits every
//!   cell verbatim with `scrolling-regions` off) stops printing "关 键 逻 辑".
//! - **Size pin.** [`PinnedBackend::pin_current_size`] freezes one physical size
//!   across a commit transaction so autoresize confirmation, insert/drain, and
//!   the repaint share one geometry even if a resize arrives mid-transaction.
//! - **No cursor round trip after the viewport's construction probe (plan 103).**
//!   [`PinnedBackend::get_cursor_position`] answers from the position we last
//!   sent instead of an `ESC[6n` query, which would block on crossterm's reader
//!   lock — held for up to one poll interval by the input thread — and can time
//!   out entirely when another client owns the terminal. A commit is bracketed
//!   by [`PinnedBackend::begin_commit`] so it never queries even before the
//!   first position exists; the construction probe's failure falls back to the
//!   origin, where a full-height viewport anchors anyway. A cached row from a
//!   taller screen is clamped to the current size before it is returned, so a
//!   shrink cannot anchor the viewport off-screen.
//! - **Frame handover (plan 103).** [`PinnedBackend::commit_frame`] releases the
//!   [`FrameWriter`] so one frame's bytes reach the terminal in a single
//!   synchronized write, clear and repaint together.

use std::cell::Cell;

use super::FrameWriter;

pub(crate) struct PinnedBackend<B> {
    pub(crate) inner: B,
    pinned_size: Option<ratatui::layout::Size>,
    /// The physical size last reported by `Backend::size`. Resize anchoring
    /// clamps a cached cursor row to it without asking the terminal again.
    last_size: Cell<Option<ratatui::layout::Size>>,
    /// Present for the real terminal, absent for test backends (which flush
    /// straight through their own writer).
    frames: Option<FrameWriter<std::io::Stdout>>,
    /// The position we last told the terminal to move to. After the viewport's
    /// construction probe it is where the cursor actually is — kloop owns the
    /// terminal and every frame ends by setting it — so it answers every later
    /// cursor query.
    last_cursor: Option<ratatui::layout::Position>,
    committing: bool,
}

impl<B> PinnedBackend<B> {
    pub(crate) fn new(inner: B) -> Self {
        Self {
            inner,
            pinned_size: None,
            last_size: Cell::new(None),
            frames: None,
            last_cursor: None,
            committing: false,
        }
    }

    /// The real terminal: `frames` is the same [`FrameWriter`] the inner
    /// backend writes into, so [`Self::commit_frame`] can release a frame.
    pub(crate) fn with_frames(inner: B, frames: FrameWriter<std::io::Stdout>) -> Self {
        Self {
            frames: Some(frames),
            ..Self::new(inner)
        }
    }

    /// Open the window where `Terminal::clear` — which `insert_before` runs at
    /// the end of every committed batch — must not query the cursor: the
    /// viewport is being torn down and repainted inside one frame, so the only
    /// thing ratatui does with the answer is put the cursor back, which the
    /// repaint does anyway.
    pub(crate) fn begin_commit(&mut self) {
        self.committing = true;
    }

    pub(crate) fn end_commit(&mut self) {
        self.committing = false;
    }

    /// Queue an erase of the terminal's scrollback (`ESC[3J`) into the pending
    /// frame, after whatever the frame already holds. ratatui's `ClearType` has
    /// no scrollback variant, so this goes around the inner backend. Test
    /// backends have no scrollback to erase.
    pub(crate) fn purge_scrollback(&mut self) {
        if let Some(frames) = &mut self.frames {
            // Appending to the pending frame cannot fail.
            let _ = crossterm::queue!(
                frames,
                crossterm::terminal::Clear(crossterm::terminal::ClearType::Purge)
            );
        }
    }
}

impl<B: ratatui::backend::Backend> PinnedBackend<B> {
    pub(crate) fn pin_current_size(&mut self) -> std::result::Result<(), B::Error> {
        self.pinned_size = Some(self.inner.size()?);
        Ok(())
    }

    pub(crate) fn unpin_size(&mut self) {
        self.pinned_size = None;
    }

    /// Hand the terminal everything drawn since the last handover as one write.
    /// Test backends have no [`FrameWriter`]; their flush is already immediate.
    pub(crate) fn commit_frame(&mut self) -> std::result::Result<(), B::Error> {
        if let Some(frames) = &self.frames {
            frames.release();
        }
        self.inner.flush()
    }
}

impl<B: ratatui::backend::Backend> ratatui::backend::Backend for PinnedBackend<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> std::result::Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        // Drop the placeholder cells that trail a wide (CJK/emoji) grapheme.
        // `Terminal::draw`'s diff already skips them, but ratatui-core's
        // no-scrolling-regions `insert_before` (`draw_lines`) blits every cell of
        // the block verbatim — including each wide char's " " continuation cell.
        // crossterm advances the cursor by one per cell, so those extra spaces
        // land as a visible gap after every wide glyph (committed-to-scrollback
        // CJK read "关 键 逻 辑"). Mirror the diff's skip here, at the one backend
        // all draws funnel through, so both paths agree. Non-contiguous cells
        // reset the run, so this is a no-op for the already-skipped draw path.
        let mut to_skip = 0usize;
        let mut next_x: Option<(u16, u16)> = None;
        let filtered = content.filter(move |&(x, y, cell)| {
            let continues = next_x == Some((x, y));
            next_x = Some((x + 1, y));
            if to_skip > 0 && continues {
                to_skip -= 1;
                return false;
            }
            to_skip = unicode_width::UnicodeWidthStr::width(cell.symbol()).saturating_sub(1);
            true
        });
        self.inner.draw(filtered)
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

    fn get_cursor_position(
        &mut self,
    ) -> std::result::Result<ratatui::layout::Position, Self::Error> {
        // After the construction probe, the position we last sent is where the
        // terminal's cursor is: kloop owns the terminal and every frame ends by
        // setting it. A real query would instead wait on crossterm's reader
        // lock — held for up to one 200ms poll by the input thread — and, when
        // another client owns the terminal and nobody answers, time out after
        // two seconds. Resize anchoring and `Terminal::clear` both used to pay
        // that; a resize must not be able to end the session. The commit
        // bracket still covers the case before any position exists (plan 103).
        if self.committing {
            return Ok(self
                .last_cursor
                .unwrap_or(ratatui::layout::Position::ORIGIN));
        }
        if let Some(position) = self.last_cursor {
            // A cached row can be outside the terminal after a shrink: the
            // viewport was anchored at the bottom of a taller screen, and the
            // terminal clamps the real cursor to the new bottom row. Mirror
            // that here, or ratatui anchors the inline viewport below the new
            // screen and the buffer resize panics. `autoresize` reports the
            // new size through `size()` before it calls this.
            let Some(size) = self.last_size.get() else {
                return Ok(position);
            };
            let position = ratatui::layout::Position {
                x: position.x.min(size.width.saturating_sub(1)),
                y: position.y.min(size.height.saturating_sub(1)),
            };
            self.last_cursor = Some(position);
            return Ok(position);
        }
        // The viewport's construction probe. The terminal must first have seen
        // everything already drawn.
        self.commit_frame()?;
        match self.inner.get_cursor_position() {
            Ok(position) => {
                self.last_cursor = Some(position);
                Ok(position)
            }
            // A full-height inline viewport computes the same top row (0) from
            // any answer, so one unanswered probe must not abort startup. The
            // cache keeps later resize and clear from querying again.
            Err(_) => {
                self.last_cursor = Some(ratatui::layout::Position::ORIGIN);
                Ok(ratatui::layout::Position::ORIGIN)
            }
        }
    }

    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> std::result::Result<(), Self::Error> {
        let position = position.into();
        self.last_cursor = Some(position);
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> std::result::Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(
        &mut self,
        clear_type: ratatui::backend::ClearType,
    ) -> std::result::Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> std::result::Result<ratatui::layout::Size, Self::Error> {
        let size = match self.pinned_size {
            Some(size) => size,
            None => self.inner.size()?,
        };
        self.last_size.set(Some(size));
        Ok(size)
    }

    fn window_size(&mut self) -> std::result::Result<ratatui::backend::WindowSize, Self::Error> {
        let mut size = self.inner.window_size()?;
        if let Some(pinned) = self.pinned_size {
            size.columns_rows = pinned;
        }
        Ok(size)
    }

    fn flush(&mut self) -> std::result::Result<(), Self::Error> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io;

    use ratatui::Terminal;
    use ratatui::TerminalOptions;
    use ratatui::Viewport;
    use ratatui::backend::Backend;
    use ratatui::backend::ClearType;
    use ratatui::backend::WindowSize;
    use ratatui::buffer::Cell as BufferCell;
    use ratatui::layout::Position;
    use ratatui::layout::Rect;
    use ratatui::layout::Size;

    use super::PinnedBackend;

    /// Counts cursor probes and can refuse them, standing in for a terminal
    /// that stopped answering CPR.
    struct ProbeBackend {
        probes: Cell<usize>,
        cursor: Cell<Position>,
        size: Cell<Size>,
        fail: bool,
    }

    impl ProbeBackend {
        fn new(size: Size, cursor: Position, fail: bool) -> Self {
            Self {
                probes: Cell::new(0),
                cursor: Cell::new(cursor),
                size: Cell::new(size),
                fail,
            }
        }

        fn probes(&self) -> usize {
            self.probes.get()
        }
    }

    impl Backend for ProbeBackend {
        type Error = io::Error;

        fn draw<'a, I>(&mut self, _content: I) -> std::result::Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a BufferCell)>,
        {
            Ok(())
        }

        fn hide_cursor(&mut self) -> std::result::Result<(), Self::Error> {
            Ok(())
        }

        fn show_cursor(&mut self) -> std::result::Result<(), Self::Error> {
            Ok(())
        }

        fn get_cursor_position(&mut self) -> std::result::Result<Position, Self::Error> {
            self.probes.set(self.probes.get() + 1);
            if self.fail {
                Err(io::Error::other(
                    "nobody answered the cursor-position query",
                ))
            } else {
                Ok(self.cursor.get())
            }
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> std::result::Result<(), Self::Error> {
            self.cursor.set(position.into());
            Ok(())
        }

        fn clear(&mut self) -> std::result::Result<(), Self::Error> {
            Ok(())
        }

        fn clear_region(&mut self, _clear_type: ClearType) -> std::result::Result<(), Self::Error> {
            Ok(())
        }

        fn size(&self) -> std::result::Result<Size, Self::Error> {
            Ok(self.size.get())
        }

        fn window_size(&mut self) -> std::result::Result<WindowSize, Self::Error> {
            Ok(WindowSize {
                columns_rows: self.size.get(),
                pixels: Size::new(0, 0),
            })
        }

        fn flush(&mut self) -> std::result::Result<(), Self::Error> {
            Ok(())
        }
    }

    fn inline_terminal(
        backend: ProbeBackend,
        height: u16,
    ) -> Terminal<PinnedBackend<ProbeBackend>> {
        Terminal::with_options(
            PinnedBackend::new(backend),
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )
        .unwrap()
    }

    #[test]
    fn a_refused_construction_probe_still_starts_and_anchors_at_the_origin() {
        let mut terminal = inline_terminal(
            ProbeBackend::new(Size::new(10, 6), Position::new(2, 5), true),
            6,
        );
        assert_eq!(terminal.backend().inner.probes(), 1);
        assert_eq!(terminal.get_frame().area(), Rect::new(0, 0, 10, 6));

        terminal.backend_mut().inner.size.set(Size::new(10, 4));
        terminal.resize(Rect::new(0, 0, 10, 4)).unwrap();
        assert_eq!(terminal.backend().inner.probes(), 1);
        assert_eq!(terminal.get_frame().area(), Rect::new(0, 0, 10, 4));
    }

    #[test]
    fn resize_and_clear_reuse_the_last_sent_position() {
        let mut terminal = inline_terminal(
            ProbeBackend::new(Size::new(10, 6), Position::new(4, 5), false),
            6,
        );
        assert_eq!(terminal.backend().inner.probes(), 1);

        terminal
            .backend_mut()
            .set_cursor_position(Position::new(1, 2))
            .unwrap();
        assert_eq!(
            terminal.backend_mut().get_cursor_position().unwrap(),
            Position::new(1, 2)
        );

        terminal.backend_mut().inner.size.set(Size::new(10, 4));
        terminal.resize(Rect::new(0, 0, 10, 4)).unwrap();
        terminal.clear().unwrap();
        assert_eq!(terminal.backend().inner.probes(), 1);
        assert_eq!(terminal.get_frame().area(), Rect::new(0, 0, 10, 4));
    }

    #[test]
    fn a_shrink_after_growing_keeps_the_viewport_on_screen() {
        const WIDTH: u16 = 20;
        const BOOT_HEIGHT: u16 = 30;
        const GROWN_HEIGHT: u16 = 60;

        let mut inner = ratatui::backend::TestBackend::new(WIDTH, BOOT_HEIGHT);
        inner
            .set_cursor_position(Position::new(0, BOOT_HEIGHT - 1))
            .unwrap();
        let mut terminal = Terminal::with_options(
            PinnedBackend::new(inner),
            TerminalOptions {
                viewport: Viewport::Inline(BOOT_HEIGHT),
            },
        )
        .unwrap();

        // Grow above the boot height, then commit a viewport-tall block. The
        // now-short viewport moves to the bottom of the taller screen, and the
        // next frame leaves the cursor on row 59.
        terminal.backend_mut().inner.resize(WIDTH, GROWN_HEIGHT);
        terminal.autoresize().unwrap();
        terminal.backend_mut().begin_commit();
        terminal.insert_before(BOOT_HEIGHT, |_| {}).unwrap();
        terminal.backend_mut().end_commit();
        terminal
            .draw(|frame| {
                frame.set_cursor_position(Position::new(0, GROWN_HEIGHT - 1));
            })
            .unwrap();
        assert_eq!(
            terminal.backend().last_cursor,
            Some(Position::new(0, GROWN_HEIGHT - 1))
        );
        assert_eq!(
            terminal.get_frame().area(),
            Rect::new(0, BOOT_HEIGHT, WIDTH, BOOT_HEIGHT)
        );

        // Shrinking back to the boot height must clamp the cached row to the
        // new bottom; otherwise ratatui anchors the viewport at row 30 on a
        // 30-row screen.
        terminal.backend_mut().inner.resize(WIDTH, BOOT_HEIGHT);
        terminal.autoresize().unwrap();
        assert_eq!(
            terminal.get_frame().area(),
            Rect::new(0, 0, WIDTH, BOOT_HEIGHT)
        );
        assert!(
            terminal
                .backend()
                .last_cursor
                .is_some_and(|position| position.y < BOOT_HEIGHT),
            "the cached cursor must be clamped into the shrunk screen"
        );
    }
}
