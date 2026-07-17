//! OS clipboard image paste (plan 38 slice 3).
//!
//! Bound to **Ctrl+V / Alt+V** — terminals keep Cmd+V for their own text paste,
//! so a distinct key reads the system clipboard directly (the same choice Claude
//! Code and codex make). Two clipboard forms are handled: a copied image *file*
//! (Finder) arrives as a path list and is validated the same way `--image` is; a
//! screenshot or browser copy arrives as raw *RGBA* data, which is PNG-encoded
//! before validation. Both end at core's shared image ingestion, so the wire
//! block is identical to `--image`'s.

use kloop_protocol::ContentBlock;

/// Read an image off the OS clipboard into an Image block, with a display label.
/// Returns a human error string when the clipboard is unavailable or holds no
/// image (the common case — a text clipboard — so the caller notes it, not fails).
pub fn clipboard_image() -> Result<(String, ContentBlock), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| format!("clipboard unavailable: {e}"))?;

    // A copied image FILE (e.g. from Finder) is a path list; read its bytes and
    // validate them exactly like `--image`. Preferred when present.
    if let Ok(files) = cb.get().file_list() {
        for f in files {
            let Ok(bytes) = std::fs::read(&f) else {
                continue;
            };
            if let Ok(block) = kloop_core::image::image_block_from_bytes(&bytes) {
                let label = f
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("image")
                    .to_string();
                return Ok((label, block));
            }
        }
    }

    // Raw image DATA (a screenshot, a browser copy) arrives as RGBA; encode PNG.
    let img = cb
        .get_image()
        .map_err(|e| format!("no image on the clipboard: {e}"))?;
    let png = encode_png(img.width, img.height, &img.bytes)
        .ok_or_else(|| "could not encode the clipboard image".to_string())?;
    let block = kloop_core::image::image_block_from_bytes(&png).map_err(|e| e.to_string())?;
    Ok((format!("pasted image {}×{}", img.width, img.height), block))
}

/// Encode raw RGBA8 pixels into PNG bytes. `None` if the buffer is not exactly
/// `width * height * 4` (a malformed clipboard read) or encoding fails.
fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Option<Vec<u8>> {
    if rgba.len() != width.checked_mul(height)?.checked_mul(4)? {
        return None;
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width as u32, height as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RGBA pixels encode to a PNG whose bytes core accepts as an Image block —
    /// the same path a real clipboard read takes after arboard hands back RGBA.
    #[test]
    fn rgba_encodes_to_a_valid_png_block() {
        let rgba = [255u8, 0, 0, 255].repeat(4); // 2x2 opaque red
        let png = encode_png(2, 2, &rgba).unwrap();
        assert_eq!(
            &png[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "png magic header"
        );
        let block = kloop_core::image::image_block_from_bytes(&png).unwrap();
        assert!(matches!(block, ContentBlock::Image { .. }));
    }

    #[test]
    fn a_mismatched_rgba_buffer_is_rejected() {
        assert!(encode_png(2, 2, &[0; 3]).is_none());
    }
}
