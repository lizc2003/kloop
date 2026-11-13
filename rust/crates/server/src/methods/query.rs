//! 配置与目录查询、读取作用域和 provider 切换入队。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;
use serde_json::Value;
use serde_json::json;

use kloop_core::provider_route::ProviderCatalog;
use kloop_core::rollout;
use kloop_core::session_store::SessionDirs;
use kloop_core::session_store::SessionStore;
use kloop_protocol::ActiveProviderRoute;

use crate::RequestId;
use crate::Server;
use crate::ThreadWorkerMsg;
use crate::params::MethodResult;
use crate::params::SwitchRequestError;
use crate::params::ensure_empty_params;
use crate::params::ensure_known_params;
use crate::params::object_params;
use crate::params::optional_nonempty_str;
use crate::params::options_from_runtime;
use crate::params::public_route_from_record;
use crate::params::resolve_cwd;
use crate::params::route_switch_error_kind;
use crate::params::str_param;
use crate::wire;
use crate::wire::Outgoing;

impl Server {
    pub(crate) fn provider_catalog_read(&self, params: &Value) -> MethodResult {
        ensure_empty_params(params, "provider/catalog/read")?;
        Ok(json!({"providers": self.provider_catalog.descriptors()}))
    }

    pub(crate) fn queue_thread_provider_switch(
        &self,
        request_id: RequestId,
        params: &Value,
    ) -> Result<(), SwitchRequestError> {
        ensure_known_params(
            params,
            "thread/provider/switch",
            &[
                "thread_id",
                "provider_id",
                "model",
                "expected_route_revision",
            ],
        )
        .map_err(|(_, message)| SwitchRequestError::InvalidParams(message))?;
        let thread_id = str_param(params, "thread_id")
            .map_err(|(_, message)| SwitchRequestError::InvalidParams(message))?;
        let provider_id = str_param(params, "provider_id")
            .map_err(|(_, message)| SwitchRequestError::InvalidParams(message))?
            .to_string();
        let model = optional_nonempty_str(params, "model")
            .map_err(|(_, message)| SwitchRequestError::InvalidParams(message))?;
        let expected_revision = params["expected_route_revision"].as_u64().ok_or_else(|| {
            SwitchRequestError::InvalidParams(
                "'expected_route_revision' must be a positive integer".into(),
            )
        })?;
        if expected_revision == 0 {
            return Err(SwitchRequestError::InvalidParams(
                "'expected_route_revision' must be a positive integer".into(),
            ));
        }
        let handle = self.threads.get(thread_id).ok_or_else(|| {
            SwitchRequestError::UnknownThread(format!("unknown or inactive thread '{thread_id}'"))
        })?;
        if let Err(error) = handle
            .provider_state
            .preflight(&provider_id, model.as_deref())
        {
            return Err(SwitchRequestError::Target {
                message: error.to_string(),
                kind: route_switch_error_kind(&error),
            });
        }
        if handle
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(SwitchRequestError::Busy(format!(
                "thread '{thread_id}' already has an active operation"
            )));
        }
        if handle
            .turn_tx
            .send(ThreadWorkerMsg::SwitchProvider {
                provider_id,
                model,
                expected_revision,
                request_id,
            })
            .is_err()
        {
            handle.running.store(false, Ordering::SeqCst);
            return Err(SwitchRequestError::WorkerUnavailable);
        }
        Ok(())
    }

    /// `config/read` and `skills/list` run their resolvers and readers on the
    /// blocking pool: a fresh skills scan, a config walk, or a dormant session
    /// replay must not hold up approvals and interrupts on the read loop.
    pub(crate) fn start_config_read(&mut self, id: RequestId, params: &Value) {
        if let Err((code, message)) =
            ensure_known_params(params, "config/read", &["thread_id", "cwd"])
        {
            return self.send(Outgoing::Error {
                id: Some(id),
                code,
                message,
                data: None,
            });
        }
        let scope = self.read_scope_snapshot();
        let params = params.clone();
        let reader = Arc::clone(&self.config_reader);
        self.spawn_read(id, move || {
            let (cwd, pinned_route) = resolve_read_scope(&params, &scope)?;
            let mut snapshot = (reader)(&cwd)
                .map_err(|e| (wire::SERVER_ERROR, format!("cannot read config: {e:#}")))?;
            snapshot.cwd = cwd.to_string_lossy().to_string();
            if let Some(route) = pinned_route {
                snapshot.route = Some(route);
            }
            Ok(json!({"config": snapshot}))
        });
    }

    pub(crate) fn start_skills_list(&mut self, id: RequestId, params: &Value) {
        let prepared =
            ensure_known_params(params, "skills/list", &["thread_id", "cwd", "force_reload"])
                .and_then(|()| match params.get("force_reload") {
                    None | Some(Value::Null) | Some(Value::Bool(_)) => Ok(()),
                    Some(_) => Err((
                        wire::INVALID_PARAMS,
                        "'force_reload' must be a boolean".into(),
                    )),
                });
        if let Err((code, message)) = prepared {
            return self.send(Outgoing::Error {
                id: Some(id),
                code,
                message,
                data: None,
            });
        }
        let scope = self.read_scope_snapshot();
        let params = params.clone();
        let reader = Arc::clone(&self.skills_reader);
        self.spawn_read(id, move || {
            let (cwd, _) = resolve_read_scope(&params, &scope)?;
            // Discovery is deliberately uncached today, so force_reload and a
            // normal call are both one fresh bounded filesystem scan.
            let mut snapshot = (reader)(&cwd)
                .map_err(|e| (wire::SERVER_ERROR, format!("cannot list skills: {e:#}")))?;
            snapshot.cwd = cwd.to_string_lossy().to_string();
            Ok(json!(snapshot))
        });
    }

    pub(crate) fn mcp_server_status_list(&self, params: &Value) -> MethodResult {
        ensure_empty_params(params, "mcp_server_status/list")?;
        Ok(json!({"servers": &self.mcp_servers}))
    }

    /// Everything a read-only resolver may need from the live server, taken
    /// under the read loop and moved to the blocking task. Thread routes and
    /// cwds are snapshotted; a switch that lands meanwhile belongs to a later
    /// read, not this one.
    fn read_scope_snapshot(&self) -> ReadScopeSnapshot {
        ReadScopeSnapshot {
            default_cwd: self.default_cwd.clone(),
            default_dirs: self.default_dirs.clone(),
            store: self.paths.store.clone(),
            catalog: Arc::clone(&self.provider_catalog),
            live: self
                .threads
                .iter()
                .map(|(thread_id, handle)| {
                    (
                        thread_id.clone(),
                        (
                            handle.cwd.clone(),
                            handle.active_route.lock().unwrap().clone(),
                        ),
                    )
                })
                .collect(),
        }
    }
}

pub(super) struct ReadScopeSnapshot {
    default_cwd: PathBuf,
    default_dirs: SessionDirs,
    store: SessionStore,
    catalog: Arc<ProviderCatalog>,
    live: HashMap<String, (PathBuf, ActiveProviderRoute)>,
}

/// Resolve the cwd selected by `{thread_id? | cwd?}`. With no selector, use the
/// server's canonical startup cwd. A dormant thread reads its persisted runtime;
/// legacy sessions without runtime metadata fail closed.
pub(super) fn resolve_read_scope(
    params: &Value,
    scope: &ReadScopeSnapshot,
) -> Result<(PathBuf, Option<ActiveProviderRoute>), (i64, String)> {
    object_params(params, "read method")?;
    let thread_id = params.get("thread_id").filter(|value| !value.is_null());
    let cwd = params.get("cwd").filter(|value| !value.is_null());
    if thread_id.is_some() && cwd.is_some() {
        return Err((
            wire::INVALID_PARAMS,
            "supply at most one of 'thread_id' or 'cwd'".into(),
        ));
    }
    if let Some(value) = thread_id {
        let thread_id = value.as_str().ok_or((
            wire::INVALID_PARAMS,
            "'thread_id' must be a string".to_string(),
        ))?;
        if thread_id.trim().is_empty() {
            return Err((wire::INVALID_PARAMS, "'thread_id' must not be empty".into()));
        }
        if let Some((cwd, route)) = scope.live.get(thread_id) {
            return Ok((cwd.clone(), Some(route.clone())));
        }
        let (path, _) = crate::methods::threads::locate_thread_in(
            thread_id,
            &scope.default_dirs,
            &scope.store,
        )?;
        let snapshot = rollout::load_session_snapshot(&path)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot read session: {e}")))?;
        let route = snapshot.provider_routes.last().ok_or((
            wire::SERVER_ERROR,
            format!("session '{thread_id}' has no provider route timeline"),
        ))?;
        let public_route = public_route_from_record(route, &scope.catalog);
        let runtime = snapshot.runtime.ok_or((
            wire::SERVER_ERROR,
            format!("session '{thread_id}' has no runtime metadata"),
        ))?;
        let options = options_from_runtime(&runtime)?;
        return Ok((options.cwd, Some(public_route)));
    }
    Ok((resolve_cwd(cwd, &scope.default_cwd)?, None))
}
