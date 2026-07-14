//! Content-addressed journal for a code-mode program's `agent()` calls (plan
//! 24, journal resume). Each `agent()` call is keyed by its JS-assigned
//! sequence number plus a canonical string of its prompt+params. On resume, a
//! call whose `(seq, key)` matches the recorded run returns the cached result
//! and skips re-spawning the sub-agent — so a long program that failed partway
//! doesn't re-burn the tokens of the sub-agents that already completed. Only
//! `agent()` is journaled (it is the expensive call); plain tool calls are not.
//!
//! Unlike cc's prefix-replay (first divergence invalidates the rest), this
//! memoizes each `(seq, key)` independently: a call whose key still matches is
//! reused even if an earlier call diverged. That is safe because the key is the
//! full (prompt + params) of the call — a call whose inputs depend on an
//! upstream change has a changed prompt, so its key changes and it re-runs
//! anyway. The JS-assigned seq (monotonic, single-threaded) makes replay
//! independent of the order sub-agent futures happen to resolve in.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    seq: u32,
    key: String,
    result: String,
}

pub enum Claim {
    /// The recorded run had this exact `(seq, key)`: reuse its result, skip the
    /// spawn.
    Hit(String),
    /// A new or diverged call: run it live, then `record` the result.
    Miss,
}

/// One program run's agent journal, persisted to disk as calls resolve.
pub struct Journal {
    /// The prior run's calls by seq (empty on a fresh run).
    old: HashMap<u32, Entry>,
    /// This run's calls (hits carried over + fresh records), persisted on every
    /// change so a mid-run crash still leaves a resumable journal.
    written: Mutex<Vec<Entry>>,
    path: PathBuf,
}

impl Journal {
    /// Open the journal at `path`, loading the prior run's entries if the file
    /// exists (a fresh run starts with none).
    pub fn open(path: PathBuf) -> Self {
        let old = std::fs::read_to_string(&path)
            .ok()
            .map(|raw| {
                raw.lines()
                    .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
                    .map(|e| (e.seq, e))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            old,
            written: Mutex::new(Vec::new()),
            path,
        }
    }

    /// Look up the call at `seq` with content `key`. A prior-run entry with the
    /// same seq AND key is a hit — its result is carried into this run's journal
    /// and returned. Anything else is a miss: the caller runs it live and calls
    /// [`Journal::record`].
    pub fn claim(&self, seq: u32, key: &str) -> Claim {
        match self.old.get(&seq) {
            Some(e) if e.key == key => {
                let mut written = self.written.lock().unwrap();
                written.push(e.clone());
                persist(&self.path, &mut written);
                Claim::Hit(e.result.clone())
            }
            _ => Claim::Miss,
        }
    }

    /// Record a freshly-run call's result into this run's journal.
    pub fn record(&self, seq: u32, key: String, result: String) {
        let mut written = self.written.lock().unwrap();
        written.push(Entry { seq, key, result });
        persist(&self.path, &mut written);
    }

    /// Whether any agent() call has been journaled this run — i.e. whether a
    /// resume would have something to skip.
    pub fn is_active(&self) -> bool {
        !self.written.lock().unwrap().is_empty()
    }
}

/// Overwrite the journal file with this run's entries, seq-sorted so a resume
/// replays them in call order regardless of completion order. Best-effort: a
/// write failure just means this run isn't resumable, which must never break
/// the running program.
fn persist(path: &PathBuf, written: &mut [Entry]) {
    written.sort_by_key(|e| e.seq);
    let mut out = String::new();
    for e in written.iter() {
        if let Ok(line) = serde_json::to_string(e) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, out);
}

/// A hash-free canonical key for an `agent()` call: its prompt plus the params
/// that affect the result (`agent_type`, `max_rounds`), in a fixed order. Kept
/// as a plain string — the journal is a local file, so there is no need for a
/// crypto hash (and no new dependency). Distinct calls get distinct keys; the
/// same call across runs gets the same key.
pub fn agent_call_key(prompt: &str, opts: &Value) -> String {
    let mut key = format!("prompt={prompt}");
    for field in ["agent_type", "max_rounds"] {
        if let Some(v) = opts.get(field) {
            key.push('\u{0}');
            key.push_str(&format!("{field}={v}"));
        }
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("kloop-journal-{}-{tag}", std::process::id()))
            .join("journal.jsonl")
    }

    #[test]
    fn key_is_stable_and_distinguishes_prompt_and_params() {
        let a = agent_call_key("do X", &json!({}));
        assert_eq!(a, agent_call_key("do X", &json!({})), "stable");
        assert_ne!(a, agent_call_key("do Y", &json!({})), "prompt matters");
        assert_ne!(
            agent_call_key("do X", &json!({"agent_type": "researcher"})),
            a,
            "agent_type matters"
        );
        // Irrelevant fields don't change the key.
        assert_eq!(agent_call_key("do X", &json!({"label": "z"})), a);
    }

    #[test]
    fn fresh_journal_misses_everything() {
        let path = tmp("fresh");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let j = Journal::open(path.clone());
        assert!(matches!(j.claim(0, "k0"), Claim::Miss));
        j.record(0, "k0".into(), "r0".into());
        // The record landed on disk.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"result\":\"r0\""), "{raw}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn resume_hits_matching_seq_and_key_misses_on_divergence() {
        let path = tmp("resume");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        // First run records two calls.
        {
            let j = Journal::open(path.clone());
            j.record(0, agent_call_key("first", &json!({})), "R0".into());
            j.record(1, agent_call_key("second", &json!({})), "R1".into());
        }
        // Resume: same calls hit; a changed key at the same seq misses.
        let j = Journal::open(path.clone());
        match j.claim(0, &agent_call_key("first", &json!({}))) {
            Claim::Hit(r) => assert_eq!(r, "R0"),
            Claim::Miss => panic!("seq 0 should hit"),
        }
        assert!(
            matches!(
                j.claim(1, &agent_call_key("CHANGED", &json!({}))),
                Claim::Miss
            ),
            "a changed prompt at seq 1 must miss"
        );
        // A seq never recorded misses.
        assert!(matches!(j.claim(9, "k9"), Claim::Miss));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn same_key_at_different_seqs_are_independent() {
        let path = tmp("dup");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        {
            let j = Journal::open(path.clone());
            let k = agent_call_key("loop body", &json!({}));
            j.record(0, k.clone(), "iter-0".into());
            j.record(1, k, "iter-1".into());
        }
        let j = Journal::open(path.clone());
        let k = agent_call_key("loop body", &json!({}));
        // Each occurrence gets its own cached result by seq.
        assert!(matches!(j.claim(0, &k), Claim::Hit(r) if r == "iter-0"));
        assert!(matches!(j.claim(1, &k), Claim::Hit(r) if r == "iter-1"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn out_of_order_records_persist_in_seq_order() {
        // Parallel agents complete out of order; the file must still be seq-sorted
        // so a resume replays them correctly.
        let path = tmp("order");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let j = Journal::open(path.clone());
        j.record(2, "k2".into(), "r2".into());
        j.record(0, "k0".into(), "r0".into());
        j.record(1, "k1".into(), "r1".into());
        let raw = std::fs::read_to_string(&path).unwrap();
        let seqs: Vec<u32> = raw
            .lines()
            .map(|l| serde_json::from_str::<Entry>(l).unwrap().seq)
            .collect();
        assert_eq!(seqs, vec![0, 1, 2], "persisted in seq order");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
