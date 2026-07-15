//! kloop-protocol — the zero-dependency leaf every other crate stands on:
//! canonical wire types (Anthropic Messages shape), streaming events, usage
//! accounting, and shared error markers. Nothing here knows about networks,
//! filesystems, or the agent loop.

use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// Canonical content block, Anthropic Messages wire shape. The OpenAI-compat
/// provider translates to/from this shape at its edge.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// Model reasoning. Replay rule: when the history goes back to the same
    /// model the block must be echoed exactly as received (empty text
    /// included) — the signature cryptographically binds it to this context
    /// and any edit is rejected. Adapters for other wire formats may reuse
    /// `signature` for their own opaque replay blob (Responses API
    /// encrypted_content).
    Thinking {
        thinking: String,
        signature: String,
    },
    /// Reasoning the API withheld; an opaque blob replayed verbatim.
    RedactedThinking {
        data: String,
    },
    /// An image supplied by the user (top-level) or, later, read by a tool.
    /// The canonical shape is Anthropic's: a tagged `source`. The OpenAI-compat
    /// and Responses adapters translate it into their data-URL shapes at their
    /// edge. Images are never offloaded (unlike large ToolResult text): the
    /// base64 must reach the model as-is, so it inlines into the rollout.
    Image {
        source: ImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

/// The content of a tool_result: plain text (the overwhelming common case —
/// every tool that returns a string) or a block array (a tool that returns an
/// image, e.g. `read_file` on an image file). Serializes **untagged**, exactly
/// matching Anthropic's `tool_result.content`, which is itself `string |
/// array`: `Text` becomes a bare JSON string — so a pre-image rollout line
/// (`"content":"…"`) round-trips unchanged — and `Blocks` becomes a content
/// array. The OpenAI-compat and Responses adapters translate `Blocks` at their
/// edge (chat/completions relocates images to a trailing user message; the
/// Responses API carries them natively in the function_call_output).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl ToolResultContent {
    /// Text view for surfaces that cannot render blocks: `Text` verbatim;
    /// `Blocks` joins its text blocks and renders each image as an
    /// `[image: <media_type>]` tag. Used by offload sizing, hook payloads, the
    /// codemode program result, and the chat/completions downgrade placeholder.
    pub fn as_text(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Text(s) => std::borrow::Cow::Borrowed(s),
            Self::Blocks(blocks) => std::borrow::Cow::Owned(
                blocks
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text } => text.clone(),
                        ContentBlock::Image {
                            source: ImageSource::Base64 { media_type, .. },
                        } => format!("[image: {media_type}]"),
                        _ => String::new(),
                    })
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        }
    }
}

impl From<String> for ToolResultContent {
    fn from(s: String) -> Self {
        Self::Text(s)
    }
}

impl From<&str> for ToolResultContent {
    fn from(s: &str) -> Self {
        Self::Text(s.to_string())
    }
}

/// Where an image's bytes come from. Only inline base64 is accepted (remote
/// URLs are refused at the entry point — SSRF surface, matching codex), but the
/// tagged wire shape leaves room for `url` later without a breaking change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    /// Anthropic's `{type:"base64", media_type, data}` — data and media_type
    /// separate. `media_type` is one of image/png|jpeg|gif|webp; `data` is the
    /// standard-alphabet base64 of the raw image bytes (no data-URL prefix).
    Base64 { media_type: String, data: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// A user message carrying `text` plus trailing content blocks (images
    /// supplied via `--image`). Empty text contributes no text block, so an
    /// image-only message is valid; the text leads so the model reads the ask
    /// before the attachments.
    pub fn user_with_blocks(text: impl Into<String>, blocks: Vec<ContentBlock>) -> Self {
        let text = text.into();
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentBlock::Text { text });
        }
        content.extend(blocks);
        Self {
            role: Role::User,
            content,
        }
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Assistant,
            content,
        }
    }

    pub fn tool_results(results: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            content: results,
        }
    }
}

/// Maximum output tokens requested per sampling call; also feeds the
/// per-round growth estimate used by predictive compaction.
pub const MAX_OUTPUT_TOKENS: u64 = 8192;

/// Real token usage reported by the provider for one sampling call.
/// `input_tokens` is only the uncached remainder: cached prompt tokens are
/// reported separately but still occupy the context window, so the full
/// context size at the request is `total()` — which anchors the
/// char-heuristic estimate for messages recorded afterwards.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Prompt tokens served from the provider's prefix cache.
    pub cache_read_input_tokens: u64,
    /// Prompt tokens written to the provider's prefix cache this request.
    pub cache_creation_input_tokens: u64,
}

impl Usage {
    pub fn total(self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_read_input_tokens
            + self.cache_creation_input_tokens
    }
}

/// The provider rejected the request for exceeding the context window.
/// Detected via anyhow downcast so the agent can compact and retry instead
/// of treating it as a transient error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverflowError;

impl std::fmt::Display for OverflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("context window exceeded")
    }
}

impl std::error::Error for OverflowError {}

/// Events emitted by a provider while one sampling request streams.
#[derive(Clone, Debug)]
pub enum StreamEvent {
    /// Incremental text for display only; the full text arrives via BlockDone.
    TextDelta(String),
    /// Incremental reasoning text for display only; the full block (with its
    /// signature) arrives via BlockDone.
    ThinkingDelta(String),
    /// A fully accumulated content block.
    BlockDone(ContentBlock),
    /// Stream finished cleanly. stop_reason is informational only: the loop
    /// decides continuation from the presence of tool_use blocks, never from
    /// stop_reason (unreliable across providers).
    Done {
        stop_reason: Option<String>,
        usage: Option<Usage>,
    },
}

/// JSON-schema spec for one tool, provider-agnostic. Owned strings because
/// tool sets are no longer static: MCP servers contribute defs at runtime.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The wire-format contract with the Anthropic Messages API: exact JSON
    /// shapes, including tag names and the is_error omission rule.
    #[test]
    fn content_block_wire_format() {
        assert_eq!(
            serde_json::to_value(ContentBlock::Text { text: "hi".into() }).unwrap(),
            json!({"type": "text", "text": "hi"})
        );
        assert_eq!(
            serde_json::to_value(ContentBlock::ToolUse {
                id: "t1".into(),
                name: "bash".into(),
                input: json!({"command": "ls"}),
            })
            .unwrap(),
            json!({"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}})
        );
        assert_eq!(
            serde_json::to_value(ContentBlock::Thinking {
                thinking: "let me see".into(),
                signature: "sig-abc".into(),
            })
            .unwrap(),
            json!({"type": "thinking", "thinking": "let me see", "signature": "sig-abc"})
        );
        assert_eq!(
            serde_json::to_value(ContentBlock::RedactedThinking {
                data: "blob".into(),
            })
            .unwrap(),
            json!({"type": "redacted_thinking", "data": "blob"})
        );
        // Image serializes straight to the Anthropic wire shape: a tagged
        // base64 source with media_type and data separate. The anthropic
        // adapter serializes the protocol raw, so this IS the request shape.
        assert_eq!(
            serde_json::to_value(ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "aGk=".into(),
                },
            })
            .unwrap(),
            json!({
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": "aGk="},
            })
        );
        // is_error omitted when false, present when true. Text content
        // serializes as a bare string (Anthropic's `content: string` form).
        assert_eq!(
            serde_json::to_value(ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "ok".into(),
                is_error: false,
            })
            .unwrap(),
            json!({"type": "tool_result", "tool_use_id": "t1", "content": "ok"})
        );
        assert_eq!(
            serde_json::to_value(ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "bad".into(),
                is_error: true,
            })
            .unwrap(),
            json!({"type": "tool_result", "tool_use_id": "t1", "content": "bad", "is_error": true})
        );
        // Block content (a tool that read an image) serializes as an array —
        // Anthropic's `content: array` form. The image block inside is the same
        // canonical shape as a top-level image.
        assert_eq!(
            serde_json::to_value(ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: ToolResultContent::Blocks(vec![ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "aGk=".into(),
                    },
                }]),
                is_error: false,
            })
            .unwrap(),
            json!({
                "type": "tool_result",
                "tool_use_id": "t1",
                "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGk="}},
                ],
            })
        );
    }

    /// tool_result content is `string | array` on the wire and survives the
    /// roundtrip either way. Critically, a pre-image rollout line — where
    /// `content` was written as a bare string — still reads back as `Text`,
    /// so old session files remain forward-compatible.
    #[test]
    fn tool_result_content_string_or_array_roundtrips() {
        let text = ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: ToolResultContent::Text("plain".into()),
            is_error: false,
        };
        let blocks = ContentBlock::ToolResult {
            tool_use_id: "t2".into(),
            content: ToolResultContent::Blocks(vec![
                ContentBlock::Text {
                    text: "see image".into(),
                },
                ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/gif".into(),
                        data: "R0lG".into(),
                    },
                },
            ]),
            is_error: false,
        };
        for block in [&text, &blocks] {
            let wire = serde_json::to_string(block).unwrap();
            assert_eq!(&serde_json::from_str::<ContentBlock>(&wire).unwrap(), block);
        }
        // A bare-string content field (the pre-image shape) deserializes to Text.
        let old = r#"{"type":"tool_result","tool_use_id":"t1","content":"legacy"}"#;
        assert_eq!(
            serde_json::from_str::<ContentBlock>(old).unwrap(),
            ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: ToolResultContent::Text("legacy".into()),
                is_error: false,
            }
        );
    }

    /// as_text() flattens blocks for text-only surfaces: text verbatim, each
    /// image rendered as an `[image: <media_type>]` tag.
    #[test]
    fn as_text_renders_blocks_with_image_tags() {
        assert_eq!(ToolResultContent::Text("hi".into()).as_text(), "hi");
        let blocks = ToolResultContent::Blocks(vec![
            ContentBlock::Text {
                text: "before".into(),
            },
            ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/webp".into(),
                    data: "x".into(),
                },
            },
        ]);
        assert_eq!(blocks.as_text(), "before\n[image: image/webp]");
    }

    #[test]
    fn message_serde_roundtrip() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    // The empty-text + signature shape is what display=omitted
                    // models actually send; it must survive the roundtrip.
                    thinking: String::new(),
                    signature: "sig".into(),
                },
                ContentBlock::RedactedThinking { data: "d".into() },
                ContentBlock::Text { text: "a".into() },
                ContentBlock::ToolUse {
                    id: "t".into(),
                    name: "bash".into(),
                    input: json!({"command": "pwd"}),
                },
            ],
        };
        let wire = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, msg);
        // Roles serialize lowercase.
        assert!(wire.contains("\"role\":\"assistant\""));
    }

    #[test]
    fn message_constructors_set_roles() {
        assert_eq!(Message::user_text("x").role, Role::User);
        assert_eq!(Message::assistant(vec![]).role, Role::Assistant);
        // Tool results ride on a user message per the wire contract.
        assert_eq!(Message::tool_results(vec![]).role, Role::User);
    }

    /// An image block survives the serde roundtrip whole, and reading a message
    /// with no image (the pre-image rollout shape) still works — forward
    /// compatibility for old session files.
    #[test]
    fn image_block_roundtrips_and_old_rollout_still_reads() {
        let msg = Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "what is this".into(),
                },
                ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/jpeg".into(),
                        data: "/9j/4AAQ".into(),
                    },
                },
            ],
        };
        let wire = serde_json::to_string(&msg).unwrap();
        assert_eq!(serde_json::from_str::<Message>(&wire).unwrap(), msg);
        // A rollout line written before images existed carries no image block
        // and reads back unchanged.
        let old = r#"{"role":"user","content":[{"type":"text","text":"hi"}]}"#;
        assert_eq!(
            serde_json::from_str::<Message>(old).unwrap(),
            Message::user_text("hi")
        );
    }

    #[test]
    fn user_with_blocks_leads_with_text_and_allows_image_only() {
        let img = ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "aGk=".into(),
            },
        };
        // Text leads, then attachments.
        assert_eq!(
            Message::user_with_blocks("look", vec![img.clone()]),
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Text {
                        text: "look".into()
                    },
                    img.clone()
                ],
            }
        );
        // Empty text contributes no text block: an image-only message.
        assert_eq!(
            Message::user_with_blocks("", vec![img.clone()]),
            Message {
                role: Role::User,
                content: vec![img],
            }
        );
    }

    /// total() is the full context size: cached prompt tokens still occupy
    /// the window, so they count alongside the uncached remainder and output.
    #[test]
    fn usage_total_includes_cache_fields() {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 42,
            cache_read_input_tokens: 900,
            cache_creation_input_tokens: 8,
        };
        assert_eq!(usage.total(), 1050);
    }
}
