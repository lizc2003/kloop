use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::SystemTime;

use sha2::Digest as _;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

const DEFAULT_MAX_OBSERVATIONS: usize = 1024;
const DEFAULT_MAX_MEMORY_BYTES: usize = 256 * 1024;
const DEFAULT_MAX_RANGES: usize = 32;
const ENTRY_OVERHEAD_BYTES: usize = 256;

/// Session-local knowledge established by model-visible file reads. This state
/// is deliberately absent from rollout persistence: resumed sessions, child
/// agents, and independent worktrees must prove freshness again.
pub struct FileState {
    inner: Mutex<Inner>,
    max_observations: usize,
    max_memory_bytes: usize,
    max_ranges: usize,
}

struct Inner {
    observations: BTreeMap<PathBuf, Entry>,
    locks: BTreeMap<PathBuf, Weak<AsyncMutex<()>>>,
    sequence: u64,
}

#[derive(Clone)]
struct Entry {
    observation: FileObservation,
    last_updated: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileObservation {
    version: FileVersion,
    coverage: ReadCoverage,
    notebook_cells: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileVersion {
    len: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    readonly: bool,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    mode: u32,
    fingerprint: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReadCoverage {
    total_units: u64,
    ranges: Vec<Range<u64>>,
    complete: bool,
}

pub(crate) enum FileStateUpdate {
    Observe {
        path: PathBuf,
        observation: FileObservation,
    },
    Replace {
        path: PathBuf,
        observation: FileObservation,
    },
    Clear {
        path: PathBuf,
    },
}

impl Default for FileState {
    fn default() -> Self {
        Self::with_limits(
            DEFAULT_MAX_OBSERVATIONS,
            DEFAULT_MAX_MEMORY_BYTES,
            DEFAULT_MAX_RANGES,
        )
    }
}

impl FileState {
    fn with_limits(max_observations: usize, max_memory_bytes: usize, max_ranges: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                observations: BTreeMap::new(),
                locks: BTreeMap::new(),
                sequence: 0,
            }),
            max_observations,
            max_memory_bytes,
            max_ranges,
        }
    }

    pub(crate) fn apply(&self, update: FileStateUpdate) {
        let mut inner = self.inner.lock().unwrap();
        inner.sequence = inner.sequence.wrapping_add(1);
        let sequence = inner.sequence;
        match update {
            FileStateUpdate::Observe {
                path,
                mut observation,
            } => {
                if let Some(existing) = inner.observations.get(&path) {
                    if existing.observation.version == observation.version
                        && existing.observation.notebook_cells == observation.notebook_cells
                    {
                        if existing.observation.coverage.complete {
                            observation.coverage.complete = true;
                        } else if existing.observation.coverage.total_units
                            == observation.coverage.total_units
                        {
                            observation
                                .coverage
                                .merge(&existing.observation.coverage, self.max_ranges);
                        }
                    }
                }
                inner.observations.insert(
                    path,
                    Entry {
                        observation,
                        last_updated: sequence,
                    },
                );
            }
            FileStateUpdate::Replace { path, observation } => {
                inner.observations.insert(
                    path,
                    Entry {
                        observation,
                        last_updated: sequence,
                    },
                );
            }
            FileStateUpdate::Clear { path } => {
                inner.observations.remove(&path);
            }
        }
        self.evict_to_limits(&mut inner);
    }

    pub(crate) fn observation(&self, path: &Path) -> Option<FileObservation> {
        self.inner
            .lock()
            .unwrap()
            .observations
            .get(path)
            .map(|entry| entry.observation.clone())
    }

    pub(crate) async fn lock_path(&self, path: &Path) -> OwnedMutexGuard<()> {
        let lock = {
            let mut inner = self.inner.lock().unwrap();
            inner.locks.retain(|_, lock| lock.strong_count() > 0);
            match inner.locks.get(path).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(AsyncMutex::new(()));
                    inner
                        .locks
                        .insert(path.to_path_buf(), Arc::downgrade(&lock));
                    lock
                }
            }
        };
        lock.lock_owned().await
    }

    fn evict_to_limits(&self, inner: &mut Inner) {
        while inner.observations.len() > self.max_observations
            || observation_memory_bytes(&inner.observations) > self.max_memory_bytes
        {
            let Some(path) = inner
                .observations
                .iter()
                .min_by(|(path_a, entry_a), (path_b, entry_b)| {
                    (entry_a.last_updated, *path_a).cmp(&(entry_b.last_updated, *path_b))
                })
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            inner.observations.remove(&path);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().observations.len()
    }
}

impl FileObservation {
    pub(crate) fn from_read(
        bytes: &[u8],
        metadata: &std::fs::Metadata,
        total_units: u64,
        range: Range<u64>,
        empty_from_start: bool,
    ) -> Self {
        Self {
            version: FileVersion::new(bytes, metadata),
            coverage: ReadCoverage::new(total_units, range, empty_from_start),
            notebook_cells: false,
        }
    }

    pub(crate) fn full(bytes: &[u8], metadata: &std::fs::Metadata) -> Self {
        let len = bytes.len() as u64;
        Self::from_read(bytes, metadata, len, 0..len, true)
    }

    pub(crate) fn full_notebook(bytes: &[u8], metadata: &std::fs::Metadata) -> Self {
        let mut observation = Self::full(bytes, metadata);
        observation.notebook_cells = true;
        observation
    }

    pub(crate) fn is_notebook(&self) -> bool {
        self.notebook_cells
    }

    pub(crate) fn version(&self) -> &FileVersion {
        &self.version
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.coverage.complete
    }
}

impl FileVersion {
    pub(crate) fn new(bytes: &[u8], metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;

        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            readonly: metadata.permissions().readonly(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            mode: metadata.mode(),
            fingerprint: sha2::Sha256::digest(bytes).into(),
        }
    }

    pub(crate) fn matches(&self, bytes: &[u8], metadata: &std::fs::Metadata) -> bool {
        self == &Self::new(bytes, metadata)
    }

    pub(crate) fn metadata_matches(&self, metadata: &std::fs::Metadata) -> bool {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;

        self.len == metadata.len()
            && self.modified == metadata.modified().ok()
            && self.created == metadata.created().ok()
            && self.readonly == metadata.permissions().readonly()
            && {
                #[cfg(unix)]
                {
                    self.device == metadata.dev()
                        && self.inode == metadata.ino()
                        && self.mode == metadata.mode()
                }
                #[cfg(not(unix))]
                {
                    true
                }
            }
    }
}

impl ReadCoverage {
    fn new(total_units: u64, mut range: Range<u64>, empty_from_start: bool) -> Self {
        range.start = range.start.min(total_units);
        range.end = range.end.min(total_units).max(range.start);
        let ranges = (range.start < range.end)
            .then_some(range)
            .into_iter()
            .collect();
        let mut coverage = Self {
            total_units,
            ranges,
            complete: total_units == 0 && empty_from_start,
        };
        coverage.recompute_complete();
        coverage
    }

    fn merge(&mut self, older: &Self, max_ranges: usize) {
        self.complete |= older.complete;
        self.ranges.extend(older.ranges.iter().cloned());
        self.ranges.sort_by_key(|range| (range.start, range.end));
        let mut merged: Vec<Range<u64>> = Vec::new();
        for range in self.ranges.drain(..) {
            if let Some(last) = merged.last_mut() {
                if range.start <= last.end {
                    last.end = last.end.max(range.end);
                    continue;
                }
            }
            if merged.len() < max_ranges {
                merged.push(range);
            }
        }
        self.ranges = merged;
        self.recompute_complete();
    }

    fn recompute_complete(&mut self) {
        self.complete |= self.total_units == 0
            || matches!(self.ranges.as_slice(), [range] if range.start == 0 && range.end >= self.total_units);
    }
}

fn observation_memory_bytes(observations: &BTreeMap<PathBuf, Entry>) -> usize {
    observations
        .iter()
        .map(|(path, entry)| {
            ENTRY_OVERHEAD_BYTES
                + path.as_os_str().as_encoded_bytes().len()
                + entry.observation.coverage.ranges.len() * std::mem::size_of::<Range<u64>>()
        })
        .sum()
}

/// Lexically normalize a path after anchoring it at an absolute cwd. Existing
/// file reads replace this with `canonicalize`, but writes to a not-yet-created
/// path still need one stable key that collapses `.` and `..` aliases.
pub(crate) fn normalize_absolute_path(cwd: &Path, raw: &Path) -> PathBuf {
    let path = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(tag: &str, bytes: &[u8]) -> (PathBuf, std::fs::Metadata) {
        let path =
            std::env::temp_dir().join(format!("kloop-file-state-{tag}-{}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        let path = std::fs::canonicalize(path).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        (path, metadata)
    }

    #[test]
    fn partial_ranges_merge_into_a_complete_observation() {
        let (path, metadata) = temp_file("coverage", b"a\nb\nc\nd\n");
        let state = FileState::default();
        state.apply(FileStateUpdate::Observe {
            path: path.clone(),
            observation: FileObservation::from_read(b"a\nb\nc\nd\n", &metadata, 4, 0..2, false),
        });
        assert!(!state.observation(&path).unwrap().is_complete());

        state.apply(FileStateUpdate::Observe {
            path: path.clone(),
            observation: FileObservation::from_read(b"a\nb\nc\nd\n", &metadata, 4, 2..4, false),
        });
        assert!(state.observation(&path).unwrap().is_complete());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn complete_mutation_observation_survives_a_partial_line_read() {
        let bytes = b"alpha\nbeta\n";
        let (path, metadata) = temp_file("complete-then-partial", bytes);
        let state = FileState::default();
        // Mutation refreshes are byte-shaped while text Read ranges are
        // line-shaped. A later partial Read of the same version must not revoke
        // the already established complete observation.
        state.apply(FileStateUpdate::Replace {
            path: path.clone(),
            observation: FileObservation::full(bytes, &metadata),
        });
        state.apply(FileStateUpdate::Observe {
            path: path.clone(),
            observation: FileObservation::from_read(bytes, &metadata, 2, 0..1, false),
        });

        assert!(state.observation(&path).unwrap().is_complete());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_new_version_replaces_old_coverage() {
        let (path, metadata) = temp_file("version", b"old\nvalue\n");
        let state = FileState::default();
        state.apply(FileStateUpdate::Observe {
            path: path.clone(),
            observation: FileObservation::from_read(b"old\nvalue\n", &metadata, 2, 0..2, false),
        });
        assert!(state.observation(&path).unwrap().is_complete());

        std::fs::write(&path, b"new\nvalue\nextra\n").unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        state.apply(FileStateUpdate::Observe {
            path: path.clone(),
            observation: FileObservation::from_read(
                b"new\nvalue\nextra\n",
                &metadata,
                3,
                0..1,
                false,
            ),
        });
        assert!(!state.observation(&path).unwrap().is_complete());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn eviction_is_oldest_first_and_memory_is_bounded() {
        let state = FileState::with_limits(2, usize::MAX, 4);
        for name in ["a", "b", "c"] {
            let (path, metadata) = temp_file(name, name.as_bytes());
            state.apply(FileStateUpdate::Replace {
                path,
                observation: FileObservation::full(name.as_bytes(), &metadata),
            });
        }
        assert_eq!(state.len(), 2);
        assert!(state
            .inner
            .lock()
            .unwrap()
            .observations
            .keys()
            .all(|path| !path.ends_with("kloop-file-state-a")));

        let tiny = FileState::with_limits(10, 1, 4);
        let (path, metadata) = temp_file("tiny", b"x");
        tiny.apply(FileStateUpdate::Replace {
            path,
            observation: FileObservation::full(b"x", &metadata),
        });
        assert_eq!(tiny.len(), 0);
    }

    #[test]
    fn notebook_qualification_never_merges_into_a_generic_observation() {
        let bytes = b"{\"cells\":[]}";
        let (path, metadata) = temp_file("notebook-kind", bytes);
        let state = FileState::default();
        state.apply(FileStateUpdate::Replace {
            path: path.clone(),
            observation: FileObservation::full_notebook(bytes, &metadata),
        });
        let qualified = state.observation(&path).unwrap();
        assert!(qualified.is_complete());
        assert!(qualified.is_notebook());

        state.apply(FileStateUpdate::Observe {
            path: path.clone(),
            observation: FileObservation::from_read(bytes, &metadata, 1, 0..1, true),
        });
        let generic = state.observation(&path).unwrap();
        assert!(generic.is_complete());
        assert!(!generic.is_notebook());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn normalization_collapses_relative_aliases() {
        let cwd = if cfg!(windows) {
            PathBuf::from(r"C:\workspace\root")
        } else {
            PathBuf::from("/workspace/root")
        };
        assert_eq!(
            normalize_absolute_path(&cwd, Path::new("src/../README.md")),
            cwd.join("README.md")
        );
    }

    #[tokio::test]
    async fn keyed_locks_serialize_only_the_same_path() {
        let state = Arc::new(FileState::default());
        let path = PathBuf::from("/same");
        let first = state.lock_path(&path).await;
        let state2 = Arc::clone(&state);
        let path2 = path.clone();
        let waiter = tokio::spawn(async move {
            let _guard = state2.lock_path(&path2).await;
            "acquired"
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(first);
        assert_eq!(waiter.await.unwrap(), "acquired");

        let _left = state.lock_path(Path::new("/left")).await;
        let right = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            state.lock_path(Path::new("/right")),
        )
        .await;
        assert!(right.is_ok());
    }
}
