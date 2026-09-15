//! Shared response bounds for model-visible web content.

pub(crate) const MAX_DOWNLOAD_BYTES: usize = 5 * 1024 * 1024;

/// Bound on formatted `web_search` output only. `web_fetch` deliberately has no
/// text cap: its body is a fetched artifact, and clipping one destroys evidence
/// that core's offload seam would otherwise keep on disk. Search output is
/// synthesized here from a fixed, small result set — there is no artifact to
/// lose — so the bound stays.
pub(crate) const MAX_TEXT_CHARS: usize = 50_000;

pub(crate) fn truncate_chars(mut text: String, max: usize) -> (String, bool) {
    match text.char_indices().nth(max).map(|(cut, _)| cut) {
        Some(cut) => {
            text.truncate(cut);
            (text, true)
        }
        None => (text, false),
    }
}
