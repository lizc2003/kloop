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
use crate::event::Delta;
use crate::event::Event;
use crate::event::Item;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::StreamEvent;
use kloop_protocol::ToolDef;
use kloop_protocol::Usage;
use kloop_provider::ProviderFailure;

pub(super) struct SampleOk {
    pub(super) blocks: Vec<ContentBlock>,
    pub(super) usage: Option<Usage>,
    pub(super) stop_reason: Option<String>,
}

pub(super) enum Sampled {
    Ok(SampleOk),
    Overflow,
    Cancelled {
        partial: Vec<ContentBlock>,
    },
    /// A retryable failure exhausted the primary model's attempt budget and may
    /// proceed to the configured fallback model.
    Failed(String),
    /// A non-retryable failure ends the turn without replay or model fallback.
    Terminal(String),
    Partial {
        error: String,
        blocks: Vec<ContentBlock>,
    },
}

enum SampleError {
    Cancelled {
        partial: Vec<ContentBlock>,
    },
    Overflow,
    Provider(ProviderFailure),
    /// Retrying or falling back after semantic content would replay output.
    AfterOutput {
        error: String,
        partial: Vec<ContentBlock>,
    },
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
    item_seq: &mut u64,
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
        // Reborrow: a retry within this turn keeps counting up from the same
        // sequence, so a message that only lands on the second attempt still
        // gets a fresh id.
        match sample_once(
            cfg,
            model,
            messages,
            tools,
            ui,
            cancel,
            stream_text,
            &mut *item_seq,
        )
        .await
        {
            Ok(ok) => return Sampled::Ok(ok),
            Err(SampleError::Cancelled { partial }) => return Sampled::Cancelled { partial },
            // Retrying an oversized request verbatim can never succeed; hand
            // it straight to the reactive compaction path.
            Err(SampleError::Overflow) => return Sampled::Overflow,
            Err(SampleError::AfterOutput { error, partial }) => {
                return Sampled::Partial {
                    error,
                    blocks: partial,
                }
            }
            Err(SampleError::Provider(error)) => {
                if !error.is_retryable() {
                    return Sampled::Terminal(error.to_string());
                }
                if attempt + 1 == MAX_ATTEMPTS {
                    return Sampled::Failed(error.to_string());
                }
                // Exponential backoff with sub-ms jitter from the clock's
                // nanoseconds. A bounded provider Retry-After takes precedence.
                let jitter = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| u64::from(d.subsec_nanos()) % 250)
                    .unwrap_or(0);
                let local_delay = Duration::from_millis((250 << attempt) + jitter);
                let delay = error.retry_after().unwrap_or(local_delay);
                ui.emit(&Event::Note(format!(
                    "sampling failed (attempt {}/{MAX_ATTEMPTS}), retrying in {delay:?}: {error}",
                    attempt + 1
                )));
                tokio::select! {
                    _ = cancel.cancelled() => return Sampled::Cancelled { partial: Vec::new() },
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
    item_seq: &mut u64,
) -> Result<SampleOk, SampleError> {
    // effective_system: working-directory line rewritten to the active
    // worktree when the session entered one (plan 35 slice 2).
    let system = cfg.effective_system();
    let mut rx = cfg.provider.stream(model, &system, messages, tools);
    let mut blocks = Vec::new();
    // Open assistant/reasoning items, one of each at a time: a delta opens the
    // item (front-ends see `ItemStarted`), later deltas stream into it, and its
    // `BlockDone` finalizes it. A sub-agent (`stream_text` false) emits no
    // message items — its text is internal. `item_seq` is owned by the turn
    // (threaded from `turn_rounds`), so ids are turn-unique, not per-round. See
    // [`crate::event`].
    let mut text_item: Option<String> = None;
    let mut think_item: Option<String> = None;
    let mut text_accum = String::new();
    let mut think_accum = String::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                complete_open_items(
                    ui,
                    &mut text_item,
                    &mut think_item,
                    &text_accum,
                    &think_accum,
                );
                return Err(SampleError::Cancelled {
                    partial: replayable_partial(blocks, &text_accum),
                });
            },
            event = rx.recv() => match event {
                None => unreachable!(
                    "ProviderStream materializes premature producer close as a typed failure"
                ),
                Some(Err(error)) => {
                    let message = error.to_string();
                    complete_open_items(
                        ui,
                        &mut text_item,
                        &mut think_item,
                        &text_accum,
                        &think_accum,
                    );
                    return Err(if error.after_semantic_output() {
                        SampleError::AfterOutput {
                            error: message,
                            partial: replayable_partial(blocks, &text_accum),
                        }
                    } else if error.is_context_overflow() {
                        SampleError::Overflow
                    } else {
                        SampleError::Provider(error)
                    });
                }
                Some(Ok(StreamEvent::TextDelta(t))) => {
                    if stream_text {
                        text_accum.push_str(&t);
                        let id = open_item(&mut text_item, item_seq, "msg", ui, || {
                            Item::AssistantMessage { text: String::new() }
                        });
                        ui.emit(&Event::ItemDelta { id, delta: Delta::Text(t) });
                    }
                }
                Some(Ok(StreamEvent::ThinkingDelta(t))) => {
                    if stream_text {
                        think_accum.push_str(&t);
                        let id = open_item(&mut think_item, item_seq, "reasoning", ui, || {
                            Item::Reasoning { text: String::new() }
                        });
                        ui.emit(&Event::ItemDelta { id, delta: Delta::Reasoning(t) });
                    }
                }
                Some(Ok(StreamEvent::BlockDone(b))) => {
                    if stream_text {
                        match &b {
                            ContentBlock::Text { text } => {
                                if let Some(id) = text_item.take() {
                                    ui.emit(&Event::ItemCompleted {
                                        id,
                                        item: Item::AssistantMessage { text: text.clone() },
                                    });
                                }
                                text_accum.clear();
                            }
                            ContentBlock::Thinking { thinking, .. } => {
                                if let Some(id) = think_item.take() {
                                    ui.emit(&Event::ItemCompleted {
                                        id,
                                        item: Item::Reasoning { text: thinking.clone() },
                                    });
                                }
                                think_accum.clear();
                            }
                            _ => {}
                        }
                    }
                    blocks.push(b);
                }
                Some(Ok(StreamEvent::Done { usage, stop_reason })) => {
                    complete_open_items(
                        ui,
                        &mut text_item,
                        &mut think_item,
                        &text_accum,
                        &think_accum,
                    );
                    return Ok(SampleOk { blocks, usage, stop_reason });
                }
            }
        }
    }
}

fn replayable_partial(mut blocks: Vec<ContentBlock>, open_text: &str) -> Vec<ContentBlock> {
    blocks.retain(|block| match block {
        ContentBlock::Text { .. } | ContentBlock::RedactedThinking { .. } => true,
        ContentBlock::Thinking { signature, .. } => !signature.is_empty(),
        ContentBlock::Image { .. }
        | ContentBlock::ToolUse { .. }
        | ContentBlock::ToolResult { .. } => false,
    });
    if !open_text.is_empty() {
        blocks.push(ContentBlock::Text {
            text: open_text.to_string(),
        });
    }
    blocks
}

fn complete_open_items(
    ui: &Arc<dyn Ui>,
    text_item: &mut Option<String>,
    think_item: &mut Option<String>,
    text: &str,
    thinking: &str,
) {
    if let Some(id) = text_item.take() {
        ui.emit(&Event::ItemCompleted {
            id,
            item: Item::AssistantMessage {
                text: text.to_string(),
            },
        });
    }
    if let Some(id) = think_item.take() {
        ui.emit(&Event::ItemCompleted {
            id,
            item: Item::Reasoning {
                text: thinking.to_string(),
            },
        });
    }
}

/// Return the open item's id, opening it (assigning an id and emitting
/// `ItemStarted`) on first use. `make` builds the empty starting item.
fn open_item(
    slot: &mut Option<String>,
    seq: &mut u64,
    prefix: &str,
    ui: &Arc<dyn Ui>,
    make: impl FnOnce() -> Item,
) -> String {
    if let Some(id) = slot {
        return id.clone();
    }
    let id = format!("{prefix}-{seq}");
    *seq += 1;
    ui.emit(&Event::ItemStarted {
        id: id.clone(),
        item: make(),
    });
    *slot = Some(id.clone());
    id
}
