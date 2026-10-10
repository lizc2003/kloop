use anyhow::Result;

use super::run_store::RunDir;
use crate::execution_provenance::ExecutionKind;
use crate::execution_provenance::ExecutionProvenanceReceipt;
use crate::execution_provenance::MAX_PROVENANCE_HISTORY_BYTES;
use crate::execution_provenance::ProvenanceHistoryUpdate;
use crate::execution_provenance::persisted_attempt_ids;
use crate::execution_provenance::update_provenance_history;

pub(super) fn fresh_attempt_id(
    run_dir: &RunDir,
    kind: ExecutionKind,
    prefix: &str,
) -> Result<String> {
    let used = run_dir
        .read_optional_bounded("provenance.json", MAX_PROVENANCE_HISTORY_BYTES)
        .ok()
        .and_then(|existing| persisted_attempt_ids(existing.as_deref(), kind))
        .unwrap_or_default();
    loop {
        let id = crate::resource_id::fresh(prefix)?;
        if !used.contains(&id) {
            return Ok(id);
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
