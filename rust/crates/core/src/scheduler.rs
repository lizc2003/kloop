use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::inbox::{Inbox, InboxItem, ScheduledOrigin};

mod cron;
mod store;
#[cfg(test)]
mod testutil;

pub use cron::{CronSpec, SchedulerTimeZone};
pub use store::DurableStore;

const MAX_JOBS: usize = 50;
const MINUTE_MS: i64 = 60_000;
const RECURRING_MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
const RECURRING_JITTER_FRACTION: f64 = 0.10;
const RECURRING_JITTER_CAP_MS: i64 = 15 * 60 * 1_000;
const ONE_SHOT_JITTER_MAX_MS: i64 = 90 * 1_000;
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
    /// A shell command run at fire time, before the prompt is delivered: exit
    /// 0 skips this fire, anything else (including a check that could not run)
    /// delivers the prompt with the check's output attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
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

/// Runs a job's `check` at fire time. The scheduler owns *when*; the runner
/// owns *how* — the bash permission gate and the sandbox live with the tools,
/// and a check faces them on every fire exactly as a model bash call would.
pub trait CheckRunner: Send + Sync {
    fn run<'a>(
        &'a self,
        command: &'a str,
    ) -> Pin<Box<dyn Future<Output = CheckOutcome> + Send + 'a>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    /// Exit 0: nothing to wake the model for.
    Passed,
    /// The command ran and did not succeed; its model-facing output.
    Failed(String),
    /// No verdict at all: refused by the gate, failed to spawn, or no runner.
    /// Delivered like a failure — a check that silently stops running would
    /// otherwise be a job that silently stopped firing.
    Unavailable(String),
}

impl CheckOutcome {
    /// The note appended to the delivered prompt; `None` means do not deliver.
    fn note(&self, command: &str) -> Option<String> {
        match self {
            Self::Passed => None,
            Self::Failed(output) => Some(format!("[scheduled check `{command}` failed]\n{output}")),
            Self::Unavailable(reason) => Some(format!(
                "[scheduled check `{command}` could not run: {reason}]"
            )),
        }
    }
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
    /// Holds the session's Config (for the bash gate), and the Config holds this
    /// scheduler: [`Scheduler::shutdown`] takes it out to break that cycle.
    check_runner: Option<Arc<dyn CheckRunner>>,
    /// One running check per job id; a fire that finds its job's check still
    /// running is skipped rather than queued or run twice.
    checks_running: HashMap<String, tokio::task::AbortHandle>,
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
                check_runner: None,
                checks_running: HashMap::new(),
            }),
            inbox,
            store,
            clock,
            timezone,
            changes,
        })
    }

    /// The scheduler for a session that replaces this one (`/clear`, rewind):
    /// same durable store, clock and time zone, delivering into the new
    /// session's inbox, not yet bound to an owner. [`Scheduler::bind_owner`]
    /// refuses to change owners, so a new session never reuses the old one;
    /// the old session's durable tasks stay under its id.
    pub fn successor(&self, inbox: Arc<Inbox>) -> Arc<Self> {
        let missed_confirmation_available =
            self.state.lock().unwrap().missed_confirmation_available;
        let successor = Self::with_parts(
            inbox,
            self.store.clone(),
            Arc::clone(&self.clock),
            self.timezone,
        );
        successor
            .state
            .lock()
            .unwrap()
            .missed_confirmation_available = missed_confirmation_available;
        successor
    }

    pub fn set_missed_confirmation_available(&self, available: bool) {
        self.state.lock().unwrap().missed_confirmation_available = available;
        self.notify_change();
    }

    /// Install the runner for `check` commands unless one is already bound. A
    /// fire before any runner exists delivers with a "could not run" note.
    pub fn bind_check_runner(&self, runner: impl FnOnce() -> Arc<dyn CheckRunner>) {
        let mut state = self.state.lock().unwrap();
        if state.check_runner.is_none() && !state.closed {
            state.check_runner = Some(runner());
        }
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
        check: Option<&str>,
    ) -> Result<ScheduledJob> {
        let owner = self.owner()?;
        if prompt.is_empty() {
            bail!("cron_create: prompt must not be empty");
        }
        if check.is_some_and(|check| check.trim().is_empty()) {
            bail!("cron_create: check must not be empty when given");
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
        // The fire time carries the jitter, and the jitter is the id's, so a
        // re-rolled id (below) has to be recomputed with — not just re-stamped
        // onto — the time already derived.
        let fire_at = |id: &str| {
            next_cron_fire(&spec, now, now, id, recurring, self.timezone).ok_or_else(|| {
                anyhow!(
                    "Cron expression '{cron}' does not match any calendar date in the next year."
                )
            })
        };
        let next_fire_at_ms = fire_at(&id)?;
        let mut job = ScheduledJob {
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
            check: check.map(str::to_string),
        };
        if durable {
            self.store.as_ref().unwrap().transaction(|jobs| {
                while jobs.iter().any(|existing| existing.id == job.id) {
                    job.id = crate::resource_id::fresh("job-")?;
                    job.next_fire_at_ms = fire_at(&job.id)?;
                }
                jobs.push(job.clone());
                Ok(())
            })?;
        } else {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                bail!("scheduler is closed");
            }
            while state
                .session_jobs
                .iter()
                .any(|existing| existing.id == job.id)
            {
                job.id = crate::resource_id::fresh("job-")?;
                job.next_fire_at_ms = fire_at(&job.id)?;
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
        let mut job = ScheduledJob {
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
            check: None,
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
            while state
                .session_jobs
                .iter()
                .any(|existing| existing.id == job.id)
            {
                job.id = crate::resource_id::fresh("job-")?;
            }
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
            state.check_runner = None;
            for (_, check) in state.checks_running.drain() {
                check.abort();
            }
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
        Ok(crate::resource_id::fresh("job-")?)
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
                match job.check.clone() {
                    None => self.deliver(job, missed, None),
                    Some(command) => self.start_check(job, missed, command),
                }
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

    fn deliver(&self, job: ScheduledJob, missed: bool, note: Option<String>) {
        let prompt = match note {
            Some(note) => format!("{}\n\n{note}", job.prompt),
            None => job.prompt,
        };
        self.inbox.push(InboxItem::ScheduledPrompt {
            id: job.id,
            origin: match job.kind {
                ScheduledKind::Cron => ScheduledOrigin::Cron,
                ScheduledKind::LoopWakeup => ScheduledOrigin::LoopWakeup,
            },
            scheduled_for_ms: job.next_fire_at_ms,
            reason: job.reason,
            prompt,
            missed,
        });
    }

    /// Run the job's check off the worker loop (a check can take its whole
    /// timeout, and the loop still owns every other job's fire), then deliver
    /// or not on its outcome.
    fn start_check(self: &Arc<Self>, job: ScheduledJob, missed: bool, command: String) {
        // The lock is held across the spawn so the task's own removal of its
        // entry cannot run before the entry is inserted.
        let mut state = self.state.lock().unwrap();
        if state.checks_running.contains_key(&job.id) {
            return;
        }
        let Some(runner) = state.check_runner.clone() else {
            drop(state);
            let note = CheckOutcome::Unavailable("no check runner is bound to this session".into())
                .note(&command);
            self.deliver(job, missed, note);
            return;
        };
        let id = job.id.clone();
        let scheduler = Arc::clone(self);
        let task = tokio::spawn(async move {
            let outcome = runner.run(&command).await;
            let closed = {
                let mut state = scheduler.state.lock().unwrap();
                state.checks_running.remove(&job.id);
                state.closed
            };
            if let Some(note) = outcome.note(&command)
                && !closed
            {
                scheduler.deliver(job, missed, Some(note));
            }
        });
        state.checks_running.insert(id, task.abort_handle());
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
    let fraction = crate::resource_id::fraction(id);
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

#[cfg(test)]
mod tests {
    use super::testutil::{TEMP_SEQ, utc_ms};
    use super::*;
    use chrono_tz::Tz;
    use std::fs;
    use std::sync::atomic::AtomicUsize;

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
                .create("0 0 30 2 *", "never", recurring, false, None)
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
            .create("0 13 * * *", "durable", true, true, None)
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
            .create("1 12 * * *", "once", false, false, None)
            .unwrap();
        let recurring = scheduler
            .create("*/5 * * * *", "again", true, false, None)
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

    /// A check that answers `outcome` every time, counting its runs; with a
    /// gate it parks each run until the gate is notified.
    struct ScriptedCheck {
        outcome: CheckOutcome,
        calls: AtomicUsize,
        gate: Option<tokio::sync::Notify>,
    }

    impl ScriptedCheck {
        fn new(outcome: CheckOutcome) -> Arc<Self> {
            Arc::new(Self {
                outcome,
                calls: AtomicUsize::new(0),
                gate: None,
            })
        }

        fn gated(outcome: CheckOutcome) -> Arc<Self> {
            Arc::new(Self {
                outcome,
                calls: AtomicUsize::new(0),
                gate: Some(tokio::sync::Notify::new()),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl CheckRunner for ScriptedCheck {
        fn run<'a>(
            &'a self,
            _command: &'a str,
        ) -> Pin<Box<dyn Future<Output = CheckOutcome> + Send + 'a>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if let Some(gate) = &self.gate {
                    gate.notified().await;
                }
                self.outcome.clone()
            })
        }
    }

    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    /// A bound scheduler with one recurring `check` job; the runner, when
    /// given, is bound before anything fires.
    fn checked_scheduler(
        runner: Option<Arc<ScriptedCheck>>,
    ) -> (Arc<Scheduler>, Arc<Inbox>, Arc<ManualClock>, ScheduledJob) {
        let clock = ManualClock::new(utc_ms(2026, 8, 3, 12, 0));
        let inbox = Arc::new(Inbox::default());
        let scheduler = Scheduler::with_clock(
            Arc::clone(&inbox),
            None,
            clock.clone(),
            SchedulerTimeZone::Named(Tz::UTC),
        );
        scheduler.bind_owner("owner-a").unwrap();
        if let Some(runner) = runner {
            scheduler.bind_check_runner(|| runner);
        }
        let job = scheduler
            .create("*/5 * * * *", "watch CI", true, false, Some("probe"))
            .unwrap();
        (scheduler, inbox, clock, job)
    }

    fn delivered(job: &ScheduledJob, prompt: &str) -> InboxItem {
        InboxItem::ScheduledPrompt {
            id: job.id.clone(),
            origin: ScheduledOrigin::Cron,
            scheduled_for_ms: job.next_fire_at_ms,
            reason: None,
            prompt: prompt.into(),
            missed: false,
        }
    }

    #[tokio::test]
    async fn a_passing_check_skips_the_fire_and_rearms_the_job() {
        let runner = ScriptedCheck::new(CheckOutcome::Passed);
        let (scheduler, inbox, clock, job) = checked_scheduler(Some(Arc::clone(&runner)));
        clock.set(job.next_fire_at_ms);
        settle().await;
        assert_eq!(runner.calls(), 1);
        assert!(inbox.drain().is_empty());
        let jobs = scheduler.list().unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].next_fire_at_ms > job.next_fire_at_ms);
        scheduler.shutdown().await;
    }

    #[tokio::test]
    async fn a_failing_check_delivers_the_prompt_with_its_output() {
        let runner = ScriptedCheck::new(CheckOutcome::Failed("red\n[exit status 1]".into()));
        let (scheduler, inbox, clock, job) = checked_scheduler(Some(runner));
        clock.set(job.next_fire_at_ms);
        settle().await;
        assert_eq!(
            inbox.drain(),
            vec![delivered(
                &job,
                "watch CI\n\n[scheduled check `probe` failed]\nred\n[exit status 1]"
            )]
        );
        scheduler.shutdown().await;
    }

    #[tokio::test]
    async fn a_check_that_cannot_run_still_delivers() {
        let runner = ScriptedCheck::new(CheckOutcome::Unavailable("blocked: denied".into()));
        let (scheduler, inbox, clock, job) = checked_scheduler(Some(runner));
        clock.set(job.next_fire_at_ms);
        settle().await;
        assert_eq!(
            inbox.drain(),
            vec![delivered(
                &job,
                "watch CI\n\n[scheduled check `probe` could not run: blocked: denied]"
            )]
        );
        scheduler.shutdown().await;

        // No runner bound at all is the same kind of "no verdict".
        let (scheduler, inbox, clock, job) = checked_scheduler(None);
        clock.set(job.next_fire_at_ms);
        settle().await;
        assert_eq!(
            inbox.drain(),
            vec![delivered(
                &job,
                "watch CI\n\n[scheduled check `probe` could not run: no check runner is bound to this session]"
            )]
        );
        scheduler.shutdown().await;
    }

    #[tokio::test]
    async fn a_fire_that_finds_its_check_still_running_is_skipped() {
        let runner = ScriptedCheck::gated(CheckOutcome::Passed);
        let (scheduler, inbox, clock, job) = checked_scheduler(Some(Arc::clone(&runner)));
        clock.set(job.next_fire_at_ms);
        settle().await;
        assert_eq!(runner.calls(), 1);

        let second = scheduler.list().unwrap()[0].next_fire_at_ms;
        clock.set(second);
        settle().await;
        assert_eq!(
            runner.calls(),
            1,
            "the second fire must not start a second run"
        );

        runner.gate.as_ref().unwrap().notify_one();
        settle().await;
        let third = scheduler.list().unwrap()[0].next_fire_at_ms;
        assert!(third > second, "the skipped fire still rearms the job");
        clock.set(third);
        settle().await;
        assert_eq!(runner.calls(), 2);
        assert!(inbox.drain().is_empty());
        scheduler.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_releases_the_runner_and_aborts_a_running_check() {
        let runner = ScriptedCheck::gated(CheckOutcome::Failed("late".into()));
        let (scheduler, inbox, clock, job) = checked_scheduler(Some(Arc::clone(&runner)));
        clock.set(job.next_fire_at_ms);
        settle().await;
        assert_eq!(runner.calls(), 1);
        // Held by the test, the scheduler, and the parked check task.
        assert_eq!(Arc::strong_count(&runner), 3);

        scheduler.shutdown().await;
        settle().await;
        // The runner holds the session Config in production, and the Config
        // holds the scheduler: only this release breaks that cycle.
        assert_eq!(Arc::strong_count(&runner), 1);
        assert!(inbox.drain().is_empty());
    }

    #[test]
    fn a_durable_check_round_trips_through_the_store() {
        let root = std::env::temp_dir().join(format!(
            "kloop-scheduler-check-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let store = DurableStore::new(root.join("scheduled_tasks.json"), "project-a".into());
        let first = Scheduler::with_clock(
            Arc::new(Inbox::default()),
            Some(store.clone()),
            ManualClock::new(utc_ms(2026, 8, 3, 12, 0)),
            SchedulerTimeZone::Named(Tz::UTC),
        );
        first.bind_owner("owner-a").unwrap();
        let job = first
            .create("0 13 * * *", "watch CI", true, true, Some("gh run list"))
            .unwrap();
        assert_eq!(store.load().unwrap(), vec![job]);
        assert!(
            first
                .create("0 13 * * *", "x", true, true, Some("  "))
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
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
            .create("0 13 * * *", "fixed", true, false, None)
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
    fn jitter_is_deterministic_and_bounded() {
        let timezone = SchedulerTimeZone::Named(Tz::UTC);
        // Real job ids: the jitter is a function of the id the scheduler stores,
        // so a fixture in some other shape tests nothing about what runs.
        let ids = ["job-11111111111", "job-jpXCZedGfVQ", "job-7hKq2mV9Qx4"];
        let hourly = CronSpec::parse("0 * * * *").unwrap();
        let after = utc_ms(2026, 8, 3, 12, 1);
        let nominal = utc_ms(2026, 8, 3, 13, 0);
        for id in ids {
            let fire = next_cron_fire(&hourly, after, after, id, true, timezone).unwrap();
            assert!(fire >= nominal, "{id}: {fire}");
            assert!(
                fire.saturating_sub(nominal) < 6 * 60 * 1_000,
                "{id}: {fire}"
            );
            assert_eq!(
                next_cron_fire(&hourly, after, after, id, true, timezone),
                Some(fire),
                "{id}: an id must always land on the same instant"
            );
        }

        let one_shot = CronSpec::parse("30 13 * * *").unwrap();
        let nominal = utc_ms(2026, 8, 3, 13, 30);
        for id in ids {
            let early = next_cron_fire(&one_shot, after, after, id, false, timezone).unwrap();
            assert!(early <= nominal, "{id}: {early}");
            assert!(
                nominal.saturating_sub(early) <= ONE_SHOT_JITTER_MAX_MS,
                "{id}: {early}"
            );
        }
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
            .create("1 12 * * *", "durable once", false, true, None)
            .unwrap();
        let session = scheduler
            .create("1 12 * * *", "session once", false, false, None)
            .unwrap();
        let recurring = scheduler
            .create("1 12 * * *", "recurring", true, false, None)
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
        let job = a1
            .create("1 12 * * *", "claim once", false, true, None)
            .unwrap();

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
        first
            .create("0 13 * * *", "session", true, false, None)
            .unwrap();
        first
            .create("0 13 * * *", "durable", true, true, None)
            .unwrap();
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
            .create("1 12 * * *", "session survives", false, false, None)
            .unwrap();
        scheduler
            .create("0 13 * * *", "durable future", true, true, None)
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
