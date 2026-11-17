//! 线程持久化、恢复、fork、公开快照与工作者启动。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;

use kloop_core::agent::Ui;
use kloop_core::event::Event;
use kloop_core::history::History;
use kloop_core::interaction::Questioner;
use kloop_core::provider_route::ProviderCatalog;
use kloop_core::rollout;
use kloop_core::rollout::Rollout;
use kloop_core::rollout::SessionSnapshot;
use kloop_core::session_store::SessionDirs;
use kloop_protocol::ActiveProviderRoute;
use kloop_protocol::clip_chars;

use crate::ConfigFactory;
use crate::PendingInteractions;
use crate::RequestId;
use crate::Server;
use crate::ThreadBuild;
use crate::ThreadHandle;
use crate::ThreadReply;
use crate::ThreadStartOptions;
use crate::ThreadWorkerState;
use crate::events;
use crate::events::EventsSyncParams;
use crate::events::RecoverySource;
use crate::events::ThreadProjection;
use crate::params::MethodResult;
use crate::params::fork_origin_json;
use crate::params::fork_origin_value;
use crate::params::parse_thread_start_options;
use crate::params::public_route_from_record;
use crate::params::resume_options;
use crate::params::runtime_from_options;
use crate::params::str_param;
use crate::thread_ui::ThreadUi;
use crate::wire;
use crate::wire::Outgoing;
use crate::worker::thread_worker;

/// `Ok(None)` means the thread's config build moved to the blocking pool; the
/// read loop answers when it lands (see [`crate::ThreadBuild`]).
pub(crate) type DeferredResult = std::result::Result<Option<Value>, (i64, String)>;

impl Server {
    /// Locate a thread's transcript and the partition holding it. A thread
    /// pins its own cwd at `thread/start`, so it can live outside the process
    /// cwd's project while `thread/resume` carries only an id: check this
    /// process's partition first, then the rest.
    pub(super) fn locate_thread(
        &self,
        thread_id: &str,
    ) -> Result<(PathBuf, SessionDirs), (i64, String)> {
        locate_thread_in(thread_id, &self.default_dirs, &self.paths.store)
    }

    pub(crate) fn thread_start(&mut self, request_id: RequestId, params: &Value) -> DeferredResult {
        let options = parse_thread_start_options(params, &self.default_cwd)?;
        let dirs = self.paths.store.ensure(&options.cwd).map_err(|e| {
            (
                wire::SERVER_ERROR,
                format!("cannot create session directory: {e}"),
            )
        })?;
        // Claim the id before writing the runtime: a timestamp collision moves
        // on to the next suffix and never shares another session's file.
        let claim = rollout::claim_new_session(&dirs.sessions)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot claim session: {e}")))?;
        let thread_id = claim.id().to_string();
        let runtime = runtime_from_options(&options);
        // The writer owns the claimed file from here on; its Drop removes a
        // session that never gets past the opening preamble. Cleaning up by
        // path here would be a second delete: another process can reuse the id
        // in the same second, and this call would remove *its* session.
        let rollout =
            Rollout::new_with_runtime_pending_route(claim, runtime.clone()).map_err(|error| {
                (
                    wire::SERVER_ERROR,
                    format!("cannot initialize session: {error}"),
                )
            })?;
        let mut history = History::new(dirs.offload.clone());
        history.attach_rollout(rollout);
        let seed = SessionSnapshot {
            messages: Vec::new(),
            runtime: Some(runtime),
            terminals: Vec::new(),
            provider_routes: history.provider_routes().to_vec(),
        };
        // `spawn_thread` owns `history`; every failure drops it before
        // returning, so the writer removes its own empty file. No second
        // unlink here (see the claim comment above).
        self.spawn_thread(
            request_id,
            thread_id,
            history,
            options,
            RecoverySource::Fresh,
            seed,
            ThreadReply::Started,
            None,
        );
        Ok(None)
    }

    pub(crate) fn thread_resume(
        &mut self,
        request_id: RequestId,
        params: &Value,
    ) -> DeferredResult {
        let thread_id = str_param(params, "thread_id")?;
        if self.threads.contains_key(thread_id) || self.pending_threads.contains(thread_id) {
            return Err((
                wire::SERVER_ERROR,
                format!("thread '{thread_id}' is already active"),
            ));
        }
        let (path, dirs) = self.locate_thread(thread_id)?;
        let inspected = rollout::inspect_session_for_writing(&path)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot read session: {e}")))?;
        let options = resume_options(inspected.snapshot(), params, &self.default_cwd)?;
        let resumed = inspected
            .recover_for_writing()
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot resume: {e}")))?;
        let count = resumed.snapshot.messages.len();
        let seed = resumed.snapshot.clone();
        let history = History::resume(dirs.offload.clone(), resumed);
        self.spawn_thread(
            request_id,
            thread_id.to_string(),
            history,
            options,
            RecoverySource::Resumed,
            seed,
            ThreadReply::Live {
                message_count: count,
            },
            None,
        );
        Ok(None)
    }

    /// Fork a session at a cut point into a fresh thread, then spawn it live
    /// (like `thread/resume`) so the client can `turn/start` on it immediately.
    /// `cut` is optional — omit it to fork at the end. A dormant source is
    /// read directly from disk; an active source is rejected so its in-flight
    /// turn cannot be copied before the terminal record makes the prefix complete.
    pub(crate) fn thread_fork(&mut self, request_id: RequestId, params: &Value) -> DeferredResult {
        let src_id = str_param(params, "thread_id")?;
        if self
            .threads
            .get(src_id)
            .is_some_and(|handle| handle.running.load(Ordering::SeqCst))
        {
            return Err((
                wire::SERVER_ERROR,
                format!("cannot fork thread '{src_id}' while a turn is running"),
            ));
        }
        let cut = match params.get("cut") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_u64().ok_or((
                wire::INVALID_PARAMS,
                "'cut' must be a non-negative integer".to_string(),
            ))?),
        };
        // A fork stays in its source's project: same repository, new branch.
        let (src, dirs) = self.locate_thread(src_id)?;
        rollout::inspect_session(&src)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot read source: {e}")))?;
        let new_path = rollout::fork_session(&src, cut, &dirs.sessions)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot fork: {e}")))?;
        let new_id = rollout::session_id_of(&new_path);
        // Until `spawn_thread` takes the history, every failure must remove the
        // fork: it is `preexisting` to its writer, so nothing else will, and
        // each retry would add another orphan to `thread/list`.
        let fork_guard = kloop_core::cleanup::RemoveFileOnDrop::new(new_path.clone());
        let resumed = match rollout::inspect_session_for_writing(&new_path)
            .and_then(|read| read.recover_for_writing())
        {
            Ok(resumed) => resumed,
            Err(error) => {
                return Err((wire::SERVER_ERROR, format!("cannot resume fork: {error}")));
            }
        };
        let options = resume_options(&resumed.snapshot, params, &self.default_cwd)?;
        let count = resumed.snapshot.messages.len();
        let seed = resumed.snapshot.clone();
        let history = History::resume(dirs.offload.clone(), resumed);
        self.spawn_thread(
            request_id,
            new_id,
            history,
            options,
            RecoverySource::Resumed,
            seed,
            ThreadReply::Live {
                message_count: count,
            },
            Some(fork_guard),
        );
        Ok(None)
    }

    /// `thread/list` is a disk read that can touch hundreds of transcripts, so
    /// it runs on the blocking pool instead of the single read loop. JSON-RPC
    /// responses may arrive out of order; the client matches them by id.
    pub(crate) fn start_thread_list(&mut self, id: RequestId, params: &Value) {
        let (limit, offset) = match parse_thread_list_params(params) {
            Ok(parsed) => parsed,
            Err((code, message)) => {
                return self.send(Outgoing::Error {
                    id: Some(id),
                    code,
                    message,
                    data: None,
                });
            }
        };
        let sessions = self.default_dirs.sessions.clone();
        let catalog = self.provider_catalog.clone();
        let running: HashMap<String, bool> = self
            .threads
            .iter()
            .map(|(thread_id, handle)| (thread_id.clone(), handle.running.load(Ordering::SeqCst)))
            .collect();
        self.spawn_read(id, move || {
            thread_list_blocking(&sessions, &catalog, &running, limit, offset)
        });
    }

    pub(crate) fn start_thread_read(&mut self, id: RequestId, params: &Value) {
        let thread_id = match str_param(params, "thread_id") {
            Ok(thread_id) => thread_id.to_string(),
            Err((code, message)) => {
                return self.send(Outgoing::Error {
                    id: Some(id),
                    code,
                    message,
                    data: None,
                });
            }
        };
        let default_dirs = self.default_dirs.clone();
        let store = self.paths.store.clone();
        let catalog = self.provider_catalog.clone();
        self.spawn_read(id, move || {
            let (path, _) = locate_thread_in(&thread_id, &default_dirs, &store)?;
            thread_read_blocking(thread_id, path, catalog)
        });
    }

    pub(crate) fn thread_events_sync(&self, params: &Value) -> MethodResult {
        let params =
            EventsSyncParams::parse(params).map_err(|message| (wire::INVALID_PARAMS, message))?;
        let handle = self.threads.get(&params.thread_id).ok_or_else(|| {
            (
                wire::INVALID_PARAMS,
                format!(
                    "thread '{}' is not active; resume it before syncing events",
                    params.thread_id
                ),
            )
        })?;
        let sync = handle
            .projection
            .sync(params.event_cursor.as_ref())
            .map_err(|error| (wire::INVALID_PARAMS, error.to_string()))?;
        serde_json::to_value(sync).map_err(|error| {
            (
                wire::SERVER_ERROR,
                format!("cannot encode event sync: {error}"),
            )
        })
    }

    /// Move one thread's config build to the blocking pool (plan 265), then
    /// return to the read loop immediately. The finished handle comes back
    /// through `builds`; `Server::finish_thread_build` registers it and answers
    /// the request.
    #[allow(clippy::too_many_arguments)]
    fn spawn_thread(
        &mut self,
        request_id: RequestId,
        thread_id: String,
        history: History,
        options: ThreadStartOptions,
        recovery_source: RecoverySource,
        seed: SessionSnapshot,
        reply: ThreadReply,
        // A fork owns a fresh file the writer treats as preexisting, so its
        // removal has to travel with the build and be disarmed only once the
        // worker has taken the history.
        guard: Option<kloop_core::cleanup::RemoveFileOnDrop>,
    ) {
        let build = PendingThreadBuild {
            thread_id: thread_id.clone(),
            options: options.clone(),
            recovery_source,
            seed,
            history,
            guard,
            factory: Arc::clone(&self.factory),
            catalog: Arc::clone(&self.provider_catalog),
            out: self.out.clone(),
            pending: self.pending.clone(),
            reverse_request_seq: self.reverse_request_seq.clone(),
            client_questions: self.client_questions,
        };
        let builds = self.builds.clone();
        self.pending_threads.insert(thread_id.clone());
        tokio::task::spawn_blocking(move || {
            let outcome = build.build();
            let _ = builds.send(ThreadBuild {
                request_id,
                thread_id,
                options,
                reply,
                outcome,
            });
        });
    }
}

/// Everything one thread needs before it can exist, moved wholesale to the
/// blocking pool. A whole Config build — project context, sandbox, skills, git
/// probes — is synchronous filesystem work, and keeping it off the read loop is
/// what lets another thread's `turn/interrupt` still be answered (plan 265).
struct PendingThreadBuild {
    thread_id: String,
    options: ThreadStartOptions,
    recovery_source: RecoverySource,
    seed: SessionSnapshot,
    history: History,
    guard: Option<kloop_core::cleanup::RemoveFileOnDrop>,
    factory: ConfigFactory,
    catalog: Arc<ProviderCatalog>,
    out: mpsc::UnboundedSender<Vec<u8>>,
    pending: PendingInteractions,
    reverse_request_seq: Arc<AtomicU64>,
    client_questions: bool,
}

impl PendingThreadBuild {
    fn build(self) -> Result<ThreadHandle, (i64, String)> {
        let PendingThreadBuild {
            thread_id,
            options,
            recovery_source,
            mut seed,
            mut history,
            mut guard,
            factory,
            catalog,
            out,
            pending,
            reverse_request_seq,
            client_questions,
        } = self;
        let projection = Arc::new(
            ThreadProjection::new(
                thread_id.clone(),
                options.cwd.to_string_lossy().to_string(),
                ActiveProviderRoute {
                    revision: 0,
                    provider_id: "pending".into(),
                    api_family: kloop_protocol::ProviderApiFamily::Mock,
                    model: options.model.clone().unwrap_or_default(),
                    continuity: kloop_protocol::ReasoningContinuity::Preserved,
                    effort: None,
                },
                SessionSnapshot {
                    messages: Vec::new(),
                    runtime: None,
                    terminals: Vec::new(),
                    provider_routes: Vec::new(),
                },
                recovery_source,
            )
            .map_err(|e| {
                (
                    wire::SERVER_ERROR,
                    format!("cannot create event generation: {e}"),
                )
            })?,
        );
        // The running turn's id, shared between the ThreadUi (which tags item
        // events with it) and the handle (which `turn/steer` reads).
        let turn = Arc::new(Mutex::new(None));
        let ui = Arc::new(ThreadUi {
            thread_id: thread_id.clone(),
            out: out.clone(),
            pending: pending.clone(),
            reverse_request_seq: reverse_request_seq.clone(),
            turn: turn.clone(),
            projection: projection.clone(),
        });
        let questioner: Option<Arc<dyn Questioner>> =
            client_questions.then(|| ui.clone() as Arc<dyn Questioner>);
        let note_ui = ui.clone();
        let mut cfg = (factory)(
            options,
            Arc::clone(&catalog),
            ui.clone(),
            questioner,
            Arc::new(move |s: &str| note_ui.emit(&Event::Note(s.to_string()))),
        )
        .map_err(|e| (wire::SERVER_ERROR, format!("cannot build config: {e:#}")))?;
        // A fresh thread opens its route timeline here. A reopened one already
        // has one, and starts on the route this thread was just built with — the
        // configured default — recording the hop when the session was written on
        // something else. Rebuilding that as a revision-1 initial route instead
        // would refuse every session that ever switched providers.
        let (route, reopened) = history
            .adopt_or_initialize_provider_route(&cfg.provider_route)
            .map_err(|error| {
                (
                    wire::SERVER_ERROR,
                    format!("cannot settle the session provider route: {error}"),
                )
            })?;
        cfg.provider_route = route;
        if let Some(reopened) = reopened {
            ui.emit(&Event::Note(reopened.to_string()));
        }
        seed.provider_routes = history.provider_routes().to_vec();
        // The factory cannot know which thread it is building for; the hook
        // events' session id is stamped here.
        cfg.bind_session(thread_id.clone())
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot bind session: {e:#}")))?;
        projection.refresh_seed(
            cfg.cwd.to_string_lossy().to_string(),
            cfg.provider_route.public_route(),
            seed,
        );
        let handle_cwd = cfg.cwd.clone();
        let active_route = Arc::new(Mutex::new(cfg.provider_route.public_route()));
        let provider_state = Arc::new(
            kloop_core::provider_route::SessionProviderState::from_timeline(
                Arc::clone(&cfg.provider_catalog),
                history.provider_routes(),
            )
            .map_err(|error| {
                (
                    wire::SERVER_ERROR,
                    format!("cannot restore provider route state: {error}"),
                )
            })?,
        );
        let (turn_tx, turn_rx) = mpsc::unbounded_channel();
        let running = Arc::new(AtomicBool::new(false));
        let current_cancel = Arc::new(Mutex::new(None));
        let turn_seq = Arc::new(AtomicU64::new(1));
        let cfg = Arc::new(cfg);
        // The steering/scheduler queue the worker's turns drain.
        let inbox = cfg.inbox.clone();
        let worker_state = ThreadWorkerState {
            running: running.clone(),
            current_cancel: current_cancel.clone(),
            turn_seq: turn_seq.clone(),
            inbox: inbox.clone(),
            active_route: active_route.clone(),
            provider_state: provider_state.clone(),
        };
        // A blocking-pool thread still carries the runtime context, so the
        // worker is spawned here and the handle is what travels back.
        tokio::spawn(thread_worker(cfg, history, ui, turn_rx, worker_state));
        // The worker owns the history (and a fork's file) from here. On any
        // earlier failure the guard drops and removes it.
        if let Some(guard) = guard.as_mut() {
            guard.disarm();
        }
        Ok(ThreadHandle {
            turn_tx,
            running,
            current_cancel,
            inbox,
            turn_seq,
            turn,
            cwd: handle_cwd,
            active_route,
            provider_state,
            projection,
        })
    }
}

/// Locate a thread's transcript and the partition holding it, without borrowing
/// the server. Kept free so blocking read tasks can use it after the read loop
/// has moved on.
pub(super) fn locate_thread_in(
    thread_id: &str,
    default_dirs: &SessionDirs,
    store: &kloop_core::session_store::SessionStore,
) -> Result<(PathBuf, SessionDirs), (i64, String)> {
    let path = rollout::checked_session_path(&default_dirs.sessions, thread_id)
        .map_err(|e| (wire::INVALID_PARAMS, format!("invalid thread id: {e}")))?;
    if path.exists() {
        return Ok((path, default_dirs.clone()));
    }
    store
        .buckets()
        .into_iter()
        .find_map(|bucket| {
            let path = rollout::checked_session_path(&bucket.dirs.sessions, thread_id).ok()?;
            path.exists().then_some((path, bucket.dirs))
        })
        .ok_or((wire::SERVER_ERROR, format!("no session '{thread_id}'")))
}

fn parse_thread_list_params(params: &Value) -> Result<(usize, usize), (i64, String)> {
    let limit = match params.get("limit") {
        None | Some(Value::Null) => 50,
        Some(value) => value
            .as_u64()
            .filter(|limit| *limit > 0)
            .map(|limit| limit.min(500) as usize)
            .ok_or((
                wire::INVALID_PARAMS,
                "'limit' must be a positive integer".to_string(),
            ))?,
    };
    let offset = match params.get("cursor") {
        None | Some(Value::Null) => 0,
        Some(Value::String(cursor)) => cursor.parse::<usize>().map_err(|_| {
            (
                wire::INVALID_PARAMS,
                "'cursor' must be a cursor returned by thread/list".to_string(),
            )
        })?,
        Some(_) => {
            return Err((
                wire::INVALID_PARAMS,
                "'cursor' must be a string".to_string(),
            ));
        }
    };
    Ok((limit, offset))
}

fn thread_list_blocking(
    sessions: &std::path::Path,
    catalog: &ProviderCatalog,
    running: &HashMap<String, bool>,
    limit: usize,
    offset: usize,
) -> MethodResult {
    let paths: Vec<PathBuf> = rollout::sessions_by_recency(sessions)
        .into_iter()
        // Sub-agent transcripts share the sessions dir but are internal;
        // like cc's sidechains and codex's source filter, keep them out of
        // the thread list (still readable by explicit id).
        .filter(|path| !rollout::is_subagent_session(path))
        .collect();
    let mut threads = Vec::new();
    let mut next_cursor = None;
    // Filtering past unreadable transcripts happens *inside* the page: the old
    // `skip(offset).take(limit)` filled a page with paths and then dropped the
    // broken ones, so one bad file silently shortened a page and the cursor
    // still stepped over it.
    for (index, path) in paths.iter().enumerate().skip(offset) {
        if threads.len() == limit {
            next_cursor = Some(index.to_string());
            break;
        }
        // The summary validates the same format/route/envelope rules a replay
        // does, then keeps only listing fields: no replayed history and no
        // pairing repair.
        let summary = match rollout::session_summary(path) {
            Ok(summary) => summary,
            Err(_) => continue,
        };
        let digest = summary.digest;
        let thread_id = digest.id.clone();
        let in_progress = running.get(&thread_id).copied().unwrap_or(false);
        let updated_at_ms = digest
            .modified
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        let route = summary
            .route
            .as_ref()
            .map(|route| public_route_from_record(route, catalog));
        threads.push(json!({
            "id": thread_id,
            "messages": summary.messages,
            "snippet": clip_chars(&digest.title, 60, "…"),
            "cwd": summary.runtime.as_ref().map(|runtime| runtime.cwd.as_str()),
            "route": route,
            "resumable": summary.runtime.is_some(),
            "in_progress": in_progress,
            "forked_from": fork_origin_value(digest.origin.as_ref()),
            "updated_at_ms": updated_at_ms,
        }));
    }
    Ok(json!({"threads": threads, "next_cursor": next_cursor}))
}

fn thread_read_blocking(
    thread_id: String,
    path: PathBuf,
    catalog: Arc<ProviderCatalog>,
) -> MethodResult {
    let snapshot = events::into_public_session_snapshot(
        rollout::load_session_snapshot(&path)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot read session: {e}")))?,
    );
    let route = snapshot
        .provider_routes
        .last()
        .map(|route| public_route_from_record(route, &catalog));
    Ok(json!({
        "thread": {
            "id": thread_id,
            "cwd": snapshot.runtime.as_ref().map(|runtime| runtime.cwd.as_str()),
            "route": route,
            "resumable": snapshot.runtime.is_some(),
            "forked_from": fork_origin_json(&path),
            "messages": snapshot.messages,
            "terminals": snapshot.terminals,
        }
    }))
}
