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
    /// Keyed like `observations` but with its own lifetime — see [`ContextReads`].
    context_reads: BTreeMap<PathBuf, ContextReads>,
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
    identity: FileIdentity,
    coverage: ReadCoverage,
    notebook_cells: bool,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileIdentity {
    pub volume: u64,
    pub file_id: [u8; 16],
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileIdentity;

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

/// Which lines of one path this session has already put in front of the model.
///
/// Deliberately not [`ReadCoverage`]: that answers "has the model seen enough
/// of the CURRENT bytes to be allowed to edit them", so it survives compaction
/// and dies when the bytes change. This answers "are those lines still in the
/// conversation", which dies at compaction — and also when the bytes change,
/// because re-reading a file something else rewrote is not going in circles.
/// Keying the reset on the file version rather than on which tool wrote is what
/// makes an out-of-band edit (a `bash` heredoc, another process) count too.
#[derive(Default)]
struct ContextReads {
    version: Option<FileVersion>,
    ranges: Vec<Range<u64>>,
    rereads: u32,
    last_updated: u64,
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
                context_reads: BTreeMap::new(),
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
                if let Some(existing) = inner.observations.get(&path)
                    && existing.observation.version == observation.version
                    && existing.observation.identity == observation.identity
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

    /// Record that `observation`'s lines just reached the model, and answer how
    /// many times this path has now been read over lines the model already had.
    ///
    /// `None` means this read was not redundant: a first read, a fresh page, or
    /// one that landed past EOF. Only a strict intersection counts, so paging
    /// forward (`1..101`, then `101..201`) never does — that negative is half of
    /// what makes the count mean anything. The other half is the version reset
    /// above: rereading bytes that changed since is legitimate.
    pub(crate) fn note_context_read(
        &self,
        path: &Path,
        observation: &FileObservation,
    ) -> Option<u32> {
        let range = observation.read_range()?;
        let mut inner = self.inner.lock().unwrap();
        inner.sequence = inner.sequence.wrapping_add(1);
        let sequence = inner.sequence;
        let max_ranges = self.max_ranges;
        let entry = inner.context_reads.entry(path.to_path_buf()).or_default();
        if entry.version.as_ref() != Some(observation.version()) {
            *entry = ContextReads {
                version: Some(observation.version().clone()),
                ..ContextReads::default()
            };
        }
        entry.last_updated = sequence;
        let overlapped = entry
            .ranges
            .iter()
            .any(|seen| range.start < seen.end && seen.start < range.end);
        entry.ranges.push(range);
        coalesce(&mut entry.ranges, max_ranges);
        let rereads = overlapped.then(|| {
            entry.rereads = entry.rereads.saturating_add(1);
            entry.rereads
        });
        self.evict_context_reads(&mut inner);
        rereads
    }

    /// Forget every context-residency range: compaction folded the tool results
    /// away, so the lines they carried are no longer in front of the model.
    ///
    /// Blunt on purpose. Compaction keeps a tail, so some results do survive and
    /// this under-counts them — the safe direction, since the failure mode worth
    /// avoiding is telling the model it already has lines it no longer has.
    pub(crate) fn forget_context_reads(&self) {
        self.inner.lock().unwrap().context_reads.clear();
    }

    /// Same LRU bound as [`Self::evict_to_limits`], on the map that call does not
    /// reach. Ordinary sessions never come near it: the map is emptied at every
    /// compaction, so only a session with compaction disabled can grow one.
    fn evict_context_reads(&self, inner: &mut Inner) {
        while inner.context_reads.len() > self.max_observations {
            let Some(path) = inner
                .context_reads
                .iter()
                .min_by(|(path_a, entry_a), (path_b, entry_b)| {
                    (entry_a.last_updated, *path_a).cmp(&(entry_b.last_updated, *path_b))
                })
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            inner.context_reads.remove(&path);
        }
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
            let existing = inner.locks.get(path).and_then(Weak::upgrade);
            match existing {
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
    pub(crate) fn from_read_with_identity(
        bytes: &[u8],
        metadata: &std::fs::Metadata,
        identity: FileIdentity,
        total_units: u64,
        range: Range<u64>,
        empty_from_start: bool,
    ) -> Self {
        Self {
            version: FileVersion::new(bytes, metadata),
            identity,
            coverage: ReadCoverage::new(total_units, range, empty_from_start),
            notebook_cells: false,
        }
    }

    pub(crate) fn full_with_identity(
        bytes: &[u8],
        metadata: &std::fs::Metadata,
        identity: FileIdentity,
    ) -> Self {
        let len = bytes.len() as u64;
        Self::from_read_with_identity(bytes, metadata, identity, len, 0..len, true)
    }

    pub(crate) fn full_notebook_with_identity(
        bytes: &[u8],
        metadata: &std::fs::Metadata,
        identity: FileIdentity,
    ) -> Self {
        let mut observation = Self::full_with_identity(bytes, metadata, identity);
        observation.notebook_cells = true;
        observation
    }

    #[cfg(test)]
    pub(crate) fn from_read(
        bytes: &[u8],
        metadata: &std::fs::Metadata,
        total_units: u64,
        range: Range<u64>,
        empty_from_start: bool,
    ) -> Self {
        Self::from_read_with_identity(
            bytes,
            metadata,
            test_identity(metadata),
            total_units,
            range,
            empty_from_start,
        )
    }

    #[cfg(test)]
    pub(crate) fn full(bytes: &[u8], metadata: &std::fs::Metadata) -> Self {
        Self::full_with_identity(bytes, metadata, test_identity(metadata))
    }

    #[cfg(test)]
    pub(crate) fn full_notebook(bytes: &[u8], metadata: &std::fs::Metadata) -> Self {
        Self::full_notebook_with_identity(bytes, metadata, test_identity(metadata))
    }

    pub(crate) fn is_notebook(&self) -> bool {
        self.notebook_cells
    }

    pub(crate) fn version(&self) -> &FileVersion {
        &self.version
    }

    pub(crate) fn identity(&self) -> FileIdentity {
        self.identity
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.coverage.complete
    }

    /// The first unit (line, or notebook cell) this session has NOT put in
    /// front of the model, 1-based — the offset a read must start from to close
    /// the coverage gap from the front. `None` once coverage is complete.
    ///
    /// The ranges are sorted and coalesced, so the first gap ends the walk.
    pub(crate) fn first_unread_unit(&self) -> Option<u64> {
        if self.coverage.complete {
            return None;
        }
        let mut covered_through = 0;
        for range in &self.coverage.ranges {
            if range.start > covered_through {
                break;
            }
            covered_through = covered_through.max(range.end);
        }
        (covered_through < self.coverage.total_units).then_some(covered_through + 1)
    }

    /// The one range THIS read established, before any merge with what the
    /// session already knew — a fresh observation carries either that or, for a
    /// read that landed past EOF, nothing at all.
    pub(crate) fn read_range(&self) -> Option<Range<u64>> {
        match self.coverage.ranges.as_slice() {
            [range] => Some(range.clone()),
            _ => None,
        }
    }
}

#[cfg(all(test, unix))]
fn test_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt as _;

    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(all(test, windows))]
fn test_identity(_metadata: &std::fs::Metadata) -> FileIdentity {
    FileIdentity {
        volume: 0,
        file_id: [0; 16],
    }
}

#[cfg(all(test, not(any(unix, windows))))]
fn test_identity(_metadata: &std::fs::Metadata) -> FileIdentity {
    FileIdentity
}

impl FileVersion {
    pub(crate) fn new(bytes: &[u8], metadata: &std::fs::Metadata) -> Self {
        Self::from_fingerprint(sha2::Sha256::digest(bytes).into(), metadata)
    }

    pub(crate) fn from_fingerprint(fingerprint: [u8; 32], metadata: &std::fs::Metadata) -> Self {
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
            fingerprint,
        }
    }

    #[cfg(test)]
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
        coalesce(&mut self.ranges, max_ranges);
        self.recompute_complete();
    }

    fn recompute_complete(&mut self) {
        self.complete |= self.total_units == 0
            || matches!(self.ranges.as_slice(), [range] if range.start == 0 && range.end >= self.total_units);
    }
}

/// Normalize a range set in place: sort it and join everything that touches.
/// Touching is enough to join (`1..101` and `101..201` become `1..201`), which
/// is why a *strict* intersection is what the reread count tests for: a merged
/// set must not turn tomorrow's fresh page into a reread.
///
/// Past `max_ranges` the set stops growing and the highest ranges are dropped.
/// That can make a later read look fresh when it was not — under-counting, the
/// same safe direction the coverage set already accepts.
fn coalesce(ranges: &mut Vec<Range<u64>>, max_ranges: usize) {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<Range<u64>> = Vec::new();
    for range in ranges.drain(..) {
        if let Some(last) = merged.last_mut()
            && range.start <= last.end
        {
            last.end = last.end.max(range.end);
            continue;
        }
        if merged.len() < max_ranges {
            merged.push(range);
        }
    }
    *ranges = merged;
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

    /// The offset a refusal hands back: the first line still missing from the
    /// front, so reading from there closes the gap in one call. A hole before
    /// what has been read wins over a later one, and complete coverage has no
    /// offset to give.
    #[test]
    fn first_unread_unit_is_the_front_of_the_gap() {
        let bytes = b"a\nb\nc\nd\n";
        let (path, metadata) = temp_file("first-unread", bytes);
        let state = FileState::default();

        state.apply(FileStateUpdate::Observe {
            path: path.clone(),
            observation: FileObservation::from_read(bytes, &metadata, 4, 0..2, false),
        });
        assert_eq!(
            state.observation(&path).unwrap().first_unread_unit(),
            Some(3)
        );

        // A middle page leaves the hole at the front, not after it.
        state.apply(FileStateUpdate::Clear { path: path.clone() });
        state.apply(FileStateUpdate::Observe {
            path: path.clone(),
            observation: FileObservation::from_read(bytes, &metadata, 4, 2..4, false),
        });
        assert_eq!(
            state.observation(&path).unwrap().first_unread_unit(),
            Some(1)
        );

        state.apply(FileStateUpdate::Observe {
            path: path.clone(),
            observation: FileObservation::from_read(bytes, &metadata, 4, 0..2, false),
        });
        assert_eq!(state.observation(&path).unwrap().first_unread_unit(), None);
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

    #[cfg(unix)]
    #[test]
    fn a_new_identity_never_inherits_old_read_coverage() {
        let bytes = b"alpha\nbeta\n";
        let (path, metadata) = temp_file("identity", bytes);
        let state = FileState::default();
        let first = FileObservation::from_read(bytes, &metadata, 2, 0..1, false);
        state.apply(FileStateUpdate::Observe {
            path: path.clone(),
            observation: first.clone(),
        });
        let mut replacement = FileObservation::from_read(bytes, &metadata, 2, 1..2, false);
        replacement.identity = FileIdentity {
            device: first.identity.device,
            inode: first.identity.inode.wrapping_add(1),
        };
        state.apply(FileStateUpdate::Observe {
            path: path.clone(),
            observation: replacement,
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
        assert!(
            state
                .inner
                .lock()
                .unwrap()
                .observations
                .keys()
                .all(|path| !path.ends_with("kloop-file-state-a"))
        );

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
