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
//! - **No cursor round trip inside a commit (plan 103).**
//!   [`PinnedBackend::begin_commit`] answers `get_cursor_position` from the
//!   position we last sent instead of an `ESC[6n` query, which would block on
//!   crossterm's reader lock — held for up to one poll interval by the input
//!   thread — while the viewport is already cleared.
//! - **Frame handover (plan 103).** [`PinnedBackend::commit_frame`] releases the
//!   [`FrameWriter`] so one frame's bytes reach the terminal in a single
//!   synchronized write, clear and repaint together.

use super::FrameWriter;

pub(crate) struct PinnedBackend<B> {
    pub(crate) inner: B,
    pinned_size: Option<ratatui::layout::Size>,
    /// Present for the real terminal, absent for test backends (which flush
    /// straight through their own writer).
    frames: Option<FrameWriter<std::io::Stdout>>,
    /// The position we last told the terminal to move to; stands in for a
    /// cursor query while a commit is in flight.
    last_cursor: Option<ratatui::layout::Position>,
    committing: bool,
}

impl<B> PinnedBackend<B> {
    pub(crate) fn new(inner: B) -> Self {
        Self {
            inner,
            pinned_size: None,
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
        // Inside a commit the honest answer costs an `ESC[6n` round trip whose
        // reply waits on crossterm's reader lock — held for up to one 200ms
        // poll by the input thread — and `Terminal::clear` asks for it right
        // after blanking the viewport. Measured: 205-335ms of black screen per
        // overflow commit (plan 103). The position we last sent is what the
        // terminal's cursor is at anyway, and the repaint sets it regardless.
        if self.committing
            && let Some(position) = self.last_cursor
        {
            return Ok(position);
        }
        // Outside a commit the query is real (viewport init, resize anchoring),
        // so the terminal must first have seen everything already drawn.
        self.commit_frame()?;
        self.inner.get_cursor_position()
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
        self.pinned_size.map_or_else(|| self.inner.size(), Ok)
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
