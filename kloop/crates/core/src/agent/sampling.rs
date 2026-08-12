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

use super::Ui;
use super::injected_context;
use crate::config::Config;
use crate::config::EffectiveWorkspace;
use crate::event::Delta;
use crate::event::Event;
use crate::event::Item;
use crate::event::ItemStatus;
use kloop_protocol::AssistantBlock;
use kloop_protocol::AssistantOutcome;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::StreamEvent;
use kloop_protocol::ToolDef;
use kloop_protocol::Usage;
use kloop_provider::ProviderFailure;

pub(super) struct SampleOk {
    pub(super) blocks: Vec<ContentBlock>,
    pub(super) usage: Option<Usage>,
    pub(super) outcome: AssistantOutcome,
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
    workspace: &EffectiveWorkspace,
    item_seq: &mut u64,
) -> Sampled {
    // Project instructions, the (depth-0) skills catalog, and the deferred-tools
    // notice ride every request as a synthetic first user message. Never
    // recorded: resume rereads fresh files, and compaction cannot swallow it.
    let injected;
    let messages = match injected_context(cfg, workspace, depth) {
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
            workspace,
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
                };
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
    workspace: &EffectiveWorkspace,
    item_seq: &mut u64,
) -> Result<SampleOk, SampleError> {
    let system = &workspace.system;
    let mut rx = cfg.provider.stream(model, system, messages, tools);
    let mut blocks: Vec<AssistantBlock> = Vec::new();
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
                finish_open_items(
                    ui,
                    &mut text_item,
                    &mut think_item,
                    &text_accum,
                    &think_accum,
                    ItemStatus::Failed,
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
                    finish_open_items(
                        ui,
                        &mut text_item,
                        &mut think_item,
                        &text_accum,
                        &think_accum,
                        ItemStatus::Failed,
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
                    text_accum.push_str(&t);
                    if stream_text && !t.is_empty() {
                        let id = open_item(&mut text_item, item_seq, "msg", ui, || {
                            Item::AssistantMessage {
                                text: String::new(),
                                status: ItemStatus::InProgress,
                            }
                        });
                        ui.emit(&Event::ItemDelta { id, delta: Delta::Text(t) });
                    }
                }
                Some(Ok(StreamEvent::ThinkingDelta(t))) => {
                    think_accum.push_str(&t);
                    if stream_text && !t.is_empty() {
                        let id = open_item(&mut think_item, item_seq, "reasoning", ui, || {
                            Item::Reasoning {
                                text: String::new(),
                                status: ItemStatus::InProgress,
                            }
                        });
                        ui.emit(&Event::ItemDelta { id, delta: Delta::Reasoning(t) });
                    }
                }
                Some(Ok(StreamEvent::BlockDone(block))) => {
                    if stream_text {
                        match &block {
                            AssistantBlock::Text { text } => {
                                let id = open_item(&mut text_item, item_seq, "msg", ui, || {
                                    Item::AssistantMessage {
                                        text: String::new(),
                                        status: ItemStatus::InProgress,
                                    }
                                });
                                let _ = text_item.take();
                                ui.emit(&Event::ItemCompleted {
                                    id,
                                    item: Item::AssistantMessage {
                                        text: text.clone(),
                                        status: ItemStatus::Completed,
                                    },
                                });
                            }
                            AssistantBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                                let id = open_item(
                                    &mut think_item,
                                    item_seq,
                                    "reasoning",
                                    ui,
                                    || Item::Reasoning {
                                        text: String::new(),
                                        status: ItemStatus::InProgress,
                                    },
                                );
                                let _ = think_item.take();
                                ui.emit(&Event::ItemCompleted {
                                    id,
                                    item: Item::Reasoning {
                                        text: thinking.clone(),
                                        status: ItemStatus::Completed,
                                    },
                                });
                            }
                            AssistantBlock::Thinking { .. }
                            | AssistantBlock::RedactedThinking { .. }
                            | AssistantBlock::ToolUse { .. } => {}
                        }
                    }
                    match &block {
                        AssistantBlock::Text { .. } => text_accum.clear(),
                        AssistantBlock::Thinking { .. } => think_accum.clear(),
                        AssistantBlock::RedactedThinking { .. }
                        | AssistantBlock::ToolUse { .. } => {}
                    }
                    blocks.push(block);
                }
                Some(Ok(StreamEvent::Terminal { outcome, usage })) => {
                    if text_item.is_some()
                        || think_item.is_some()
                        || !text_accum.is_empty()
                        || !think_accum.is_empty()
                    {
                        finish_open_items(
                            ui,
                            &mut text_item,
                            &mut think_item,
                            &text_accum,
                            &think_accum,
                            ItemStatus::Failed,
                        );
                        return Err(SampleError::AfterOutput {
                            error: "provider terminal arrived before display output closed".into(),
                            partial: replayable_partial(blocks, &text_accum),
                        });
                    }
                    return Ok(SampleOk {
                        blocks: blocks
                            .into_iter()
                            .map(AssistantBlock::into_content_block)
                            .collect(),
                        usage,
                        outcome,
                    });
                }
            }
        }
    }
}

fn replayable_partial(blocks: Vec<AssistantBlock>, open_text: &str) -> Vec<ContentBlock> {
    let mut replayable: Vec<ContentBlock> = blocks
        .into_iter()
        .filter_map(|block| match block {
            AssistantBlock::Text { text } => Some(ContentBlock::Text { text }),
            AssistantBlock::RedactedThinking { data } => {
                Some(ContentBlock::RedactedThinking { data })
            }
            AssistantBlock::Thinking {
                thinking,
                signature,
            } if !signature.is_empty() => Some(ContentBlock::Thinking {
                thinking,
                signature,
            }),
            AssistantBlock::Thinking { .. } | AssistantBlock::ToolUse { .. } => None,
        })
        .collect();
    if !open_text.is_empty() {
        replayable.push(ContentBlock::Text {
            text: open_text.to_string(),
        });
    }
    replayable
}

fn finish_open_items(
    ui: &Arc<dyn Ui>,
    text_item: &mut Option<String>,
    think_item: &mut Option<String>,
    text: &str,
    thinking: &str,
    status: ItemStatus,
) {
    if let Some(id) = text_item.take() {
        ui.emit(&Event::ItemCompleted {
            id,
            item: Item::AssistantMessage {
                text: text.to_string(),
                status,
            },
        });
    }
    if let Some(id) = think_item.take() {
        ui.emit(&Event::ItemCompleted {
            id,
            item: Item::Reasoning {
                text: thinking.to_string(),
                status,
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
