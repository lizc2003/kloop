//! 每个 MCP 服务的工具目录、运行期就绪状态、调用与目录刷新。

use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use kloop_core::tools::SourceDefinitionState;
use kloop_core::tools::SourceOutput;
use kloop_core::tools::SourceVersion;
use kloop_core::tools::ToolSource;
use kloop_mcp::McpAuthenticationError;
use kloop_mcp::McpClient;
use kloop_mcp::McpNotification;
use kloop_mcp::McpRpcError;
use kloop_mcp::McpServerCapabilities;
use kloop_mcp::McpSessionReinitialized;
use kloop_mcp::McpToolCallError;
use kloop_mcp::McpTransportState;
use kloop_mcp::oauth::OAuthSession;
use kloop_protocol::ToolDef;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::config::McpServerConfig;
use super::config::qualified_name_component;
use super::lifecycle::McpAuthAvailability;
use super::lifecycle::McpCapabilitySummary;
use super::lifecycle::McpFailureKind;
use super::lifecycle::McpHealthResult;
use super::lifecycle::McpLifecycleOwner;
use super::lifecycle::McpLifecycleState;
use super::lifecycle::McpReadinessReceipt;
use super::lifecycle::McpRefreshState;
use super::lifecycle::endpoint_binding;
use super::lifecycle::failure_kind;
use super::qualified_name;

/// Model-visible tool names must satisfy the providers' `[a-zA-Z0-9_-]`
/// pattern AND kloop's permission-rule grammar (alnum + `_` only, so a
/// "p"-persisted allow rule parses back on the next start). 64 is the
/// stricter (OpenAI-compat) length limit.
const MAX_TOOL_NAME_LEN: usize = 64;
const TOOL_REFRESH_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_millis(100), Duration::from_millis(500)];

/// One immutable tool-generation snapshot. A tools/list_changed refresh builds
/// the complete replacement first, then publishes it under one write lock.
pub(super) struct McpToolCatalog {
    generation: u64,
    pub(super) defs: Arc<[ToolDef]>,
    raw_names: HashMap<String, String>,
    readonly: HashSet<String>,
}

pub(super) struct McpRuntimeState {
    terminal: bool,
    pub(super) client: Option<Arc<McpClient>>,
    oauth: Option<Arc<OAuthSession>>,
    dynamic_refresh: bool,
    pub(super) capabilities: McpServerCapabilities,
    pub(super) catalog: Arc<McpToolCatalog>,
    pub(super) receipt: McpReadinessReceipt,
}

/// One configured server exposed through core's tool seam, including startup
/// failures. Calls hold the read side of `refresh_gate` through the wire request;
/// refresh and transport-health transitions take the write side.
pub(super) struct McpToolSource {
    server: McpServerConfig,
    qualified_prefix: String,
    pub(super) state: RwLock<McpRuntimeState>,
    pub(super) refresh_gate: tokio::sync::RwLock<()>,
}

impl McpToolSource {
    pub(super) fn planned(server: &McpServerConfig) -> Arc<Self> {
        Arc::new(Self {
            server: server.clone(),
            qualified_prefix: format!("{}__", qualified_name_component(&server.name)),
            state: RwLock::new(McpRuntimeState {
                terminal: false,
                client: None,
                oauth: None,
                dynamic_refresh: false,
                capabilities: McpServerCapabilities::default(),
                catalog: Arc::new(build_catalog(server, 0, Vec::new(), &|_| {})),
                receipt: McpReadinessReceipt::new(server),
            }),
            refresh_gate: tokio::sync::RwLock::new(()),
        })
    }

    #[cfg(test)]
    pub(super) fn receipt(&self) -> McpReadinessReceipt {
        self.state.read().unwrap().receipt.clone()
    }

    fn transition(&self, update: impl FnOnce(&mut McpRuntimeState)) {
        let mut state = self.state.write().unwrap();
        if state.terminal {
            return;
        }
        debug_assert_eq!(state.receipt.server_identity, self.server.name);
        debug_assert_eq!(state.receipt.endpoint, endpoint_binding(&self.server));
        update(&mut state);
        state.receipt.catalog_generation = state.catalog.generation;
        state.receipt.advance();
    }

    pub(super) fn starting(&self) {
        self.transition(|state| {
            state.receipt.lifecycle = McpLifecycleState::Starting;
            state.receipt.last_health = McpHealthResult::Unknown;
            state.receipt.failure = None;
        });
    }

    pub(super) fn ready(
        &self,
        client: Arc<McpClient>,
        oauth: Option<Arc<OAuthSession>>,
        capabilities: McpServerCapabilities,
        advertised: Vec<ToolDef>,
        has_notifications: bool,
        warn: &dyn Fn(&str),
    ) {
        let catalog = Arc::new(build_catalog(&self.server, 0, advertised, warn));
        self.transition(|state| {
            state.client = Some(client);
            state.oauth = oauth;
            state.dynamic_refresh = has_notifications;
            state.catalog = catalog;
            state.receipt.auth = if state.oauth.is_some() {
                McpAuthAvailability::OAuthAvailable
            } else if state.receipt.auth == McpAuthAvailability::StaticCredentialConfigured {
                McpAuthAvailability::StaticCredentialAvailable
            } else {
                state.receipt.auth
            };
            state.capabilities = capabilities.clone();
            state.receipt.capabilities = McpCapabilitySummary::from(&capabilities);
            state.receipt.lifecycle = McpLifecycleState::Ready;
            state.receipt.refresh = if has_notifications {
                McpRefreshState::Idle
            } else {
                McpRefreshState::StartupFixed
            };
            state.receipt.last_health = McpHealthResult::Healthy;
            state.receipt.failure = None;
        });
    }

    pub(super) fn startup_failed(
        &self,
        auth: Option<McpAuthAvailability>,
        failure: McpFailureKind,
    ) {
        self.transition(|state| {
            if let Some(auth) = auth {
                state.receipt.auth = auth;
            }
            state.receipt.lifecycle = McpLifecycleState::Failed;
            state.receipt.refresh = McpRefreshState::Unavailable;
            state.receipt.last_health = McpHealthResult::Failed;
            state.receipt.failure = Some(failure);
        });
    }

    fn public_name(&self) -> String {
        self.server
            .name
            .chars()
            .filter(|character| {
                !character.is_control()
                    && !matches!(
                        *character,
                        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}'
                    )
            })
            .take(128)
            .collect()
    }

    fn unavailable_reason_from(&self, receipt: &McpReadinessReceipt) -> String {
        let failure = receipt.failure.unwrap_or(McpFailureKind::Request);
        format!(
            "MCP server \"{}\" is {:?}: {}. Its configured tools are unavailable, not missing.",
            self.public_name(),
            receipt.lifecycle,
            failure.reason()
        )
    }

    fn unavailable_reason(&self) -> String {
        self.unavailable_reason_from(&self.state.read().unwrap().receipt)
    }

    pub(super) fn abort_client(&self) {
        if let Some(client) = self.state.read().unwrap().client.clone() {
            client.abort();
        }
    }

    pub(super) async fn shutdown_client(&self) {
        let client = { self.state.read().unwrap().client.clone() };
        if let Some(client) = client {
            client.shutdown().await;
        }
    }

    pub(super) fn close(&self, failure: McpFailureKind) {
        self.transition(|state| {
            state.terminal = true;
            state.receipt.lifecycle = McpLifecycleState::Closed;
            state.receipt.last_health = McpHealthResult::Closed;
            state.receipt.failure = Some(failure);
        });
    }

    pub(super) async fn transport_health(&self, health: McpTransportState) {
        match health {
            McpTransportState::Healthy => {
                let recover = {
                    let state = self.state.read().unwrap();
                    matches!(
                        state.receipt.failure,
                        Some(
                            McpFailureKind::TransportClosed
                                | McpFailureKind::TransportRead
                                | McpFailureKind::TransportWrite
                                | McpFailureKind::RequestTimeout
                                | McpFailureKind::Request
                        )
                    ) && state.client.is_some()
                };
                if recover {
                    let _lifecycle = self.refresh_gate.write().await;
                    self.transition(|state| {
                        state.receipt.lifecycle = McpLifecycleState::Ready;
                        state.receipt.refresh = if state.dynamic_refresh {
                            McpRefreshState::Idle
                        } else {
                            McpRefreshState::StartupFixed
                        };
                        state.receipt.last_health = McpHealthResult::Healthy;
                        state.receipt.failure = None;
                    });
                }
            }
            McpTransportState::Degraded(failure) => {
                let _lifecycle = self.refresh_gate.write().await;
                self.transition(|state| {
                    state.receipt.lifecycle = McpLifecycleState::Degraded;
                    state.receipt.last_health = McpHealthResult::Degraded;
                    state.receipt.failure = Some(failure_kind(failure));
                });
            }
            McpTransportState::Closed(failure) => {
                let _lifecycle = self.refresh_gate.write().await;
                self.transition(|state| {
                    state.terminal = true;
                    state.receipt.lifecycle = McpLifecycleState::Closed;
                    state.receipt.last_health = McpHealthResult::Closed;
                    state.receipt.failure = Some(failure_kind(failure));
                });
            }
        }
    }

    pub(super) async fn authentication_refreshed(&self) {
        let _lifecycle = self.refresh_gate.write().await;
        if self.state.read().unwrap().receipt.failure == Some(McpFailureKind::Authentication) {
            return;
        }
        self.transition(|state| {
            state.receipt.auth = McpAuthAvailability::OAuthAvailable;
            if state.receipt.lifecycle == McpLifecycleState::Ready {
                state.receipt.last_health = McpHealthResult::Healthy;
            }
        });
    }

    pub(super) async fn notification_closed(&self) {
        let _lifecycle = self.refresh_gate.write().await;
        self.transition(|state| {
            state.terminal = true;
            state.receipt.lifecycle = McpLifecycleState::Closed;
            state.receipt.last_health = McpHealthResult::Closed;
            state.receipt.failure = Some(McpFailureKind::NotificationClosed);
        });
    }

    fn mark_authentication_failed(state: &mut McpRuntimeState) {
        state.receipt.auth = match state.receipt.auth {
            McpAuthAvailability::StaticCredentialConfigured
            | McpAuthAvailability::StaticCredentialAvailable
            | McpAuthAvailability::StaticCredentialUnavailable => {
                McpAuthAvailability::StaticCredentialUnavailable
            }
            McpAuthAvailability::Unauthenticated | McpAuthAvailability::LoginRequired => {
                McpAuthAvailability::LoginRequired
            }
            McpAuthAvailability::OAuthAvailable | McpAuthAvailability::ReauthenticationRequired => {
                McpAuthAvailability::ReauthenticationRequired
            }
            McpAuthAvailability::NotApplicable => McpAuthAvailability::NotApplicable,
        };
        state.receipt.lifecycle = McpLifecycleState::Failed;
        state.receipt.last_health = McpHealthResult::Failed;
        state.receipt.failure = Some(McpFailureKind::Authentication);
    }

    fn classify_request_error(&self, error: anyhow::Error) -> anyhow::Error {
        if error.downcast_ref::<McpRpcError>().is_some()
            || error.downcast_ref::<McpToolCallError>().is_some()
        {
            return error;
        }
        if let Some(session) = error.downcast_ref::<McpSessionReinitialized>() {
            let capabilities = session.capabilities.clone();
            self.transition(|state| {
                state.capabilities = capabilities.clone();
                state.receipt.capabilities = McpCapabilitySummary::from(&capabilities);
                state.receipt.lifecycle = McpLifecycleState::Stale;
                state.receipt.refresh = McpRefreshState::Refreshing;
                state.receipt.last_health = McpHealthResult::Stale;
                state.receipt.failure = Some(McpFailureKind::RefreshInProgress);
            });
            return anyhow::anyhow!(session.to_string());
        }
        let state = self.state.read().unwrap();
        let auth_failed = error.downcast_ref::<McpAuthenticationError>().is_some()
            || state
                .oauth
                .as_ref()
                .is_some_and(|oauth| oauth.needs_relogin());
        drop(state);
        self.transition(|state| {
            if auth_failed {
                Self::mark_authentication_failed(state);
            } else {
                state.receipt.lifecycle = McpLifecycleState::Degraded;
                state.receipt.last_health = McpHealthResult::Degraded;
                state.receipt.failure = Some(McpFailureKind::Request);
            }
        });
        anyhow::anyhow!(self.unavailable_reason())
    }

    pub(super) fn classify_request<T>(&self, result: Result<T>) -> Result<T> {
        result.map_err(|error| self.classify_request_error(error))
    }

    pub(super) fn available_capabilities(&self) -> Result<McpServerCapabilities> {
        let state = self.state.read().unwrap();
        if !state.receipt.is_ready() {
            bail!(self.unavailable_reason_from(&state.receipt));
        }
        Ok(state.capabilities.clone())
    }

    pub(super) fn resource_preflight(&self, directory: bool) -> Result<()> {
        let capabilities = self.available_capabilities()?;
        if !capabilities.resources {
            bail!(
                "Server \"{}\" does not advertise MCP resource support.",
                self.public_name()
            );
        }
        if directory && !capabilities.directory_read {
            bail!(
                "Server \"{}\" does not support directory listing.",
                self.public_name()
            );
        }
        Ok(())
    }

    pub(super) fn ready_client(&self) -> Result<(Arc<McpClient>, McpServerCapabilities)> {
        let state = self.state.read().unwrap();
        if !state.receipt.is_ready() {
            bail!(self.unavailable_reason_from(&state.receipt));
        }
        let client = state
            .client
            .clone()
            .context("ready MCP source has no client")?;
        Ok((client, state.capabilities.clone()))
    }

    async fn refresh_with_delays(&self, delays: &[Duration]) {
        let _refresh = self.refresh_gate.write().await;
        let client = {
            let state = self.state.read().unwrap();
            if state.client.is_none() || state.receipt.lifecycle == McpLifecycleState::Closed {
                return;
            }
            state.client.clone()
        };
        let Some(client) = client else {
            return;
        };
        self.transition(|state| {
            state.receipt.lifecycle = McpLifecycleState::Stale;
            state.receipt.refresh = McpRefreshState::Refreshing;
            state.receipt.last_health = McpHealthResult::Stale;
            state.receipt.failure = Some(McpFailureKind::RefreshInProgress);
        });

        let mut last_error = None;
        let mut advertised = None;
        for attempt in 0..=delays.len() {
            match client.list_tools().await {
                Ok(tools) => {
                    advertised = Some(tools);
                    break;
                }
                Err(error) => {
                    if let Some(session) = error.downcast_ref::<McpSessionReinitialized>() {
                        let capabilities = session.capabilities.clone();
                        self.transition(|state| {
                            state.capabilities = capabilities.clone();
                            state.receipt.capabilities = McpCapabilitySummary::from(&capabilities);
                        });
                    }
                    last_error = Some(error);
                }
            }
            if let Some(delay) = delays.get(attempt) {
                self.transition(|state| {
                    state.receipt.refresh = McpRefreshState::Retrying {
                        attempt: u8::try_from(attempt + 1).unwrap_or(u8::MAX),
                    };
                });
                tokio::time::sleep(*delay).await;
            }
        }

        if let Some(advertised) = advertised {
            let mut state = self.state.write().unwrap();
            if state.terminal {
                return;
            }
            let generation = state
                .catalog
                .generation
                .checked_add(1)
                .expect("MCP catalog generation exhausted");
            state.catalog = Arc::new(build_catalog(&self.server, generation, advertised, &|_| {}));
            state.receipt.catalog_generation = generation;
            state.receipt.lifecycle = McpLifecycleState::Ready;
            state.receipt.refresh = if state.dynamic_refresh {
                McpRefreshState::Idle
            } else {
                McpRefreshState::StartupFixed
            };
            state.receipt.last_health = McpHealthResult::Healthy;
            state.receipt.failure = None;
            state.receipt.advance();
        } else {
            let explicit_auth_failure = last_error
                .as_ref()
                .is_some_and(|error| error.downcast_ref::<McpAuthenticationError>().is_some());
            let auth_failed = explicit_auth_failure
                || self
                    .state
                    .read()
                    .unwrap()
                    .oauth
                    .as_ref()
                    .is_some_and(|oauth| oauth.needs_relogin());
            self.transition(|state| {
                if auth_failed {
                    Self::mark_authentication_failed(state);
                } else {
                    state.receipt.lifecycle = McpLifecycleState::Degraded;
                    state.receipt.last_health = McpHealthResult::Degraded;
                    state.receipt.failure = Some(McpFailureKind::Refresh);
                }
                state.receipt.refresh = McpRefreshState::RetryExhausted;
            });
            drop(last_error);
        }
    }

    pub(super) async fn refresh_with_retry(&self) {
        self.refresh_with_delays(&TOOL_REFRESH_RETRY_DELAYS).await;
    }
}
impl ToolSource for McpToolSource {
    fn defs(&self) -> Arc<[ToolDef]> {
        let state = self.state.read().unwrap();
        if state.receipt.is_ready() {
            state.catalog.defs.clone()
        } else {
            Arc::from(Vec::<ToolDef>::new())
        }
    }

    fn definition_generation(&self, _tool: &str) -> u64 {
        self.state.read().unwrap().catalog.generation
    }

    fn readiness_revision(&self, _tool: &str) -> u64 {
        self.state.read().unwrap().receipt.readiness_revision
    }

    fn catalog_version(&self) -> SourceVersion {
        let state = self.state.read().unwrap();
        SourceVersion {
            definition_generation: state.catalog.generation,
            readiness_revision: state.receipt.readiness_revision,
        }
    }

    fn definition_snapshot(&self, tool: &str) -> Option<(ToolDef, u64)> {
        let state = self.state.read().unwrap();
        state.receipt.is_ready().then_some(())?;
        state
            .catalog
            .defs
            .iter()
            .find(|def| def.name == tool)
            .cloned()
            .map(|def| (def, state.catalog.generation))
    }

    fn definition_state(&self, tool: &str) -> SourceDefinitionState {
        let state = self.state.read().unwrap();
        if state.receipt.is_ready() {
            return match state.catalog.defs.iter().find(|def| def.name == tool) {
                Some(definition) => SourceDefinitionState::Available {
                    definition: definition.clone(),
                    version: SourceVersion {
                        definition_generation: state.catalog.generation,
                        readiness_revision: state.receipt.readiness_revision,
                    },
                },
                None => SourceDefinitionState::Missing,
            };
        }
        let prefix_matches = tool
            .get(..self.qualified_prefix.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(&self.qualified_prefix));
        if state.catalog.raw_names.contains_key(tool) || prefix_matches {
            SourceDefinitionState::Unavailable {
                reason: self.unavailable_reason_from(&state.receipt),
            }
        } else {
            SourceDefinitionState::Missing
        }
    }

    fn is_readonly(&self, tool: &str) -> bool {
        let state = self.state.read().unwrap();
        state.receipt.is_ready() && state.catalog.readonly.contains(tool)
    }

    fn call<'a>(
        &'a self,
        tool: &'a str,
        input: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
        self.call_at_version(tool, input, None)
    }

    fn call_at_generation<'a>(
        &'a self,
        tool: &'a str,
        input: &'a Value,
        generation: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
        let version = generation.map(|definition_generation| SourceVersion {
            definition_generation,
            readiness_revision: self.readiness_revision(tool),
        });
        self.call_at_version(tool, input, version)
    }

    fn call_at_version<'a>(
        &'a self,
        tool: &'a str,
        input: &'a Value,
        version: Option<SourceVersion>,
    ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
        Box::pin(async move {
            let _call = self.refresh_gate.read().await;
            let (client, catalog, receipt) = {
                let state = self.state.read().unwrap();
                (
                    state.client.clone(),
                    state.catalog.clone(),
                    state.receipt.clone(),
                )
            };
            if !receipt.is_ready() {
                bail!(self.unavailable_reason_from(&receipt));
            }
            let current = SourceVersion {
                definition_generation: catalog.generation,
                readiness_revision: receipt.readiness_revision,
            };
            if version.is_some_and(|expected| expected != current) {
                bail!(
                    "MCP tool definition or server readiness changed after discovery; run tool_search for {tool} again before retrying"
                );
            }
            let raw = catalog
                .raw_names
                .get(tool)
                .cloned()
                .with_context(|| format!("unknown mcp tool: {tool}"))?;
            let client = client.context("ready MCP source has no client")?;
            let structured =
                self.classify_request(client.call_tool_structured(&raw, input).await)?;
            let text = kloop_mcp::render_result(&structured);
            let blocks = kloop_mcp::content_blocks(&structured["content"]);
            Ok(SourceOutput {
                text,
                blocks,
                structured: Some(structured),
            })
        })
    }
}

/// Namespace a server's advertised tools, dropping any oversized or sanitized
/// collision. The same builder is used at startup and for every refresh.
fn build_catalog(
    server: &McpServerConfig,
    generation: u64,
    advertised: Vec<ToolDef>,
    warn: &dyn Fn(&str),
) -> McpToolCatalog {
    let readonly_raw: HashSet<&str> = server.readonly_tools.iter().map(String::as_str).collect();
    let mut defs = Vec::new();
    let mut raw_names = HashMap::new();
    let mut readonly = HashSet::new();
    for mut def in advertised {
        let qualified = qualified_name(&server.name, &def.name);
        if qualified.len() > MAX_TOOL_NAME_LEN {
            warn(&format!(
                "mcp server '{}': tool name '{qualified}' exceeds {MAX_TOOL_NAME_LEN} chars; skipped",
                server.name
            ));
            continue;
        }
        if raw_names.contains_key(&qualified) {
            warn(&format!(
                "mcp server '{}': tool name collision on '{qualified}' after sanitization; \
                 the later tool is skipped",
                server.name
            ));
            continue;
        }
        if readonly_raw.contains(def.name.as_str()) {
            readonly.insert(qualified.clone());
        }
        raw_names.insert(qualified.clone(), std::mem::take(&mut def.name));
        def.name = qualified;
        defs.push(def);
    }
    McpToolCatalog {
        generation,
        defs: Arc::from(defs),
        raw_names,
        readonly,
    }
}

#[cfg(test)]
fn build_source(
    server: &McpServerConfig,
    client: Arc<McpClient>,
    advertised: Vec<ToolDef>,
    warn: &dyn Fn(&str),
) -> Arc<McpToolSource> {
    let source = McpToolSource::planned(server);
    source.starting();
    source.ready(
        client.clone(),
        None,
        client.capabilities(),
        advertised,
        /*has_notifications=*/ false,
        warn,
    );
    source
}

#[cfg(test)]
async fn retry_tool_refresh_with_delays<F, Fut>(delays: &[Duration], mut refresh: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let mut last_error = None;
    for attempt in 0..=delays.len() {
        match refresh().await {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        if let Some(delay) = delays.get(attempt) {
            tokio::time::sleep(*delay).await;
        }
    }
    Err(last_error.expect("refresh loop always attempts at least once"))
}

pub(super) fn spawn_tool_refresh(
    lifecycle: &McpLifecycleOwner,
    source: Arc<McpToolSource>,
    mut notifications: tokio::sync::broadcast::Receiver<McpNotification>,
    source_closed: CancellationToken,
) {
    let cancel = lifecycle.cancel_token();
    lifecycle.spawn(async move {
        loop {
            let notification = tokio::select! {
                _ = cancel.cancelled() => break,
                _ = source_closed.cancelled() => break,
                notification = notifications.recv() => notification,
            };
            let refresh_needed = match notification {
                Ok(McpNotification::ToolsListChanged)
                | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                Ok(McpNotification::ResourcesListChanged)
                | Ok(McpNotification::ResourceUpdated { .. }) => false,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    source.notification_closed().await;
                    source_closed.cancel();
                    source.shutdown_client().await;
                    break;
                }
            };
            if !refresh_needed {
                continue;
            }

            let mut closed = false;
            loop {
                match notifications.try_recv() {
                    Ok(_) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                        closed = true;
                        break;
                    }
                }
            }
            if closed {
                source.notification_closed().await;
                source_closed.cancel();
                source.shutdown_client().await;
                break;
            }
            source.refresh_with_retry().await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use serde_json::json;

    use super::super::classify_startup_failure;
    use super::super::config::McpTransport;
    use super::super::testutil::resource_pair;

    #[tokio::test]
    async fn static_bearer_availability_tracks_rejected_credentials() {
        let server = McpServerConfig {
            name: "static".into(),
            transport: McpTransport::Http {
                url: "https://example.com/mcp".into(),
                bearer_token: Some("sk-static".into()),
                http_headers: BTreeMap::new(),
                oauth_client_id: None,
                oauth_scopes: Vec::new(),
            },
            readonly_tools: Vec::new(),
        };
        // A configured token is always present, so `Unavailable` now has one
        // cause left: the server turned it down.
        let (startup_auth, startup_failure) = classify_startup_failure(
            &server,
            &anyhow::Error::new(McpAuthenticationError),
            None,
            None,
        );
        assert_eq!(
            startup_auth,
            Some(McpAuthAvailability::StaticCredentialUnavailable)
        );
        assert_eq!(startup_failure, McpFailureKind::Authentication);

        let missing = McpToolSource::planned(&server);
        missing.starting();
        missing.startup_failed(
            Some(McpAuthAvailability::StaticCredentialUnavailable),
            McpFailureKind::Authentication,
        );
        let missing_receipt = missing.receipt();
        assert_eq!(
            missing_receipt.auth,
            McpAuthAvailability::StaticCredentialUnavailable
        );
        assert_eq!(
            missing_receipt.failure,
            Some(McpFailureKind::Authentication)
        );

        let (client_io, _server_io) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(client_io);
        let ready = McpToolSource::planned(&server);
        ready.starting();
        ready.ready(
            Arc::new(McpClient::over(reader, writer, None)),
            None,
            McpServerCapabilities::default(),
            Vec::new(),
            /*has_notifications=*/ false,
            &|_| {},
        );
        assert_eq!(
            ready.receipt().auth,
            McpAuthAvailability::StaticCredentialAvailable
        );
        let _ = ready.classify_request_error(anyhow::Error::new(McpAuthenticationError));
        let failed = ready.receipt();
        assert_eq!(failed.lifecycle, McpLifecycleState::Failed);
        assert_eq!(failed.failure, Some(McpFailureKind::Authentication));
        assert_eq!(
            failed.auth,
            McpAuthAvailability::StaticCredentialUnavailable
        );
    }

    #[tokio::test]
    async fn session_reinitialize_updates_capabilities_and_requires_revalidation() {
        let (client_io, _server_io) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(client_io);
        let server = McpServerConfig {
            name: "session".into(),
            transport: McpTransport::Http {
                url: "https://example.com/mcp".into(),
                bearer_token: None,
                http_headers: BTreeMap::new(),
                oauth_client_id: None,
                oauth_scopes: Vec::new(),
            },
            readonly_tools: Vec::new(),
        };
        let source = McpToolSource::planned(&server);
        source.starting();
        source.ready(
            Arc::new(McpClient::over(reader, writer, None)),
            None,
            McpServerCapabilities::default(),
            vec![ToolDef {
                name: "echo".into(),
                description: "echo".into(),
                schema: json!({"type": "object"}),
            }],
            /*has_notifications=*/ false,
            &|_| {},
        );

        let error = source.classify_request_error(anyhow::Error::new(McpSessionReinitialized {
            capabilities: McpServerCapabilities {
                resources: true,
                directory_read: true,
                ..Default::default()
            },
        }));
        assert!(error.to_string().contains("revalidate"));
        let receipt = source.receipt();
        assert_eq!(receipt.lifecycle, McpLifecycleState::Stale);
        assert_eq!(receipt.refresh, McpRefreshState::Refreshing);
        assert_eq!(receipt.capabilities.resources, Some(true));
        assert_eq!(receipt.capabilities.directory_read, Some(true));
        assert!(source.defs().is_empty());
    }

    #[tokio::test]
    async fn mcp_source_rejects_calls_against_a_stale_definition_generation() {
        let (client_io, _server_io) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(client_io);
        let client = Arc::new(McpClient::over(reader, writer, None));
        let server = McpServerConfig {
            name: "fixture".into(),
            transport: McpTransport::Stdio {
                command: vec!["unused".into()],
                env: BTreeMap::new(),
            },
            readonly_tools: Vec::new(),
        };
        let source = build_source(
            &server,
            client,
            vec![ToolDef {
                name: "echo".into(),
                description: "echo".into(),
                schema: json!({"type": "object"}),
            }],
            &|_| {},
        );

        let error = match source
            .call_at_generation("fixture__echo", &json!({}), Some(1))
            .await
        {
            Ok(_) => panic!("stale generation unexpectedly dispatched"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("definition or server readiness changed after discovery")
        );
    }

    #[tokio::test]
    async fn refresh_publication_waits_for_in_flight_generation_calls() {
        let (resource_server, mut wire) = resource_pair("fixture");
        let server = McpServerConfig {
            name: "fixture".into(),
            transport: McpTransport::Stdio {
                command: vec!["unused".into()],
                env: BTreeMap::new(),
            },
            readonly_tools: Vec::new(),
        };
        let client = resource_server
            .source
            .state
            .read()
            .unwrap()
            .client
            .clone()
            .unwrap();
        let source = build_source(
            &server,
            client,
            vec![ToolDef {
                name: "echo".into(),
                description: "old".into(),
                schema: json!({"type": "object", "properties": {"old": {"type": "string"}}}),
            }],
            &|_| {},
        );

        let call_source = source.clone();
        let call = tokio::spawn(async move {
            call_source
                .call_at_generation("fixture__echo", &json!({"old": "x"}), Some(0))
                .await
        });
        let call_request = wire.recv().await;
        assert_eq!(call_request["method"], "tools/call");

        let refresh_source = source.clone();
        let refresh = tokio::spawn(async move { refresh_source.refresh_with_retry().await });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), wire.recv())
                .await
                .is_err(),
            "refresh must not publish or list while an old-generation call holds the gate"
        );

        wire.respond(
            &call_request["id"],
            json!({"content": [{"type": "text", "text": "old-result"}]}),
        )
        .await;
        let output = call.await.unwrap().unwrap();
        assert_eq!(output.text, "old-result");

        let list_request = wire.recv().await;
        assert_eq!(list_request["method"], "tools/list");
        wire.respond(
            &list_request["id"],
            json!({
                "tools": [{
                    "name": "echo",
                    "description": "new",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"new": {"type": "string"}}
                    }
                }]
            }),
        )
        .await;
        refresh.await.unwrap();
        assert_eq!(source.definition_generation("fixture__echo"), 1);
    }

    #[tokio::test]
    async fn closed_receipt_cannot_be_overwritten_by_late_refresh_or_request() {
        let (resource_server, mut wire) = resource_pair("fixture");
        let client = resource_server
            .source
            .state
            .read()
            .unwrap()
            .client
            .clone()
            .unwrap();
        let server = McpServerConfig {
            name: "fixture".into(),
            transport: McpTransport::Stdio {
                command: vec!["unused".into()],
                env: BTreeMap::new(),
            },
            readonly_tools: Vec::new(),
        };
        let source = build_source(
            &server,
            client,
            vec![ToolDef {
                name: "echo".into(),
                description: "old".into(),
                schema: json!({"type": "object"}),
            }],
            &|_| {},
        );
        let refresh_source = source.clone();
        let refresh = tokio::spawn(async move { refresh_source.refresh_with_delays(&[]).await });
        let request = wire.recv().await;
        source.close(McpFailureKind::Shutdown);
        wire.respond(
            &request["id"],
            json!({
                "tools": [{
                    "name": "echo",
                    "description": "late",
                    "inputSchema": {"type": "object"}
                }]
            }),
        )
        .await;
        refresh.await.unwrap();
        let _ = source.classify_request_error(anyhow::anyhow!("late request failure"));
        source.transport_health(McpTransportState::Healthy).await;

        let receipt = source.receipt();
        assert_eq!(receipt.lifecycle, McpLifecycleState::Closed);
        assert_eq!(receipt.failure, Some(McpFailureKind::Shutdown));
        assert_eq!(receipt.catalog_generation, 0);
    }

    #[tokio::test]
    async fn refresh_failure_retains_catalog_but_blocks_until_recovery() {
        let (resource_server, mut wire) = resource_pair("fixture");
        let client = resource_server
            .source
            .state
            .read()
            .unwrap()
            .client
            .clone()
            .unwrap();
        let server = McpServerConfig {
            name: "fixture".into(),
            transport: McpTransport::Stdio {
                command: vec!["unused".into()],
                env: BTreeMap::new(),
            },
            readonly_tools: Vec::new(),
        };
        let source = build_source(
            &server,
            client,
            vec![ToolDef {
                name: "echo".into(),
                description: "old".into(),
                schema: json!({"type": "object"}),
            }],
            &|_| {},
        );
        let discovered = match source.definition_state("fixture__echo") {
            SourceDefinitionState::Available { version, .. } => version,
            state => panic!("unexpected initial state: {state:?}"),
        };

        let refresh_source = source.clone();
        let refresh =
            tokio::spawn(
                async move { refresh_source.refresh_with_delays(&[Duration::ZERO]).await },
            );
        for _ in 0..2 {
            let request = wire.recv().await;
            assert_eq!(request["method"], "tools/list");
            wire.respond_error(&request["id"], -32000, "SECRET_UPSTREAM_FAILURE")
                .await;
        }
        refresh.await.unwrap();

        let degraded = source.receipt();
        assert_eq!(degraded.lifecycle, McpLifecycleState::Degraded);
        assert_eq!(degraded.refresh, McpRefreshState::RetryExhausted);
        assert_eq!(degraded.failure, Some(McpFailureKind::Refresh));
        assert_eq!(degraded.catalog_generation, 0);
        assert!(source.defs().is_empty());
        assert_eq!(
            source.state.read().unwrap().catalog.defs[0].name,
            "fixture__echo"
        );
        match source.definition_state("fixture__echo") {
            SourceDefinitionState::Unavailable { reason } => {
                assert!(reason.contains("retained but stale"), "{reason}");
                assert!(!reason.contains("SECRET_UPSTREAM_FAILURE"), "{reason}");
            }
            state => panic!("degraded catalog was exposed as {state:?}"),
        }

        let recovery_source = source.clone();
        let recovery = tokio::spawn(async move { recovery_source.refresh_with_delays(&[]).await });
        let request = wire.recv().await;
        wire.respond(
            &request["id"],
            json!({
                "tools": [{
                    "name": "echo",
                    "description": "new",
                    "inputSchema": {"type": "object"}
                }]
            }),
        )
        .await;
        recovery.await.unwrap();
        let recovered = source.receipt();
        assert_eq!(recovered.lifecycle, McpLifecycleState::Ready);
        assert_eq!(recovered.catalog_generation, 1);
        let current = match source.definition_state("fixture__echo") {
            SourceDefinitionState::Available { version, .. } => version,
            state => panic!("unexpected recovered state: {state:?}"),
        };
        assert_ne!(current, discovered);
    }

    #[tokio::test]
    async fn tool_refresh_retries_transient_failures_with_a_bound() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = attempts.clone();
        retry_tool_refresh_with_delays(&[Duration::ZERO, Duration::ZERO], move || {
            let seen = seen.clone();
            async move {
                let attempt = seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if attempt < 2 {
                    bail!("transient refresh failure");
                }
                Ok(())
            }
        })
        .await
        .unwrap();
        assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 3);

        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = attempts.clone();
        let error = retry_tool_refresh_with_delays(&[Duration::ZERO], move || {
            let seen = seen.clone();
            async move {
                seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                bail!("permanent refresh failure")
            }
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("permanent refresh failure"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 2);
    }
}
