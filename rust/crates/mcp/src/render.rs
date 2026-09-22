//! Rendering an MCP `content` array for the two consumers above the wire: the
//! plain text a `tool_result` carries, and — when a result actually carries a
//! picture — canonical [`ContentBlock`]s that keep the image an image.

use base64::Engine;
use kloop_protocol::ContentBlock;
use kloop_protocol::ImageSource;
use serde_json::Value;

use crate::MAX_CALL_IMAGE_BYTES;

/// Flatten a `CallToolResult`'s content array into the plain text a tool_result
/// carries — the model-facing rendering. Exposed so the CLI can render the text
/// side of a structured result without a second wire call.
pub fn render_result(result: &Value) -> String {
    render_content(&result["content"])
}

/// Flatten an MCP content array into the plain text a tool_result carries.
/// Text passes through; resources keep their text with a provenance tag;
/// binary payloads degrade to a placeholder (this is the text-only rendering —
/// [`content_blocks`] is what keeps images as real image blocks).
fn render_content(content: &Value) -> String {
    let Some(items) = content.as_array() else {
        return "(no content)".to_string();
    };
    if items.is_empty() {
        return "(no content)".to_string();
    }
    items.iter().map(render_item).collect::<Vec<_>>().join("\n")
}

/// One MCP content item rendered to plain text. An image becomes an
/// `[image: <mime>]` tag here; [`content_blocks`] instead keeps it as an image
/// block when the mime type is one the models accept.
fn render_item(item: &Value) -> String {
    match item["type"].as_str() {
        Some("text") => item["text"].as_str().unwrap_or("").to_string(),
        Some("image") => format!(
            "[image: {}]",
            item["mimeType"].as_str().unwrap_or("unknown type")
        ),
        Some("audio") => format!(
            "[audio: {}]",
            item["mimeType"].as_str().unwrap_or("unknown type")
        ),
        Some("resource") => {
            let uri = item["resource"]["uri"].as_str().unwrap_or("?");
            match item["resource"]["text"].as_str() {
                Some(text) => format!("[resource {uri}]\n{text}"),
                None => format!("[resource {uri}: binary]"),
            }
        }
        Some("resource_link") => {
            format!("[resource link: {}]", item["uri"].as_str().unwrap_or("?"))
        }
        _ => format!("[unsupported content: {item}]"),
    }
}

/// Media types the models accept as image blocks (Anthropic's set). An MCP
/// image with any other mime stays text — a bad block would make the whole
/// request fail, so degrade rather than risk it.
fn supported_image_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

fn usable_image(item: &Value) -> bool {
    item["type"] == "image"
        && item["mimeType"].as_str().is_some_and(supported_image_mime)
        && item["data"].as_str().is_some_and(|data| {
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .is_ok_and(|decoded| decoded.len() <= MAX_CALL_IMAGE_BYTES)
        })
}

/// Build canonical content blocks from an MCP content array **when it carries
/// at least one usable image** — so the model sees the picture instead of an
/// `[image: …]` tag. Returns `None` for a text-only (or image-free) result,
/// whose flattened-text path stays byte-identical. When an image is present,
/// every non-image item folds into `Text` blocks (rendered as in
/// [`render_item`]), preserving order relative to the images.
pub fn content_blocks(content: &Value) -> Option<Vec<ContentBlock>> {
    let items = content.as_array()?;
    let has_usable_image = items.iter().any(usable_image);
    if !has_usable_image {
        return None;
    }
    let mut blocks = Vec::new();
    let mut pending = String::new();
    for item in items {
        let mime = item["mimeType"].as_str();
        if usable_image(item) {
            if !pending.is_empty() {
                blocks.push(ContentBlock::Text {
                    text: std::mem::take(&mut pending),
                });
            }
            blocks.push(ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: mime.unwrap_or_default().to_string(),
                    data: item["data"].as_str().unwrap_or_default().to_string(),
                },
            });
        } else {
            if !pending.is_empty() {
                pending.push('\n');
            }
            pending.push_str(&render_item(item));
        }
    }
    if !pending.is_empty() {
        blocks.push(ContentBlock::Text { text: pending });
    }
    Some(blocks)
}
