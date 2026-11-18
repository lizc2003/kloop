//! 线程事件投影与审批、提问的反向请求适配。

use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use kloop_core::agent::Ui;
use kloop_core::event::Event;
use kloop_core::interaction::QuestionOutcome;
use kloop_core::interaction::QuestionRequest;
use kloop_core::interaction::Questioner;
use kloop_core::permissions::ApprovalScope;
use kloop_core::permissions::Approver;
use kloop_core::permissions::ConfirmPreview;
use kloop_core::permissions::ConfirmRequest;
use kloop_core::permissions::Decision;

use crate::PendingGuard;
use crate::PendingInteraction;
use crate::PendingInteractions;
use crate::RequestId;
use crate::events::ThreadProjection;
use crate::wire;
use crate::wire::Outgoing;

/// Per-thread `Ui` + `Approver`: events become thread-tagged notifications,
/// confirmations become `approval/request` server requests answered by id.
pub(super) struct ThreadUi {
    pub(super) thread_id: String,
    pub(super) out: mpsc::UnboundedSender<Vec<u8>>,
    pub(super) pending: PendingInteractions,
    pub(super) reverse_request_seq: Arc<AtomicU64>,
    /// The running turn's id (shared with the handle), used to tag item events.
    pub(super) turn: Arc<Mutex<Option<u64>>>,
    pub(super) projection: Arc<ThreadProjection>,
    /// Input is gone: a request this thread asks now has no answer coming.
    pub(super) shutdown: CancellationToken,
}

impl ThreadUi {
    pub(super) fn notify(&self, method: &'static str, params: Value) {
        let _ = self.projection.publish(method, params, &self.out);
    }

    fn active_turn_id(&self) -> Option<u64> {
        *self.turn.lock().unwrap()
    }

    fn turn_id(&self) -> u64 {
        self.active_turn_id().unwrap_or(0)
    }
}

impl Ui for ThreadUi {
    /// Project the core [`Event`] stream onto the native wire via the shared
    /// [`wire::project_event`]. The turn bracket (`turn/started`/`turn/completed`)
    /// is constructed by the worker, which owns the turn id.
    fn emit(&self, ev: &Event) {
        if let Some((method, params)) = wire::project_event(ev, self.turn_id()) {
            self.notify(method, params);
        }
    }
}

/// The two refusals are codex's: `decline` is "not this, carry on", `cancel` is
/// "not this, and stop until I say" (plan 216). Anything the client did not
/// clearly say — the removed accept_always token, an unknown or missing
/// decision — also stops: an approval channel that has gone wrong is no
/// reason to let the model keep acting.
pub(super) fn approval_decision(result: &Value) -> Decision {
    match result["decision"].as_str() {
        Some("accept") => Decision::Allow(ApprovalScope::Once),
        Some("accept_for_session") => Decision::Allow(ApprovalScope::WorkspaceSession),
        Some("accept_for_project") => Decision::Allow(ApprovalScope::Project),
        Some("decline") => Decision::Deny,
        _ => Decision::Stop,
    }
}

fn approval_scope_name(scope: ApprovalScope) -> &'static str {
    match scope {
        ApprovalScope::Once => "once",
        ApprovalScope::WorkspaceSession => "workspace_session",
        ApprovalScope::Project => "project",
    }
}

impl Approver for ThreadUi {
    fn confirm(
        &self,
        req: ConfirmRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Decision> + Send + '_>> {
        // Server reverse-request ids live in the server's own integer counter
        // space (§ wire): a no-method response always answers one of ours.
        let id = RequestId::Num(self.reverse_request_seq.fetch_add(1, Ordering::SeqCst) as i64);
        let (reply, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(id.clone(), PendingInteraction::Approval(reply));
        // The kind comes from core, which knows what it built. Reading it off
        // the preview's presence instead (what this did until plan 194) made
        // every plan approval arrive as a file change.
        let kind = match &req.preview {
            Some(ConfirmPreview::FileChange(_)) => "file_change",
            Some(ConfirmPreview::Plan(_)) => "plan",
            None => "command",
        };
        let approval_scopes = req
            .approval_scopes
            .iter()
            .copied()
            .map(approval_scope_name)
            .collect::<Vec<_>>();
        let mut params = json!({
            "turn_id": self.turn_id(),
            "kind": kind,
            "description": req.description,
            "approval_scopes": approval_scopes,
        });
        if let Some(rules) = &req.remember_rules {
            params["remember_rules"] = json!(rules);
        }
        if let Some(preview) = &req.preview {
            params["preview"] = Value::String(preview.text().to_string());
        }
        params["thread_id"] = Value::String(self.thread_id.clone());
        let sent = self
            .out
            .send(
                Outgoing::ServerRequest {
                    id: id.clone(),
                    method: "approval/request",
                    params,
                }
                .into_bytes(),
            )
            .is_ok();
        let guard = PendingGuard {
            id,
            pending: self.pending.clone(),
        };
        // A client that is gone cannot answer, and one that never got the
        // question cannot either: both stop, like any reply it did not give.
        let shutdown = self.shutdown.clone();
        Box::pin(async move {
            let _guard = guard;
            if !sent {
                return Decision::Stop;
            }
            tokio::select! {
                decision = rx => decision.unwrap_or(Decision::Stop),
                _ = shutdown.cancelled() => Decision::Stop,
            }
        })
    }
}

impl Questioner for ThreadUi {
    fn ask(
        &self,
        request: QuestionRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = QuestionOutcome> + Send + '_>> {
        let shutdown = self.shutdown.clone();
        Box::pin(async move {
            let mut answers = Vec::with_capacity(request.questions.len());
            for (question_index, question) in request.questions.iter().enumerate() {
                let Some(turn_id) = self.active_turn_id() else {
                    return QuestionOutcome::Unavailable(
                        "question requested outside an active turn".into(),
                    );
                };
                let id =
                    RequestId::Num(self.reverse_request_seq.fetch_add(1, Ordering::SeqCst) as i64);
                let (reply, rx) = oneshot::channel();
                self.pending.lock().unwrap().insert(
                    id.clone(),
                    PendingInteraction::Question {
                        question_index,
                        reply,
                    },
                );
                let guard = PendingGuard {
                    id: id.clone(),
                    pending: self.pending.clone(),
                };
                let params = json!({
                    "thread_id": self.thread_id,
                    "turn_id": turn_id,
                    "question_index": question_index,
                    "question": question,
                });
                if self
                    .out
                    .send(
                        Outgoing::ServerRequest {
                            id,
                            method: "question/request",
                            params,
                        }
                        .into_bytes(),
                    )
                    .is_err()
                {
                    return QuestionOutcome::Unavailable(
                        "server output channel closed while asking a question".into(),
                    );
                }
                let outcome = tokio::select! {
                    outcome = rx => outcome.unwrap_or_else(|_| {
                        QuestionOutcome::Unavailable(
                            "question response channel was dropped".into(),
                        )
                    }),
                    _ = shutdown.cancelled() => {
                        return QuestionOutcome::Unavailable(
                            "the server stopped reading before the question was answered"
                                .into(),
                        );
                    }
                };
                drop(guard);
                match outcome {
                    QuestionOutcome::Answered(mut one) if one.len() == 1 => {
                        answers.push(one.remove(0));
                    }
                    QuestionOutcome::Answered(_) => {
                        return QuestionOutcome::Unavailable(
                            "question response contained the wrong answer count".into(),
                        );
                    }
                    QuestionOutcome::Cancelled => return QuestionOutcome::Cancelled,
                    QuestionOutcome::Unavailable(error) => {
                        return QuestionOutcome::Unavailable(error);
                    }
                }
            }
            match request.validate_answers(&answers) {
                Ok(()) => QuestionOutcome::Answered(answers),
                Err(error) => QuestionOutcome::Unavailable(format!(
                    "client returned an invalid question answer: {error}"
                )),
            }
        })
    }
}

#[cfg(test)]
mod approval_response_tests {
    use super::*;

    /// Input is gone. A client cannot answer an approval asked after that,
    /// and waiting would hold the turn (and the process) open; the wait ends
    /// as a Stop, the same answer as a dropped reply.
    #[tokio::test]
    async fn shutdown_releases_an_approval_without_a_client() {
        use kloop_protocol::ActiveProviderRoute;
        use kloop_protocol::ProviderApiFamily;
        use kloop_protocol::ReasoningContinuity;

        let (out, mut lines) = tokio::sync::mpsc::unbounded_channel();
        let projection = Arc::new(
            ThreadProjection::new(
                "shutdown".into(),
                "/".into(),
                ActiveProviderRoute {
                    revision: 1,
                    provider_id: "mock".into(),
                    api_family: ProviderApiFamily::Mock,
                    model: "mock".into(),
                    continuity: ReasoningContinuity::Preserved,
                    effort: None,
                },
                kloop_core::rollout::SessionSnapshot {
                    messages: Vec::new(),
                    runtime: None,
                    terminals: Vec::new(),
                    provider_routes: Vec::new(),
                },
                crate::events::RecoverySource::Fresh,
            )
            .unwrap(),
        );
        let shutdown = CancellationToken::new();
        let ui = ThreadUi {
            thread_id: "shutdown".into(),
            out,
            pending: Default::default(),
            reverse_request_seq: Arc::new(AtomicU64::new(1)),
            turn: Arc::new(Mutex::new(Some(1))),
            projection,
            shutdown: shutdown.clone(),
        };
        let pending = ui.confirm(ConfirmRequest {
            description: "write the file".into(),
            title: None,
            detail: None,
            notice: None,
            approval_scopes: vec![ApprovalScope::Once],
            remember_rules: None,
            preview: None,
        });
        // The request went out; then the read loop ends.
        let _ = lines.recv().await;
        shutdown.cancel();
        assert_eq!(pending.await, Decision::Stop);
    }

    #[test]
    fn scoped_approval_tokens_map_exactly_and_legacy_fails_closed() {
        assert_eq!(
            approval_decision(&json!({"decision": "accept"})),
            Decision::Allow(ApprovalScope::Once)
        );
        assert_eq!(
            approval_decision(&json!({"decision": "accept_for_session"})),
            Decision::Allow(ApprovalScope::WorkspaceSession)
        );
        assert_eq!(
            approval_decision(&json!({"decision": "accept_for_project"})),
            Decision::Allow(ApprovalScope::Project)
        );
        // decline is codex's "not this, carry on"; everything else refuses AND
        // stops the turn (plan 216) — cancel by meaning, the rest fail closed.
        assert_eq!(
            approval_decision(&json!({"decision": "decline"})),
            Decision::Deny
        );
        for result in [
            json!({"decision": "cancel"}),
            json!({"decision": "accept_always"}),
            json!({"decision": "unknown"}),
            json!({"decision": 1}),
            json!({}),
        ] {
            assert_eq!(approval_decision(&result), Decision::Stop, "{result}");
        }
    }
}
