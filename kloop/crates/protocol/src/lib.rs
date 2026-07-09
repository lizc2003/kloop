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
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
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
/// input + output = the full context size at that request, which anchors
/// the char-heuristic estimate for messages recorded afterwards.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn total(self) -> u64 {
        self.input_tokens + self.output_tokens
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

/// JSON-schema spec for one tool, provider-agnostic.
#[derive(Clone, Debug)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: serde_json::Value,
}
