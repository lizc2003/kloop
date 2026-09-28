//! What an upstream error *is*, read from its text and its label.
//!
//! Three questions, asked of every error a provider sends back: is the request
//! too long for the window (compact, never replay it verbatim), is the account
//! out of quota or budget (stop — no retry can fix it), or neither (the HTTP
//! status or the stream label decides). The HTTP error path in `stream.rs` and
//! every rail's in-stream error frame ask here, so the two paths share one
//! answer.

use std::sync::LazyLock;

use regex::RegexSet;
use regex::RegexSetBuilder;
use serde_json::Value;

use crate::ProviderFailure;

/// Context-window overflow as each provider words it, ported from pi
/// (`packages/ai/src/utils/overflow.ts`, MIT) one entry per provider so a new
/// wording is one more line.
///
/// Deliberately not ported: pi's `request_too_large`. That is Anthropic's 413
/// for a request *body* over its byte limit (images, attachments), not a token
/// overflow; compaction trims by token estimate, may not shrink the bytes at
/// all, and would still pay for a summary. It stays an ordinary non-retryable
/// failure. pi's Cerebras "400/413 (no body)" rule is also absent: it keys on
/// the provider's identity, which an OpenAI-compatible base URL does not carry.
const OVERFLOW: &[&str] = &[
    r"prompt (?:is )?too long",               // Anthropic, z.ai
    r"input is too long for requested model", // Amazon Bedrock
    r"exceeds the context window",            // OpenAI (Chat & Responses)
    r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))", // OpenAI-compatible proxies (LiteLLM)
    r"input token count.*exceeds the maximum", // Google Gemini
    r"maximum prompt length is \d+",           // xAI
    r"reduce the length of the messages",      // Groq
    r"maximum context length is \d+ tokens",   // OpenRouter (most backends)
    r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?", // OpenRouter/Poolside
    r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)", // Together AI
    r"exceeds the limit of \d+",           // GitHub Copilot
    r"exceeds the available context size", // llama.cpp server
    r"greater than the context length",    // LM Studio
    r"context window exceeds limit",       // MiniMax
    r"exceeded model token limit",         // Kimi
    r"too large for model with \d+ maximum context length", // Mistral
    r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?", // DS4 server
    r"model_context_window_exceeded",      // z.ai finish reason surfaced as an error
    r"prompt too long; exceeded (?:max )?context length", // Ollama
    r"range of input length should be",    // DashScope / Qwen
    r"context[_ ]length[_ ]exceeded",      // generic
    r"too many tokens",                    // generic
    r"token limit exceeded",               // generic
    // kloop's own pre-table substring. Broader than the three entries above that
    // contain it (`maximum context length is 128,000 tokens` has a comma `\d+`
    // refuses); kept so nothing the old check caught stops being caught.
    r"maximum context length",
];

/// Text that matches an overflow entry but is throttling — a summary would pay
/// to "fix" what waiting a few seconds fixes. The collision pi met is Bedrock's
/// `Too many tokens, please wait before trying again`. pi anchors its first
/// entry as `^(Throttling error|Service unavailable):` on its own rendering of
/// Bedrock errors; kloop matches raw wire text, so the words stand alone.
const NOT_OVERFLOW: &[&str] = &[
    r"throttling",
    r"service unavailable",
    r"rate limit",
    r"too many requests",
];

/// Quota or billing exhausted: the request is fine, the account is not, and a
/// retry only waits to be refused again. The generic entries of pi's
/// `packages/ai/src/utils/retry.ts`; its gateway-specific ones are not ported.
const QUOTA: &[&str] = &[
    r"insufficient_quota",
    r"out of budget",
    r"quota exceeded",
    r"billing",
];

static OVERFLOW_SET: LazyLock<RegexSet> = LazyLock::new(|| case_insensitive(OVERFLOW));
static NOT_OVERFLOW_SET: LazyLock<RegexSet> = LazyLock::new(|| case_insensitive(NOT_OVERFLOW));
static QUOTA_SET: LazyLock<RegexSet> = LazyLock::new(|| case_insensitive(QUOTA));

fn case_insensitive(patterns: &[&str]) -> RegexSet {
    match RegexSetBuilder::new(patterns)
        .case_insensitive(true)
        .build()
    {
        Ok(set) => set,
        Err(error) => panic!("fixed classification pattern failed to compile: {error}"),
    }
}

/// Whether provider-authored `text` says the request overflowed the context
/// window. Throttling wording vetoes a match. Callers that know the HTTP status
/// gate on it too: the text alone cannot tell `Too many tokens` the overflow
/// from `Too many tokens, please wait` the throttle.
pub(crate) fn is_overflow_text(text: &str) -> bool {
    !NOT_OVERFLOW_SET.is_match(text) && OVERFLOW_SET.is_match(text)
}

/// Whether provider-authored `text` says the account is out of quota or budget.
pub(crate) fn is_quota_text(text: &str) -> bool {
    QUOTA_SET.is_match(text)
}

/// Classify one in-stream error frame into a typed failure. `error` is the
/// object the rail took `label` from; the text tables match its whole
/// serialization, so a code, a type or a message can each carry the signal.
///
/// A label that already names throttling or overload outranks the text: the
/// label is the provider's own vocabulary, the message is prose a relay may
/// rewrite. Only client-side / permanent conditions (a fatal label, or quota
/// text) are fatal; every other error — transient upstream, overload, rate
/// limit, or an unrecognized label — defaults to retryable. This is the inverse
/// of the HTTP-status retry allowlist in `failure.rs`: there a small set is
/// admitted for retry, here a small set is denied it. Safe because core still
/// gates the actual retry on `after_semantic_output` (see `stream.rs`), so a
/// retryable stream error only ever replays before any semantic output.
pub(crate) fn stream_failure(
    rail: &str,
    label: &str,
    error: &Value,
    secret: &str,
) -> ProviderFailure {
    let text = error.to_string();
    if !is_throttle_label(label) && is_overflow_text(&text) {
        return ProviderFailure::context_overflow();
    }
    let message = match crate::error_detail(error, secret) {
        Some(detail) => format!("{rail} stream error ({label}): {detail}"),
        None => format!("{rail} stream error ({label})"),
    };
    if is_fatal_stream_error(label) || is_quota_text(&text) {
        ProviderFailure::protocol(message)
    } else {
        ProviderFailure::incomplete_protocol(message)
    }
}

/// Labels that say "slow down", whatever the message beside them says.
fn is_throttle_label(label: &str) -> bool {
    matches!(
        label,
        "rate_limit_error" | "overloaded_error" | "rate_limit_exceeded"
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One wording per provider, from the examples pi keeps beside its table.
    #[test]
    fn every_provider_overflow_wording_is_recognized() {
        let wordings = [
            "prompt is too long: 213462 tokens > 200000 maximum",
            r#"{"code":"1261","message":"Prompt too long"}"#,
            "Input is too long for requested model.",
            "Your input exceeds the context window of this model",
            "Requested token count exceeds the model's maximum context length of 131072 tokens",
            "Input length (265330) exceeds model's maximum context length (262144).",
            "The input token count (1196265) exceeds the maximum number of tokens allowed (1048575)",
            "This model's maximum prompt length is 131072 but the request contains 537812 tokens",
            "Please reduce the length of the messages or completion",
            "This endpoint's maximum context length is 131072 tokens. However, you requested about 200000 tokens",
            "Input length 300000 exceeds the maximum allowed input length of 262144 tokens.",
            "The input (300000 tokens) is longer than the model's context length (262144 tokens).",
            "the request exceeds the available context size, try increasing it",
            "tokens to keep from the initial prompt is greater than the context length",
            "prompt token count of 200000 exceeds the limit of 128000",
            "invalid params, context window exceeds limit",
            "Your request exceeded model token limit: 262144 (requested: 300000)",
            "Prompt contains 300000 tokens, too large for model with 262144 maximum context length",
            "Prompt has 300000 tokens, but the configured context size is 262144 tokens",
            "model_context_window_exceeded",
            "prompt too long; exceeded max context length by 1200 tokens",
            "Range of input length should be [1, 98304]",
            r#"{"code":"context_length_exceeded"}"#,
            "This model's maximum context length is 128,000 tokens",
        ];
        let recognized: Vec<bool> = wordings.iter().map(|text| is_overflow_text(text)).collect();
        assert_eq!(recognized, vec![true; wordings.len()]);
    }

    #[test]
    fn byte_limits_throttles_and_plain_rejections_are_not_overflow() {
        let not_overflow = [
            r#"{"type":"error","error":{"type":"request_too_large","message":"Request exceeds the maximum size"}}"#,
            "Throttling error: Too many tokens, please wait before trying again.",
            "Rate limit reached for gpt-4o in organization org-1 on tokens per min",
            "429 Too Many Requests",
            r#"{"error":{"type":"invalid_request_error","message":"messages: text content blocks must be non-empty"}}"#,
        ];
        let recognized: Vec<bool> = not_overflow
            .iter()
            .map(|text| is_overflow_text(text))
            .collect();
        assert_eq!(recognized, vec![false; not_overflow.len()]);
    }

    #[test]
    fn quota_wording_is_told_apart_from_rate_limits() {
        let recognized: Vec<bool> = [
            r#"{"error":{"code":"insufficient_quota"}}"#,
            "You exceeded your current quota, please check your plan and billing details.",
            "Quota exceeded for this project",
            "rate limit exceeded",
        ]
        .iter()
        .map(|text| is_quota_text(text))
        .collect();
        assert_eq!(recognized, vec![true, true, true, false]);
    }
}
