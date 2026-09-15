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
use std::sync::Mutex;
#[cfg(test)]
use std::sync::atomic::AtomicU64;
#[cfg(test)]
use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use super::super::run_store::RunDir;
use crate::execution_provenance::ExecutionProvenanceReceipt;

#[cfg(test)]
static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

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
}

/// One run's agent journal, persisted whenever a live call completes.
pub struct Journal {
    /// Only current-version entries are eligible for replay. Versions 1 and 2,
    /// and unknown future versions, are deliberately cache misses.
    old: HashMap<String, Entry>,
    /// Hits carried over plus fresh records, persisted on every change so a
    /// mid-run failure leaves a resumable journal.
    written: Mutex<HashMap<String, Entry>>,
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
        let raw = match &storage {
            #[cfg(test)]
            Storage::Path(path) => std::fs::read_to_string(path).ok(),
            Storage::Run(run_dir, name) => run_dir
                .read(name)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok()),
        };
        let old: HashMap<String, Entry> = raw
            .map(|raw| {
                raw.lines()
                    .filter_map(|line| serde_json::from_str::<Entry>(line).ok())
                    .filter(|entry| entry.version == JOURNAL_VERSION)
                    .map(|mut entry| {
                        entry.provenance = entry.provenance.take().and_then(|value| {
                            ExecutionProvenanceReceipt::from_persisted_value(&value)
                                .ok()
                                .filter(|receipt| {
                                    receipt.is_agent_journal_evidence(
                                        expected_parent_durable.as_deref(),
                                    )
                                })
                                .map(|receipt| receipt.to_persisted_value())
                        });
                        (entry.call_id.clone(), entry)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            written: Mutex::new(old.clone()),
            old,
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

    /// Record a successful live call with its admitted child receipt.
    pub fn record(
        &self,
        call_id: String,
        prompt: String,
        opts: Value,
        result: Value,
        receipt: &ExecutionProvenanceReceipt,
    ) {
        let mut written = self.written.lock().unwrap();
        written.insert(
            call_id.clone(),
            Entry {
                version: JOURNAL_VERSION,
                call_id,
                input: AgentInput { prompt, opts },
                result,
                provenance: receipt
                    .is_agent_journal_evidence(self.storage.expected_parent_durable())
                    .then(|| receipt.to_persisted_value()),
            },
        );
        persist(&self.storage, &written);
    }

    /// Whether this run has any reusable current-version agent result.
    pub fn is_active(&self) -> bool {
        !self.written.lock().unwrap().is_empty()
    }
}

/// Rewrite current-version entries in stable topology order. Persistence is
/// best-effort: losing resume data must not fail the live Program/Workflow.
fn persist(storage: &Storage, written: &HashMap<String, Entry>) {
    let mut entries: Vec<&Entry> = written.values().collect();
    entries.sort_by(|left, right| left.call_id.cmp(&right.call_id));
    let mut out = String::new();
    for entry in entries {
        if let Ok(line) = serde_json::to_string(entry) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    match storage {
        #[cfg(test)]
        Storage::Path(path) => persist_path(path, &out),
        Storage::Run(run_dir, name) => {
            let _ = run_dir.write_atomic(name, out.as_bytes());
        }
    }
}

#[cfg(test)]
fn persist_path(path: &PathBuf, out: &str) {
    let Some(dir) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(".journal.tmp-{}-{seq}", std::process::id()));
    let write = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .and_then(|mut file| {
            use std::io::Write as _;
            file.write_all(out.as_bytes())?;
            file.sync_all()
        });
    if write.is_ok() {
        let _ = std::fs::rename(&temp, path);
    }
    let _ = std::fs::remove_file(temp);
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
        Journal::record(journal, call_id, prompt, opts, result, &receipt);
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
        std::fs::write(
            &path,
            format!("{missing}\n{malformed}\n{oversized}\n{non_agent}\n"),
        )
        .unwrap();
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
        let entries: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(entries.len(), 5);
        assert!(
            entries[..4]
                .iter()
                .all(|entry| entry["provenance"].is_null())
        );
        assert_eq!(entries[4]["provenance"]["execution"]["kind"], "agent");
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
        run_dir
            .write_atomic(
                "journal.jsonl",
                format!("{}\n", serde_json::to_string(&entry).unwrap()).as_bytes(),
            )
            .unwrap();

        let journal = Journal::open_run(run_dir.clone(), "journal.jsonl");
        assert!(matches!(
            journal.claim("root/agent/0", "copied", &json!({})),
            Claim::Hit(value) if value == "cached"
        ));
        let right = ExecutionProvenanceReceipt::test_journal_agent("agent-2", &run_id_text);
        journal.record(
            "root/agent/1".into(),
            "live".into(),
            json!({}),
            json!("fresh"),
            &right,
        );

        let entries: Vec<Value> = String::from_utf8(run_dir.read("journal.jsonl").unwrap())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(entries.len(), 2);
        assert!(entries[0]["provenance"].is_null());
        assert_eq!(
            entries[1]["provenance"]["parent"]["durable"]["id"],
            run_id_text
        );
        drop(journal);
        drop(run_dir);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn out_of_order_completions_persist_in_topology_order() {
        let path = tmp("order-v3");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let journal = Journal::open(path.clone());
        for id in [
            "root/parallel/0/branch/2/agent/0",
            "root/parallel/0/branch/0/agent/0",
            "root/parallel/0/branch/1/agent/0",
        ] {
            record(&journal, id.into(), id.into(), json!({}), id.into());
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        let ids: Vec<String> = raw
            .lines()
            .map(|line| serde_json::from_str::<Entry>(line).unwrap().call_id)
            .collect();
        assert_eq!(
            ids,
            vec![
                "root/parallel/0/branch/0/agent/0",
                "root/parallel/0/branch/1/agent/0",
                "root/parallel/0/branch/2/agent/0"
            ]
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
