//! Image ingestion: sniff the format from magic bytes (never the extension),
//! fit the image to what the wire accepts, and build a canonical base64 Image
//! content block. Shared by the `--image` CLI entry (plan 29 slice 1),
//! `read_file`, notebook outputs, and the TUI's clipboard paste. Pure over its
//! byte input, so every entry point tests the same validation.
//!
//! Plan 156 item 4 added the pixel half. A byte cap alone does not bound an
//! image: a high-compression JPEG can be 500 KB and 6000x4000. What that costs
//! is not model tokens — the API downscales an oversized image itself, capping
//! the visual-token bill — but request bytes, and kloop re-sends the whole
//! conversation every turn. Two documented API rules make it a correctness
//! matter too: an image over 8000x8000 is rejected outright, and a request
//! carrying more than 20 image blocks applies a stricter per-image dimension
//! limit to *all* of them, the documented remedy being "resize each image so
//! that neither dimension exceeds 2000 px".

use anyhow::Result;
use anyhow::bail;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use image::DynamicImage;
use image::ImageReader;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;

use kloop_protocol::ContentBlock;
use kloop_protocol::ImageSource;

/// Raw-byte cap for a single image (5 MiB) — also the ceiling `read_file` pulls
/// off disk. A file over this is refused rather than read and shrunk: raising
/// the read ceiling is plan 61's decision, not this one's.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Raw-byte target for what actually goes on the wire. Base64 inflates by 4/3,
/// and the tightest documented per-image ceiling is 5 MB base64 (Amazon Bedrock
/// and Google Cloud; the Claude API direct allows 10 MB). Targeting 3/4 of the
/// cap keeps one encoding portable across every route kloop can be pointed at.
const WIRE_TARGET_BYTES: usize = MAX_IMAGE_BYTES * 3 / 4;

/// Longest edge kloop sends. Deliberately *not* a token-cost number: the API
/// downscales to its own resolution tier (2576 px long edge on 4.7+ models)
/// regardless of what arrives. 2000 is the documented threshold that keeps a
/// request safe once it carries more than 20 image blocks, and it is where both
/// cc and grok landed for the same reason.
const MAX_WIRE_DIMENSION: u32 = 2000;

/// Longest edge the ladder will descend to before giving up. Below this the
/// image stops being worth looking at, and a refusal beats an unreadable
/// thumbnail the model will confidently misread.
const MIN_WIRE_DIMENSION: u32 = 256;

/// JPEG quality steps, best first. cc's ladder.
const JPEG_QUALITY_STEPS: &[u8] = &[80, 60, 40, 20];

/// Decompression-bomb guards, applied to the header before any pixel buffer is
/// allocated. A 5 MiB file is free to claim 40000x40000; these are what stop it.
const MAX_DECODE_PIXELS: u64 = 64_000_000;
const MAX_DECODE_ALLOC_BYTES: u64 = 256 * 1024 * 1024;

/// A wire-ready image plus what had to be done to it. `resized` is `None` when
/// the source bytes went out untouched.
#[derive(Debug, PartialEq)]
pub struct PreparedImage {
    pub block: ContentBlock,
    pub resized: Option<Resized>,
}

/// The dimensions a caller can tell the model about, so a downscale is never
/// silent — the model is reading pixels and needs to know it is not seeing the
/// original ones.
#[derive(Debug, Eq, PartialEq)]
pub struct Resized {
    pub from: (u32, u32),
    pub to: (u32, u32),
}

/// Sniff the media type from the leading magic bytes, returning the Anthropic
/// media_type string or None for anything outside png/jpeg/gif/webp. The file
/// extension is never trusted: the wire needs the true type, and a mislabeled
/// file would be rejected by the API regardless. GIF animation is not inspected
/// here; the API uses the first frame of an animation either way.
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
    Ok(prepare_image_from_bytes(bytes)?.block)
}

/// [`image_block_from_bytes`] plus the record of any downscale, for the callers
/// that can pass that fact on to the model.
pub fn prepare_image_from_bytes(bytes: &[u8]) -> Result<PreparedImage> {
    let Some(media_type) = detect_media_type(bytes) else {
        bail!("unsupported image format (expected png, jpeg, gif, or webp)");
    };
    if bytes.len() > MAX_IMAGE_BYTES {
        bail!(
            "image is {} bytes, over the {MAX_IMAGE_BYTES} byte limit",
            bytes.len()
        );
    }

    // No readable header: the sniff says it is an image but the decoder cannot
    // measure it. Send the bytes as they are — under the byte cap this is what
    // kloop did before pixels were considered at all, and it is better than
    // refusing a file the API may well accept.
    let Some((width, height)) = header_dimensions(bytes) else {
        return Ok(PreparedImage {
            block: base64_block(media_type, bytes),
            resized: None,
        });
    };

    if bytes.len() <= WIRE_TARGET_BYTES
        && width <= MAX_WIRE_DIMENSION
        && height <= MAX_WIRE_DIMENSION
    {
        return Ok(PreparedImage {
            block: base64_block(media_type, bytes),
            resized: None,
        });
    }

    let decoded = decode_guarded(bytes, width, height)?;
    let fitted = encode_within_budget(&decoded, media_type)?;
    Ok(PreparedImage {
        block: base64_block(fitted.media_type, &fitted.bytes),
        resized: Some(Resized {
            from: (width, height),
            to: fitted.dimensions,
        }),
    })
}

fn base64_block(media_type: &str, bytes: &[u8]) -> ContentBlock {
    ContentBlock::Image {
        source: ImageSource::Base64 {
            media_type: media_type.to_string(),
            data: STANDARD.encode(bytes),
        },
    }
}

/// Dimensions from the header alone, without allocating a pixel buffer.
fn header_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

fn decode_guarded(bytes: &[u8], width: u32, height: u32) -> Result<DynamicImage> {
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_DECODE_PIXELS {
        bail!(
            "image is {width}x{height} ({pixels} pixels), over the {MAX_DECODE_PIXELS} pixel decode limit; downscale or crop it first"
        );
    }
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| anyhow::anyhow!("cannot read image header: {error}"))?;
    reader.limits(limits);
    reader
        .decode()
        .map_err(|error| anyhow::anyhow!("cannot decode image: {error}"))
}

struct FittedImage {
    bytes: Vec<u8>,
    media_type: &'static str,
    dimensions: (u32, u32),
}

/// Descend edge sizes until one of them encodes inside the byte target.
///
/// PNG is tried first at every rung and kept whenever it fits, rather than
/// taking whichever candidate is smaller: kloop's images are overwhelmingly
/// screenshots, and the API's own guidance is that heavy JPEG compression makes
/// text hard to read. A photo's PNG will not fit and falls through to the JPEG
/// ladder on its own.
fn encode_within_budget(decoded: &DynamicImage, source_type: &str) -> Result<FittedImage> {
    // Never upscale: a small-but-heavy image is re-encoded at its own size.
    let longest = decoded.width().max(decoded.height());
    let mut edge = MAX_WIRE_DIMENSION.min(longest);
    loop {
        let scaled = if edge < longest {
            std::borrow::Cow::Owned(decoded.resize(edge, edge, FilterType::Lanczos3))
        } else {
            std::borrow::Cow::Borrowed(decoded)
        };
        let dimensions = (scaled.width(), scaled.height());

        if let Some(bytes) = encode_png(&scaled).filter(|png| png.len() <= WIRE_TARGET_BYTES) {
            return Ok(FittedImage {
                bytes,
                media_type: "image/png",
                dimensions,
            });
        }
        for quality in JPEG_QUALITY_STEPS {
            if let Some(bytes) =
                encode_jpeg(&scaled, *quality).filter(|jpeg| jpeg.len() <= WIRE_TARGET_BYTES)
            {
                return Ok(FittedImage {
                    bytes,
                    media_type: "image/jpeg",
                    dimensions,
                });
            }
        }

        if edge <= MIN_WIRE_DIMENSION {
            bail!(
                "{source_type} image still exceeds the {WIRE_TARGET_BYTES} byte wire budget at {}x{}; crop it or convert it first",
                dimensions.0,
                dimensions.1
            );
        }
        edge = (edge * 3 / 4).max(MIN_WIRE_DIMENSION);
    }
}

fn encode_png(image: &DynamicImage) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .ok()
        .map(|()| buf)
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    // JPEG has no alpha channel; encoding RGBA directly fails.
    let opaque = DynamicImage::ImageRgb8(image.to_rgb8());
    JpegEncoder::new_with_quality(&mut buf, quality)
        .encode_image(&opaque)
        .ok()
        .map(|()| buf)
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

    /// Broad flat bands: real pixels the encoders can measure, but compressible
    /// enough that a big one stays well inside the byte budget — which is
    /// exactly the shape the byte cap alone cannot catch.
    fn banded_png(width: u32, height: u32) -> Vec<u8> {
        let buffer = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x / 256) as u8, (y / 256) as u8, 128])
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(buffer)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    fn decoded_dimensions(block: &ContentBlock) -> (u32, u32) {
        let ContentBlock::Image {
            source: ImageSource::Base64 { data, .. },
        } = block
        else {
            panic!("a prepared image is a base64 image block");
        };
        let bytes = STANDARD.decode(data).unwrap();
        header_dimensions(&bytes).expect("a prepared image has a readable header")
    }

    fn media_type_of(block: &ContentBlock) -> String {
        let ContentBlock::Image {
            source: ImageSource::Base64 { media_type, .. },
        } = block
        else {
            panic!("a prepared image is a base64 image block");
        };
        media_type.clone()
    }

    /// Inside both budgets nothing is touched — the same bytes the caller read
    /// are the bytes that go out, so a re-read is byte-identical.
    #[test]
    fn an_image_inside_both_budgets_goes_out_untouched() {
        let png = banded_png(64, 48);
        let prepared = prepare_image_from_bytes(&png).unwrap();
        assert_eq!(
            prepared,
            PreparedImage {
                block: base64_block("image/png", &png),
                resized: None,
            }
        );
    }

    /// The gap this exists to close: small in bytes, enormous in pixels. The
    /// byte cap alone passes it; the pixel budget is what catches it.
    #[test]
    fn a_pixel_oversized_image_is_downscaled_even_when_its_bytes_are_small() {
        let png = banded_png(2100, 1400);
        assert!(
            png.len() <= WIRE_TARGET_BYTES,
            "the byte budget alone must not be what catches this image ({} bytes)",
            png.len()
        );

        let prepared = prepare_image_from_bytes(&png).unwrap();
        assert_eq!(
            prepared.resized,
            Some(Resized {
                from: (2100, 1400),
                to: (2000, 1333),
            })
        );
        assert_eq!(decoded_dimensions(&prepared.block), (2000, 1333));
        // Flat bands are line-art shaped, so the PNG rung fits and the ladder
        // never reaches JPEG — the screenshots kloop actually reads keep their
        // text crisp instead of being re-encoded lossily for no reason.
        assert_eq!(media_type_of(&prepared.block), "image/png");
    }

    /// Fitting to a 2000 box never enlarges what is already smaller, and the
    /// aspect ratio survives the fit.
    #[test]
    fn a_smaller_image_is_never_upscaled_to_the_budget() {
        let png = banded_png(2100, 525);
        let prepared = prepare_image_from_bytes(&png).unwrap();
        assert_eq!(decoded_dimensions(&prepared.block), (2000, 500));

        let wide = banded_png(1200, 300);
        let untouched = prepare_image_from_bytes(&wide).unwrap();
        assert_eq!(untouched.resized, None);
        assert_eq!(decoded_dimensions(&untouched.block), (1200, 300));
    }

    /// A 68-byte file may claim 1.6 gigapixels. The guard reads that off the
    /// header and refuses before anything allocates a pixel buffer.
    #[test]
    fn a_decompression_bomb_is_refused_from_its_header() {
        // A structurally complete 68-byte PNG whose IHDR claims 40000x40000.
        const BOMB: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x9C, 0x40, 0x00, 0x00, 0x9C, 0x40, 0x08, 0x02, 0x00, 0x00,
            0x00, 0xDE, 0x6E, 0x99, 0x52, 0x00, 0x00, 0x00, 0x0B, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x60, 0x40, 0x05, 0x00, 0x00, 0x10, 0x00, 0x01, 0x39, 0xBD, 0x8F, 0x65,
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        assert_eq!(header_dimensions(BOMB), Some((40000, 40000)));
        let error = prepare_image_from_bytes(BOMB).unwrap_err().to_string();
        assert!(error.contains("40000x40000"), "{error}");
        assert!(error.contains("pixel decode limit"), "{error}");
    }

    /// Bytes that sniff as an image but carry no readable header keep the
    /// pre-pixel behaviour: sent as they are rather than refused.
    #[test]
    fn an_unreadable_header_falls_back_to_sending_the_bytes() {
        let prepared = prepare_image_from_bytes(JPEG).unwrap();
        assert_eq!(
            prepared,
            PreparedImage {
                block: base64_block("image/jpeg", JPEG),
                resized: None,
            }
        );
    }
}
