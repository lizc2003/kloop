//! kloop-protocol — the zero-dependency leaf every other crate stands on:
//! canonical wire types (Anthropic Messages shape), streaming events, usage
//! accounting, and shared error markers. Nothing here knows about networks,
//! filesystems, or the agent loop.

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de::Error as _;
use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// A live Agent address inside one local kloop session. It is deliberately not
/// a URL, Agent Card identity, provider role, or durable execution id.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LocalAgentId {
    Main,
    Agent(String),
}

impl LocalAgentId {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Main => "main",
            Self::Agent(id) => id,
        }
    }

    /// The legacy display projection used by hooks and turn-owned UI items.
    pub fn display_label(&self) -> &str {
        match self {
            Self::Main => "",
            Self::Agent(id) => id,
        }
    }

    pub fn is_main(&self) -> bool {
        matches!(self, Self::Main)
    }
}

impl fmt::Display for LocalAgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LocalAgentId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "main" {
            return Ok(Self::Main);
        }
        let Some(number) = value.strip_prefix("agent-") else {
            return Err("local Agent id must be `main` or `agent-N`".into());
        };
        if number.is_empty()
            || number.starts_with('0')
            || !number.bytes().all(|byte| byte.is_ascii_digit())
            || number.parse::<u64>().is_err()
        {
            return Err("local Agent id must use canonical `agent-N` with N >= 1".into());
        }
        Ok(Self::Agent(value.to_string()))
    }
}

impl Serialize for LocalAgentId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for LocalAgentId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LocalMessageId(String);

impl LocalMessageId {
    pub fn new(sequence: u64) -> Option<Self> {
        (sequence > 0).then(|| Self(format!("message-{sequence}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LocalMessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for LocalMessageId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some(number) = value.strip_prefix("message-") else {
            return Err("local message id must be `message-N`".into());
        };
        if number.is_empty()
            || number.starts_with('0')
            || !number.bytes().all(|byte| byte.is_ascii_digit())
            || number.parse::<u64>().is_err()
        {
            return Err("local message id must use canonical `message-N` with N >= 1".into());
        }
        Ok(Self(value.to_string()))
    }
}

impl Serialize for LocalMessageId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for LocalMessageId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

/// Opaque correlation scope for one in-memory local Agent directory. It must
/// not expose a session path, credential, endpoint, or tenant identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LocalContextId(String);

impl LocalContextId {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
            return Err("local context id must be 1..=128 printable bytes".into());
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LocalContextId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for LocalContextId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for LocalContextId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        LocalContextId::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LocalAgentPart {
    Text { text: String },
}

/// A transport-neutral local message. `from`, `to`, and `summary` are local
/// routing/display fields; a future A2A adapter must project only the content
/// identity/context/parts into an A2A Message and resolve `to` separately.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalAgentMessage {
    pub message_id: LocalMessageId,
    pub context_id: LocalContextId,
    pub from: LocalAgentId,
    pub to: LocalAgentId,
    pub summary: String,
    pub parts: Vec<LocalAgentPart>,
}

impl LocalAgentMessage {
    pub fn text(
        message_id: LocalMessageId,
        context_id: LocalContextId,
        from: LocalAgentId,
        to: LocalAgentId,
        summary: String,
        text: String,
    ) -> Self {
        Self {
            message_id,
            context_id,
            from,
            to,
            summary,
            parts: vec![LocalAgentPart::Text { text }],
        }
    }

    pub fn text_body(&self) -> &str {
        let [LocalAgentPart::Text { text }] = self.parts.as_slice() else {
            unreachable!("local Agent messages are constructed with one text part")
        };
        text
    }
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
        #[serde(default, skip_serializing_if = "String::is_empty")]
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

impl ContentBlock {
    pub fn has_reasoning(&self) -> bool {
        match self {
            Self::Thinking { .. } | Self::RedactedThinking { .. } => true,
            Self::ToolResult {
                content: ToolResultContent::Blocks(blocks),
                ..
            } => blocks.iter().any(Self::has_reasoning),
            Self::Text { .. }
            | Self::Image { .. }
            | Self::ToolUse { .. }
            | Self::ToolResult {
                content: ToolResultContent::Text(_),
                ..
            } => false,
        }
    }

    pub fn has_redacted_reasoning(&self) -> bool {
        match self {
            Self::RedactedThinking { .. } => true,
            Self::ToolResult {
                content: ToolResultContent::Blocks(blocks),
                ..
            } => blocks.iter().any(Self::has_redacted_reasoning),
            Self::Text { .. }
            | Self::Thinking { .. }
            | Self::Image { .. }
            | Self::ToolUse { .. }
            | Self::ToolResult {
                content: ToolResultContent::Text(_),
                ..
            } => false,
        }
    }

    pub fn into_without_reasoning(self) -> Option<Self> {
        match self {
            Self::Thinking { .. } | Self::RedactedThinking { .. } => None,
            Self::ToolResult {
                tool_use_id,
                content: ToolResultContent::Blocks(blocks),
                is_error,
            } => Some(Self::ToolResult {
                tool_use_id,
                content: ToolResultContent::Blocks(
                    blocks
                        .into_iter()
                        .filter_map(Self::into_without_reasoning)
                        .collect(),
                ),
                is_error,
            }),
            other => Some(other),
        }
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderApiFamily {
    AnthropicMessages,
    OpenAiChatCompletions,
    OpenAiResponses,
    Mock,
}

impl ProviderApiFamily {
    pub fn requires_exact_reasoning_replay(self) -> bool {
        self != Self::OpenAiChatCompletions
    }
}

/// How hard the model is asked to think, as a kloop-owned bounded vocabulary.
/// Each rail renders it into its own request field ([`crate::ProviderApiFamily`]).
///
/// The six levels are the set live endpoints were measured to accept (2026-08-28,
/// plan 102) — an OpenAI-family model enumerated exactly these on refusal, and an
/// Anthropic model took every one of them from `low` up. **Which subset a given
/// model accepts is the model's own contract, not the rail's**, so this enum only
/// bounds kloop's spelling (catching `/effort hgih`); a level the model refuses
/// comes back as the provider's own error, which names what it does support.
///
/// Absent (`None` at the call site) means kloop sends no field at all and the
/// provider's default stands — a different thing from [`ReasoningEffort::None`],
/// which explicitly asks the model for no reasoning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    /// `effort: "none"` — the model does no reasoning. Distinct from sending no
    /// field at all, which leaves the provider's own default in force.
    None,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    Max,
}

impl ReasoningEffort {
    pub const ALL: &'static [ReasoningEffort] = &[
        Self::None,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Render a set for an error or help line: `none, low, medium, high, …`.
    pub fn join(levels: &[ReasoningEffort]) -> String {
        levels
            .iter()
            .map(|level| level.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ReasoningEffort {
    type Err = UnknownReasoningEffort;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|level| level.as_str().eq_ignore_ascii_case(raw.trim()))
            .ok_or_else(|| UnknownReasoningEffort(raw.trim().to_string()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownReasoningEffort(pub String);

impl fmt::Display for UnknownReasoningEffort {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unknown effort '{}' (known: {})",
            self.0,
            ReasoningEffort::join(ReasoningEffort::ALL)
        )
    }
}

impl std::error::Error for UnknownReasoningEffort {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAvailabilityCode {
    Ready,
    MissingCredential,
    InvalidConfiguration,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDescriptor {
    pub id: String,
    pub api_family: ProviderApiFamily,
    pub default_model: String,
    pub models: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    pub availability: ProviderAvailabilityCode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptKind {
    Primary,
    Fallback,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAttemptIdentity {
    pub route_revision: u64,
    pub provider_id: String,
    pub api_family: ProviderApiFamily,
    pub endpoint_fingerprint: String,
    pub model: String,
    pub attempt_kind: ProviderAttemptKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRouteSource {
    Initial,
    ExplicitSwitch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningContinuity {
    Preserved,
    Filtered,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRouteReceipt {
    pub revision: u64,
    pub boundary: u64,
    pub source: ProviderRouteSource,
    pub provider_id: String,
    pub api_family: ProviderApiFamily,
    pub endpoint_fingerprint: String,
    pub primary_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    pub continuity: ReasoningContinuity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveProviderRoute {
    pub revision: u64,
    pub provider_id: String,
    pub api_family: ProviderApiFamily,
    pub model: String,
    /// Bounded public receipt for reasoning continuity at this route boundary.
    pub continuity: ReasoningContinuity,
    /// The session's reasoning effort, or None when kloop sends no field.
    /// Session-local: it rides the frozen route but is not part of the durable
    /// route timeline, so a resumed session re-seeds it from configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
}

/// Private replay identity bound by the History owner when a provider-produced
/// assistant message is appended. It is never part of a public projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderResponseProvenance {
    pub route_revision: u64,
    pub origin_boundary: u64,
    pub provider_id: String,
    pub api_family: ProviderApiFamily,
    pub endpoint_fingerprint: String,
    pub model: String,
    pub attempt_kind: ProviderAttemptKind,
}

impl ProviderResponseProvenance {
    pub fn matches_attempt(&self, attempt: &ProviderAttemptIdentity) -> bool {
        self.route_revision == attempt.route_revision
            && self.provider_id == attempt.provider_id
            && self.api_family == attempt.api_family
            && self.endpoint_fingerprint == attempt.endpoint_fingerprint
            && self.model == attempt.model
            && self.attempt_kind == attempt.attempt_kind
    }

    pub fn exact_replay_compatible(&self, attempt: &ProviderAttemptIdentity) -> bool {
        self.provider_id == attempt.provider_id
            && self.api_family == attempt.api_family
            && self.endpoint_fingerprint == attempt.endpoint_fingerprint
            && self.model == attempt.model
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    /// Internal replay identity. Provider adapters remove it from request wire,
    /// and public history projections remove it together with opaque reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_provenance: Option<ProviderResponseProvenance>,
    /// What put this message here, when it was not the user typing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub injected: Option<Injected>,
}

/// What put a user-role message into the history, when the user did not type it.
/// The inbox reinjects sub-agent results, background program/shell output,
/// scheduled prompts, peer messages and mid-turn steering as user messages so
/// the model folds them in; compaction replaces a prefix with a summary the same
/// way. A replayed transcript has to tell those apart from what the user
/// actually said, and the framing prose cannot do it: that prose is prompt
/// wording, it gets edited, and matching on it silently stops recognising every
/// session written before the edit. So the producer records what it made.
///
/// Like `provider_provenance` this is internal: provider adapters build their
/// request wire field by field, so it never reaches a provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "injected", rename_all = "snake_case")]
pub enum Injected {
    /// The user typing while a turn ran — their words, wrapped for the model.
    Steering,
    SubAgent {
        label: String,
    },
    Program {
        label: String,
    },
    Workflow {
        task_id: String,
    },
    Shell {
        id: String,
    },
    Scheduled {
        id: String,
    },
    MissedScheduled {
        id: String,
    },
    SchedulerFailure,
    PeerMessage {
        from: String,
    },
    PeerUndeliverable,
    /// Compaction's replacement for the prefix it folded away.
    ContextSummary,
    /// Compaction had to drop a prefix it could not summarize at all.
    DroppedPrefix,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
            provider_provenance: None,
            injected: None,
        }
    }

    /// A user-role message the harness produced rather than the user: the text
    /// is what the model reads, `injected` is what a reader (and a replayed
    /// transcript) needs to know it by.
    pub fn injected(injected: Injected, text: impl Into<String>) -> Self {
        Self {
            injected: Some(injected),
            ..Self::user_text(text)
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
            provider_provenance: None,
            injected: None,
        }
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Assistant,
            content,
            provider_provenance: None,
            injected: None,
        }
    }

    pub fn assistant_from_provider(
        content: Vec<ContentBlock>,
        provenance: ProviderResponseProvenance,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content,
            provider_provenance: Some(provenance),
            injected: None,
        }
    }

    pub fn tool_results(results: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            content: results,
            provider_provenance: None,
            injected: None,
        }
    }

    pub fn has_reasoning(&self) -> bool {
        self.content.iter().any(ContentBlock::has_reasoning)
    }

    /// Display-safe history: provider identity and opaque payloads stay
    /// internal. Display reasoning text remains visible, matching live
    /// `reasoning` item events, but its replay signature is removed.
    pub fn into_public_projection(mut self) -> Self {
        fn project_block(block: ContentBlock) -> Option<ContentBlock> {
            match block {
                ContentBlock::Thinking { thinking, .. } => Some(ContentBlock::Thinking {
                    thinking,
                    signature: String::new(),
                }),
                ContentBlock::RedactedThinking { .. } => None,
                ContentBlock::ToolResult {
                    tool_use_id,
                    content: ToolResultContent::Blocks(blocks),
                    is_error,
                } => Some(ContentBlock::ToolResult {
                    tool_use_id,
                    content: ToolResultContent::Blocks(
                        blocks.into_iter().filter_map(project_block).collect(),
                    ),
                    is_error,
                }),
                other => Some(other),
            }
        }

        self.content = self.content.into_iter().filter_map(project_block).collect();
        self.provider_provenance = None;
        self
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Everything the provider counted as prompt for this request. Cached
    /// reads and cache writes are both prompt; only writes were a miss, which
    /// is what makes this the denominator of a cache hit rate.
    pub fn prompt_tokens(self) -> u64 {
        self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens
    }
}

/// A provider-produced assistant block. This is intentionally narrower than
/// [`ContentBlock`]: images and tool results belong to input/history and can
/// never be constructed by the assistant-output stream seam.
#[derive(Clone, Debug, PartialEq)]
pub enum AssistantBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

impl AssistantBlock {
    /// Whether this completed block contains semantic output. Whitespace text
    /// is meaningful; only the exact empty forms are absent. A signature-only
    /// thinking block is load-bearing replay state and therefore semantic.
    pub fn has_semantic_payload(&self) -> bool {
        match self {
            Self::Text { text } => !text.is_empty(),
            Self::Thinking {
                thinking,
                signature,
            } => !thinking.is_empty() || !signature.is_empty(),
            Self::RedactedThinking { data } => !data.is_empty(),
            Self::ToolUse { .. } => true,
        }
    }

    /// Convert a validated provider output block at the sampling/history
    /// boundary. There is deliberately no reverse blanket conversion.
    pub fn into_content_block(self) -> ContentBlock {
        match self {
            Self::Text { text } => ContentBlock::Text { text },
            Self::Thinking {
                thinking,
                signature,
            } => ContentBlock::Thinking {
                thinking,
                signature,
            },
            Self::RedactedThinking { data } => ContentBlock::RedactedThinking { data },
            Self::ToolUse { id, name, input } => ContentBlock::ToolUse { id, name, input },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputLimitKind {
    MaxOutputTokens,
    ModelContextWindow,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncompleteReason {
    PauseTurn,
    Provider(String),
}

/// Why a syntactically complete provider message ended. This is an internal
/// agent contract, not the public native protocol terminal shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "reason", rename_all = "snake_case")]
pub enum AssistantOutcome {
    EndTurn,
    ToolUse,
    OutputLimit(OutputLimitKind),
    Refused,
    Filtered,
    Incomplete(IncompleteReason),
}

/// Events emitted by a provider while one sampling request streams.
#[derive(Clone, Debug)]
pub enum StreamEvent {
    /// Incremental text for display only; the full text arrives via BlockDone.
    TextDelta(String),
    /// Incremental reasoning text for display only; the full block (with its
    /// signature) arrives via BlockDone.
    ThinkingDelta(String),
    /// A fully accumulated, validated assistant output block.
    BlockDone(AssistantBlock),
    /// Stream finished with a mandatory semantic outcome.
    Terminal {
        outcome: AssistantOutcome,
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
            provider_provenance: Some(ProviderResponseProvenance {
                route_revision: 1,
                origin_boundary: 2,
                provider_id: "anthropic".into(),
                api_family: ProviderApiFamily::AnthropicMessages,
                endpoint_fingerprint: "endpoint-sha256".into(),
                model: "model-a".into(),
                attempt_kind: ProviderAttemptKind::Primary,
            }),
            injected: None,
        };
        let wire = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, msg);
        // Roles serialize lowercase.
        assert!(wire.contains("\"role\":\"assistant\""));
        // An absent `injected` is the ordinary case and stays off the wire.
        assert!(!wire.contains("injected"));
        let steer = Message::injected(Injected::Steering, "check the poller");
        let back: Message = serde_json::from_str(&serde_json::to_string(&steer).unwrap()).unwrap();
        assert_eq!(back, steer);
        assert_eq!(back.injected, Some(Injected::Steering));
        let sub = Message::injected(
            Injected::SubAgent {
                label: "agent-1".into(),
            },
            "…",
        );
        let wire = serde_json::to_string(&sub).unwrap();
        assert!(wire.contains(r#""injected":"sub_agent""#), "{wire}");
        assert_eq!(serde_json::from_str::<Message>(&wire).unwrap(), sub);
    }

    #[test]
    fn reasoning_helpers_recurse_through_tool_result_blocks() {
        let nested = ContentBlock::ToolResult {
            tool_use_id: "outer".into(),
            content: ToolResultContent::Blocks(vec![
                ContentBlock::Text {
                    text: "keep".into(),
                },
                ContentBlock::ToolResult {
                    tool_use_id: "inner".into(),
                    content: ToolResultContent::Blocks(vec![
                        ContentBlock::Thinking {
                            thinking: "secret".into(),
                            signature: "opaque".into(),
                        },
                        ContentBlock::RedactedThinking {
                            data: "encrypted".into(),
                        },
                        ContentBlock::Image {
                            source: ImageSource::Base64 {
                                media_type: "image/png".into(),
                                data: "AA==".into(),
                            },
                        },
                    ]),
                    is_error: false,
                },
            ]),
            is_error: false,
        };
        let message = Message::tool_results(vec![nested.clone()]);
        assert!(message.has_reasoning());
        assert!(nested.has_redacted_reasoning());
        assert_eq!(
            nested.into_without_reasoning(),
            Some(ContentBlock::ToolResult {
                tool_use_id: "outer".into(),
                content: ToolResultContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "keep".into(),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "inner".into(),
                        content: ToolResultContent::Blocks(vec![ContentBlock::Image {
                            source: ImageSource::Base64 {
                                media_type: "image/png".into(),
                                data: "AA==".into(),
                            },
                        }]),
                        is_error: false,
                    },
                ]),
                is_error: false,
            })
        );
    }

    #[test]
    fn public_projection_strips_provider_and_opaque_reasoning() {
        let message = Message::assistant_from_provider(
            vec![
                ContentBlock::Thinking {
                    thinking: "display summary".into(),
                    signature: "opaque-signature".into(),
                },
                ContentBlock::RedactedThinking {
                    data: "opaque-redacted".into(),
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: ToolResultContent::Blocks(vec![ContentBlock::Thinking {
                        thinking: "nested summary".into(),
                        signature: "nested-opaque".into(),
                    }]),
                    is_error: false,
                },
            ],
            ProviderResponseProvenance {
                route_revision: 1,
                origin_boundary: 2,
                provider_id: "anthropic".into(),
                api_family: ProviderApiFamily::AnthropicMessages,
                endpoint_fingerprint: "endpoint-sha256".into(),
                model: "model-a".into(),
                attempt_kind: ProviderAttemptKind::Primary,
            },
        );

        let public = message.into_public_projection();
        assert_eq!(public.provider_provenance, None);
        assert_eq!(
            public.content,
            vec![
                ContentBlock::Thinking {
                    thinking: "display summary".into(),
                    signature: String::new(),
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: ToolResultContent::Blocks(vec![ContentBlock::Thinking {
                        thinking: "nested summary".into(),
                        signature: String::new(),
                    }]),
                    is_error: false,
                },
            ]
        );
        let wire = serde_json::to_string(&public).unwrap();
        assert!(!wire.contains("provider_provenance"));
        assert!(!wire.contains("signature"));
        assert!(!wire.contains("opaque"));
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
            provider_provenance: None,
            injected: None,
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
                provider_provenance: None,
                injected: None,
            }
        );
        // Empty text contributes no text block: an image-only message.
        assert_eq!(
            Message::user_with_blocks("", vec![img.clone()]),
            Message {
                role: Role::User,
                content: vec![img],
                provider_provenance: None,
                injected: None,
            }
        );
    }

    #[test]
    fn local_agent_ids_are_canonical_and_string_encoded() {
        for (wire, id, label) in [
            ("main", LocalAgentId::Main, ""),
            (
                "agent-42",
                LocalAgentId::Agent("agent-42".into()),
                "agent-42",
            ),
        ] {
            assert_eq!(wire.parse::<LocalAgentId>().unwrap(), id);
            assert_eq!(id.as_str(), wire);
            assert_eq!(id.display_label(), label);
            assert_eq!(serde_json::to_value(&id).unwrap(), json!(wire));
            assert_eq!(
                serde_json::from_value::<LocalAgentId>(json!(wire)).unwrap(),
                id
            );
        }
        for invalid in [
            "",
            "Main",
            "agent-",
            "agent-0",
            "agent-01",
            "agent-x",
            "program-1",
        ] {
            assert!(invalid.parse::<LocalAgentId>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn local_message_is_distinct_from_provider_chat_and_a2a_task() {
        let message = LocalAgentMessage::text(
            LocalMessageId::new(7).unwrap(),
            LocalContextId::new("local-context-3").unwrap(),
            LocalAgentId::Agent("agent-3".into()),
            LocalAgentId::Main,
            "review the race".into(),
            "Check close ordering.".into(),
        );
        assert_eq!(message.text_body(), "Check close ordering.");
        assert_eq!(
            serde_json::to_value(&message).unwrap(),
            json!({
                "message_id": "message-7",
                "context_id": "local-context-3",
                "from": "agent-3",
                "to": "main",
                "summary": "review the race",
                "parts": [{"type": "text", "text": "Check close ordering."}],
            })
        );

        // A2A 1.0 addresses the peer by endpoint, outside Message. This fixture
        // freezes the future adapter seam: local route/summary never become A2A
        // metadata and a local message id is not a Task id.
        let a2a_message = json!({
            "kind": "message",
            "messageId": message.message_id.as_str(),
            "contextId": message.context_id.as_str(),
            "role": "user",
            "parts": [{"kind": "text", "text": message.text_body()}],
        });
        assert!(a2a_message.get("to").is_none());
        assert!(a2a_message.get("from").is_none());
        assert!(a2a_message.get("summary").is_none());
        assert!(a2a_message.get("taskId").is_none());
        assert!(a2a_message.get("url").is_none());
    }

    #[test]
    fn local_ids_and_context_reject_noncanonical_wire_values() {
        for invalid in ["message-0", "message-01", "message-x", "task-1"] {
            assert!(invalid.parse::<LocalMessageId>().is_err(), "{invalid}");
        }
        assert!(LocalContextId::new("").is_err());
        assert!(LocalContextId::new("bad\ncontext").is_err());
        assert!(LocalContextId::new("x".repeat(129)).is_err());
    }

    #[test]
    fn usage_serde_round_trip_preserves_canonical_fields() {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 42,
            cache_read_input_tokens: 900,
            cache_creation_input_tokens: 8,
        };
        let wire = serde_json::to_value(usage).unwrap();
        assert_eq!(
            wire,
            json!({
                "input_tokens": 100,
                "output_tokens": 42,
                "cache_read_input_tokens": 900,
                "cache_creation_input_tokens": 8,
            })
        );
        assert_eq!(serde_json::from_value::<Usage>(wire).unwrap(), usage);
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

    #[test]
    fn assistant_block_semantic_payload_preserves_exact_empty_rules() {
        assert!(
            !AssistantBlock::Text {
                text: String::new()
            }
            .has_semantic_payload()
        );
        assert!(AssistantBlock::Text { text: " ".into() }.has_semantic_payload());
        assert!(
            !AssistantBlock::RedactedThinking {
                data: String::new()
            }
            .has_semantic_payload()
        );
        assert!(
            !AssistantBlock::Thinking {
                thinking: String::new(),
                signature: String::new(),
            }
            .has_semantic_payload()
        );
        assert!(
            AssistantBlock::Thinking {
                thinking: String::new(),
                signature: "sig".into(),
            }
            .has_semantic_payload()
        );
        assert!(
            AssistantBlock::ToolUse {
                id: "t".into(),
                name: "bash".into(),
                input: json!({}),
            }
            .has_semantic_payload()
        );
    }

    #[test]
    fn assistant_block_converts_only_to_assistant_content_shapes() {
        let cases = [
            (
                AssistantBlock::Text { text: "hi".into() },
                ContentBlock::Text { text: "hi".into() },
            ),
            (
                AssistantBlock::Thinking {
                    thinking: String::new(),
                    signature: "sig".into(),
                },
                ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: "sig".into(),
                },
            ),
            (
                AssistantBlock::RedactedThinking { data: "d".into() },
                ContentBlock::RedactedThinking { data: "d".into() },
            ),
            (
                AssistantBlock::ToolUse {
                    id: "t".into(),
                    name: "bash".into(),
                    input: json!({"command": "pwd"}),
                },
                ContentBlock::ToolUse {
                    id: "t".into(),
                    name: "bash".into(),
                    input: json!({"command": "pwd"}),
                },
            ),
        ];
        for (assistant, content) in cases {
            assert_eq!(assistant.into_content_block(), content);
        }
    }

    /// The effort vocabulary round-trips through the words the wire and the
    /// `/effort` command use, case-insensitively, and rejects anything else.
    #[test]
    fn reasoning_effort_parses_its_own_words_and_rejects_others() {
        let parsed: Vec<ReasoningEffort> = ["none", "LOW", " medium ", "High", "xhigh", "max"]
            .iter()
            .map(|raw| raw.parse().unwrap())
            .collect();
        assert_eq!(parsed, ReasoningEffort::ALL);
        assert_eq!(
            ReasoningEffort::ALL
                .iter()
                .map(|level| level.as_str())
                .collect::<Vec<_>>(),
            ["none", "low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(
            "x-high".parse::<ReasoningEffort>(),
            Err(UnknownReasoningEffort("x-high".into()))
        );
        assert_eq!(
            serde_json::to_value(ReasoningEffort::XHigh).unwrap(),
            serde_json::json!("xhigh")
        );
    }
}
