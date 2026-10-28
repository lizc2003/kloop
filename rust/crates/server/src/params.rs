//! 方法参数、会话运行时映射与协议错误分类。

use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;
use serde_json::json;

use kloop_core::history::ProviderSwitchError;
use kloop_core::provider_route::SwitchError;
use kloop_core::rollout;
use kloop_core::rollout::SessionRuntime;
use kloop_core::rollout::SessionSnapshot;
use kloop_protocol::ActiveProviderRoute;
use kloop_protocol::ContentBlock;
use kloop_protocol::ImageSource;

use crate::ThreadStartOptions;
use crate::wire;

pub(super) type MethodResult = Result<Value, (i64, String)>;

/// `turn/steer` is the one method besides the provider switch whose refusal
/// carries a machine-readable `data.kind`, so it has its own error type.
pub(super) enum SteerError {
    Request((i64, String)),
    Refused(kloop_core::inbox::SteerRefused),
}

pub(super) fn steer_refused_data(refused: kloop_core::inbox::SteerRefused) -> Value {
    match refused {
        kloop_core::inbox::SteerRefused::NoActiveTurn => json!({"kind": refused.kind()}),
        kloop_core::inbox::SteerRefused::TurnMismatch { active } => {
            json!({"kind": refused.kind(), "active_turn_id": active})
        }
    }
}

#[derive(Debug)]
pub(super) enum SwitchRequestError {
    InvalidParams(String),
    UnknownThread(String),
    Target { message: String, kind: &'static str },
    Busy(String),
    WorkerUnavailable,
}

impl SwitchRequestError {
    pub(super) fn code(&self) -> i64 {
        match self {
            Self::InvalidParams(_) | Self::UnknownThread(_) | Self::Target { .. } => {
                wire::INVALID_PARAMS
            }
            Self::Busy(_) | Self::WorkerUnavailable => wire::SERVER_ERROR,
        }
    }

    pub(super) fn kind(&self) -> &'static str {
        match self {
            Self::InvalidParams(_) => "invalid_params",
            Self::UnknownThread(_) => "unknown",
            Self::Target { kind, .. } => kind,
            Self::Busy(_) => "busy",
            Self::WorkerUnavailable => "unavailable",
        }
    }
}

impl std::fmt::Display for SwitchRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidParams(message) | Self::UnknownThread(message) | Self::Busy(message) => {
                formatter.write_str(message)
            }
            Self::Target { message, .. } => formatter.write_str(message),
            Self::WorkerUnavailable => formatter.write_str("thread worker is unavailable"),
        }
    }
}

pub(super) fn switch_error_kind(error: &ProviderSwitchError) -> &'static str {
    match error {
        ProviderSwitchError::Route(SwitchError::UnknownProvider(_)) => "unknown_provider",
        ProviderSwitchError::Route(SwitchError::UnknownModel { .. }) => "unknown_model",
        ProviderSwitchError::Route(SwitchError::Unavailable { .. }) => "provider_unavailable",
        ProviderSwitchError::Route(SwitchError::StaleRevision { .. }) => "stale_revision",
        ProviderSwitchError::Persistence(_) => "route_persistence_failed",
        ProviderSwitchError::History(_) => "history_projection_failed",
        ProviderSwitchError::Route(
            SwitchError::RouteDrift(_)
            | SwitchError::RevisionExhausted
            | SwitchError::InvalidRevision
            | SwitchError::InvalidTimeline
            | SwitchError::InvalidInheritedProviderModel,
        ) => "invalid_route",
    }
}

pub(super) fn object_params(params: &Value, method: &str) -> Result<(), (i64, String)> {
    match params {
        Value::Null | Value::Object(_) => Ok(()),
        _ => Err((
            wire::INVALID_PARAMS,
            format!("{method} params must be an object"),
        )),
    }
}

pub(super) fn ensure_known_params(
    params: &Value,
    method: &str,
    allowed: &[&str],
) -> Result<(), (i64, String)> {
    object_params(params, method)?;
    let Some(params) = params.as_object() else {
        return Ok(());
    };
    if let Some(key) = params.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err((
            wire::INVALID_PARAMS,
            format!("{method} does not accept parameter '{key}'"),
        ));
    }
    Ok(())
}

pub(super) fn ensure_empty_params(params: &Value, method: &str) -> Result<(), (i64, String)> {
    object_params(params, method)?;
    if params.as_object().is_some_and(|params| !params.is_empty()) {
        return Err((
            wire::INVALID_PARAMS,
            format!("{method} does not accept parameters"),
        ));
    }
    Ok(())
}

pub(super) fn resolve_cwd(
    value: Option<&Value>,
    default_cwd: &Path,
) -> Result<PathBuf, (i64, String)> {
    let cwd = match value {
        None | Some(Value::Null) => default_cwd.to_path_buf(),
        Some(Value::String(raw)) if !raw.trim().is_empty() => {
            let path = PathBuf::from(raw);
            let path = if path.is_absolute() {
                path
            } else {
                default_cwd.join(path)
            };
            std::fs::canonicalize(&path).map_err(|e| {
                (
                    wire::INVALID_PARAMS,
                    format!("cannot resolve cwd '{}': {e}", path.display()),
                )
            })?
        }
        Some(Value::String(_)) => {
            return Err((wire::INVALID_PARAMS, "'cwd' must not be empty".into()));
        }
        Some(_) => return Err((wire::INVALID_PARAMS, "'cwd' must be a string".into())),
    };
    if !cwd.is_dir() {
        return Err((
            wire::INVALID_PARAMS,
            format!("cwd '{}' is not a directory", cwd.display()),
        ));
    }
    Ok(cwd)
}

pub(super) fn runtime_from_options(options: &ThreadStartOptions) -> SessionRuntime {
    SessionRuntime {
        cwd: options.cwd.to_string_lossy().to_string(),
    }
}

pub(super) fn options_from_runtime(
    runtime: &SessionRuntime,
) -> Result<ThreadStartOptions, (i64, String)> {
    let cwd = std::fs::canonicalize(&runtime.cwd).map_err(|e| {
        (
            wire::SERVER_ERROR,
            format!("cannot restore cwd '{}': {e}", runtime.cwd),
        )
    })?;
    if !cwd.is_dir() {
        return Err((
            wire::SERVER_ERROR,
            format!("restored cwd '{}' is not a directory", cwd.display()),
        ));
    }
    Ok(ThreadStartOptions {
        cwd,
        provider_id: None,
        model: None,
    })
}

pub(super) fn resume_options(
    snapshot: &SessionSnapshot,
    _params: &Value,
    _default_cwd: &Path,
) -> Result<ThreadStartOptions, (i64, String)> {
    let runtime = snapshot.runtime.as_ref().ok_or((
        wire::SERVER_ERROR,
        "session is missing runtime metadata".into(),
    ))?;
    // The recorded provider/model are deliberately not pinned onto the options:
    // a reopened thread starts on the configured default, exactly like a new
    // one, and `spawn_thread` records the hop when that differs from what the
    // session was written on. A client that wants a specific route asks for it
    // with `thread/provider/switch` once the thread is live.
    options_from_runtime(runtime)
}

pub(super) fn thread_runtime_json(
    thread_id: &str,
    options: &ThreadStartOptions,
    route: &ActiveProviderRoute,
) -> Value {
    json!({
        "id": thread_id,
        "cwd": options.cwd.to_string_lossy(),
        "route": route,
        "resumable": true,
    })
}

pub(super) fn fork_origin_json(path: &Path) -> Value {
    let Some(origin) = rollout::fork_origin(path) else {
        return Value::Null;
    };
    let Some((thread_id, cut)) = origin.rsplit_once('#') else {
        return Value::Null;
    };
    json!({
        "thread_id": thread_id,
        "cut": cut.parse::<u64>().ok(),
    })
}

pub(super) fn parse_thread_start_options(
    params: &Value,
    default_cwd: &Path,
) -> Result<ThreadStartOptions, (i64, String)> {
    let cwd = resolve_cwd(params.get("cwd"), default_cwd)?;

    let model = match params.get("model") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) if !raw.trim().is_empty() => Some(raw.trim().to_string()),
        Some(Value::String(_)) => {
            return Err((wire::INVALID_PARAMS, "'model' must not be empty".into()));
        }
        Some(_) => return Err((wire::INVALID_PARAMS, "'model' must be a string".into())),
    };

    let provider_id = match params.get("provider_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) if !raw.trim().is_empty() => Some(raw.trim().to_string()),
        Some(Value::String(_)) => {
            return Err((
                wire::INVALID_PARAMS,
                "'provider_id' must not be empty".into(),
            ));
        }
        Some(_) => {
            return Err((
                wire::INVALID_PARAMS,
                "'provider_id' must be a string".into(),
            ));
        }
    };

    Ok(ThreadStartOptions {
        cwd,
        provider_id,
        model,
    })
}

pub(super) fn str_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, (i64, String)> {
    params[key].as_str().ok_or_else(|| {
        (
            wire::INVALID_PARAMS,
            format!("missing string param '{key}'"),
        )
    })
}

/// Parse a `turn/start` (or `turn/steer`) `input` into its user text and any
/// images, which trail it. The structured form is an array of content parts —
/// `{type:"text",text}` and `{type:"image",source:{…}}`, the canonical block
/// shapes — and text parts join with newlines. Any other block is refused: a
/// user message has no place for one, and a provider would reject or drop it.
/// A bare string is accepted too as a single text part, for a client that only
/// sends text.
pub(super) fn parse_input(params: &Value) -> Result<(String, Vec<ImageSource>), (i64, String)> {
    let input = &params["input"];
    if let Some(s) = input.as_str() {
        return Ok((s.to_string(), Vec::new()));
    }
    let Some(parts) = input.as_array() else {
        return Err((
            wire::INVALID_PARAMS,
            "missing 'input' (a string or an array of content parts)".into(),
        ));
    };
    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    for part in parts {
        let block: ContentBlock = serde_json::from_value(part.clone())
            .map_err(|e| (wire::INVALID_PARAMS, format!("bad input part: {e}")))?;
        match block {
            ContentBlock::Text { text } => text_parts.push(text),
            ContentBlock::Image { source } => images.push(source),
            _ => {
                return Err((
                    wire::INVALID_PARAMS,
                    "input takes only text and image parts".into(),
                ));
            }
        }
    }
    Ok((text_parts.join("\n"), images))
}
