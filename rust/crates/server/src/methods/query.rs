//! 配置与目录查询、读取作用域和 provider 切换入队。

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use anyhow::Result;
use serde_json::Value;
use serde_json::json;

use kloop_core::provider_route::SwitchError;
use kloop_core::rollout;
use kloop_protocol::ActiveProviderRoute;

use crate::RequestId;
use crate::Server;
use crate::ThreadWorkerMsg;
use crate::params::MethodResult;
use crate::params::SwitchRequestError;
use crate::params::ensure_empty_params;
use crate::params::ensure_known_params;
use crate::params::object_params;
use crate::params::options_from_runtime;
use crate::params::resolve_cwd;
use crate::params::str_param;
use crate::wire;

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
        let model = match params.get("model") {
            None | Some(Value::Null) => None,
            Some(Value::String(model)) if !model.trim().is_empty() => Some(model.clone()),
            Some(Value::String(_)) => {
                return Err(SwitchRequestError::InvalidParams(
                    "'model' must not be empty".into(),
                ));
            }
            Some(_) => {
                return Err(SwitchRequestError::InvalidParams(
                    "'model' must be a string".into(),
                ));
            }
        };
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
            let kind = match &error {
                SwitchError::UnknownProvider(_) => "unknown_provider",
                SwitchError::UnknownModel { .. } => "unknown_model",
                SwitchError::Unavailable { .. } => "provider_unavailable",
                SwitchError::StaleRevision { .. } => "stale_revision",
                SwitchError::RouteDrift(_)
                | SwitchError::RevisionExhausted
                | SwitchError::InvalidRevision
                | SwitchError::InvalidTimeline
                | SwitchError::InvalidInheritedProviderModel => "invalid_route",
            };
            return Err(SwitchRequestError::Target {
                message: error.to_string(),
                kind,
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

    pub(crate) fn config_read(&self, params: &Value) -> MethodResult {
        ensure_known_params(params, "config/read", &["thread_id", "cwd"])?;
        let (cwd, pinned_route) = self.read_scope(params)?;
        let mut snapshot = (self.config_reader)(&cwd)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot read config: {e:#}")))?;
        snapshot.cwd = cwd.to_string_lossy().to_string();
        if let Some(route) = pinned_route {
            snapshot.route = Some(route);
        }
        Ok(json!({"config": snapshot}))
    }

    pub(crate) fn skills_list(&self, params: &Value) -> MethodResult {
        ensure_known_params(params, "skills/list", &["thread_id", "cwd", "force_reload"])?;
        match params.get("force_reload") {
            None | Some(Value::Null) | Some(Value::Bool(_)) => {}
            Some(_) => {
                return Err((
                    wire::INVALID_PARAMS,
                    "'force_reload' must be a boolean".into(),
                ));
            }
        }
        let (cwd, _) = self.read_scope(params)?;
        // Discovery is deliberately uncached today, so force_reload and a normal
        // call are both one fresh bounded filesystem scan.
        let mut snapshot = (self.skills_reader)(&cwd)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot list skills: {e:#}")))?;
        snapshot.cwd = cwd.to_string_lossy().to_string();
        Ok(json!(snapshot))
    }

    pub(crate) fn mcp_server_status_list(&self, params: &Value) -> MethodResult {
        ensure_empty_params(params, "mcp_server_status/list")?;
        Ok(json!({"servers": &self.mcp_servers}))
    }

    /// Resolve the cwd selected by `{thread_id? | cwd?}`. With no selector, use
    /// the server's canonical startup cwd. A dormant thread reads its persisted
    /// runtime; legacy sessions without runtime metadata fail closed.
    fn read_scope(
        &self,
        params: &Value,
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
            if let Some(handle) = self.threads.get(thread_id) {
                return Ok((
                    handle.cwd.clone(),
                    Some(handle.active_route.lock().unwrap().clone()),
                ));
            }
            let (path, _) = self.locate_thread(thread_id)?;
            let snapshot = rollout::load_session_snapshot(&path)
                .map_err(|e| (wire::SERVER_ERROR, format!("cannot read session: {e}")))?;
            let route = snapshot.provider_routes.last().ok_or((
                wire::SERVER_ERROR,
                format!("session '{thread_id}' has no provider route timeline"),
            ))?;
            let public_route = ActiveProviderRoute {
                revision: route.route_revision,
                effort: self.provider_catalog.default_effort(&route.provider_id),
                provider_id: route.provider_id.clone(),
                api_family: route.api_family,
                model: route.model.clone(),
                continuity: route.continuity,
            };
            let runtime = snapshot.runtime.ok_or((
                wire::SERVER_ERROR,
                format!("session '{thread_id}' has no runtime metadata"),
            ))?;
            let options = options_from_runtime(&runtime)?;
            return Ok((options.cwd, Some(public_route)));
        }
        Ok((resolve_cwd(cwd, &self.default_cwd)?, None))
    }
}
