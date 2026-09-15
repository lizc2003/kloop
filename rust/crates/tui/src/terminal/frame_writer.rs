//! `FrameWriter`: hand the terminal one write per frame instead of a dozen.
//!
//! ratatui-crossterm builds every non-cell command with `execute!`, which
//! flushes the writer each time — a cursor move, a clear, show/hide cursor all
//! reach the terminal on their own. For kloop's full-height inline viewport
//! that is not merely chatty: `insert_before` ends by clearing the whole
//! viewport, so that clear is flushed *by itself* and the screen sits blank
//! until the repaint arrives in a later write (plan 103; the black flash).
//!
//! So this writer swallows mid-frame flushes: bytes pile up until the draw loop
//! calls `PinnedBackend::commit_frame`, which releases exactly one real write —
//! clear and repaint together — wrapped in synchronized output (DEC private
//! mode 2026, ignored by terminals that lack it) so a terminal that does
//! support it never presents a half-painted frame either.

use std::io::Result;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;

/// Tell the terminal to hold its rendering until the matching end: the frame is
/// presented as one update instead of whatever happened to be parsed by the
/// next refresh.
const BEGIN_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026h";
const END_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026l";

/// Shared handle: one clone is the backend's writer (it only ever appends), the
/// other lives on [`PinnedBackend`](super::PinnedBackend) to release the frame.
/// Cheap to clone and `Send`, so the terminal stays usable from the async loop.
pub(crate) struct FrameWriter<W: Write>(Arc<Mutex<Frame<W>>>);

// Hand-written: the derive would demand `W: Clone`, and the whole point is that
// the two handles share one `Stdout` and one pending frame.
impl<W: Write> Clone for FrameWriter<W> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

struct Frame<W: Write> {
    out: W,
    pending: Vec<u8>,
    /// While held, `flush` keeps buffering; `release` clears it for exactly one
    /// flush, which is the frame handover.
    held: bool,
}

impl<W: Write> FrameWriter<W> {
    pub(crate) fn new(out: W) -> Self {
        Self(Arc::new(Mutex::new(Frame {
            out,
            pending: Vec::new(),
            held: true,
        })))
    }

    /// Let the next `flush` through. The frame re-arms itself afterwards, so
    /// every handover is deliberate.
    pub(crate) fn release(&self) {
        self.frame().held = false;
    }

    fn frame(&self) -> std::sync::MutexGuard<'_, Frame<W>> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }
}

impl<W: Write> Write for FrameWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> Result<usize> {
        self.frame().pending.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> Result<()> {
        let mut frame = self.frame();
        if frame.held {
            return Ok(());
        }
        frame.held = true;
        if frame.pending.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut frame.pending);
        frame.out.write_all(BEGIN_SYNCHRONIZED_UPDATE)?;
        frame.out.write_all(&pending)?;
        frame.out.write_all(END_SYNCHRONIZED_UPDATE)?;
        frame.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records each real write so a test can count frame handovers.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<Vec<u8>>>>);

    impl Sink {
        fn writes(&self) -> Vec<Vec<u8>> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> Result<usize> {
            self.0.lock().unwrap().push(bytes.to_vec());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }

    /// The whole point: a frame's clear and repaint reach the terminal in one
    /// write, wrapped in synchronized output — never a flushed clear followed by
    /// a separate repaint (which is what leaves the screen blank in between).
    #[test]
    fn mid_frame_flushes_buffer_and_release_writes_one_synchronized_frame() {
        let sink = Sink::default();
        let mut writer = FrameWriter::new(sink.clone());

        writer.write_all(b"\x1b[2J").unwrap();
        writer.flush().unwrap();
        writer.write_all(b"repaint").unwrap();
        writer.flush().unwrap();
        assert!(sink.writes().is_empty(), "mid-frame flushes must not write");

        writer.release();
        writer.flush().unwrap();
        assert_eq!(
            sink.writes(),
            vec![
                BEGIN_SYNCHRONIZED_UPDATE.to_vec(),
                b"\x1b[2Jrepaint".to_vec(),
                END_SYNCHRONIZED_UPDATE.to_vec(),
            ]
        );

        // The release covers exactly one handover, and an empty frame writes
        // nothing at all (an idle redraw must not emit bare sync markers).
        writer.flush().unwrap();
        writer.release();
        writer.flush().unwrap();
        assert_eq!(sink.writes().len(), 3);
    }
}
