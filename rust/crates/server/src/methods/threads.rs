//! 线程持久化、恢复、fork、公开快照与工作者启动。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::Result;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;

use kloop_core::agent::Ui;
use kloop_core::event::Event;
use kloop_core::history::History;
use kloop_core::interaction::Questioner;
use kloop_core::rollout;
use kloop_core::rollout::Rollout;
use kloop_core::rollout::SessionSnapshot;
use kloop_core::session_store::SessionDirs;
use kloop_protocol::ActiveProviderRoute;

use crate::Server;
use crate::ThreadHandle;
use crate::ThreadStartOptions;
use crate::ThreadWorkerState;
use crate::events;
use crate::events::EventsSyncParams;
use crate::events::RecoverySource;
use crate::events::ThreadProjection;
use crate::params::MethodResult;
use crate::params::fork_origin_json;
use crate::params::parse_thread_start_options;
use crate::params::resume_options;
use crate::params::runtime_from_options;
use crate::params::str_param;
use crate::params::thread_runtime_json;
use crate::thread_ui::ThreadUi;
use crate::wire;
use crate::worker::thread_worker;

impl Server {
    /// Locate a thread's transcript and the partition holding it. A thread
    /// pins its own cwd at `thread/start`, so it can live outside the process
    /// cwd's project while `thread/resume` carries only an id: check this
    /// process's partition first, then the rest.
    pub(super) fn locate_thread(
        &self,
        thread_id: &str,
    ) -> Result<(PathBuf, SessionDirs), (i64, String)> {
        let path = rollout::checked_session_path(&self.default_dirs.sessions, thread_id)
            .map_err(|e| (wire::INVALID_PARAMS, format!("invalid thread id: {e}")))?;
        if path.exists() {
            return Ok((path, self.default_dirs.clone()));
        }
        self.paths
            .store
            .buckets()
            .into_iter()
            .find_map(|bucket| {
                let path = rollout::checked_session_path(&bucket.dirs.sessions, thread_id).ok()?;
                path.exists().then_some((path, bucket.dirs))
            })
            .ok_or((wire::SERVER_ERROR, format!("no session '{thread_id}'")))
    }

    pub(crate) fn thread_start(&mut self, params: &Value) -> MethodResult {
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
        let path = claim.path().to_path_buf();
        let runtime = runtime_from_options(&options);
        let rollout = match Rollout::new_with_runtime_pending_route(path.clone(), runtime.clone()) {
            Ok(rollout) => rollout,
            Err(error) => {
                let _ = std::fs::remove_file(&path);
                return Err((
                    wire::SERVER_ERROR,
                    format!("cannot initialize session: {error}"),
                ));
            }
        };
        let mut history = History::new(dirs.offload.clone());
        history.attach_rollout(rollout);
        let seed = SessionSnapshot {
            messages: Vec::new(),
            runtime: Some(runtime),
            terminals: Vec::new(),
            provider_routes: history.provider_routes().to_vec(),
        };
        if let Err(error) = self.spawn_thread(
            thread_id.clone(),
            history,
            options,
            RecoverySource::Fresh,
            seed,
        ) {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        let route = self
            .threads
            .get(&thread_id)
            .expect("spawned thread is registered")
            .active_route
            .lock()
            .unwrap()
            .clone();
        Ok(json!({"thread": {"id": thread_id, "route": route}}))
    }

    pub(crate) fn thread_resume(&mut self, params: &Value) -> MethodResult {
        let thread_id = str_param(params, "thread_id")?;
        if self.threads.contains_key(thread_id) {
            return Err((
                wire::SERVER_ERROR,
                format!("thread '{thread_id}' is already active"),
            ));
        }
        let (path, dirs) = self.locate_thread(thread_id)?;
        let inspected = rollout::inspect_session(&path)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot read session: {e}")))?;
        let options = resume_options(&inspected.snapshot(), params, &self.default_cwd)?;
        let resumed = inspected
            .recover_for_writing()
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot resume: {e}")))?;
        let count = resumed.messages.len();
        let seed = resumed.snapshot.clone();
        let history = History::resume(dirs.offload.clone(), resumed);
        self.spawn_thread(
            thread_id.to_string(),
            history,
            options.clone(),
            RecoverySource::Resumed,
            seed,
        )?;
        let route = self
            .threads
            .get(thread_id)
            .expect("spawned thread is registered")
            .active_route
            .lock()
            .unwrap()
            .clone();
        Ok(json!({
            "thread": thread_runtime_json(thread_id, &options, &route),
            "message_count": count,
        }))
    }

    /// Fork a session at a cut point into a fresh thread, then spawn it live
    /// (like `thread/resume`) so the client can `turn/start` on it immediately.
    /// `cut` is optional — omit it to fork at the end. A dormant source is
    /// read directly from disk; an active source is rejected so its in-flight
    /// turn cannot be copied before the terminal record makes the prefix complete.
    pub(crate) fn thread_fork(&mut self, params: &Value) -> MethodResult {
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
        let resumed =
            match rollout::inspect_session(&new_path).and_then(|read| read.recover_for_writing()) {
                Ok(resumed) => resumed,
                Err(error) => {
                    let _ = std::fs::remove_file(&new_path);
                    return Err((wire::SERVER_ERROR, format!("cannot resume fork: {error}")));
                }
            };
        let options = resume_options(&resumed.snapshot, params, &self.default_cwd)?;
        let count = resumed.messages.len();
        let seed = resumed.snapshot.clone();
        let history = History::resume(dirs.offload.clone(), resumed);
        if let Err(error) = self.spawn_thread(
            new_id.clone(),
            history,
            options.clone(),
            RecoverySource::Resumed,
            seed,
        ) {
            let _ = std::fs::remove_file(&new_path);
            return Err(error);
        }
        let route = self
            .threads
            .get(&new_id)
            .expect("spawned fork is registered")
            .active_route
            .lock()
            .unwrap()
            .clone();
        Ok(json!({
            "thread": thread_runtime_json(&new_id, &options, &route),
            "message_count": count,
        }))
    }

    pub(crate) fn thread_list(&self, params: &Value) -> MethodResult {
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
        let paths: Vec<PathBuf> = rollout::sessions_by_recency(&self.default_dirs.sessions)
            .into_iter()
            // Sub-agent transcripts share the sessions dir but are internal;
            // like cc's sidechains and codex's source filter, keep them out
            // of the thread list (still readable by explicit id).
            .filter(|path| !rollout::is_subagent_session(path))
            .collect();
        let mut threads = Vec::new();
        for path in paths.iter().skip(offset).take(limit) {
            let snapshot = match rollout::load_session_snapshot(path) {
                Ok(snapshot) => snapshot,
                Err(_) => continue,
            };
            let runtime = snapshot.runtime.as_ref();
            let thread_id = rollout::session_id_of(path);
            let in_progress = self
                .threads
                .get(&thread_id)
                .is_some_and(|handle| handle.running.load(Ordering::SeqCst));
            let updated_at_ms = path
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0);
            let route = snapshot
                .provider_routes
                .last()
                .map(|route| ActiveProviderRoute {
                    revision: route.route_revision,
                    effort: self.provider_catalog.default_effort(&route.provider_id),
                    provider_id: route.provider_id.clone(),
                    api_family: route.api_family,
                    model: route.model.clone(),
                    continuity: route.continuity,
                });
            threads.push(json!({
                "id": thread_id,
                "messages": snapshot.messages.len(),
                "snippet": rollout::first_user_snippet(&snapshot.messages),
                "cwd": runtime.map(|runtime| runtime.cwd.as_str()),
                "route": route,
                "resumable": runtime.is_some(),
                "in_progress": in_progress,
                "forked_from": fork_origin_json(path),
                "updated_at_ms": updated_at_ms,
            }));
        }
        let consumed = offset.saturating_add(limit).min(paths.len());
        let next_cursor = (consumed < paths.len()).then(|| consumed.to_string());
        Ok(json!({"threads": threads, "next_cursor": next_cursor}))
    }

    pub(crate) fn thread_read(&self, params: &Value) -> MethodResult {
        let thread_id = str_param(params, "thread_id")?;
        let (path, _) = self.locate_thread(thread_id)?;
        let snapshot = events::into_public_session_snapshot(
            rollout::load_session_snapshot(&path)
                .map_err(|e| (wire::SERVER_ERROR, format!("cannot read session: {e}")))?,
        );
        let route = snapshot
            .provider_routes
            .last()
            .map(|route| ActiveProviderRoute {
                revision: route.route_revision,
                // Effort never enters the durable timeline, so a route read off
                // disk reports the effort a session there would start at.
                effort: self.provider_catalog.default_effort(&route.provider_id),
                provider_id: route.provider_id.clone(),
                api_family: route.api_family,
                model: route.model.clone(),
                continuity: route.continuity,
            });
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

    fn spawn_thread(
        &mut self,
        thread_id: String,
        mut history: History,
        options: ThreadStartOptions,
        recovery_source: RecoverySource,
        mut seed: SessionSnapshot,
    ) -> Result<(), (i64, String)> {
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
            out: self.out.clone(),
            pending: self.pending.clone(),
            reverse_request_seq: self.reverse_request_seq.clone(),
            turn: turn.clone(),
            projection: projection.clone(),
        });
        let questioner: Option<Arc<dyn Questioner>> = self
            .client_questions
            .then(|| ui.clone() as Arc<dyn Questioner>);
        let note_ui = ui.clone();
        let mut cfg = (self.factory)(
            options,
            Arc::clone(&self.provider_catalog),
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
        if history.has_provider_route() {
            let (route, reopened) =
                history
                    .adopt_provider_route(&cfg.provider_route)
                    .map_err(|error| {
                        (
                            wire::SERVER_ERROR,
                            format!("cannot adopt the session provider route: {error}"),
                        )
                    })?;
            cfg.provider_route = route;
            if let Some(reopened) = reopened {
                ui.emit(&Event::Note(reopened.to_string()));
            }
        } else {
            history
                .ensure_initial_provider_route(&cfg.provider_route)
                .map_err(|error| {
                    (
                        wire::SERVER_ERROR,
                        format!("cannot persist initial provider route: {error}"),
                    )
                })?;
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
        tokio::spawn(thread_worker(cfg, history, ui, turn_rx, worker_state));
        self.threads.insert(
            thread_id,
            ThreadHandle {
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
            },
        );
        Ok(())
    }
}
