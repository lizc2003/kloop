//! The provider seam: everything inside the crate speaks the canonical
//! Anthropic Messages shape; adapters translate at this boundary only.

mod anthropic;
mod failure;
mod openai;
mod responses;
pub mod sse;
mod stream;
mod tool_input;

use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use serde_json::Value;
use serde_json::json;

pub use failure::ProviderFailure;
pub use failure::ProviderFailureKind;
pub use failure::TimeoutStage;
pub use stream::ProviderStream;
pub use stream::StreamResult;

pub(crate) use stream::SseFrames;
pub(crate) use stream::StreamCompletion;
pub(crate) use stream::StreamSink;
pub(crate) use stream::send_checked;
use stream::spawn_stream;

use sha2::Digest as _;
use sha2::Sha256;

use kloop_protocol::ANTHROPIC_MAX_OUTPUT_TOKENS;
use kloop_protocol::AssistantBlock;
use kloop_protocol::AssistantOutcome;
use kloop_protocol::Message;
use kloop_protocol::OPENAI_MAX_OUTPUT_TOKENS;
use kloop_protocol::ProviderApiFamily;
use kloop_protocol::ProviderAttemptIdentity;
use kloop_protocol::ProviderResponseProvenance;
use kloop_protocol::ReasoningEffort;
use kloop_protocol::ToolDef;
use kloop_protocol::Usage;

/// One process-wide HTTP client shared by every adapter. reqwest pools
/// connections and reuses TLS sessions, but only within a single `Client`, so
/// a fresh `Client::new()` per request would discard that on every turn.
pub(crate) fn http_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new).clone()
}

/// What the Mock provider saw in one `stream()` call; lets core tests assert
/// the request shape (e.g. injected context messages) without a wire.
#[derive(Clone, Debug)]
pub struct MockRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub effort: Option<ReasoningEffort>,
    pub cache_key: Option<String>,
}

/// One scripted Mock response: content blocks, a gate-delayed response, a
/// truncated response, or a typed provider failure.
pub enum MockTurn {
    Blocks(Vec<AssistantBlock>),
    /// Completed blocks without display deltas, for lifecycle regression tests.
    BlocksWithoutDeltas(Vec<AssistantBlock>),
    /// Return an explicit semantic terminal after the supplied blocks.
    Outcome {
        blocks: Vec<AssistantBlock>,
        outcome: AssistantOutcome,
    },
    /// Return an explicit semantic terminal and canonical usage after the supplied blocks.
    Response {
        blocks: Vec<AssistantBlock>,
        outcome: AssistantOutcome,
        usage: Usage,
    },
    /// Report that sampling started, then wait for an explicit release before
    /// emitting blocks. Tests use this to coordinate concurrent and cancelled
    /// requests without wall-clock timing assumptions.
    Gate {
        started: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
        blocks: Vec<AssistantBlock>,
    },
    /// Blocks delivered, but the stream reports the output limit was hit.
    Truncated(Vec<AssistantBlock>),
    /// Content deltas arrive, then the stream fails before any block completes.
    PartialError(Vec<AssistantBlock>, String),
    /// Complete blocks arrive, then the attempt fails before its terminal.
    BlocksThenError(Vec<AssistantBlock>, ProviderFailure),
    /// The request is rejected for exceeding the context window.
    Overflow,
    /// A retryable transport failure.
    Error(String),
    /// An explicitly classified failure for retry/fallback policy tests.
    Failure(ProviderFailure),
}

/// The `thinking` request parameter on the Anthropic wire. Whatever the mode,
/// thinking blocks the model sends are always accumulated and replayed — on
/// current models thinking is on by default even with no field sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThinkingMode {
    /// Send no thinking field (current models then run adaptive).
    #[default]
    Unset,
    /// `{"type": "disabled"}`.
    Off,
    /// `{"type": "adaptive"}` — explicit, for models where omitting means off.
    Adaptive,
    /// `{"type": "enabled", "budget_tokens": n}` for pre-adaptive models
    /// (rejected by current ones). Thinking spends from max_tokens, so the
    /// request raises max_tokens by the budget instead of clamping the budget.
    Budget(u64),
}

/// How a provider presents its credential on the wire. Both wires have a
/// conventional spelling — Messages sends `x-api-key`, the OpenAI rails send
/// `Authorization: Bearer` — but a gateway in the middle picks its own, so the
/// spelling is configuration rather than a property of the rail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthScheme {
    /// `x-api-key: <secret>`
    ApiKey,
    /// `Authorization: Bearer <secret>`
    Bearer,
}

/// A secret and the header that carries it, kept together because they are only
/// correct together: the scheme decides what goes on the wire, and the bare
/// secret — never the header value around it — is what `redact_secret` has to
/// match in provider-authored text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credential {
    scheme: AuthScheme,
    secret: String,
}

impl Credential {
    pub fn new(scheme: AuthScheme, secret: impl Into<String>) -> Self {
        Self {
            scheme,
            secret: secret.into(),
        }
    }

    pub fn api_key(secret: impl Into<String>) -> Self {
        Self::new(AuthScheme::ApiKey, secret)
    }

    pub fn bearer(secret: impl Into<String>) -> Self {
        Self::new(AuthScheme::Bearer, secret)
    }

    pub fn scheme(&self) -> AuthScheme {
        self.scheme
    }

    pub(crate) fn secret(&self) -> &str {
        &self.secret
    }

    pub(crate) fn apply(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.scheme {
            AuthScheme::ApiKey => req.header("x-api-key", &self.secret),
            AuthScheme::Bearer => req.bearer_auth(&self.secret),
        }
    }
}

/// One knob, rendered. `effort` is what the user chose; `thinking` is what that
/// choice became for the model it is being sent to. They travel together because
/// they are two halves of one setting — a model reads its reasoning depth from
/// one field or the other, never both — and resolving which is the route's job,
/// not the renderer's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reasoning {
    pub effort: Option<ReasoningEffort>,
    pub thinking: ThinkingMode,
}

impl Reasoning {
    pub fn new(effort: Option<ReasoningEffort>, thinking: ThinkingMode) -> Self {
        Self { effort, thinking }
    }
}

pub enum Provider {
    Anthropic {
        cred: Credential,
        base: String,
        /// Prompt caching: mark cache_control breakpoints on the last tool,
        /// the system block, and the last message block. The `thinking` field
        /// is deliberately absent — it is resolved per request from the model
        /// and the session effort, the way `effort` itself already is.
        prompt_cache: bool,
    },
    OpenAiCompat {
        cred: Credential,
        base: String,
    },
    /// OpenAI Responses API (`/responses`), stateless: `store: false`, with
    /// reasoning carried across requests via encrypted_content blobs riding
    /// in `Thinking.signature`.
    OpenAiResponses {
        cred: Credential,
        base: String,
    },
    /// Scripted turns for keyless end-to-end runs; each `stream()` call pops one turn.
    Mock {
        turns: Mutex<VecDeque<MockTurn>>,
        /// Requests as seen, shared out by `mock_recording`.
        seen: Arc<Mutex<Vec<MockRequest>>>,
    },
}

/// Provider-agnostic detection of "request too large for the context window"
/// error payloads.
pub(crate) fn is_overflow_message(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("prompt is too long")
        || lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
}

pub(crate) fn parse_sse_json(rail: &str, data: &str) -> Result<Value, ProviderFailure> {
    serde_json::from_str(data)
        .map_err(|error| ProviderFailure::protocol(format!("{rail} malformed SSE JSON: {error}")))
}

/// Faithfully surface a stream-level error's identifier: prefer the machine
/// `code`, then the `type`, then "unknown". Proxies send transient errors as a
/// bare `{type}` with no code, so both are consulted.
pub(crate) fn error_label(error: &Value) -> &str {
    error["code"]
        .as_str()
        .or_else(|| error["type"].as_str())
        .unwrap_or("unknown")
}

/// The relay's own sentence about what went wrong, which `error_label` throws
/// away. Without it a note reads `stream error (server_error)` and the reader
/// cannot tell an overloaded upstream from a rejected request.
///
/// Redacted through the same primitive as an HTTP error body and bounded on top
/// of it: this text is written by whoever relayed it, at whatever length they
/// chose, and it lands in a note the TUI renders.
pub(crate) fn error_detail(error: &Value, secret: &str) -> Option<String> {
    let message = error["message"].as_str()?.trim();
    if message.is_empty() {
        return None;
    }
    let safe = stream::redact_secret(message, secret)?;
    if safe.chars().count() <= MAX_ERROR_DETAIL_CHARS {
        return Some(safe);
    }
    let truncated: String = safe.chars().take(MAX_ERROR_DETAIL_CHARS).collect();
    Some(format!("{truncated}…"))
}

const MAX_ERROR_DETAIL_CHARS: usize = 300;

/// Classify a faithfully-surfaced stream-error `label` into a typed failure.
/// Only client-side / permanent conditions are fatal; every other error —
/// transient upstream, overload, rate limit, or an unrecognized label — defaults
/// to retryable. This is the inverse of the HTTP-status retry whitelist in
/// `failure.rs`: there a small set is admitted for retry, here a small set is
/// denied it. Safe because core still gates the actual retry on
/// `after_semantic_output` (see `stream.rs`), so a retryable stream error only
/// ever replays before any semantic output. Context-window overflow is
/// classified earlier by each rail and never reaches here.
pub(crate) fn stream_error(rail: &str, label: &str, detail: Option<String>) -> ProviderFailure {
    let message = match detail {
        Some(detail) => format!("{rail} stream error ({label}): {detail}"),
        None => format!("{rail} stream error ({label})"),
    };
    if is_fatal_stream_error(label) {
        ProviderFailure::protocol(message)
    } else {
        ProviderFailure::incomplete_protocol(message)
    }
}

/// Client-side / permanent stream-error identifiers that must not retry, unioned
/// across the OpenAI-family (`code`/`type`) and Anthropic (`type`) vocabularies;
/// the strings do not collide. Transient conditions (`upstream_error`,
/// `server_error`, `overloaded_error`, `rate_limit_error`, `api_error`, …) are
/// deliberately absent so they default to retryable.
fn is_fatal_stream_error(label: &str) -> bool {
    matches!(
        label,
        "insufficient_quota"
            | "usage_not_included"
            | "cyber_policy"
            | "invalid_prompt"
            | "bio_policy"
            | "invalid_request_error"
            | "authentication_error"
            | "permission_error"
            | "not_found_error"
            | "request_too_large"
            | "billing_error"
    )
}

fn validate_assistant_blocks(
    rail: &str,
    blocks: &[AssistantBlock],
) -> Result<bool, ProviderFailure> {
    let mut tool_ids = HashSet::new();
    let mut has_tool = false;
    for block in blocks {
        // An unreadable call is still a call: it owns an id, it will be paired
        // with a tool_result, and the identity rules below are about the wire,
        // which the model's broken JSON says nothing about.
        let identity = match block {
            AssistantBlock::ToolUse { id, name, input } => {
                if !input.is_object() {
                    return Err(ProviderFailure::protocol(format!(
                        "{rail} completed tool {name} with non-object input"
                    )));
                }
                Some((id, name))
            }
            AssistantBlock::InvalidToolUse { id, name, .. } => Some((id, name)),
            AssistantBlock::Text { .. }
            | AssistantBlock::Thinking { .. }
            | AssistantBlock::RedactedThinking { .. } => None,
        };
        if let Some((id, name)) = identity {
            has_tool = true;
            if id.is_empty() || name.is_empty() {
                return Err(ProviderFailure::protocol(format!(
                    "{rail} completed a tool call with an empty identity"
                )));
            }
            if !tool_ids.insert(id) {
                return Err(ProviderFailure::protocol(format!(
                    "{rail} completed duplicate tool call id"
                )));
            }
        }
    }
    Ok(has_tool)
}

pub(crate) fn validate_assistant_output(
    rail: &str,
    outcome: &AssistantOutcome,
    blocks: &[AssistantBlock],
) -> Result<(), ProviderFailure> {
    let has_tool = validate_assistant_blocks(rail, blocks)?;
    match (matches!(outcome, AssistantOutcome::ToolUse), has_tool) {
        (true, false) => Err(ProviderFailure::protocol(format!(
            "{rail} reported tool use without a completed tool call"
        ))),
        (false, true) => Err(ProviderFailure::protocol(format!(
            "{rail} completed a tool call for a non-tool outcome"
        ))),
        _ => Ok(()),
    }
}

/// Bounded, char-boundary-safe view of what the model emitted. Long enough to
/// show a whole ordinary tool call, short enough not to flood a terminal with
/// a runaway one.
fn tool_input_excerpt(raw: &str) -> String {
    const MAX: usize = 400;
    if raw.len() <= MAX {
        return raw.to_string();
    }
    let cut = raw
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= MAX)
        .last()
        .unwrap_or(0);
    format!("{}… ({} bytes total)", &raw[..cut], raw.len())
}

/// The completed tool call for one rail, never failing on the model's own
/// mistakes. Arguments arrive as a string the model generated: invalid JSON is
/// its error to fix, not a broken wire, so an unreadable one becomes
/// [`AssistantBlock::InvalidToolUse`] and the turn hands it back instead of
/// dying. Empty arguments stay the ordinary "no parameters" call.
pub(crate) fn tool_use_block(id: String, name: String, raw: &str) -> AssistantBlock {
    const NOT_AN_OBJECT: &str = "tool arguments must be a JSON object";
    if raw.trim().is_empty() {
        return AssistantBlock::ToolUse {
            id,
            name,
            input: json!({}),
        };
    }
    let invalid = |error: String| AssistantBlock::InvalidToolUse {
        id: id.clone(),
        name: name.clone(),
        raw: tool_input_excerpt(raw),
        error,
    };
    let parsed = match serde_json::from_str::<Value>(raw) {
        Ok(input) => input,
        // Models get this string wrong at a low but real rate. `repair` reads
        // the few unambiguous mistakes and refuses the rest; what it returns
        // has still been through `serde_json`.
        Err(error) => match crate::tool_input::repair(raw) {
            Some(input) => input,
            None => return invalid(error.to_string()),
        },
    };
    if parsed.is_object() {
        AssistantBlock::ToolUse {
            id,
            name,
            input: parsed,
        }
    } else {
        invalid(NOT_AN_OBJECT.into())
    }
}

impl Provider {
    pub fn api_family(&self) -> ProviderApiFamily {
        match self {
            Self::Anthropic { .. } => ProviderApiFamily::AnthropicMessages,
            Self::OpenAiCompat { .. } => ProviderApiFamily::OpenAiChatCompletions,
            Self::OpenAiResponses { .. } => ProviderApiFamily::OpenAiResponses,
            Self::Mock { .. } => ProviderApiFamily::Mock,
        }
    }

    pub fn endpoint_fingerprint_for(api_family: ProviderApiFamily, endpoint: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(format!("{api_family:?}\n{endpoint}"));
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub fn endpoint_fingerprint(&self) -> String {
        let endpoint = match self {
            Self::Anthropic { base, .. }
            | Self::OpenAiCompat { base, .. }
            | Self::OpenAiResponses { base, .. } => base.as_str(),
            Self::Mock { .. } => "mock",
        };
        Self::endpoint_fingerprint_for(self.api_family(), endpoint)
    }

    pub fn attempt_identity(
        &self,
        provider_id: impl Into<String>,
        route_revision: u64,
        model: impl Into<String>,
    ) -> ProviderAttemptIdentity {
        ProviderAttemptIdentity {
            route_revision,
            provider_id: provider_id.into(),
            api_family: self.api_family(),
            endpoint_fingerprint: self.endpoint_fingerprint(),
            model: model.into(),
        }
    }

    /// Fixture helper for constructing a provider-produced message without a
    /// rollout. Production messages are bound by `History` from a frozen attempt.
    pub fn response_provenance(&self, model: &str) -> ProviderResponseProvenance {
        let attempt = self.attempt_identity("test", 1, model);
        ProviderResponseProvenance {
            route_revision: attempt.route_revision,
            origin_boundary: 1,
            provider_id: attempt.provider_id,
            api_family: attempt.api_family,
            endpoint_fingerprint: attempt.endpoint_fingerprint,
            model: attempt.model,
        }
    }

    fn validate_attempt(&self, attempt: &ProviderAttemptIdentity) -> Result<(), ProviderFailure> {
        if attempt.api_family != self.api_family()
            || attempt.endpoint_fingerprint != self.endpoint_fingerprint()
        {
            return Err(ProviderFailure::protocol(
                "frozen provider attempt does not match the selected provider client",
            ));
        }
        Ok(())
    }

    fn validate_reasoning_replay(
        &self,
        attempt: &ProviderAttemptIdentity,
        messages: &[Message],
    ) -> Result<(), ProviderFailure> {
        self.validate_attempt(attempt)?;
        for message in messages {
            if !message.has_reasoning() {
                continue;
            }
            if attempt.api_family == ProviderApiFamily::OpenAiChatCompletions {
                return Err(ProviderFailure::protocol(
                    "chat provider request view must not contain reasoning",
                ));
            }
            if !message
                .provider_provenance
                .as_ref()
                .is_some_and(|source| source.exact_replay_compatible(attempt))
            {
                return Err(ProviderFailure::protocol(
                    "reasoning replay requires the same provider route and exact model",
                ));
            }
        }
        Ok(())
    }

    pub fn mock(turns: Vec<Vec<AssistantBlock>>) -> Self {
        Self::mock_scripted(turns.into_iter().map(MockTurn::Blocks).collect())
    }

    pub fn mock_scripted(turns: Vec<MockTurn>) -> Self {
        Provider::Mock {
            turns: Mutex::new(turns.into()),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Like `mock_scripted`, but also hands back the request log.
    pub fn mock_recording(turns: Vec<MockTurn>) -> (Self, Arc<Mutex<Vec<MockRequest>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = Provider::Mock {
            turns: Mutex::new(turns.into()),
            seen: seen.clone(),
        };
        (provider, seen)
    }

    /// Compatibility test seam. Production callers use `stream_attempt` with a
    /// route receipt minted by the session provider owner.
    pub fn stream(
        self: &Arc<Self>,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> ProviderStream {
        let attempt = self.attempt_identity("test", 1, model);
        self.stream_attempt(
            &attempt,
            Reasoning::default(),
            /*cache_key*/ None,
            system,
            messages,
            tools,
        )
    }

    /// Start one streaming request from an immutable provider attempt. The final
    /// route/reasoning guard executes before any adapter can construct HTTP I/O.
    ///
    /// `effort` is the session's reasoning knob, passed per request rather than
    /// baked into the `Provider` so `/effort` can change it mid-conversation
    /// (the catalog hands out one cached `Provider` per configured id). `None`
    /// sends no effort field on any rail. The caller has already validated it
    /// against this rail's [`ProviderApiFamily::accepted_efforts`].
    ///
    /// `cache_key` is the session-stable cache-affinity hint: the session id,
    /// shared by sampling, compaction, and sub-agents. It only steers which
    /// backend serves the request — never what the request means — so a wrong
    /// or missing value costs cache hits, not correctness. Measured against
    /// 网关, sending nothing makes hits a coin flip: the same 7,697-token
    /// prefix sent three times in a row cached 0, then 6,656, then 0 again.
    ///
    /// Each rail carries it the way its ecosystem expects: Responses as the
    /// `prompt_cache_key` body field (codex sends the session id there), and
    /// Anthropic as the `x-claude-code-session-id` header — that rail has no
    /// such body field, because Anthropic's own cache is prefix-keyed and
    /// workspace-scoped and needs no affinity hint. The header exists for the
    /// gateways that sit in front of it. Chat spells it the same way Responses
    /// does; see that arm for why the field rides there on acceptance rather
    /// than on a measured win.
    pub fn stream_attempt(
        self: &Arc<Self>,
        attempt: &ProviderAttemptIdentity,
        reasoning: Reasoning,
        cache_key: Option<&str>,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> ProviderStream {
        if let Err(error) = self.validate_reasoning_replay(attempt, messages) {
            return spawn_stream(move |_sink| async move { Err(error) });
        }
        let model = attempt.model.as_str();
        let Reasoning { effort, thinking } = reasoning;
        match self.as_ref() {
            Provider::Mock { turns, seen } => {
                seen.lock().unwrap().push(MockRequest {
                    model: model.to_string(),
                    system: system.to_string(),
                    messages: messages.to_vec(),
                    tools: tools.to_vec(),
                    effort,
                    cache_key: cache_key.map(str::to_string),
                });
                let turn = turns.lock().unwrap().pop_front().unwrap_or_else(|| {
                    MockTurn::Blocks(vec![AssistantBlock::Text {
                        text: "mock exhausted".into(),
                    }])
                });
                spawn_stream(move |sink| async move { run_mock_turn(turn, &sink).await })
            }
            Provider::Anthropic {
                cred,
                base,
                prompt_cache,
            } => {
                let url = format!("{base}/v1/messages");
                let cred = cred.clone();
                let cache = *prompt_cache;
                let mut body = json!({
                    "model": model,
                    "max_tokens": ANTHROPIC_MAX_OUTPUT_TOKENS,
                    "system": anthropic::system_value(system, cache),
                    "messages": anthropic::messages_value(messages, cache),
                    "tools": anthropic::tools_value(tools, cache),
                    "stream": true,
                });
                // Effort is the only reasoning knob; `thinking` is what it
                // rendered to for this model (resolved by the catalog, which is
                // the only place that knows the model's dialect). A budget-
                // dialect model takes the whole of it there, so no effort field
                // goes on the wire — sending one is an error on exactly those
                // models.
                match thinking {
                    ThinkingMode::Budget(_) => {}
                    _ => {
                        if let Some(effort) = effort.filter(|e| *e != ReasoningEffort::None) {
                            body["output_config"] = json!({"effort": effort.as_str()});
                        }
                    }
                }
                match thinking {
                    ThinkingMode::Unset => {}
                    ThinkingMode::Off => body["thinking"] = json!({"type": "disabled"}),
                    ThinkingMode::Adaptive => body["thinking"] = json!({"type": "adaptive"}),
                    ThinkingMode::Budget(n) => {
                        body["thinking"] = json!({"type": "enabled", "budget_tokens": n});
                        body["max_tokens"] = json!(ANTHROPIC_MAX_OUTPUT_TOKENS + n);
                    }
                }
                let session = cache_key.map(str::to_string);
                spawn_stream(move |sink| async move {
                    anthropic::stream(&url, &cred, session.as_deref(), &body, &sink).await
                })
            }
            Provider::OpenAiResponses { cred, base } => {
                let url = format!("{base}/responses");
                let cred = cred.clone();
                let mut body = json!({
                    "model": model,
                    "instructions": system,
                    "input": responses::to_input_items(messages),
                    "tools": tools.iter().map(|t| json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.schema,
                    })).collect::<Vec<_>>(),
                    "max_output_tokens": OPENAI_MAX_OUTPUT_TOKENS,
                    "parallel_tool_calls": true,
                    "store": false,
                    "include": ["reasoning.encrypted_content"],
                    "stream": true,
                });
                if let Some(effort) = effort {
                    body["reasoning"] = json!({"effort": effort.as_str(), "summary": "auto"});
                }
                // Empty is not a key: an unbound session would otherwise pin
                // every such run onto one shared bucket.
                if let Some(cache_key) = cache_key.filter(|key| !key.is_empty()) {
                    body["prompt_cache_key"] = json!(cache_key);
                }
                spawn_stream(move |sink| async move {
                    responses::stream(&url, &cred, &body, &sink).await
                })
            }
            Provider::OpenAiCompat { cred, base } => {
                let url = format!("{base}/chat/completions");
                let cred = cred.clone();
                let messages = match openai::to_openai_messages(system, messages) {
                    Ok(messages) => messages,
                    Err(error) => {
                        return spawn_stream(move |_sink| async move { Err(error) });
                    }
                };
                let mut body = json!({
                    "model": model,
                    "max_tokens": OPENAI_MAX_OUTPUT_TOKENS,
                    "stream_options": {"include_usage": true},
                    "messages": messages,
                    "tools": tools.iter().map(|t| json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.schema,
                        },
                    })).collect::<Vec<_>>(),
                    "stream": true,
                });
                // Only reasoning models accept this; a non-reasoning model
                // rejects it, which is why the field is absent unless the
                // session explicitly set an effort.
                if let Some(effort) = effort {
                    body["reasoning_effort"] = json!(effort.as_str());
                }
                // Same field and same empty-is-not-a-key rule as Responses,
                // and caching demonstrably works here: an identical 3,076-token
                // prompt sent three times reported `cached_tokens: 2816` on the
                // third, with prompt cost dropping ~12x. `prompt_tokens_details`
                // is absent on a miss rather than zeroed, so its absence means
                // "no hit", not "not reported" — which is why `usage_from` in
                // `openai.rs` reads a missing block as 0 instead of failing.
                if let Some(cache_key) = cache_key.filter(|key| !key.is_empty()) {
                    body["prompt_cache_key"] = json!(cache_key);
                }
                spawn_stream(
                    move |sink| async move { openai::stream(&url, &cred, &body, &sink).await },
                )
            }
        }
    }
}

fn mock_outcome(blocks: &[AssistantBlock]) -> AssistantOutcome {
    if blocks.iter().any(|block| {
        matches!(
            block,
            AssistantBlock::ToolUse { .. } | AssistantBlock::InvalidToolUse { .. }
        )
    }) {
        AssistantOutcome::ToolUse
    } else {
        AssistantOutcome::EndTurn
    }
}

async fn run_mock_turn(
    turn: MockTurn,
    sink: &StreamSink,
) -> Result<StreamCompletion, ProviderFailure> {
    let (blocks, outcome, usage, with_deltas) = match turn {
        MockTurn::Blocks(blocks) => {
            let outcome = mock_outcome(&blocks);
            (blocks, outcome, None, true)
        }
        MockTurn::BlocksWithoutDeltas(blocks) => {
            let outcome = mock_outcome(&blocks);
            (blocks, outcome, None, false)
        }
        MockTurn::Outcome { blocks, outcome } => (blocks, outcome, None, true),
        MockTurn::Response {
            blocks,
            outcome,
            usage,
        } => (blocks, outcome, Some(usage), true),
        MockTurn::Gate {
            started,
            release,
            blocks,
        } => {
            let _ = started.send(());
            let _ = release.await;
            let outcome = mock_outcome(&blocks);
            (blocks, outcome, None, true)
        }
        MockTurn::Truncated(blocks) => (
            blocks,
            AssistantOutcome::OutputLimit(kloop_protocol::OutputLimitKind::MaxOutputTokens),
            None,
            true,
        ),
        MockTurn::PartialError(blocks, message) => {
            emit_deltas(&blocks, sink).await?;
            return Err(ProviderFailure::transport(message));
        }
        MockTurn::BlocksThenError(blocks, failure) => {
            validate_assistant_blocks("mock", &blocks)?;
            emit_blocks(blocks, sink).await?;
            return Err(failure);
        }
        MockTurn::Overflow => return Err(ProviderFailure::context_overflow()),
        MockTurn::Error(message) => return Err(ProviderFailure::transport(message)),
        MockTurn::Failure(failure) => return Err(failure),
    };
    validate_assistant_output("mock", &outcome, &blocks)?;
    if with_deltas {
        emit_blocks(blocks, sink).await?;
    } else {
        for block in blocks {
            sink.block_done(block).await?;
        }
    }
    Ok(StreamCompletion::new(outcome, usage))
}

async fn emit_deltas(blocks: &[AssistantBlock], sink: &StreamSink) -> Result<(), ProviderFailure> {
    for block in blocks {
        match block {
            AssistantBlock::Text { text } => sink.text_delta(text.clone()).await?,
            AssistantBlock::Thinking { thinking, .. } => {
                sink.thinking_delta(thinking.clone()).await?
            }
            AssistantBlock::RedactedThinking { .. }
            | AssistantBlock::ToolUse { .. }
            | AssistantBlock::InvalidToolUse { .. } => {}
        }
    }
    Ok(())
}

async fn emit_blocks(
    blocks: Vec<AssistantBlock>,
    sink: &StreamSink,
) -> Result<(), ProviderFailure> {
    emit_deltas(&blocks, sink).await?;
    for block in blocks {
        sink.block_done(block).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_input_requires_complete_json_object() {
        let block = |raw: &str| tool_use_block("t1".into(), "bash".into(), raw);
        let ok = |input: Value| AssistantBlock::ToolUse {
            id: "t1".into(),
            name: "bash".into(),
            input,
        };

        assert_eq!(block(""), ok(json!({})));
        assert_eq!(block("  \n"), ok(json!({})));
        assert_eq!(block(r#"{"command":"pwd"}"#), ok(json!({"command": "pwd"})));

        // A repairable mistake never becomes a failed call: one unquoted value
        // is worth a rewrite, not a whole extra sampling round.
        assert_eq!(
            block(r#"{"command": "ls", "description": 看一眼}"#),
            ok(json!({"command": "ls", "description": "看一眼"}))
        );

        // Valid JSON that is not an object is the model's mistake too, and
        // takes the same road back to it.
        for raw in ["null", "[]", "true", "1", r#""text""#] {
            assert_eq!(
                block(raw),
                AssistantBlock::InvalidToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    raw: raw.into(),
                    error: "tool arguments must be a JSON object".into(),
                }
            );
        }

        // The text itself travels with the call: an offset alone cannot say
        // whether the model wrote it wrong or we reassembled it wrong.
        let (raw, error) = match block("{oops") {
            AssistantBlock::InvalidToolUse { raw, error, .. } => (raw, error),
            other => panic!("expected an invalid call, got {other:?}"),
        };
        assert_eq!(raw, "{oops");
        assert!(error.contains("column"), "{error}");

        let long = format!(r#"{{"command":"{}"#, "x".repeat(500));
        let AssistantBlock::InvalidToolUse { raw, .. } = block(&long) else {
            panic!("expected an invalid call");
        };
        assert!(raw.ends_with("(512 bytes total)"), "{raw}");
        assert!(raw.len() < 460, "{}", raw.len());

        // Cutting a multi-byte argument must not cut a character in half.
        let wide = format!(r#"{{"command":"{}"#, "中".repeat(300));
        let AssistantBlock::InvalidToolUse { raw, .. } = block(&wide) else {
            panic!("expected an invalid call");
        };
        assert!(raw.ends_with("(912 bytes total)"), "{raw}");
    }

    #[test]
    fn reasoning_replay_requires_exact_provider_family_and_model() {
        let reasoning = |provenance: Option<ProviderResponseProvenance>| Message {
            role: kloop_protocol::Role::Assistant,
            content: vec![kloop_protocol::ContentBlock::Thinking {
                thinking: "summary".into(),
                signature: "opaque".into(),
            }],
            provider_provenance: provenance,
            injected: None,
        };
        let provider = Provider::mock(Vec::new());
        let attempt = provider.attempt_identity("test", 1, "model-a");
        let exact = provider.response_provenance("model-a");
        assert!(
            provider
                .validate_reasoning_replay(&attempt, &[reasoning(Some(exact.clone()))])
                .is_ok()
        );

        for provenance in [
            None,
            Some(ProviderResponseProvenance {
                model: "model-b".into(),
                ..exact.clone()
            }),
            Some(ProviderResponseProvenance {
                provider_id: "another-provider".into(),
                ..exact.clone()
            }),
            Some(ProviderResponseProvenance {
                api_family: ProviderApiFamily::OpenAiResponses,
                ..exact.clone()
            }),
        ] {
            let error = provider
                .validate_reasoning_replay(&attempt, &[reasoning(provenance)])
                .unwrap_err();
            assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
            assert!(!error.is_retryable());
            assert!(!error.after_semantic_output());
        }
    }

    #[test]
    fn final_reasoning_guard_rejects_nested_or_chat_reasoning() {
        let mock = Provider::mock(Vec::new());
        let mock_attempt = mock.attempt_identity("test", 1, "model-a");
        let nested = Message::assistant(vec![kloop_protocol::ContentBlock::ToolResult {
            tool_use_id: "nested".into(),
            content: kloop_protocol::ToolResultContent::Blocks(vec![
                kloop_protocol::ContentBlock::Thinking {
                    thinking: "hidden".into(),
                    signature: "opaque".into(),
                },
            ]),
            is_error: false,
        }]);
        let error = mock
            .validate_reasoning_replay(&mock_attempt, &[nested])
            .unwrap_err();
        assert_eq!(error.kind(), &ProviderFailureKind::Protocol);

        let chat = Provider::OpenAiCompat {
            cred: Credential::bearer("unused"),
            base: "https://chat.invalid".into(),
        };
        let chat_attempt = chat.attempt_identity("chat", 1, "chat-model");
        let chat_reasoning = Message::assistant_from_provider(
            vec![kloop_protocol::ContentBlock::Thinking {
                thinking: "must be removed by core".into(),
                signature: "opaque".into(),
            }],
            chat.response_provenance("chat-model"),
        );
        let error = chat
            .validate_reasoning_replay(&chat_attempt, &[chat_reasoning])
            .unwrap_err();
        assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
        assert!(error.to_string().contains("must not contain reasoning"));
    }

    #[test]
    fn chat_replay_still_requires_validated_source() {
        let chat = Provider::OpenAiCompat {
            cred: Credential::bearer("unused"),
            base: "https://chat.invalid".into(),
        };
        let foreign = Message::assistant_from_provider(
            vec![kloop_protocol::ContentBlock::Thinking {
                thinking: "must not cross rails".into(),
                signature: "opaque".into(),
            }],
            ProviderResponseProvenance {
                route_revision: 1,
                origin_boundary: 2,
                provider_id: "responses".into(),
                api_family: ProviderApiFamily::OpenAiResponses,
                endpoint_fingerprint: "responses-fingerprint".into(),
                model: "model-a".into(),
            },
        );
        let attempt = chat.attempt_identity("chat", 2, "chat-model");

        assert!(
            chat.validate_reasoning_replay(&attempt, &[foreign])
                .is_err(),
            "chat strips reasoning only after the core request view validates its source"
        );
    }

    #[tokio::test]
    async fn mock_rejects_invalid_blocks_and_outcome_mismatches_before_emitting() {
        let tool = |id: &str, input: Value| AssistantBlock::ToolUse {
            id: id.into(),
            name: "bash".into(),
            input,
        };
        let cases = vec![
            MockTurn::Outcome {
                blocks: vec![tool("", json!({}))],
                outcome: AssistantOutcome::ToolUse,
            },
            MockTurn::Outcome {
                blocks: vec![tool("t1", json!([]))],
                outcome: AssistantOutcome::ToolUse,
            },
            MockTurn::Outcome {
                blocks: vec![tool("t1", json!({})), tool("t1", json!({}))],
                outcome: AssistantOutcome::ToolUse,
            },
            MockTurn::Outcome {
                blocks: vec![AssistantBlock::Text { text: "x".into() }],
                outcome: AssistantOutcome::ToolUse,
            },
            MockTurn::Outcome {
                blocks: vec![tool("t1", json!({}))],
                outcome: AssistantOutcome::EndTurn,
            },
        ];

        for turn in cases {
            let provider = Arc::new(Provider::mock_scripted(vec![turn]));
            let mut stream = provider.stream("mock", "system", &[], &[]);
            let error = stream.recv().await.unwrap().unwrap_err();
            assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
            assert!(!error.after_semantic_output());
            assert!(stream.recv().await.is_none());
        }
    }

    #[tokio::test]
    async fn dropping_provider_stream_aborts_its_producer() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let provider = Arc::new(Provider::mock_scripted(vec![MockTurn::Gate {
            started: started_tx,
            release: release_rx,
            blocks: Vec::new(),
        }]));
        let stream = provider.stream("mock", "system", &[], &[]);
        started_rx.await.unwrap();

        drop(stream);
        tokio::task::yield_now().await;

        assert!(
            release_tx.send(()).is_err(),
            "aborting the producer must drop the gate receiver"
        );
    }
}
