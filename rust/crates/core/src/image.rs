//! Image ingestion: sniff the format from magic bytes (never the extension),
//! enforce the size cap, and build a canonical base64 Image content block.
//! Shared by the `--image` CLI entry (plan 29 slice 1) and, later, a
//! `view_image` tool (slice 2). Pure over its byte input, so both entry points
//! test the same validation.

use anyhow::Result;
use anyhow::bail;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use kloop_protocol::ContentBlock;
use kloop_protocol::ImageSource;

/// Raw-byte cap for a single image (5 MiB), matching cc's single-image limit.
/// Oversized images are refused, not resized — client-side downscaling is
/// deferred (the user shrinks the image and retries).
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Sniff the media type from the leading magic bytes, returning the Anthropic
/// media_type string or None for anything outside png/jpeg/gif/webp. The file
/// extension is never trusted: the wire needs the true type, and a mislabeled
/// file would be rejected by the API regardless. GIF animation is not
/// inspected here (deferred); both static and animated GIFs pass the sniff.
pub fn detect_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        // WEBP is a RIFF container tagged "WEBP" at offset 8.
        Some("image/webp")
    } else {
        None
    }
}

/// Validate the format and size of raw image bytes, then wrap them into a
/// canonical base64 Image block. Errors on an unsupported format or an
/// oversized image.
pub fn image_block_from_bytes(bytes: &[u8]) -> Result<ContentBlock> {
    let Some(media_type) = detect_media_type(bytes) else {
        bail!("unsupported image format (expected png, jpeg, gif, or webp)");
    };
    if bytes.len() > MAX_IMAGE_BYTES {
        bail!(
            "image is {} bytes, over the {MAX_IMAGE_BYTES} byte limit",
            bytes.len()
        );
    }
    Ok(ContentBlock::Image {
        source: ImageSource::Base64 {
            media_type: media_type.to_string(),
            data: STANDARD.encode(bytes),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal valid magic-byte prefixes for each accepted format.
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0];
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0];
    const GIF: &[u8] = b"GIF89a\x00\x00";
    const WEBP: &[u8] = b"RIFF\x00\x00\x00\x00WEBP\x00\x00";

    #[test]
    fn detects_each_supported_format() {
        assert_eq!(detect_media_type(PNG), Some("image/png"));
        assert_eq!(detect_media_type(JPEG), Some("image/jpeg"));
        assert_eq!(detect_media_type(b"GIF87a\x00"), Some("image/gif"));
        assert_eq!(detect_media_type(GIF), Some("image/gif"));
        assert_eq!(detect_media_type(WEBP), Some("image/webp"));
    }

    #[test]
    fn rejects_unknown_and_truncated() {
        assert_eq!(detect_media_type(b"not an image"), None);
        assert_eq!(detect_media_type(b""), None);
        // "RIFF" without the "WEBP" tag (e.g. a WAV) is not an image.
        assert_eq!(detect_media_type(b"RIFF\x00\x00\x00\x00WAVE"), None);
        // A short buffer must not panic on the RIFF offset slice.
        assert_eq!(detect_media_type(b"RIFF"), None);
    }

    #[test]
    fn builds_base64_block_from_png() {
        let block = image_block_from_bytes(PNG).unwrap();
        assert_eq!(
            block,
            ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: STANDARD.encode(PNG),
                },
            }
        );
    }

    #[test]
    fn rejects_unsupported_format() {
        assert!(image_block_from_bytes(b"plain text").is_err());
    }

    #[test]
    fn rejects_oversized_image() {
        // A valid PNG header followed by too many bytes.
        let mut big = PNG.to_vec();
        big.resize(MAX_IMAGE_BYTES + 1, 0);
        assert!(image_block_from_bytes(&big).is_err());
    }
}
