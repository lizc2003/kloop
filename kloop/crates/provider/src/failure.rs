use std::fmt;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeoutStage {
    Open,
    Idle,
    Wall,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderFailureKind {
    ContextOverflow,
    Http { status: u16 },
    Transport,
    Timeout { stage: TimeoutStage },
    Protocol,
    ResponseTooLarge,
    Cancelled,
}

/// A typed failure from one provider sampling attempt.
///
/// Retry eligibility belongs to the producer-side classification, not to error
/// string matching in core. Display remains bounded and credential-safe for
/// lossy CLI/TUI/native projections, while core retains this typed value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderFailure {
    kind: ProviderFailureKind,
    message: String,
    retryable: bool,
    retry_after: Option<Duration>,
    semantic_output: bool,
}

impl ProviderFailure {
    pub fn kind(&self) -> &ProviderFailureKind {
        &self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// Rebuild durable terminal evidence. The restored value is display/audit
    /// data only; retry admission is never resumed from a turn terminal.
    #[doc(hidden)]
    pub fn from_recorded_terminal(
        kind: ProviderFailureKind,
        message: String,
        retryable: bool,
        retry_after: Option<Duration>,
        semantic_output: bool,
    ) -> Self {
        Self {
            kind,
            message,
            retryable,
            retry_after,
            semantic_output,
        }
    }

    pub fn is_retryable(&self) -> bool {
        self.retryable
    }

    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// Whether this attempt emitted text, reasoning, or a complete semantic
    /// block before failing. Once true, replaying the request is unsafe.
    pub fn after_semantic_output(&self) -> bool {
        self.semantic_output
    }

    pub fn with_semantic_output(mut self, semantic_output: bool) -> Self {
        self.semantic_output |= semantic_output;
        self
    }

    pub fn is_context_overflow(&self) -> bool {
        self.kind == ProviderFailureKind::ContextOverflow
    }

    pub fn transport(message: impl Into<String>) -> Self {
        Self::new(
            ProviderFailureKind::Transport,
            message,
            /*retryable*/ true,
            None,
        )
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new(
            ProviderFailureKind::Protocol,
            message,
            /*retryable*/ false,
            None,
        )
    }

    pub fn incomplete_protocol(message: impl Into<String>) -> Self {
        Self::new(
            ProviderFailureKind::Protocol,
            message,
            /*retryable*/ true,
            None,
        )
    }

    pub(crate) fn context_overflow() -> Self {
        Self::new(
            ProviderFailureKind::ContextOverflow,
            "context window exceeded",
            /*retryable*/ false,
            None,
        )
    }

    pub fn http(status: u16, message: impl Into<String>, retry_after: Option<Duration>) -> Self {
        let retryable = status == 408 || status == 429 || (500..=599).contains(&status);
        Self::new(
            ProviderFailureKind::Http { status },
            message,
            retryable,
            retry_after.filter(|_| retryable),
        )
    }

    pub(crate) fn timeout(stage: TimeoutStage, message: impl Into<String>) -> Self {
        Self::new(
            ProviderFailureKind::Timeout { stage },
            message,
            /*retryable*/ true,
            None,
        )
    }

    pub(crate) fn response_too_large(message: impl Into<String>) -> Self {
        Self::new(
            ProviderFailureKind::ResponseTooLarge,
            message,
            /*retryable*/ false,
            None,
        )
    }

    pub(crate) fn cancelled() -> Self {
        Self::new(
            ProviderFailureKind::Cancelled,
            "provider stream consumer cancelled",
            /*retryable*/ false,
            None,
        )
    }

    fn new(
        kind: ProviderFailureKind,
        message: impl Into<String>,
        retryable: bool,
        retry_after: Option<Duration>,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable,
            retry_after,
            semantic_output: false,
        }
    }
}

impl fmt::Display for ProviderFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A retryable protocol failure is an upstream hiccup relayed mid-stream,
        // not a contract the provider broke. Both used to render as "provider
        // protocol error", which reads as permanent when it is not — and the
        // reader has no other way to tell whether re-running is worth it.
        let label = match (&self.kind, self.retryable) {
            (ProviderFailureKind::Protocol, true) => "provider stream interrupted",
            _ => self.kind.label(),
        };
        write!(f, "{label}: {}", self.message)
    }
}

impl std::error::Error for ProviderFailure {}

impl ProviderFailureKind {
    fn label(&self) -> &'static str {
        match self {
            Self::ContextOverflow => "context overflow",
            Self::Http { .. } => "provider http error",
            Self::Transport => "provider transport error",
            Self::Timeout {
                stage: TimeoutStage::Open,
            } => "provider open timeout",
            Self::Timeout {
                stage: TimeoutStage::Idle,
            } => "provider idle timeout",
            Self::Timeout {
                stage: TimeoutStage::Wall,
            } => "provider wall timeout",
            Self::Protocol => "provider protocol error",
            Self::ResponseTooLarge => "provider response limit",
            Self::Cancelled => "provider cancelled",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two protocol constructors share a kind, so Display is the only thing
    /// telling a reader whether re-running is worth it.
    #[test]
    fn a_retryable_protocol_failure_does_not_read_as_a_permanent_one() {
        let transient =
            ProviderFailure::incomplete_protocol("openai-responses stream error (server_error)");
        let permanent = ProviderFailure::protocol("openai-responses returned an unknown event");
        assert_eq!(transient.kind(), permanent.kind());
        assert_eq!(
            transient.to_string(),
            "provider stream interrupted: openai-responses stream error (server_error)"
        );
        assert_eq!(
            permanent.to_string(),
            "provider protocol error: openai-responses returned an unknown event"
        );
    }

    #[test]
    fn http_retryability_is_an_explicit_status_allowlist() {
        for status in [408, 429, 500, 503, 599] {
            let failure = ProviderFailure::http(
                status,
                format!("status {status}"),
                Some(Duration::from_secs(12)),
            );
            assert!(failure.is_retryable(), "status {status}");
            assert_eq!(failure.retry_after(), Some(Duration::from_secs(12)));
        }
        for status in [400, 401, 403, 404, 409] {
            let failure = ProviderFailure::http(
                status,
                format!("status {status}"),
                Some(Duration::from_secs(12)),
            );
            assert!(!failure.is_retryable(), "status {status}");
            assert_eq!(failure.retry_after(), None);
        }
    }
}
