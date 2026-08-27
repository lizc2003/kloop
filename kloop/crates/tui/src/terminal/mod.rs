//! The terminal/backend layer: the one place kloop compensates for ratatui-core
//! quirks around its inline viewport → native-scrollback commit path.
//!
//! kloop's inline viewport freezes finalized cells into the terminal's native
//! scrollback with `Terminal::insert_before`. All the compensations here exist
//! under one deliberate choice — the `scrolling-regions` cargo feature is OFF —
//! and that choice is linked to them: turning the feature back on silently
//! restores the two behaviours the last two items work around. **Re-check every
//! item on any ratatui bump; this doc is the checklist.**
//!
//! - **`scrolling-regions` OFF (plan 99).** The feature scrolls with DECSTBM
//!   one-row region scrolls that iTerm2 smears; kloop leaves it off so
//!   `insert_before` scrolls each batch off the bottom with a plain
//!   LF-at-bottom `append_lines` every terminal handles.
//! - **Wide-char continuation cells (plan 101)** → [`pinned_backend`].
//!   `Terminal::draw`'s diff skips the " " placeholder trailing each wide
//!   (CJK/emoji) grapheme, but the no-scrolling-regions `insert_before`
//!   (`draw_lines`) blits every cell verbatim. crossterm advances one column per
//!   cell, so those spaces land as a visible gap ("关 键 逻 辑").
//!   [`PinnedBackend`]'s `draw` mirrors the diff's skip at the one backend all
//!   draws funnel through, so both paths agree.
//! - **Per-`insert_before` fixed cost (plan 100)** → [`scrollback`]. A
//!   full-height inline viewport pays one full-screen scroll + `clear_region` +
//!   flush per call regardless of block height. Committing a long `-c` backlog
//!   one cell at a time is O(cells) full-screen repaints;
//!   [`scrollback::insert_scrollback_blocks`] coalesces it to O(batches).
//!
//! [`PinnedBackend`] also pins one physical size across a commit transaction so
//! autoresize confirmation, insert/drain, and the repaint share one geometry
//! even if a resize arrives mid-transaction (consumed by `draw_frame`).

mod pinned_backend;
mod scrollback;

pub(crate) use pinned_backend::PinnedBackend;
pub(crate) use scrollback::insert_scrollback_blocks;
