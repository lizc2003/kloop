use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use std::time::SystemTime;

use futures::Stream;
use futures::StreamExt;
use kloop_protocol::ContentBlock;
use kloop_protocol::StreamEvent;
use kloop_protocol::Usage;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio::time::Sleep;

use crate::ProviderFailure;
use crate::TimeoutStage;

pub(crate) const STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(45);
pub(crate) const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
pub(crate) const STREAM_WALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub(crate) const STREAM_MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
pub(crate) const STREAM_MAX_FRAME_BYTES: usize = 1024 * 1024;
const HTTP_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

pub type StreamResult = Result<StreamEvent, ProviderFailure>;

#[derive(Debug)]
pub(crate) struct StreamCompletion {
    pub(crate) stop_reason: Option<String>,
    pub(crate) usage: Option<Usage>,
}

impl StreamCompletion {
    pub(crate) fn new(stop_reason: Option<String>, usage: Option<Usage>) -> Self {
        Self { stop_reason, usage }
    }
}

#[derive(Clone)]
pub(crate) struct StreamSink {
    tx: mpsc::Sender<StreamResult>,
}

impl StreamSink {
    pub(crate) async fn text_delta(&self, text: String) -> Result<(), ProviderFailure> {
        self.send(StreamEvent::TextDelta(text)).await
    }

    pub(crate) async fn thinking_delta(&self, text: String) -> Result<(), ProviderFailure> {
        self.send(StreamEvent::ThinkingDelta(text)).await
    }

    pub(crate) async fn block_done(&self, block: ContentBlock) -> Result<(), ProviderFailure> {
        self.send(StreamEvent::BlockDone(block)).await
    }

    async fn send(&self, event: StreamEvent) -> Result<(), ProviderFailure> {
        self.tx
            .send(Ok(event))
            .await
            .map_err(|_| ProviderFailure::cancelled())
    }
}

/// A stream receiver that owns its producer. Dropping it aborts an in-flight
/// HTTP body read immediately instead of leaving a detached task alive until
/// the provider sends another chunk.
pub struct ProviderStream {
    rx: mpsc::Receiver<StreamResult>,
    producer: JoinHandle<()>,
    semantic_output: bool,
    terminal_seen: bool,
}

impl ProviderStream {
    pub async fn recv(&mut self) -> Option<StreamResult> {
        if self.terminal_seen {
            return None;
        }
        match self.rx.recv().await {
            Some(Ok(event)) => {
                self.semantic_output |= semantic_event(&event);
                self.terminal_seen = matches!(event, StreamEvent::Done { .. });
                Some(Ok(event))
            }
            Some(Err(error)) => {
                self.terminal_seen = true;
                Some(Err(error.with_semantic_output(self.semantic_output)))
            }
            None => {
                self.terminal_seen = true;
                Some(Err(ProviderFailure::incomplete_protocol(
                    "provider producer closed without a terminal event",
                )
                .with_semantic_output(self.semantic_output)))
            }
        }
    }
}

fn semantic_event(event: &StreamEvent) -> bool {
    match event {
        StreamEvent::TextDelta(text) | StreamEvent::ThinkingDelta(text) => !text.is_empty(),
        StreamEvent::BlockDone(block) => match block {
            ContentBlock::Text { text } => !text.is_empty(),
            ContentBlock::Thinking {
                thinking,
                signature,
            } => !thinking.is_empty() || !signature.is_empty(),
            ContentBlock::RedactedThinking { data } => !data.is_empty(),
            ContentBlock::ToolUse { .. } => true,
            ContentBlock::Image { .. } | ContentBlock::ToolResult { .. } => false,
        },
        StreamEvent::Done { .. } => false,
    }
}

impl Drop for ProviderStream {
    fn drop(&mut self) {
        self.producer.abort();
    }
}

pub(crate) fn spawn_stream<F, Fut>(run: F) -> ProviderStream
where
    F: FnOnce(StreamSink) -> Fut + Send + 'static,
    Fut: Future<Output = Result<StreamCompletion, ProviderFailure>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel(64);
    let sink = StreamSink { tx: tx.clone() };
    let producer = tokio::spawn(async move {
        let terminal = match run(sink).await {
            Ok(completion) => Ok(StreamEvent::Done {
                stop_reason: completion.stop_reason,
                usage: completion.usage,
            }),
            Err(error) => Err(error),
        };
        let _ = tx.send(terminal).await;
    });
    ProviderStream {
        rx,
        producer,
        semantic_output: false,
        terminal_seen: false,
    }
}

#[derive(Clone, Copy)]
struct StreamLimits {
    idle: Duration,
    wall: Duration,
    response_bytes: usize,
}

impl Default for StreamLimits {
    fn default() -> Self {
        Self {
            idle: STREAM_IDLE_TIMEOUT,
            wall: STREAM_WALL_TIMEOUT,
            response_bytes: STREAM_MAX_RESPONSE_BYTES,
        }
    }
}

pub(crate) struct GuardedBody<S> {
    stream: S,
    received: usize,
    limits: StreamLimits,
    idle_sleep: Pin<Box<Sleep>>,
    wall_sleep: Pin<Box<Sleep>>,
}

impl<S> GuardedBody<S> {
    pub(crate) fn new(stream: S) -> Self {
        Self::from_limits(stream, StreamLimits::default())
    }

    #[cfg(test)]
    fn with_limits(stream: S, idle: Duration, wall: Duration, response_bytes: usize) -> Self {
        Self::from_limits(
            stream,
            StreamLimits {
                idle,
                wall,
                response_bytes,
            },
        )
    }

    fn from_limits(stream: S, limits: StreamLimits) -> Self {
        let now = Instant::now();
        Self {
            stream,
            received: 0,
            limits,
            idle_sleep: Box::pin(tokio::time::sleep_until(now + limits.idle)),
            wall_sleep: Box::pin(tokio::time::sleep_until(now + limits.wall)),
        }
    }

    pub(crate) async fn next<T, E>(&mut self) -> Result<Option<T>, ProviderFailure>
    where
        S: Stream<Item = Result<T, E>> + Unpin,
        T: AsRef<[u8]>,
        E: fmt::Display,
    {
        self.idle_sleep
            .as_mut()
            .reset(Instant::now() + self.limits.idle);
        let next = tokio::select! {
            biased;
            _ = self.wall_sleep.as_mut() => {
                return Err(ProviderFailure::timeout(
                    TimeoutStage::Wall,
                    format!("stream exceeded {}s", self.limits.wall.as_secs()),
                ));
            }
            _ = self.idle_sleep.as_mut() => {
                return Err(ProviderFailure::timeout(
                    TimeoutStage::Idle,
                    format!("no response chunk for {}s", self.limits.idle.as_secs()),
                ));
            }
            value = self.stream.next() => value,
        };
        let Some(chunk) = next else {
            return Ok(None);
        };
        let chunk = chunk.map_err(|error| {
            ProviderFailure::transport(format!("response body read failed: {error}"))
        })?;
        self.received = self
            .received
            .checked_add(chunk.as_ref().len())
            .ok_or_else(|| {
                ProviderFailure::response_too_large("response byte counter overflowed")
            })?;
        if self.received > self.limits.response_bytes {
            return Err(ProviderFailure::response_too_large(format!(
                "response exceeded {} bytes",
                self.limits.response_bytes
            )));
        }
        Ok(Some(chunk))
    }
}

pub(crate) async fn send_checked(
    req: reqwest::RequestBuilder,
    label: &str,
    secret: &str,
) -> Result<reqwest::Response, ProviderFailure> {
    let resp = match wait_for_open(req.send(), STREAM_OPEN_TIMEOUT).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            return Err(ProviderFailure::transport(format!(
                "{label} request failed: {error}"
            )))
        }
        Err(()) => {
            return Err(ProviderFailure::timeout(
                TimeoutStage::Open,
                format!(
                    "{label} response headers did not arrive within {}s",
                    STREAM_OPEN_TIMEOUT.as_secs()
                ),
            ))
        }
    };
    if resp.status().is_success() {
        if resp
            .content_length()
            .is_some_and(|bytes| bytes > STREAM_MAX_RESPONSE_BYTES as u64)
        {
            return Err(ProviderFailure::response_too_large(format!(
                "response Content-Length exceeded {} bytes",
                STREAM_MAX_RESPONSE_BYTES
            )));
        }
        return Ok(resp);
    }

    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| retry_after_delay(value, SystemTime::now()));
    let text = bounded_http_error_body(resp).await;
    if crate::is_overflow_message(&text) {
        return Err(ProviderFailure::context_overflow());
    }
    let text = sanitized_http_error(&text, secret);
    Err(ProviderFailure::http(
        status,
        format!("{label} http {status}: {text}"),
        retry_after,
    ))
}

async fn wait_for_open<F, T>(future: F, limit: Duration) -> Result<T, ()>
where
    F: Future<Output = T>,
{
    tokio::time::timeout(limit, future).await.map_err(|_| ())
}

async fn bounded_http_error_body(mut response: reqwest::Response) -> String {
    let read = async {
        let mut bytes = Vec::new();
        let mut truncated = false;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    let remaining = HTTP_ERROR_BODY_BYTES.saturating_sub(bytes.len());
                    if chunk.len() >= remaining {
                        bytes.extend_from_slice(&chunk[..remaining]);
                        // Stop at the cap even when this chunk lands exactly on
                        // it: waiting for another byte/EOF would let an error
                        // response hold the request open for 45 more seconds.
                        truncated = true;
                        break;
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(error) => {
                    return format!("[failed to read response body: {error}]");
                }
            }
        }
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if truncated {
            text.push_str("… [truncated]");
        }
        text
    };
    match tokio::time::timeout(STREAM_OPEN_TIMEOUT, read).await {
        Ok(text) => text,
        Err(_) => "[response body read timed out]".into(),
    }
}

pub(crate) fn sanitized_http_error(text: &str, secret: &str) -> String {
    if !secret.is_empty() && secret.len() < 8 {
        return "[response body redacted]".into();
    }
    let redacted = if secret.is_empty() {
        text.to_string()
    } else {
        text.replace(secret, "[redacted]")
    };
    let mut chars = redacted.chars();
    let mut bounded: String = chars.by_ref().take(4096).collect();
    if chars.next().is_some() {
        bounded.push_str("… [truncated]");
    }
    bounded
}

pub(crate) fn retry_after_delay(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    let delay = if let Ok(seconds) = value.parse::<u64>() {
        Duration::from_secs(seconds)
    } else {
        httpdate::parse_http_date(value)
            .ok()?
            .duration_since(now)
            .unwrap_or(Duration::ZERO)
    };
    Some(delay.min(MAX_RETRY_AFTER))
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::future::pending;

    use futures::stream;

    use super::*;

    #[test]
    fn fixed_guard_defaults_are_the_public_contract() {
        assert_eq!(STREAM_OPEN_TIMEOUT, Duration::from_secs(45));
        assert_eq!(STREAM_IDLE_TIMEOUT, Duration::from_secs(15 * 60));
        assert_eq!(STREAM_WALL_TIMEOUT, Duration::from_secs(30 * 60));
        assert_eq!(STREAM_MAX_RESPONSE_BYTES, 10 * 1024 * 1024);
        assert_eq!(STREAM_MAX_FRAME_BYTES, 1024 * 1024);
    }

    #[tokio::test(start_paused = true)]
    async fn open_wait_is_bounded() {
        let result = wait_for_open(pending::<()>(), Duration::from_secs(45)).await;
        assert_eq!(result, Err(()));
    }

    #[tokio::test(start_paused = true)]
    async fn body_distinguishes_idle_and_wall_timeouts() {
        let mut idle = GuardedBody::with_limits(
            stream::pending::<Result<Vec<u8>, Infallible>>(),
            Duration::from_secs(5),
            Duration::from_secs(20),
            100,
        );
        let idle_error = idle.next().await.unwrap_err();
        assert_eq!(
            idle_error.kind(),
            &crate::ProviderFailureKind::Timeout {
                stage: TimeoutStage::Idle
            }
        );

        let mut wall = GuardedBody::with_limits(
            stream::pending::<Result<Vec<u8>, Infallible>>(),
            Duration::from_secs(20),
            Duration::from_secs(5),
            100,
        );
        let wall_error = wall.next().await.unwrap_err();
        assert_eq!(
            wall_error.kind(),
            &crate::ProviderFailureKind::Timeout {
                stage: TimeoutStage::Wall
            }
        );
    }

    #[tokio::test]
    async fn body_caps_total_response_bytes() {
        let chunks: Vec<Result<Vec<u8>, Infallible>> = vec![Ok(vec![0; 6]), Ok(vec![0; 5])];
        let mut body = GuardedBody::with_limits(
            stream::iter(chunks),
            Duration::from_secs(1),
            Duration::from_secs(2),
            10,
        );
        assert_eq!(body.next().await.unwrap().unwrap().len(), 6);
        let error = body.next().await.unwrap_err();
        assert_eq!(error.kind(), &crate::ProviderFailureKind::ResponseTooLarge);
    }

    #[test]
    fn retry_after_supports_seconds_dates_and_cap() {
        let now = httpdate::parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        assert_eq!(retry_after_delay("12", now), Some(Duration::from_secs(12)));
        assert_eq!(
            retry_after_delay("Sun, 06 Nov 1994 08:50:07 GMT", now),
            Some(Duration::from_secs(30))
        );
        assert_eq!(retry_after_delay("600", now), Some(Duration::from_secs(60)));
        assert_eq!(retry_after_delay("later", now), None);
    }

    #[test]
    fn http_errors_redact_known_keys_and_bound_untrusted_bodies() {
        let secret = "SENTINEL-provider-key";
        let error = sanitized_http_error(
            &format!("upstream reflected Authorization: Bearer {secret}"),
            secret,
        );
        assert!(!error.contains(secret));
        assert!(error.contains("[redacted]"));

        let long = "界".repeat(5000);
        let bounded = sanitized_http_error(&long, secret);
        assert!(bounded.chars().count() < 4200);
        assert!(bounded.ends_with("… [truncated]"));
    }

    #[test]
    fn short_credentials_redact_the_entire_response_body() {
        assert_eq!(
            sanitized_http_error("server echoed abc", "abc"),
            "[response body redacted]"
        );
    }
}
