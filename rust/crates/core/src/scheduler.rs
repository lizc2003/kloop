use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Datelike, Local, Offset as _, TimeZone as _, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::inbox::{Inbox, InboxItem, ScheduledOrigin};

const MAX_JOBS: usize = 50;
const MAX_SCAN_MINUTES: i64 = 527_040;
const MINUTE_MS: i64 = 60_000;
const RECURRING_MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
const RECURRING_JITTER_FRACTION: f64 = 0.10;
const RECURRING_JITTER_CAP_MS: i64 = 15 * 60 * 1_000;
const ONE_SHOT_JITTER_MAX_MS: i64 = 90 * 1_000;
const STORE_VERSION: u32 = 1;
const MAX_STORE_BYTES: u64 = 4 * 1024 * 1024;
/// How long a durable job written by a second runtime of the same session may
/// stay invisible here. Owners are session ids, so that means one session opened
/// twice; claims stay single-delivery either way, because the store's own lock
/// linearises them (plan 58) — this interval only bounds how soon the other
/// runtime's jobs are noticed. The bound that matters is
/// `requires_missed_confirmation`'s one minute: claiming later than that turns a
/// due job into one the user has to confirm, so this stays well under it. Only a
/// session actually holding durable jobs pays it; everyone else sleeps on
/// `notify_change`.
const STORE_POLL_MS: i64 = 15_000;
static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
    fn sleep_until(&self, deadline_ms: i64) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

#[derive(Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        Utc::now().timestamp_millis()
    }

    fn sleep_until(&self, deadline_ms: i64) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let delay = deadline_ms.saturating_sub(self.now_ms()).max(0) as u64;
        Box::pin(tokio::time::sleep(Duration::from_millis(delay)))
    }
}

pub struct ManualClock {
    now_ms: AtomicI64,
    activity: watch::Sender<u64>,
}

impl ManualClock {
    pub fn new(now_ms: i64) -> Arc<Self> {
        let (activity, _) = watch::channel(0);
        Arc::new(Self {
            now_ms: AtomicI64::new(now_ms),
            activity,
        })
    }

    pub fn set(&self, now_ms: i64) {
        self.now_ms.store(now_ms, Ordering::Release);
        let next = (*self.activity.borrow()).wrapping_add(1);
        self.activity.send_replace(next);
    }

    pub fn advance(&self, duration: Duration) {
        let millis = i64::try_from(duration.as_millis()).unwrap_or(i64::MAX);
        self.set(self.now_ms().saturating_add(millis));
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.now_ms.load(Ordering::Acquire)
    }

    fn sleep_until(&self, deadline_ms: i64) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let mut activity = self.activity.subscribe();
            loop {
                if self.now_ms() >= deadline_ms {
                    return;
                }
                if activity.changed().await.is_err() {
                    return;
                }
            }
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedulerTimeZone {
    System,
    Named(Tz),
}

impl SchedulerTimeZone {
    pub fn from_environment() -> Self {
        std::env::var("TZ")
            .ok()
            .and_then(|value| Tz::from_str(&value).ok())
            .map(Self::Named)
            .unwrap_or(Self::System)
    }

    pub fn named(name: &str) -> Result<Self> {
        Ok(Self::Named(
            Tz::from_str(name).map_err(|_| anyhow!("unknown timezone '{name}'"))?,
        ))
    }

    fn local_parts(self, timestamp_ms: i64) -> Result<LocalParts> {
        match self {
            Self::System => {
                let value = Local
                    .timestamp_millis_opt(timestamp_ms)
                    .single()
                    .ok_or_else(|| anyhow!("timestamp is outside the local timezone range"))?;
                Ok(LocalParts::from_datetime(value))
            }
            Self::Named(timezone) => {
                let value = timezone
                    .timestamp_millis_opt(timestamp_ms)
                    .single()
                    .ok_or_else(|| anyhow!("timestamp is outside timezone {timezone}"))?;
                Ok(LocalParts::from_datetime(value))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LocalParts {
    minute: u8,
    hour: u8,
    day_of_month: u8,
    month: u8,
    day_of_week: u8,
    offset_seconds: i32,
}

impl LocalParts {
    fn from_datetime<T: chrono::TimeZone>(value: chrono::DateTime<T>) -> Self {
        Self {
            minute: value.minute() as u8,
            hour: value.hour() as u8,
            day_of_month: value.day() as u8,
            month: value.month() as u8,
            day_of_week: value.weekday().num_days_from_sunday() as u8,
            offset_seconds: value.offset().fix().local_minus_utc(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronSpec {
    source: String,
    minute: Vec<u8>,
    hour: Vec<u8>,
    day_of_month: Vec<u8>,
    month: Vec<u8>,
    day_of_week: Vec<u8>,
}

impl CronSpec {
    pub fn parse(source: &str) -> Result<Self> {
        let fields: Vec<&str> = source.split_whitespace().collect();
        if fields.len() != 5 {
            bail!("Invalid cron expression '{source}'. Expected 5 fields: M H DoM Mon DoW.");
        }
        let spec = Self {
            source: source.trim().to_string(),
            minute: parse_field(fields[0], 0, 59, false)?,
            hour: parse_field(fields[1], 0, 23, false)?,
            day_of_month: parse_field(fields[2], 1, 31, false)?,
            month: parse_field(fields[3], 1, 12, false)?,
            day_of_week: parse_field(fields[4], 0, 6, true)?,
        };
        Ok(spec)
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn next_after(&self, after_ms: i64, timezone: SchedulerTimeZone) -> Option<i64> {
        let start = after_ms
            .div_euclid(MINUTE_MS)
            .saturating_add(1)
            .saturating_mul(MINUTE_MS);
        let limit = start.saturating_add((MAX_SCAN_MINUTES - 1).saturating_mul(MINUTE_MS));
        let mut candidate = start;
        while candidate <= limit {
            let parts = timezone.local_parts(candidate).ok()?;
            if self.matches(parts) {
                return Some(candidate);
            }
            candidate = self.skip(timezone, candidate, parts)?;
        }
        None
    }

    /// Advance past a candidate that does not match, by whole calendar fields
    /// rather than a minute at a time, so a legal expression that never matches
    /// (`0 0 30 2 *`) exhausts the year in hundreds of steps instead of half a
    /// million timezone conversions.
    ///
    /// A skip is measured in local calendar fields, so it only holds while local
    /// time advances in lockstep with UTC. Across a DST transition the step is
    /// halved until it lands on the offset it started from; that costs a handful
    /// of lookups near the transition instead of degrading the whole scan.
    fn skip(&self, timezone: SchedulerTimeZone, candidate: i64, parts: LocalParts) -> Option<i64> {
        let mut minutes = self.skip_minutes(parts);
        while minutes > 1 {
            let target = candidate.saturating_add(minutes.saturating_mul(MINUTE_MS));
            if timezone.local_parts(target).ok()?.offset_seconds == parts.offset_seconds {
                return Some(target);
            }
            minutes /= 2;
        }
        Some(candidate.saturating_add(MINUTE_MS))
    }

    /// Minutes that provably hold no match, given the local fields of a
    /// candidate that already failed `matches`. Always at least one.
    fn skip_minutes(&self, parts: LocalParts) -> i64 {
        let into_day = i64::from(parts.hour) * 60 + i64::from(parts.minute);
        if !self.month.contains(&parts.month) || !self.day_matches(parts) {
            // dom and dow are an OR, which makes a whole day the atom here: a
            // skip by dom alone would step over days the dow half accepts.
            return 24 * 60 - into_day;
        }
        if !self.hour.contains(&parts.hour) {
            let next = self
                .hour
                .iter()
                .copied()
                .find(|hour| *hour > parts.hour)
                .map_or(24, i64::from);
            return next * 60 - into_day;
        }
        let next = self
            .minute
            .iter()
            .copied()
            .find(|minute| *minute > parts.minute)
            .map_or(60, i64::from);
        next - i64::from(parts.minute)
    }

    pub fn human_schedule(&self, timezone: SchedulerTimeZone) -> String {
        let fields: Vec<&str> = self.source.split_whitespace().collect();
        let [minute, hour, dom, month, dow] = fields.as_slice() else {
            return self.source.clone();
        };
        if *hour == "*" && *dom == "*" && *month == "*" && *dow == "*" {
            if *minute == "*" {
                return "Every minute".into();
            }
            if let Some(step) = minute.strip_prefix("*/") {
                if step == "1" {
                    return "Every minute".into();
                }
                return format!("Every {step} minutes");
            }
            if minute.chars().all(|value| value.is_ascii_digit()) {
                if *minute == "0" {
                    return "Every hour".into();
                }
                return format!("Every hour at :{minute:0>2}");
            }
        }
        if minute.chars().all(|value| value.is_ascii_digit())
            && hour.chars().all(|value| value.is_ascii_digit())
            && *dom == "*"
            && *month == "*"
            && *dow == "*"
        {
            let minute = minute.parse::<u8>().unwrap_or_default();
            let hour = hour.parse::<u8>().unwrap_or_default();
            let suffix = match timezone {
                SchedulerTimeZone::Named(Tz::UTC) => " UTC",
                _ => "",
            };
            return format!("Every day at {hour}:{minute:02}{suffix}");
        }
        self.source.clone()
    }

    fn matches(&self, parts: LocalParts) -> bool {
        self.minute.contains(&parts.minute)
            && self.hour.contains(&parts.hour)
            && self.month.contains(&parts.month)
            && self.day_matches(parts)
    }

    /// Vixie cron's day rule: two unrestricted fields match every day, one
    /// unrestricted field defers to the other, two restricted fields are an OR.
    fn day_matches(&self, parts: LocalParts) -> bool {
        let dom_all = self.day_of_month.len() == 31;
        let dow_all = self.day_of_week.len() == 7;
        let dom = self.day_of_month.contains(&parts.day_of_month);
        let dow = self.day_of_week.contains(&parts.day_of_week);
        match (dom_all, dow_all) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            (false, false) => dom || dow,
        }
    }
}

fn parse_field(source: &str, min: u8, max: u8, day_of_week: bool) -> Result<Vec<u8>> {
    let mut values = HashSet::new();
    for item in source.split(',') {
        if item.is_empty() {
            bail!("invalid cron field '{source}'");
        }
        let (base, step) = match item.split_once('/') {
            Some((base, step)) => {
                if step.contains('/') {
                    bail!("invalid cron field '{source}'");
                }
                let step = step
                    .parse::<u8>()
                    .ok()
                    .filter(|value| *value > 0)
                    .ok_or_else(|| anyhow!("invalid cron field '{source}'"))?;
                (base, step)
            }
            None => (item, 1),
        };
        let upper = if day_of_week { 7 } else { max };
        let (start, end) = if base == "*" {
            (min, upper)
        } else if let Some((start, end)) = base.split_once('-') {
            let start = start
                .parse::<u8>()
                .map_err(|_| anyhow!("invalid cron field '{source}'"))?;
            let end = end
                .parse::<u8>()
                .map_err(|_| anyhow!("invalid cron field '{source}'"))?;
            if start > end {
                bail!("invalid cron field '{source}'");
            }
            (start, end)
        } else {
            if item.contains('/') {
                bail!("invalid cron field '{source}'");
            }
            let value = base
                .parse::<u8>()
                .map_err(|_| anyhow!("invalid cron field '{source}'"))?;
            (value, value)
        };
        if start < min || end > upper {
            bail!("invalid cron field '{source}'");
        }
        let mut value = start;
        loop {
            values.insert(if day_of_week && value == 7 { 0 } else { value });
            let Some(next) = value.checked_add(step) else {
                break;
            };
            if next > end {
                break;
            }
            value = next;
        }
    }
    if values.is_empty() {
        bail!("invalid cron field '{source}'");
    }
    let mut values: Vec<u8> = values.into_iter().collect();
    values.sort_unstable();
    Ok(values)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledKind {
    Cron,
    LoopWakeup,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledJob {
    pub id: String,
    pub owner: String,
    pub cron: String,
    pub prompt: String,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at_ms: Option<i64>,
    pub next_fire_at_ms: i64,
    pub recurring: bool,
    pub durable: bool,
    pub kind: ScheduledKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub generation: u64,
}

impl ScheduledJob {
    pub fn human_schedule(&self, timezone: SchedulerTimeZone) -> String {
        CronSpec::parse(&self.cron)
            .map(|spec| spec.human_schedule(timezone))
            .unwrap_or_else(|_| self.cron.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WakeupResult {
    pub scheduled_for_ms: i64,
    pub clamped_delay_seconds: u64,
    pub was_clamped: bool,
    pub stopped: bool,
    pub cancelled_wakeups: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    project_key: String,
    jobs: Vec<ScheduledJob>,
}

#[cfg(unix)]
fn lock_store_file(file: &File) -> Result<()> {
    rustix::fs::flock(file, rustix::fs::FlockOperation::LockExclusive)
        .context("lock scheduler store")
}

#[cfg(unix)]
fn unlock_store_file(file: &File) -> Result<()> {
    rustix::fs::flock(file, rustix::fs::FlockOperation::Unlock).context("unlock scheduler store")
}

#[cfg(windows)]
fn lock_store_file(file: &File) -> Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::LOCKFILE_EXCLUSIVE_LOCK;
    use windows_sys::Win32::Storage::FileSystem::LockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            LOCKFILE_EXCLUSIVE_LOCK,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error()).context("lock scheduler store");
    }
    Ok(())
}

#[cfg(windows)]
fn unlock_store_file(file: &File) -> Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let result = unsafe {
        UnlockFileEx(
            file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error()).context("unlock scheduler store");
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn lock_store_file(_file: &File) -> Result<()> {
    bail!("durable scheduler locking is unsupported on this platform")
}

#[cfg(not(any(unix, windows)))]
fn unlock_store_file(_file: &File) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_store_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .context("sync scheduler store directory")
}

#[cfg(not(unix))]
fn sync_store_directory(_path: &Path) -> Result<()> {
    // Windows does not expose a portable directory-entry flush equivalent.
    Ok(())
}

#[derive(Clone, Debug)]
pub struct DurableStore {
    path: PathBuf,
    lock_path: PathBuf,
    project_key: String,
}

impl DurableStore {
    pub fn new(path: PathBuf, project_key: String) -> Self {
        let lock_path = path.with_extension("lock");
        Self {
            path,
            lock_path,
            project_key,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Result<Vec<ScheduledJob>> {
        self.with_lock(|store| Ok(store.jobs.clone()))
    }

    fn transaction<T>(
        &self,
        operation: impl FnOnce(&mut Vec<ScheduledJob>) -> Result<T>,
    ) -> Result<T> {
        self.with_lock(|store| {
            let output = operation(&mut store.jobs)?;
            self.write_locked(store)?;
            Ok(output)
        })
    }

    fn transaction_if_changed<T>(
        &self,
        operation: impl FnOnce(&mut Vec<ScheduledJob>) -> Result<(T, bool)>,
    ) -> Result<T> {
        self.with_lock(|store| {
            let (output, changed) = operation(&mut store.jobs)?;
            if changed {
                self.write_locked(store)?;
            }
            Ok(output)
        })
    }

    fn with_lock<T>(&self, operation: impl FnOnce(&mut StoreFile) -> Result<T>) -> Result<T> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow!("scheduler store path has no parent"))?;
        ensure_private_dir(parent)?;
        reject_symlink(&self.lock_path)?;
        let lock = private_open(&self.lock_path, true, false)?;
        lock_store_file(&lock)?;
        let mut store = self.read_locked()?;
        let result = operation(&mut store);
        let _ = unlock_store_file(&lock);
        result
    }

    fn read_locked(&self) -> Result<StoreFile> {
        reject_symlink(&self.path)?;
        let mut file = match private_open(&self.path, false, false) {
            Ok(file) => file,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(StoreFile {
                    version: STORE_VERSION,
                    project_key: self.project_key.clone(),
                    jobs: Vec::new(),
                });
            }
            Err(error) => return Err(error),
        };
        let metadata = file.metadata().context("inspect scheduler store")?;
        if !metadata.is_file() || metadata.len() > MAX_STORE_BYTES {
            bail!("scheduler store must be a regular file no larger than 4 MiB");
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.read_to_end(&mut bytes)
            .context("read scheduler store")?;
        let store: StoreFile = serde_json::from_slice(&bytes).context("parse scheduler store")?;
        if store.version != STORE_VERSION || store.project_key != self.project_key {
            bail!("scheduler store identity or schema version mismatch");
        }
        for job in &store.jobs {
            CronSpec::parse(&job.cron)
                .with_context(|| format!("scheduler store contains invalid job {}", job.id))?;
            if job.owner.is_empty() || job.id.len() != 8 {
                bail!("scheduler store contains malformed job identity");
            }
        }
        Ok(store)
    }

    fn write_locked(&self, store: &StoreFile) -> Result<()> {
        reject_symlink(&self.path)?;
        let parent = self.path.parent().unwrap();
        let name = self
            .path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow!("scheduler store filename is not UTF-8"))?;
        let sequence = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(".{name}.tmp-{}-{sequence}", std::process::id()));
        let payload = serde_json::to_vec_pretty(store).context("serialize scheduler store")?;
        let result = (|| {
            let mut file = private_open(&temp, true, true)?;
            file.write_all(&payload)
                .context("write scheduler temp file")?;
            file.write_all(b"\n")
                .context("finish scheduler temp file")?;
            file.sync_all().context("sync scheduler temp file")?;
            fs::rename(&temp, &self.path).context("replace scheduler store")?;
            sync_store_directory(parent)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

#[cfg(unix)]
fn private_open(path: &Path, create: bool, create_new: bool) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(create || create_new)
        .create(create)
        .create_new(create_new)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    options
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

#[cfg(not(unix))]
fn private_open(path: &Path, create: bool, create_new: bool) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(create || create_new)
        .create(create)
        .create_new(create_new)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("scheduler path is a symlink: {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("scheduler store parent must be a real directory");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("protect {}", path.display()))?;
    }
    Ok(())
}

/// What one worker tick needs to know about the two registries: when to wake, and
/// whether any durable job makes the store worth polling at all.
#[derive(Default)]
struct Horizon {
    deadline: Option<i64>,
    durable_jobs: bool,
}

struct SchedulerState {
    owner: Option<String>,
    session_jobs: Vec<ScheduledJob>,
    missed_confirmation_available: bool,
    closed: bool,
    worker: Option<tokio::task::JoinHandle<()>>,
}

pub struct Scheduler {
    state: Mutex<SchedulerState>,
    inbox: Arc<Inbox>,
    store: Option<DurableStore>,
    clock: Arc<dyn Clock>,
    timezone: SchedulerTimeZone,
    changes: watch::Sender<u64>,
}

impl Scheduler {
    pub fn in_memory(inbox: Arc<Inbox>) -> Arc<Self> {
        Self::with_parts(
            inbox,
            None,
            Arc::new(SystemClock),
            SchedulerTimeZone::from_environment(),
        )
    }

    pub fn persistent(
        inbox: Arc<Inbox>,
        store: DurableStore,
        timezone: SchedulerTimeZone,
    ) -> Arc<Self> {
        Self::with_parts(inbox, Some(store), Arc::new(SystemClock), timezone)
    }

    pub fn with_clock(
        inbox: Arc<Inbox>,
        store: Option<DurableStore>,
        clock: Arc<dyn Clock>,
        timezone: SchedulerTimeZone,
    ) -> Arc<Self> {
        Self::with_parts(inbox, store, clock, timezone)
    }

    fn with_parts(
        inbox: Arc<Inbox>,
        store: Option<DurableStore>,
        clock: Arc<dyn Clock>,
        timezone: SchedulerTimeZone,
    ) -> Arc<Self> {
        let (changes, _) = watch::channel(0);
        Arc::new(Self {
            state: Mutex::new(SchedulerState {
                owner: None,
                session_jobs: Vec::new(),
                missed_confirmation_available: true,
                closed: false,
                worker: None,
            }),
            inbox,
            store,
            clock,
            timezone,
            changes,
        })
    }

    pub fn set_missed_confirmation_available(&self, available: bool) {
        self.state.lock().unwrap().missed_confirmation_available = available;
        self.notify_change();
    }

    pub fn bind_owner(self: &Arc<Self>, owner: impl Into<String>) -> Result<()> {
        let owner = owner.into();
        if owner.trim().is_empty() {
            bail!("scheduler owner session is empty");
        }
        if let Some(store) = &self.store {
            store.load()?;
        }
        let mut state = self.state.lock().unwrap();
        if let Some(current) = &state.owner {
            if current != &owner {
                bail!("scheduler is already bound to owner '{current}'");
            }
            return Ok(());
        }
        state.owner = Some(owner);
        state.closed = false;
        if tokio::runtime::Handle::try_current().is_ok() {
            let scheduler = Arc::clone(self);
            state.worker = Some(tokio::spawn(async move { scheduler.run_worker().await }));
        }
        drop(state);
        self.notify_change();
        Ok(())
    }

    pub fn timezone(&self) -> SchedulerTimeZone {
        self.timezone
    }

    pub fn create(
        &self,
        cron: &str,
        prompt: &str,
        recurring: bool,
        durable: bool,
    ) -> Result<ScheduledJob> {
        let owner = self.owner()?;
        if prompt.is_empty() {
            bail!("cron_create: prompt must not be empty");
        }
        let spec = CronSpec::parse(cron)?;
        let now = self.clock.now_ms();
        if self.list()?.len() >= MAX_JOBS {
            bail!("Too many scheduled jobs (max 50). Cancel one first.");
        }
        if durable && self.store.is_none() {
            bail!("durable scheduler storage is unavailable for this session");
        }
        let id = self.fresh_id()?;
        let next_fire_at_ms = next_cron_fire(&spec, now, now, &id, recurring, self.timezone)
            .ok_or_else(|| {
                anyhow!(
                    "Cron expression '{cron}' does not match any calendar date in the next year."
                )
            })?;
        let job = ScheduledJob {
            id,
            owner,
            cron: spec.source().to_string(),
            prompt: prompt.to_string(),
            created_at_ms: now,
            last_fired_at_ms: None,
            next_fire_at_ms,
            recurring,
            durable,
            kind: ScheduledKind::Cron,
            reason: None,
            generation: 1,
        };
        if durable {
            self.store.as_ref().unwrap().transaction(|jobs| {
                jobs.push(job.clone());
                Ok(())
            })?;
        } else {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                bail!("scheduler is closed");
            }
            state.session_jobs.push(job.clone());
        }
        self.notify_change();
        Ok(job)
    }

    pub fn list(&self) -> Result<Vec<ScheduledJob>> {
        let owner = self.owner()?;
        let mut jobs = if let Some(store) = &self.store {
            store
                .load()?
                .into_iter()
                .filter(|job| job.owner == owner)
                .collect()
        } else {
            Vec::new()
        };
        jobs.extend(
            self.state
                .lock()
                .unwrap()
                .session_jobs
                .iter()
                .filter(|job| job.owner == owner)
                .cloned(),
        );
        jobs.sort_by(|left, right| {
            (left.next_fire_at_ms, left.created_at_ms, &left.id).cmp(&(
                right.next_fire_at_ms,
                right.created_at_ms,
                &right.id,
            ))
        });
        Ok(jobs)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let owner = self.owner()?;
        let mut removed = if let Some(store) = &self.store {
            store.transaction_if_changed(|jobs| {
                let before = jobs.len();
                jobs.retain(|job| !(job.id == id && job.owner == owner));
                let removed = jobs.len() != before;
                Ok((removed, removed))
            })?
        } else {
            false
        };
        let mut state = self.state.lock().unwrap();
        let before = state.session_jobs.len();
        state
            .session_jobs
            .retain(|job| !(job.id == id && job.owner == owner));
        removed |= state.session_jobs.len() != before;
        drop(state);
        if !removed {
            bail!("No scheduled job with id '{id}'");
        }
        self.notify_change();
        Ok(())
    }

    pub fn schedule_wakeup(
        &self,
        delay_seconds: f64,
        reason: &str,
        prompt: &str,
    ) -> Result<WakeupResult> {
        let owner = self.owner()?;
        if !delay_seconds.is_finite() || reason.trim().is_empty() || prompt.is_empty() {
            bail!("schedule_wakeup requires finite delay_seconds, reason, and prompt");
        }
        let rounded = delay_seconds.round();
        let clamped = rounded.clamp(60.0, 3_600.0) as u64;
        let was_clamped = rounded != clamped as f64;
        let now = self.clock.now_ms();
        let target = now
            .saturating_add((clamped as i64).saturating_mul(1_000))
            .saturating_add(MINUTE_MS - 1)
            .div_euclid(MINUTE_MS)
            .saturating_mul(MINUTE_MS);
        let id = self.fresh_id()?;
        let parts = self.timezone.local_parts(target)?;
        let cron = format!("{} {} * * *", parts.minute, parts.hour);
        let job = ScheduledJob {
            id,
            owner,
            cron,
            prompt: prompt.to_string(),
            created_at_ms: now,
            last_fired_at_ms: None,
            next_fire_at_ms: target,
            recurring: false,
            durable: false,
            kind: ScheduledKind::LoopWakeup,
            reason: Some(reason.to_string()),
            generation: 1,
        };
        let existing = self.list()?;
        let replacing_existing = existing
            .iter()
            .any(|job| job.kind == ScheduledKind::LoopWakeup);
        if !replacing_existing && existing.len() >= MAX_JOBS {
            bail!("Too many scheduled jobs (max 50). Cancel one first.");
        }
        let cancelled_wakeups = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                bail!("scheduler is closed");
            }
            let before = state.session_jobs.len();
            state
                .session_jobs
                .retain(|existing| existing.kind != ScheduledKind::LoopWakeup);
            let cancelled = before - state.session_jobs.len();
            state.session_jobs.push(job);
            cancelled
        };
        self.notify_change();
        Ok(WakeupResult {
            scheduled_for_ms: target,
            clamped_delay_seconds: clamped,
            was_clamped,
            stopped: false,
            cancelled_wakeups,
        })
    }

    pub fn stop_wakeup(&self) -> Result<WakeupResult> {
        self.owner()?;
        let cancelled_wakeups = {
            let mut state = self.state.lock().unwrap();
            let before = state.session_jobs.len();
            state
                .session_jobs
                .retain(|job| job.kind != ScheduledKind::LoopWakeup);
            before - state.session_jobs.len()
        };
        self.notify_change();
        Ok(WakeupResult {
            scheduled_for_ms: 0,
            clamped_delay_seconds: 0,
            was_clamped: false,
            stopped: true,
            cancelled_wakeups,
        })
    }

    pub async fn shutdown(&self) -> usize {
        let worker = {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            state.session_jobs.clear();
            state.worker.take()
        };
        self.notify_change();
        if let Some(worker) = worker {
            worker.abort();
            let _ = worker.await;
        }
        0
    }

    fn owner(&self) -> Result<String> {
        let state = self.state.lock().unwrap();
        if state.closed {
            bail!("scheduler is closed");
        }
        state
            .owner
            .clone()
            .ok_or_else(|| anyhow!("scheduler is not bound to a session owner"))
    }

    fn fresh_id(&self) -> Result<String> {
        let existing: HashSet<String> = self.list()?.into_iter().map(|job| job.id).collect();
        for _ in 0..16 {
            let mut bytes = [0_u8; 4];
            getrandom::getrandom(&mut bytes)
                .map_err(|error| anyhow!("generate scheduler job id: {error}"))?;
            let id = format!(
                "{:02x}{:02x}{:02x}{:02x}",
                bytes[0], bytes[1], bytes[2], bytes[3]
            );
            if !existing.contains(&id) {
                return Ok(id);
            }
        }
        bail!("could not allocate a unique scheduler job id")
    }

    fn notify_change(&self) {
        let next = (*self.changes.borrow()).wrapping_add(1);
        self.changes.send_replace(next);
    }

    async fn run_worker(self: Arc<Self>) {
        let mut changes = self.changes.subscribe();
        let mut last_failure: Option<String> = None;
        loop {
            if self.state.lock().unwrap().closed {
                return;
            }
            let now = self.clock.now_ms();
            let (due, mut failure) = match self.claim_due(now) {
                Ok(result) => result,
                Err(error) => (
                    Vec::new(),
                    Some(format!("scheduler worker failed closed: {error:#}")),
                ),
            };
            for job in due {
                let missed = requires_missed_confirmation(&job, now);
                self.inbox.push(InboxItem::ScheduledPrompt {
                    id: job.id,
                    origin: match job.kind {
                        ScheduledKind::Cron => ScheduledOrigin::Cron,
                        ScheduledKind::LoopWakeup => ScheduledOrigin::LoopWakeup,
                    },
                    scheduled_for_ms: job.next_fire_at_ms,
                    reason: job.reason,
                    prompt: job.prompt,
                    missed,
                });
            }
            let horizon = match self.horizon(now) {
                Ok(horizon) => horizon,
                Err(error) => {
                    let next = format!("scheduler store failed closed: {error:#}");
                    match &mut failure {
                        Some(summary) => {
                            summary.push_str("; ");
                            summary.push_str(&next);
                        }
                        None => failure = Some(next),
                    }
                    Horizon::default()
                }
            };
            if failure != last_failure {
                if let Some(summary) = &failure {
                    self.inbox.push(InboxItem::SchedulerFailure {
                        summary: summary.clone(),
                    });
                }
                last_failure = failure;
            }
            // A session with no durable job has nothing another runtime could be
            // writing on its behalf, so it sleeps on `notify_change` alone and
            // never touches the disk.
            let deadline = if horizon.durable_jobs {
                let store_poll = now.saturating_add(STORE_POLL_MS);
                Some(
                    horizon
                        .deadline
                        .map_or(store_poll, |job| job.min(store_poll)),
                )
            } else {
                horizon.deadline
            };
            let Some(deadline) = deadline else {
                if changes.changed().await.is_err() {
                    return;
                }
                continue;
            };
            tokio::select! {
                _ = self.clock.sleep_until(deadline) => {}
                changed = changes.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    }

    fn horizon(&self, now: i64) -> Result<Horizon> {
        let confirmation_available = self.state.lock().unwrap().missed_confirmation_available;
        let jobs = self.list()?;
        Ok(Horizon {
            deadline: jobs
                .iter()
                .filter(|job| confirmation_available || !requires_missed_confirmation(job, now))
                .map(|job| job.next_fire_at_ms)
                .min(),
            durable_jobs: jobs.iter().any(|job| job.durable),
        })
    }

    fn claim_due(&self, now: i64) -> Result<(Vec<ScheduledJob>, Option<String>)> {
        let owner = self.owner()?;
        let timezone = self.timezone;
        let confirmation_available = self.state.lock().unwrap().missed_confirmation_available;
        let mut due = Vec::new();
        let mut failure = None;
        {
            let mut state = self.state.lock().unwrap();
            let mut retained = Vec::with_capacity(state.session_jobs.len());
            for mut job in state.session_jobs.drain(..) {
                if job.owner != owner || job.next_fire_at_ms > now {
                    retained.push(job);
                    continue;
                }
                let fired = job.clone();
                match advance_after_fire(&mut job, now, timezone) {
                    Ok(keep) => {
                        if keep {
                            retained.push(job);
                        }
                        due.push(fired);
                    }
                    Err(error) => {
                        retained.push(fired);
                        if failure.is_none() {
                            failure = Some(format!(
                                "session job failed closed without being claimed: {error:#}"
                            ));
                        }
                    }
                }
            }
            state.session_jobs = retained;
        }
        if let Some(store) = &self.store {
            match store.transaction_if_changed(|jobs| {
                let mut due = Vec::new();
                let mut retained = Vec::with_capacity(jobs.len());
                for mut job in jobs.drain(..) {
                    if job.owner != owner || job.next_fire_at_ms > now {
                        retained.push(job);
                        continue;
                    }
                    if !confirmation_available && requires_missed_confirmation(&job, now) {
                        retained.push(job);
                        continue;
                    }
                    let fired = job.clone();
                    if advance_after_fire(&mut job, now, timezone)? {
                        retained.push(job);
                    }
                    due.push(fired);
                }
                *jobs = retained;
                let changed = !due.is_empty();
                Ok((due, changed))
            }) {
                Ok(mut durable_due) => due.append(&mut durable_due),
                Err(error) => {
                    let store_failure =
                        format!("durable jobs failed closed without being claimed: {error:#}");
                    match &mut failure {
                        Some(summary) => {
                            summary.push_str("; ");
                            summary.push_str(&store_failure);
                        }
                        None => failure = Some(store_failure),
                    }
                }
            }
        }
        due.sort_by(|left, right| {
            (left.next_fire_at_ms, left.created_at_ms, &left.id).cmp(&(
                right.next_fire_at_ms,
                right.created_at_ms,
                &right.id,
            ))
        });
        Ok((due, failure))
    }
}

fn requires_missed_confirmation(job: &ScheduledJob, now: i64) -> bool {
    job.durable
        && !job.recurring
        && job.kind == ScheduledKind::Cron
        && now.saturating_sub(job.next_fire_at_ms) >= MINUTE_MS
}

fn advance_after_fire(
    job: &mut ScheduledJob,
    now: i64,
    timezone: SchedulerTimeZone,
) -> Result<bool> {
    if !job.recurring || job.kind == ScheduledKind::LoopWakeup {
        return Ok(false);
    }
    if now.saturating_sub(job.created_at_ms) >= RECURRING_MAX_AGE_MS {
        return Ok(false);
    }
    let spec = CronSpec::parse(&job.cron)?;
    let next = next_cron_fire(&spec, now, job.created_at_ms, &job.id, true, timezone);
    let Some(next) = next else {
        return Ok(false);
    };
    job.last_fired_at_ms = Some(now);
    job.next_fire_at_ms = next;
    job.generation = job.generation.saturating_add(1);
    Ok(true)
}

fn next_cron_fire(
    spec: &CronSpec,
    after_ms: i64,
    created_at_ms: i64,
    id: &str,
    recurring: bool,
    timezone: SchedulerTimeZone,
) -> Option<i64> {
    let nominal = spec.next_after(after_ms, timezone)?;
    let fraction = id_fraction(id);
    if recurring {
        let following = spec.next_after(nominal, timezone)?;
        let period = following.saturating_sub(nominal);
        let jitter = ((period as f64) * RECURRING_JITTER_FRACTION * fraction) as i64;
        Some(nominal.saturating_add(jitter.min(RECURRING_JITTER_CAP_MS)))
    } else {
        let minute = timezone.local_parts(nominal).ok()?.minute;
        if minute % 30 != 0 {
            return Some(nominal);
        }
        let jitter = (ONE_SHOT_JITTER_MAX_MS as f64 * fraction) as i64;
        Some(nominal.saturating_sub(jitter).max(created_at_ms))
    }
}

fn id_fraction(id: &str) -> f64 {
    u32::from_str_radix(id.get(..8).unwrap_or_default(), 16)
        .map(|value| value as f64 / (u32::MAX as f64 + 1.0))
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone};

    fn utc_ms(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn cron_parser_matches_exact_field_contract() {
        let spec = CronSpec::parse("*/15 9-17 * * 1-5").unwrap();
        assert_eq!(spec.minute, vec![0, 15, 30, 45]);
        assert_eq!(spec.hour, (9..=17).collect::<Vec<_>>());
        assert_eq!(spec.day_of_week, vec![1, 2, 3, 4, 5]);
        assert!(CronSpec::parse("* * * *").is_err());
        assert!(CronSpec::parse("*/0 * * * *").is_err());
        assert!(CronSpec::parse("0 0 * JAN *").is_err());
        assert_eq!(CronSpec::parse("0 0 * * 7").unwrap().day_of_week, vec![0]);
    }

    #[test]
    fn dom_and_dow_use_cron_or_semantics() {
        let tz = SchedulerTimeZone::Named(Tz::UTC);
        let spec = CronSpec::parse("0 0 13 * 1").unwrap();
        let after = utc_ms(2026, 4, 12, 23, 59);
        assert_eq!(spec.next_after(after, tz), Some(utc_ms(2026, 4, 13, 0, 0)));
        let after = utc_ms(2026, 4, 13, 0, 0);
        assert_eq!(spec.next_after(after, tz), Some(utc_ms(2026, 4, 20, 0, 0)));
    }

    #[test]
    fn timezone_scan_handles_dst_gap_and_fold() {
        let tz = SchedulerTimeZone::named("America/New_York").unwrap();
        let gap = CronSpec::parse("30 2 * * *").unwrap();
        let before_gap = utc_ms(2026, 3, 8, 5, 0);
        assert_eq!(
            gap.next_after(before_gap, tz),
            Some(utc_ms(2026, 3, 9, 6, 30))
        );

        let fold = CronSpec::parse("30 1 * * *").unwrap();
        let before_fold = utc_ms(2026, 11, 1, 4, 0);
        let first = fold.next_after(before_fold, tz).unwrap();
        let second = fold.next_after(first, tz).unwrap();
        assert_eq!(first, utc_ms(2026, 11, 1, 5, 30));
        assert_eq!(second, utc_ms(2026, 11, 1, 6, 30));
    }

    /// The oracle the skipping scan has to agree with: the minute-by-minute walk
    /// `next_after` used to be. Test-only — the production path has no fallback.
    fn naive_next_after(
        spec: &CronSpec,
        after_ms: i64,
        timezone: SchedulerTimeZone,
    ) -> Option<i64> {
        let mut candidate = after_ms
            .div_euclid(MINUTE_MS)
            .saturating_add(1)
            .saturating_mul(MINUTE_MS);
        for _ in 0..MAX_SCAN_MINUTES {
            if spec.matches(timezone.local_parts(candidate).ok()?) {
                return Some(candidate);
            }
            candidate = candidate.saturating_add(MINUTE_MS);
        }
        None
    }

    #[test]
    fn field_skipping_agrees_with_a_minute_by_minute_scan() {
        // Each zone is paired with its own transitions: New York shifts a whole
        // hour, Lord Howe only thirty minutes, which is what a skip measured in
        // local fields gets wrong if it ignores the offset.
        let anchors = [
            (
                SchedulerTimeZone::Named(Tz::UTC),
                utc_ms(2026, 6, 17, 9, 12),
            ),
            (
                SchedulerTimeZone::named("America/New_York").unwrap(),
                utc_ms(2026, 3, 8, 4, 0),
            ),
            (
                SchedulerTimeZone::named("America/New_York").unwrap(),
                utc_ms(2026, 11, 1, 3, 0),
            ),
            (
                SchedulerTimeZone::named("Australia/Lord_Howe").unwrap(),
                utc_ms(2026, 4, 4, 14, 0),
            ),
            (
                SchedulerTimeZone::named("Australia/Lord_Howe").unwrap(),
                utc_ms(2026, 10, 3, 14, 0),
            ),
        ];
        let cases = [
            ("*/15 * * * *", 40),
            ("0 9 * * 1-5", 8),
            ("0 0 * * 0", 3),
            ("30 3 1 * *", 2),
            ("0 0 13 * 5", 6),
        ];
        for (timezone, anchor) in anchors {
            for (source, rounds) in cases {
                let spec = CronSpec::parse(source).unwrap();
                let mut after = anchor;
                for _ in 0..rounds {
                    let expected = naive_next_after(&spec, after, timezone);
                    assert_eq!(
                        spec.next_after(after, timezone),
                        expected,
                        "{source} after {after} in {timezone:?}"
                    );
                    let Some(next) = expected else { break };
                    after = next;
                }
            }
        }

        // Scans that span most of the window, including one that never matches.
        let timezone = SchedulerTimeZone::Named(Tz::UTC);
        for (source, after) in [
            ("0 0 1 3 *", utc_ms(2026, 1, 15, 0, 0)),
            ("0 0 29 2 *", utc_ms(2027, 3, 1, 0, 0)),
            ("0 0 30 2 *", utc_ms(2026, 1, 1, 0, 0)),
        ] {
            let spec = CronSpec::parse(source).unwrap();
            assert_eq!(
                spec.next_after(after, timezone),
                naive_next_after(&spec, after, timezone),
                "{source} after {after}"
            );
        }
    }

    #[test]
    fn never_matching_cron_is_rejected_without_walking_the_year() {
        let clock = ManualClock::new(utc_ms(2026, 8, 3, 12, 0));
        let scheduler = Scheduler::with_clock(
            Arc::new(Inbox::default()),
            None,
            clock,
            SchedulerTimeZone::Named(Tz::UTC),
        );
        scheduler.bind_owner("owner-a").unwrap();
        for recurring in [false, true] {
            let started = std::time::Instant::now();
            let error = scheduler
                .create("0 0 30 2 *", "never", recurring, false)
                .unwrap_err()
                .to_string();
            let elapsed = started.elapsed();
            assert_eq!(
                error,
                "Cron expression '0 0 30 2 *' does not match any calendar date in the next year."
            );
            assert!(elapsed < Duration::from_millis(100), "{elapsed:?}");
        }
    }

    #[tokio::test]
    async fn only_a_session_holding_durable_jobs_polls_the_store() {
        let root = std::env::temp_dir().join(format!(
            "kloop-scheduler-poll-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let path = root.join("scheduled_tasks.json");
        let clock = ManualClock::new(utc_ms(2026, 8, 3, 12, 0));
        let inbox = Arc::new(Inbox::default());
        let scheduler = Scheduler::with_clock(
            Arc::clone(&inbox),
            Some(DurableStore::new(path.clone(), "project-a".into())),
            clock.clone(),
            SchedulerTimeZone::Named(Tz::UTC),
        );
        scheduler.bind_owner("owner-a").unwrap();
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert!(inbox.drain().is_empty());

        // No durable job: the worker is parked on `notify_change`, so a store it
        // could not even parse goes unnoticed however far the clock moves.
        fs::create_dir_all(&root).unwrap();
        fs::write(&path, b"{bad json").unwrap();
        clock.advance(Duration::from_secs(600));
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(inbox.drain().is_empty());

        // One durable job later, the same corrupt store is read within a poll.
        fs::remove_file(&path).unwrap();
        scheduler
            .create("0 13 * * *", "durable", true, true)
            .unwrap();
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        inbox.drain();
        fs::write(&path, b"{bad json").unwrap();
        let mut activity = inbox.subscribe_activity();
        clock.advance(Duration::from_millis(STORE_POLL_MS as u64));
        tokio::time::timeout(Duration::from_secs(1), activity.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(
            inbox
                .drain()
                .iter()
                .any(|item| matches!(item, InboxItem::SchedulerFailure { .. }))
        );

        scheduler.shutdown().await;
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn month_end_and_leap_year_are_scanned() {
        let tz = SchedulerTimeZone::Named(Tz::UTC);
        let leap = CronSpec::parse("0 0 29 2 *").unwrap();
        assert_eq!(
            leap.next_after(utc_ms(2027, 3, 1, 0, 0), tz),
            Some(utc_ms(2028, 2, 29, 0, 0))
        );
        let impossible = CronSpec::parse("0 0 31 2 *").unwrap();
        assert_eq!(impossible.next_after(utc_ms(2026, 1, 1, 0, 0), tz), None);
        assert!(NaiveDate::from_ymd_opt(2028, 2, 29).is_some());
    }

    #[tokio::test]
    async fn manual_clock_fires_one_shot_and_rearms_recurring() {
        let start = utc_ms(2026, 8, 3, 12, 0);
        let clock = ManualClock::new(start);
        let inbox = Arc::new(Inbox::default());
        let scheduler = Scheduler::with_clock(
            Arc::clone(&inbox),
            None,
            clock.clone(),
            SchedulerTimeZone::Named(Tz::UTC),
        );
        scheduler.bind_owner("owner-a").unwrap();
        let one_shot = scheduler
            .create("1 12 * * *", "once", false, false)
            .unwrap();
        let recurring = scheduler
            .create("*/5 * * * *", "again", true, false)
            .unwrap();
        clock.set(one_shot.next_fire_at_ms.max(recurring.next_fire_at_ms));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let messages = inbox.drain();
        assert_eq!(messages.len(), 2);
        let jobs = scheduler.list().unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].recurring);
        scheduler.shutdown().await;
    }

    #[tokio::test]
    async fn dynamic_wakeup_replaces_and_stop_only_cancels_loop_job() {
        let start = utc_ms(2026, 8, 3, 12, 0);
        let clock = ManualClock::new(start);
        let inbox = Arc::new(Inbox::default());
        let scheduler =
            Scheduler::with_clock(inbox, None, clock, SchedulerTimeZone::Named(Tz::UTC));
        scheduler.bind_owner("owner-a").unwrap();
        scheduler
            .create("0 13 * * *", "fixed", true, false)
            .unwrap();
        let first = scheduler
            .schedule_wakeup(1.0, "first", "/loop work")
            .unwrap();
        assert!(first.was_clamped);
        let second = scheduler
            .schedule_wakeup(3_700.0, "second", "/loop work")
            .unwrap();
        assert_eq!(second.cancelled_wakeups, 1);
        assert_eq!(second.clamped_delay_seconds, 3_600);
        let stopped = scheduler.stop_wakeup().unwrap();
        assert_eq!(stopped.cancelled_wakeups, 1);
        assert_eq!(scheduler.list().unwrap().len(), 1);
        scheduler.shutdown().await;
    }

    #[test]
    fn durable_store_is_atomic_owner_partitioned_and_fail_closed() {
        let root = std::env::temp_dir().join(format!(
            "kloop-scheduler-store-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let path = root.join("scheduled_tasks.json");
        let store = DurableStore::new(path.clone(), "project-a".into());
        store
            .transaction(|jobs| {
                jobs.push(ScheduledJob {
                    id: "c0ffee58".into(),
                    owner: "owner-a".into(),
                    cron: "0 0 * * *".into(),
                    prompt: "persist".into(),
                    created_at_ms: 1,
                    last_fired_at_ms: None,
                    next_fire_at_ms: 2,
                    recurring: false,
                    durable: true,
                    kind: ScheduledKind::Cron,
                    reason: None,
                    generation: 1,
                });
                Ok(())
            })
            .unwrap();
        assert_eq!(store.load().unwrap().len(), 1);
        fs::write(&path, b"{bad json").unwrap();
        assert!(store.load().is_err());
        assert_eq!(fs::read(&path).unwrap(), b"{bad json");
        assert!(fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn jitter_is_deterministic_and_bounded() {
        let timezone = SchedulerTimeZone::Named(Tz::UTC);
        let hourly = CronSpec::parse("0 * * * *").unwrap();
        let after = utc_ms(2026, 8, 3, 12, 1);
        let low = next_cron_fire(&hourly, after, after, "00000000", true, timezone).unwrap();
        let high = next_cron_fire(&hourly, after, after, "ffffffff", true, timezone).unwrap();
        assert_eq!(low, utc_ms(2026, 8, 3, 13, 0));
        assert!(high >= low);
        assert!(high.saturating_sub(low) < 6 * 60 * 1_000);
        assert_eq!(
            next_cron_fire(&hourly, after, after, "ffffffff", true, timezone),
            Some(high)
        );

        let one_shot = CronSpec::parse("30 13 * * *").unwrap();
        let nominal = utc_ms(2026, 8, 3, 13, 30);
        let early = next_cron_fire(&one_shot, after, after, "ffffffff", false, timezone).unwrap();
        assert!(early <= nominal);
        assert!(nominal.saturating_sub(early) <= ONE_SHOT_JITTER_MAX_MS);
    }

    #[tokio::test]
    async fn only_late_durable_one_shots_require_confirmation() {
        let root = std::env::temp_dir().join(format!(
            "kloop-scheduler-missed-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let start = utc_ms(2026, 8, 3, 12, 0);
        let clock = ManualClock::new(start);
        let inbox = Arc::new(Inbox::default());
        let store = DurableStore::new(root.join("scheduled_tasks.json"), "project-a".into());
        let scheduler = Scheduler::with_clock(
            Arc::clone(&inbox),
            Some(store),
            clock.clone(),
            SchedulerTimeZone::Named(Tz::UTC),
        );
        scheduler.bind_owner("owner-a").unwrap();
        let durable = scheduler
            .create("1 12 * * *", "durable once", false, true)
            .unwrap();
        let session = scheduler
            .create("1 12 * * *", "session once", false, false)
            .unwrap();
        let recurring = scheduler
            .create("1 12 * * *", "recurring", true, false)
            .unwrap();
        let latest = durable
            .next_fire_at_ms
            .max(session.next_fire_at_ms)
            .max(recurring.next_fire_at_ms);
        let mut activity = inbox.subscribe_activity();
        clock.set(latest.saturating_add(MINUTE_MS));
        tokio::time::timeout(Duration::from_secs(1), activity.changed())
            .await
            .unwrap()
            .unwrap();
        tokio::task::yield_now().await;

        let mut classifications = inbox
            .drain()
            .into_iter()
            .filter_map(|item| match item {
                InboxItem::ScheduledPrompt { prompt, missed, .. } => Some((prompt, missed)),
                InboxItem::SchedulerFailure { summary } => panic!("unexpected failure: {summary}"),
                _ => None,
            })
            .collect::<Vec<_>>();
        classifications.sort();
        assert_eq!(
            classifications,
            vec![
                ("durable once".into(), true),
                ("recurring".into(), false),
                ("session once".into(), false),
            ]
        );
        scheduler.shutdown().await;
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn durable_claim_is_owner_scoped_and_process_safe() {
        let root = std::env::temp_dir().join(format!(
            "kloop-scheduler-claim-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let start = utc_ms(2026, 8, 3, 12, 0);
        let clock = ManualClock::new(start);
        let store = DurableStore::new(root.join("scheduled_tasks.json"), "project-a".into());
        let inbox_a1 = Arc::new(Inbox::default());
        let inbox_a2 = Arc::new(Inbox::default());
        let inbox_b = Arc::new(Inbox::default());
        let a1 = Scheduler::with_clock(
            Arc::clone(&inbox_a1),
            Some(store.clone()),
            clock.clone(),
            SchedulerTimeZone::Named(Tz::UTC),
        );
        let a2 = Scheduler::with_clock(
            Arc::clone(&inbox_a2),
            Some(store.clone()),
            clock.clone(),
            SchedulerTimeZone::Named(Tz::UTC),
        );
        let b = Scheduler::with_clock(
            Arc::clone(&inbox_b),
            Some(store),
            clock.clone(),
            SchedulerTimeZone::Named(Tz::UTC),
        );
        a1.bind_owner("owner-a").unwrap();
        a2.bind_owner("owner-a").unwrap();
        b.bind_owner("owner-b").unwrap();
        let job = a1.create("1 12 * * *", "claim once", false, true).unwrap();

        clock.set(job.next_fire_at_ms);
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let a_fires = inbox_a1.drain().len() + inbox_a2.drain().len();
        assert_eq!(a_fires, 1);
        assert!(inbox_b.drain().is_empty());
        assert!(a1.list().unwrap().is_empty());
        assert!(a2.list().unwrap().is_empty());

        a1.shutdown().await;
        a2.shutdown().await;
        b.shutdown().await;
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn shutdown_drops_session_jobs_but_preserves_durable_jobs() {
        let root = std::env::temp_dir().join(format!(
            "kloop-scheduler-shutdown-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let clock = ManualClock::new(utc_ms(2026, 8, 3, 12, 0));
        let store = DurableStore::new(root.join("scheduled_tasks.json"), "project-a".into());
        let first = Scheduler::with_clock(
            Arc::new(Inbox::default()),
            Some(store.clone()),
            clock.clone(),
            SchedulerTimeZone::Named(Tz::UTC),
        );
        first.bind_owner("owner-a").unwrap();
        first.create("0 13 * * *", "session", true, false).unwrap();
        first.create("0 13 * * *", "durable", true, true).unwrap();
        first.shutdown().await;

        let resumed = Scheduler::with_clock(
            Arc::new(Inbox::default()),
            Some(store),
            clock,
            SchedulerTimeZone::Named(Tz::UTC),
        );
        resumed.bind_owner("owner-a").unwrap();
        let jobs = resumed.list().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].prompt, "durable");
        resumed.shutdown().await;
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn corrupt_durable_store_does_not_discard_session_due_work_or_spam_failures() {
        let root = std::env::temp_dir().join(format!(
            "kloop-scheduler-failure-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let path = root.join("scheduled_tasks.json");
        let clock = ManualClock::new(utc_ms(2026, 8, 3, 12, 0));
        let inbox = Arc::new(Inbox::default());
        let scheduler = Scheduler::with_clock(
            Arc::clone(&inbox),
            Some(DurableStore::new(path.clone(), "project-a".into())),
            clock.clone(),
            SchedulerTimeZone::Named(Tz::UTC),
        );
        scheduler.bind_owner("owner-a").unwrap();
        let session = scheduler
            .create("1 12 * * *", "session survives", false, false)
            .unwrap();
        scheduler
            .create("0 13 * * *", "durable future", true, true)
            .unwrap();
        fs::write(&path, b"{bad json").unwrap();

        let mut activity = inbox.subscribe_activity();
        clock.set(session.next_fire_at_ms);
        tokio::time::timeout(Duration::from_secs(1), activity.changed())
            .await
            .unwrap()
            .unwrap();
        for _ in 0..3 {
            clock.advance(Duration::from_secs(1));
            tokio::task::yield_now().await;
        }
        let items = inbox.drain();
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item, InboxItem::ScheduledPrompt { .. }))
                .count(),
            1
        );
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item, InboxItem::SchedulerFailure { .. }))
                .count(),
            1
        );
        assert!(items.iter().any(|item| matches!(
            item,
            InboxItem::ScheduledPrompt { prompt, .. } if prompt == "session survives"
        )));
        assert_eq!(fs::read(&path).unwrap(), b"{bad json");

        scheduler.shutdown().await;
        fs::remove_dir_all(root).unwrap();
    }
}
