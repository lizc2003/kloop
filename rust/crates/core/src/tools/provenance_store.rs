use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::Result;
use anyhow::anyhow;

use super::run_store::RunDir;
use crate::execution_provenance::ExecutionKind;
use crate::execution_provenance::ExecutionProvenanceReceipt;
use crate::execution_provenance::MAX_PROVENANCE_HISTORY_BYTES;
use crate::execution_provenance::ProvenanceHistoryUpdate;
use crate::execution_provenance::persisted_attempt_sequences;
use crate::execution_provenance::update_provenance_history;

pub(super) fn reserve_attempt_sequence(
    run_dir: &RunDir,
    kind: ExecutionKind,
    sequence: &AtomicU64,
) -> Result<u64> {
    let used = run_dir
        .read_optional_bounded("provenance.json", MAX_PROVENANCE_HISTORY_BYTES)
        .ok()
        .and_then(|existing| persisted_attempt_sequences(existing.as_deref(), kind))
        .unwrap_or_default();
    reserve_sequence(sequence, &used)
}

fn reserve_sequence(sequence: &AtomicU64, used: &[u64]) -> Result<u64> {
    loop {
        let candidate = sequence
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1).filter(|_| current > 0)
            })
            .map_err(|_| anyhow!("execution sequence exhausted"))?;
        if !used.contains(&candidate) {
            return Ok(candidate);
        }
    }
}

pub(super) fn record_attempt(run_dir: &RunDir, receipt: &ExecutionProvenanceReceipt) {
    let existing =
        match run_dir.read_optional_bounded("provenance.json", MAX_PROVENANCE_HISTORY_BYTES) {
            Ok(existing) => existing,
            Err(_) => return,
        };
    if let ProvenanceHistoryUpdate::Write(bytes) =
        update_provenance_history(existing.as_deref(), receipt)
    {
        let _ = run_dir.write_atomic("provenance.json", &bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restarted_sequence_skips_ids_already_in_durable_history() {
        let sequence = AtomicU64::new(1);
        assert_eq!(reserve_sequence(&sequence, &[1, 3]).unwrap(), 2);
        assert_eq!(reserve_sequence(&sequence, &[1, 3]).unwrap(), 4);
    }

    #[test]
    fn exhausted_sequence_fails_without_reusing_an_id() {
        let sequence = AtomicU64::new(u64::MAX);
        assert!(reserve_sequence(&sequence, &[]).is_err());
    }
}
