//! Shared response bounds for model-visible web content.

pub(crate) const MAX_DOWNLOAD_BYTES: usize = 5 * 1024 * 1024;
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
