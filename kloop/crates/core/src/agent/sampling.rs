//! The sampling layer: one model request with bounded retry + backoff
//! ([`sample_with_retry`]), and the single stream-draining call underneath it
//! ([`sample_once`]). Split from the agent loop (`super`) so `turn_rounds` reads
//! as pure orchestration — it calls `sample_with_retry` and matches on
//! [`Sampled`]; the backoff, overflow/cancel classification, and stream
//! consumption all stay local here.

use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use tokio_util::sync::CancellationToken;

use super::injected_context;
use super::Ui;
use crate::config::Config;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::OverflowError;
use kloop_protocol::StreamEvent;
use kloop_protocol::ToolDef;
use kloop_protocol::Usage;

pub(super) struct SampleOk {
    pub(super) blocks: Vec<ContentBlock>,
    pub(super) usage: Option<Usage>,
    pub(super) stop_reason: Option<String>,
}

pub(super) enum Sampled {
    Ok(SampleOk),
    Overflow,
    Cancelled,
    Failed(String),
}

enum SampleError {
    Cancelled,
    Overflow,
    Retryable(String),
}

const MAX_ATTEMPTS: u32 = 3;

#[allow(clippy::too_many_arguments)]
pub(super) async fn sample_with_retry(
    cfg: &Arc<Config>,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    stream_text: bool,
    depth: u8,
) -> Sampled {
    // Project instructions, the (depth-0) skills catalog, and the deferred-tools
    // notice ride every request as a synthetic first user message. Never
    // recorded: resume rereads fresh files, and compaction cannot swallow it.
    let injected;
    let messages = match injected_context(cfg, depth) {
        Some(context) => {
            let mut with_context = Vec::with_capacity(messages.len() + 1);
            with_context.push(Message::user_text(context));
            with_context.extend_from_slice(messages);
            injected = with_context;
            &injected[..]
        }
        None => messages,
    };
    for attempt in 0..MAX_ATTEMPTS {
        match sample_once(cfg, model, messages, tools, ui, cancel, stream_text).await {
            Ok(ok) => return Sampled::Ok(ok),
            Err(SampleError::Cancelled) => return Sampled::Cancelled,
            // Retrying an oversized request verbatim can never succeed; hand
            // it straight to the reactive compaction path.
            Err(SampleError::Overflow) => return Sampled::Overflow,
            Err(SampleError::Retryable(e)) => {
                if attempt + 1 == MAX_ATTEMPTS {
                    return Sampled::Failed(e);
                }
                // Exponential backoff with sub-ms jitter from the clock's
                // nanoseconds — good enough without pulling in `rand`.
                let jitter = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| u64::from(d.subsec_nanos()) % 250)
                    .unwrap_or(0);
                let delay = Duration::from_millis((250 << attempt) + jitter);
                ui.note(&format!(
                    "sampling failed (attempt {}/{MAX_ATTEMPTS}), retrying in {delay:?}: {e}",
                    attempt + 1
                ));
                tokio::select! {
                    _ = cancel.cancelled() => return Sampled::Cancelled,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
    unreachable!("retry loop always returns")
}

#[allow(clippy::too_many_arguments)]
async fn sample_once(
    cfg: &Arc<Config>,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    stream_text: bool,
) -> Result<SampleOk, SampleError> {
    let mut rx = cfg.provider.stream(model, &cfg.system, messages, tools);
    let mut blocks = Vec::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Err(SampleError::Cancelled),
            event = rx.recv() => match event {
                None => return Err(SampleError::Retryable("stream closed early".into())),
                Some(Err(e)) => {
                    if e.downcast_ref::<OverflowError>().is_some() {
                        return Err(SampleError::Overflow);
                    }
                    return Err(SampleError::Retryable(format!("{e:#}")));
                }
                Some(Ok(StreamEvent::TextDelta(t))) => {
                    if stream_text {
                        ui.text_delta(&t);
                    }
                }
                Some(Ok(StreamEvent::ThinkingDelta(t))) => {
                    if stream_text {
                        ui.thinking_delta(&t);
                    }
                }
                Some(Ok(StreamEvent::BlockDone(b))) => blocks.push(b),
                Some(Ok(StreamEvent::Done { usage, stop_reason })) => {
                    return Ok(SampleOk { blocks, usage, stop_reason })
                }
            }
        }
    }
}
