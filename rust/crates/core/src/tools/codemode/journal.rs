//! Topology-addressed journal for code-mode and Workflow `agent()` calls.
//!
//! Every call carries a stable identity assigned by the JavaScript orchestration
//! topology (root ordinal, or helper/item/stage/branch path). A replay is a hit
//! only when that identity and the complete structured prompt/options input both
//! match. This is best-effort memoization of an expensive model call, not an
//! exactly-once guarantee for workspace or other external state.

use std::collections::HashMap;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Result;
#[cfg(test)]
use anyhow::anyhow;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use super::super::run_store::RunDir;
use crate::execution_provenance::ExecutionProvenanceReceipt;

const JOURNAL_VERSION: u8 = 3;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct AgentInput {
    prompt: String,
    opts: Value,
}

impl AgentInput {
    fn new(prompt: &str, opts: &Value) -> Self {
        Self {
            prompt: prompt.to_string(),
            opts: opts.clone(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    version: u8,
    call_id: String,
    input: AgentInput,
    result: Value,
    #[serde(default)]
    provenance: Option<Value>,
}

pub enum Claim {
    /// The prior run had this exact topology identity and complete input.
    Hit(Value),
    /// A new, moved, or changed call: run it live, then record the result.
    Miss,
}

enum Storage {
    #[cfg(test)]
    Path(PathBuf),
    Run(RunDir, &'static str),
}

impl Storage {
    fn expected_parent_durable(&self) -> Option<&str> {
        match self {
            #[cfg(test)]
            Self::Path(_) => None,
            Self::Run(run_dir, _) => Some(run_dir.id().as_str()),
        }
    }

    fn append(&self, entry: &Entry) -> Result<()> {
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        match self {
            #[cfg(test)]
            Self::Path(path) => append_path(path, line.as_bytes()),
            Self::Run(run_dir, name) => run_dir.append(name, line.as_bytes()),
        }
    }

    /// The journal bytes as they are on disk, or `None` when there is no file.
    fn read(&self) -> Option<Vec<u8>> {
        match self {
            #[cfg(test)]
            Self::Path(path) => std::fs::read(path).ok(),
            Self::Run(run_dir, name) => run_dir.read(name).ok(),
        }
    }

    fn truncate(&self, len: u64) -> Result<()> {
        match self {
            #[cfg(test)]
            Self::Path(path) => truncate_path(path, len),
            Self::Run(run_dir, name) => run_dir.truncate(name, len),
        }
    }
}

/// One run's agent journal. Every live call appends one JSONL line, so a
/// mid-run failure leaves a resumable journal and the cost is linear: rewriting
/// every entry and fsyncing twice per completed call was quadratic, and an
/// over-limit rewrite used to fail silently (plan 257).
pub struct Journal {
    /// Entries loaded at open. Only current-version entries are eligible for
    /// replay; versions 1 and 2, and unknown future versions, are cache misses.
    old: HashMap<String, Entry>,
    /// Reusable entries known to be on disk: what resumed runs can actually skip.
    persisted: AtomicUsize,
    /// Set after the first failed append so the warning is published only once.
    write_failed: AtomicBool,
    storage: Storage,
}

impl Journal {
    /// Open a path-backed journal for unit tests.
    #[cfg(test)]
    pub fn open(path: PathBuf) -> Self {
        Self::from_storage(Storage::Path(path))
    }

    /// Open a journal inside an already verified run directory.
    pub(in crate::tools) fn open_run(run_dir: RunDir, name: &'static str) -> Self {
        Self::from_storage(Storage::Run(run_dir, name))
    }

    fn from_storage(storage: Storage) -> Self {
        let expected_parent_durable = storage.expected_parent_durable().map(str::to_string);
        // A killed append can leave an unterminated tail: cut it before parsing.
        // Parsing would only skip it, and the next append would then land on that
        // same line and be lost with it. Lines are decoded one at a time, so a
        // tail cut inside a multi-byte character cannot take the whole file with
        // it.
        let bytes = storage.read().unwrap_or_default();
        let complete = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |last| last + 1);
        if complete != bytes.len() {
            let _ = storage.truncate(complete as u64);
        }
        let old: HashMap<String, Entry> = bytes[..complete]
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .filter_map(|line| std::str::from_utf8(line).ok())
            .filter_map(|line| serde_json::from_str::<Entry>(line).ok())
            .filter(|entry| entry.version == JOURNAL_VERSION)
            .map(|mut entry| {
                entry.provenance = entry.provenance.take().and_then(|value| {
                    ExecutionProvenanceReceipt::from_persisted_value(&value)
                        .ok()
                        .filter(|receipt| {
                            receipt.is_agent_journal_evidence(expected_parent_durable.as_deref())
                        })
                        .map(|receipt| receipt.to_persisted_value())
                });
                (entry.call_id.clone(), entry)
            })
            .collect();
        let persisted = old.len();
        Self {
            old,
            persisted: AtomicUsize::new(persisted),
            write_failed: AtomicBool::new(false),
            storage,
        }
    }

    /// Look up one topology-addressed call. Both identity and complete input
    /// must match; a moved or changed call runs live.
    pub fn claim(&self, call_id: &str, prompt: &str, opts: &Value) -> Claim {
        let input = AgentInput::new(prompt, opts);
        match self.old.get(call_id) {
            Some(entry) if entry.input == input => Claim::Hit(entry.result.clone()),
            _ => Claim::Miss,
        }
    }

    /// Record a successful live call with its admitted child receipt. A failed
    /// append returns a warning the first time: the live Program/Workflow keeps
    /// running, but the calls from here on cannot be replayed on resume.
    pub fn record(
        &self,
        call_id: String,
        prompt: String,
        opts: Value,
        result: Value,
        receipt: &ExecutionProvenanceReceipt,
    ) -> Option<String> {
        let entry = Entry {
            version: JOURNAL_VERSION,
            call_id,
            input: AgentInput { prompt, opts },
            result,
            provenance: receipt
                .is_agent_journal_evidence(self.storage.expected_parent_durable())
                .then(|| receipt.to_persisted_value()),
        };
        match self.storage.append(&entry) {
            Ok(()) => {
                self.persisted.fetch_add(1, Ordering::Relaxed);
                None
            }
            Err(error) => {
                if self.write_failed.swap(true, Ordering::Relaxed) {
                    return None;
                }
                Some(format!(
                    "agent() journal stopped persisting ({error:#}); completed calls from here on \
                     cannot be replayed if this run has to be resumed"
                ))
            }
        }
    }

    /// Whether resume can actually skip a completed call — entries still in
    /// memory but never written do not count.
    pub fn is_active(&self) -> bool {
        self.persisted.load(Ordering::Relaxed) > 0
    }
}

#[cfg(test)]
fn append_path(path: &PathBuf, bytes: &[u8]) -> Result<()> {
    let Some(dir) = path.parent() else {
        return Err(anyhow!("journal path has no parent directory"));
    };
    std::fs::create_dir_all(dir)?;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    use std::io::Write as _;
    file.write_all(bytes)?;
    file.sync_data()?;
    Ok(())
}

#[cfg(test)]
fn truncate_path(path: &PathBuf, len: u64) -> Result<()> {
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.set_len(len)?;
    file.sync_data()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::run_store::RunId;
    use crate::tools::run_store::RunNamespace;
    use crate::tools::run_store::RunStore;
    use serde_json::json;

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("kloop-journal-{}-{tag}", std::process::id()))
            .join("journal.jsonl")
    }

    fn record(journal: &Journal, call_id: String, prompt: String, opts: Value, result: Value) {
        let receipt = ExecutionProvenanceReceipt::test_journal_agent("agent-1", "run-1-1");
        assert!(
            Journal::record(journal, call_id, prompt, opts, result, &receipt).is_none(),
            "path-backed journal append should succeed"
        );
    }

    #[test]
    fn fresh_journal_misses_and_persists_structured_input() {
        let path = tmp("fresh-v3");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let journal = Journal::open(path.clone());
        assert!(matches!(
            journal.claim("root/agent/0", "do X", &json!({})),
            Claim::Miss
        ));
        record(
            &journal,
            "root/agent/0".into(),
            "do X".into(),
            json!({"label": "x"}),
            "result".into(),
        );
        let raw = std::fs::read_to_string(&path).unwrap();
        let entry: Entry = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(entry.version, JOURNAL_VERSION);
        assert_eq!(entry.call_id, "root/agent/0");
        assert_eq!(entry.input.prompt, "do X");
        assert_eq!(entry.input.opts, json!({"label": "x"}));
        assert!(entry.provenance.is_some());
        let reopened = Journal::open(path.clone());
        assert!(matches!(
            reopened.claim("root/agent/0", "do X", &json!({"label": "x"})),
            Claim::Hit(value) if value == "result"
        ));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn replay_requires_call_id_and_complete_input() {
        let path = tmp("matching-v3");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        {
            let journal = Journal::open(path.clone());
            record(
                &journal,
                "root/agent/0".into(),
                "first".into(),
                json!({"agent_type": "researcher", "label": "scan"}),
                "R0".into(),
            );
        }
        let journal = Journal::open(path.clone());
        assert!(matches!(
            journal.claim(
                "root/agent/0",
                "first",
                &json!({"label": "scan", "agent_type": "researcher"})
            ),
            Claim::Hit(value) if value == "R0"
        ));
        assert!(matches!(
            journal.claim(
                "root/agent/1",
                "first",
                &json!({"agent_type": "researcher", "label": "scan"})
            ),
            Claim::Miss
        ));
        assert!(matches!(
            journal.claim(
                "root/agent/0",
                "changed",
                &json!({"agent_type": "researcher", "label": "scan"})
            ),
            Claim::Miss
        ));
        assert!(matches!(
            journal.claim(
                "root/agent/0",
                "first",
                &json!({"agent_type": "researcher", "label": "changed"})
            ),
            Claim::Miss
        ));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn structured_input_has_no_delimiter_collisions() {
        let path = tmp("nul-v3");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        {
            let journal = Journal::open(path.clone());
            record(
                &journal,
                "root/agent/0".into(),
                "a\0label=\"b\"".into(),
                json!({}),
                "cached".into(),
            );
        }
        let journal = Journal::open(path.clone());
        assert!(matches!(
            journal.claim("root/agent/0", "a", &json!({"label": "b"})),
            Claim::Miss
        ));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn duplicate_inputs_at_distinct_topology_ids_remain_independent() {
        let path = tmp("duplicate-v3");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        {
            let journal = Journal::open(path.clone());
            record(
                &journal,
                "root/parallel/0/branch/0/agent/0".into(),
                "same".into(),
                json!({}),
                "left".into(),
            );
            record(
                &journal,
                "root/parallel/0/branch/1/agent/0".into(),
                "same".into(),
                json!({}),
                "right".into(),
            );
        }
        let journal = Journal::open(path.clone());
        assert!(matches!(
            journal.claim("root/parallel/0/branch/0/agent/0", "same", &json!({})),
            Claim::Hit(value) if value == "left"
        ));
        assert!(matches!(
            journal.claim("root/parallel/0/branch/1/agent/0", "same", &json!({})),
            Claim::Hit(value) if value == "right"
        ));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn old_and_future_versions_are_safe_cache_misses() {
        let path = tmp("versions-v3");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let legacy = json!({"version": 1, "seq": 0, "key": "legacy", "result": "old"});
        let version_two = json!({
            "version": 2,
            "call_id": "root/agent/1",
            "input": {"prompt": "v2", "opts": {}},
            "result": "v2"
        });
        let future = json!({
            "version": 4,
            "call_id": "root/agent/2",
            "input": {"prompt": "future", "opts": {}},
            "result": "future"
        });
        std::fs::write(&path, format!("{legacy}\n{version_two}\n{future}\n")).unwrap();
        let journal = Journal::open(path.clone());
        assert!(matches!(
            journal.claim("root/agent/0", "legacy", &json!({})),
            Claim::Miss
        ));
        assert!(matches!(
            journal.claim("root/agent/1", "v2", &json!({})),
            Claim::Miss
        ));
        assert!(matches!(
            journal.claim("root/agent/2", "future", &json!({})),
            Claim::Miss
        ));
        assert!(!journal.is_active());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn malformed_missing_oversized_or_wrong_kind_v3_receipt_keeps_hit_without_rewrite() {
        let path = tmp("v3-unavailable-provenance");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let missing = json!({
            "version": 3,
            "call_id": "root/agent/0",
            "input": {"prompt": "missing", "opts": {}},
            "result": "cached-missing"
        });
        let malformed = json!({
            "version": 3,
            "call_id": "root/agent/1",
            "input": {"prompt": "malformed", "opts": {}},
            "result": "cached-malformed",
            "provenance": {"execution": "not-a-receipt"}
        });
        let oversized = json!({
            "version": 3,
            "call_id": "root/agent/2",
            "input": {"prompt": "oversized", "opts": {}},
            "result": "cached-oversized",
            "provenance": {"padding": "x".repeat(crate::execution_provenance::MAX_RECEIPT_BYTES)}
        });
        let non_agent = json!({
            "version": 3,
            "call_id": "root/agent/3",
            "input": {"prompt": "wrong-kind", "opts": {}},
            "result": "cached-wrong-kind",
            "provenance": ExecutionProvenanceReceipt::test_program("program-1", "run-1-1")
                .to_persisted_value()
        });
        let seeded = format!("{missing}\n{malformed}\n{oversized}\n{non_agent}\n");
        std::fs::write(&path, &seeded).unwrap();
        let journal = Journal::open(path.clone());
        assert!(matches!(
            journal.claim("root/agent/0", "missing", &json!({})),
            Claim::Hit(value) if value == "cached-missing"
        ));
        assert!(matches!(
            journal.claim("root/agent/1", "malformed", &json!({})),
            Claim::Hit(value) if value == "cached-malformed"
        ));
        assert!(matches!(
            journal.claim("root/agent/2", "oversized", &json!({})),
            Claim::Hit(value) if value == "cached-oversized"
        ));

        assert!(matches!(
            journal.claim("root/agent/3", "wrong-kind", &json!({})),
            Claim::Hit(value) if value == "cached-wrong-kind"
        ));

        record(
            &journal,
            "root/agent/4".into(),
            "live".into(),
            json!({}),
            "fresh".into(),
        );
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 5);
        // Append-only: the seeded lines are left byte-for-byte alone. An unusable
        // receipt is ignored when the journal is opened, not scrubbed on disk.
        assert!(raw.starts_with(&seeded), "{raw}");
        let appended: Value = serde_json::from_str(raw.lines().last().unwrap()).unwrap();
        assert_eq!(appended["provenance"]["execution"]["kind"], "agent");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn run_backed_journal_strips_receipt_from_another_durable_run() {
        let root =
            std::env::temp_dir().join(format!("kloop-journal-run-binding-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = RunStore::new(&root.join("offload"), RunNamespace::Program).unwrap();
        let run_id_text = format!("run-{}-91", std::process::id());
        let run_id = RunId::parse(&run_id_text).unwrap();
        let run_dir = store.create(&run_id).unwrap();
        let wrong = ExecutionProvenanceReceipt::test_journal_agent("agent-1", "run-1-1");
        let entry = Entry {
            version: JOURNAL_VERSION,
            call_id: "root/agent/0".into(),
            input: AgentInput::new("copied", &json!({})),
            result: json!("cached"),
            provenance: Some(wrong.to_persisted_value()),
        };
        let seeded = format!("{}\n", serde_json::to_string(&entry).unwrap());
        run_dir
            .write_atomic("journal.jsonl", seeded.as_bytes())
            .unwrap();

        let journal = Journal::open_run(run_dir.clone(), "journal.jsonl");
        assert!(matches!(
            journal.claim("root/agent/0", "copied", &json!({})),
            Claim::Hit(value) if value == "cached"
        ));
        let right = ExecutionProvenanceReceipt::test_journal_agent("agent-2", &run_id_text);
        assert!(
            journal
                .record(
                    "root/agent/1".into(),
                    "live".into(),
                    json!({}),
                    json!("fresh"),
                    &right,
                )
                .is_none(),
            "run-backed journal append should succeed"
        );

        let raw = String::from_utf8(run_dir.read("journal.jsonl").unwrap()).unwrap();
        // The foreign receipt stays as written; it is not trusted on open, and an
        // append never rewrites what is already on disk.
        assert!(raw.starts_with(&seeded), "{raw}");
        let appended: Value = serde_json::from_str(raw.lines().last().unwrap()).unwrap();
        assert_eq!(
            appended["provenance"]["parent"]["durable"]["id"],
            run_id_text
        );
        drop(journal);
        drop(run_dir);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn records_append_instead_of_rewriting_and_later_wins_on_replay() {
        let path = tmp("append-v3");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let journal = Journal::open(path.clone());
        record(
            &journal,
            "root/agent/0".into(),
            "first".into(),
            json!({}),
            "R0".into(),
        );
        let first = std::fs::read_to_string(&path).unwrap();
        record(
            &journal,
            "root/agent/1".into(),
            "second".into(),
            json!({}),
            "R1".into(),
        );
        let both = std::fs::read_to_string(&path).unwrap();
        // Append-only: the earlier bytes are still a prefix of the file.
        assert!(both.starts_with(&first), "{both}");
        // A rewritten entry for the same topology id is appended too; the last
        // line is the one resume sees.
        record(
            &journal,
            "root/agent/0".into(),
            "first".into(),
            json!({}),
            "R0-v2".into(),
        );
        let reopened = Journal::open(path.clone());
        assert!(matches!(
            reopened.claim("root/agent/0", "first", &json!({})),
            Claim::Hit(value) if value == "R0-v2"
        ));
        assert!(matches!(
            reopened.claim("root/agent/1", "second", &json!({})),
            Claim::Hit(value) if value == "R1"
        ));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A killed append leaves an unterminated tail. It has to be cut, not just
    /// skipped: the next record would otherwise be appended to it and lost, and
    /// decoding the file as one string would discard every record before it when
    /// the cut landed inside a multi-byte character.
    #[test]
    fn an_unterminated_tail_is_cut_before_the_next_record() {
        let path = tmp("partial-tail-v3");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let complete = json!({
            "version": 3,
            "call_id": "root/agent/0",
            "input": {"prompt": "done", "opts": {}},
            "result": "R0"
        });
        let mut bytes = format!("{}\n", serde_json::to_string(&complete).unwrap()).into_bytes();
        bytes.extend_from_slice(br#"{"version":3,"call_id":"root/agent/1","input":{"prompt":"do"#);
        bytes.extend_from_slice(&[0xe4, 0xb8]); // half a UTF-8 sequence
        std::fs::write(&path, &bytes).unwrap();

        let journal = Journal::open(path.clone());
        assert!(journal.is_active());
        assert!(matches!(
            journal.claim("root/agent/0", "done", &json!({})),
            Claim::Hit(value) if value == "R0"
        ));
        record(
            &journal,
            "root/agent/1".into(),
            "redo".into(),
            json!({}),
            "R1".into(),
        );
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 2, "{raw}");

        let reopened = Journal::open(path.clone());
        assert!(matches!(
            reopened.claim("root/agent/1", "redo", &json!({})),
            Claim::Hit(value) if value == "R1"
        ));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_thousand_records_round_trip() {
        let path = tmp("many-v3");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        {
            let journal = Journal::open(path.clone());
            for index in 0..1000 {
                let id = format!("root/parallel/{index}/agent/0");
                record(&journal, id.clone(), id.clone(), json!({}), id.into());
            }
            assert!(journal.is_active());
        }
        let lines = std::fs::read_to_string(&path).unwrap();
        assert_eq!(lines.lines().count(), 1000);
        let reopened = Journal::open(path.clone());
        assert!(matches!(
            reopened.claim(
                "root/parallel/999/agent/0",
                "root/parallel/999/agent/0",
                &json!({})
            ),
            Claim::Hit(value) if value == "root/parallel/999/agent/0"
        ));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_unwritable_journal_warns_once_and_is_not_active() {
        let path = tmp("unwritable-v3");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        std::fs::create_dir_all(&path).unwrap();
        let journal = Journal::open(path.clone());
        let receipt = ExecutionProvenanceReceipt::test_journal_agent("agent-1", "run-1-1");
        let warning = Journal::record(
            &journal,
            "root/agent/0".into(),
            "do X".into(),
            json!({}),
            "result".into(),
            &receipt,
        )
        .expect("a failed append must be reported");
        assert!(warning.contains("stopped persisting"), "{warning}");
        // Nothing reached the disk, so resume would skip nothing.
        assert!(!journal.is_active());
        // The warning is published once, not once per call.
        assert!(
            Journal::record(
                &journal,
                "root/agent/1".into(),
                "do Y".into(),
                json!({}),
                "result".into(),
                &receipt,
            )
            .is_none()
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
