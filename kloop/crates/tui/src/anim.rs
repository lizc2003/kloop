//! Status-line animation primitives (plan 38 slice 5): a braille spinner, a
//! moving "shimmer" light band over a verb, and an elapsed-time formatter. All
//! pure functions of a wall-clock–derived `phase` (the event loop advances the
//! phase from the turn's elapsed time and forces a redraw each frame), so there
//! is no clock in here and the whole module is unit-tested without a terminal.
//! When motion is reduced (a `dumb` terminal or `KLOOP_NO_ANIM`), the spinner
//! freezes to one glyph and the shimmer degrades to a plain bold verb.

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Span;

/// One phase step per this many milliseconds — the spinner advances a glyph and
/// the shimmer band moves a column each step. The event loop uses the same value
/// as its redraw cadence while animating.
pub const STEP_MS: u64 = 100;

/// The braille spinner cycle. `phase` indexes into it (mod its length).
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The static glyph shown when motion is reduced (or a turn is momentarily
/// blocked): a steady bullet rather than a spinning one.
const STATIC_GLYPH: &str = "●";

/// The spinner glyph for `phase`; a steady bullet when motion is reduced.
pub fn spinner_glyph(phase: usize, reduced: bool) -> &'static str {
    if reduced {
        STATIC_GLYPH
    } else {
        SPINNER[phase % SPINNER.len()]
    }
}

/// How many characters the bright band spans.
const BAND: usize = 4;

/// Render `text` with a moving highlight band: the whole word is dim except a
/// `BAND`-wide bright run that sweeps left→right and off, once per cycle. Reduced
/// motion returns a single bold span (no sweep). Consecutive same-brightness
/// chars coalesce, so a short verb is at most a few spans.
pub fn shimmer_spans(text: &str, phase: usize, reduced: bool) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if reduced || n == 0 {
        return vec![Span::styled(
            text.to_string(),
            Style::new().add_modifier(Modifier::BOLD),
        )];
    }
    // The band's leading edge sweeps 0..n+BAND, so it enters from the left and
    // fully exits on the right before the next cycle.
    let head = phase % (n + BAND);
    let lit = |i: usize| i < head && i + BAND >= head; // i in [head-BAND, head)

    let bright = Style::new().add_modifier(Modifier::BOLD);
    let faint = Style::new().add_modifier(Modifier::DIM);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut buf_lit: Option<bool> = None;
    for (i, &c) in chars.iter().enumerate() {
        let l = lit(i);
        if buf_lit.is_some_and(|b| b != l) {
            let style = if buf_lit == Some(true) { bright } else { faint };
            spans.push(Span::styled(std::mem::take(&mut buf), style));
        }
        buf.push(c);
        buf_lit = Some(l);
    }
    if !buf.is_empty() {
        let style = if buf_lit == Some(true) { bright } else { faint };
        spans.push(Span::styled(buf, style));
    }
    spans
}

/// Compact elapsed like `8s`, `1m05s`, `2h03m` (seconds dropped past an hour).
pub fn format_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Whether the environment asks for reduced motion: a `dumb` terminal, or
/// `KLOOP_NO_ANIM` set to anything. Read once at startup by the event loop.
pub fn reduced_motion() -> bool {
    std::env::var_os("KLOOP_NO_ANIM").is_some() || std::env::var("TERM").is_ok_and(|t| t == "dumb")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn spinner_cycles_unless_reduced() {
        assert_eq!(spinner_glyph(0, false), "⠋");
        assert_eq!(spinner_glyph(10, false), "⠋"); // wraps
        assert_ne!(spinner_glyph(1, false), spinner_glyph(0, false));
        assert_eq!(spinner_glyph(3, true), STATIC_GLYPH);
    }

    #[test]
    fn shimmer_preserves_text_and_moves_the_band() {
        // The concatenated spans always reproduce the input, whatever the phase.
        for phase in 0..20 {
            assert_eq!(text_of(&shimmer_spans("Working", phase, false)), "Working");
        }
        // Reduced motion: a single bold span.
        let r = shimmer_spans("Working", 5, true);
        assert_eq!(r.len(), 1);
        assert!(r[0].style.add_modifier.contains(Modifier::BOLD));

        // At some phase, a middle char is bright while an outer one is dim — i.e.
        // the band is a proper subset (a real sweep, not all-on/all-off).
        let mut saw_mixed = false;
        for phase in 0..12 {
            let spans = shimmer_spans("Working", phase, false);
            let has_bright = spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
            let has_dim = spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::DIM));
            saw_mixed |= has_bright && has_dim;
        }
        assert!(saw_mixed, "the band should light only part of the word");
    }

    #[test]
    fn elapsed_formats_across_scales() {
        assert_eq!(format_elapsed(8), "8s");
        assert_eq!(format_elapsed(65), "1m05s");
        assert_eq!(format_elapsed(3600 + 3 * 60 + 4), "1h03m");
    }
}
