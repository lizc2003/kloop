//! One admission sequence for `agent()` calls, shared by `run_program` and
//! `workflow`.
//!
//! Both bridges used to spell the same five steps themselves — claim the journal
//! entry, charge the total-call cap, wait for a live slot, re-check cancellation,
//! record the result — and had already drifted in how a journal miss was counted
//! and which options they copied. The journal and the caps are the same facts, so
//! they live in one place; each bridge keeps only its own option mapping and the
//! wording of its errors.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde_json::Value;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::codemode::journal::Claim;
use super::codemode::journal::Journal;
use crate::execution_provenance::ExecutionProvenanceReceipt;

pub(super) struct AgentCallGate {
    journal: Option<Arc<Journal>>,
    count: AtomicU64,
    max: u64,
    slots: Arc<Semaphore>,
}

/// A journal hit already has its result; a live call needs a slot permit that
/// the caller holds until the child settles.
pub(super) enum Admission {
    Hit(Value),
    Live(OwnedSemaphorePermit),
}

pub(super) struct AgentCall {
    claim: Option<Claim>,
    counted: Option<u64>,
}

impl AgentCallGate {
    pub(super) fn new(
        journal: Option<Arc<Journal>>,
        max_agents: u64,
        max_concurrency: usize,
    ) -> Self {
        Self {
            journal,
            count: AtomicU64::new(0),
            max: max_agents,
            slots: Arc::new(Semaphore::new(max_concurrency)),
        }
    }

    /// Claim the journal entry and charge the cap synchronously, before any
    /// await: concurrent misses get distinct counts from the atomic increment,
    /// and a hit is neither charged nor made to wait for a slot.
    pub(super) fn begin(&self, call_id: &str, prompt: &str, opts: &Value) -> AgentCall {
        match self
            .journal
            .as_ref()
            .map(|journal| journal.claim(call_id, prompt, opts))
        {
            Some(Claim::Hit(value)) => AgentCall {
                claim: Some(Claim::Hit(value)),
                counted: None,
            },
            Some(Claim::Miss) | None => AgentCall {
                claim: None,
                counted: Some(self.count.fetch_add(1, Ordering::Relaxed)),
            },
        }
    }

    /// Wait for a live slot, observing cancellation both before and during the
    /// wait. A stopped Program/Workflow must not start new children.
    pub(super) async fn admit(
        &self,
        mut call: AgentCall,
        cancel: &CancellationToken,
        interrupted: &str,
        cap_error: impl FnOnce(u64) -> String,
    ) -> Result<Admission, String> {
        if cancel.is_cancelled() {
            return Err(interrupted.to_string());
        }
        if let Some(Claim::Hit(value)) = call.claim.take() {
            return Ok(Admission::Hit(value));
        }
        if call.counted.is_some_and(|count| count >= self.max) {
            return Err(cap_error(self.max));
        }
        let permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(format!("{interrupted} while waiting for an agent slot"));
            }
            permit = self.slots.clone().acquire_owned() => permit.map_err(|_| {
                let owner = interrupted.split_whitespace().next().unwrap_or("agent");
                format!("{owner} agent concurrency limiter closed unexpectedly")
            })?,
        };
        if cancel.is_cancelled() {
            return Err(interrupted.to_string());
        }
        Ok(Admission::Live(permit))
    }

    /// Persist a successful live call when this run has a journal.
    pub(super) fn record(
        &self,
        call_id: String,
        prompt: String,
        opts: Value,
        result: Value,
        receipt: &Arc<ExecutionProvenanceReceipt>,
    ) -> Option<String> {
        self.journal
            .as_ref()?
            .record(call_id, prompt, opts, result, receipt)
    }
}
