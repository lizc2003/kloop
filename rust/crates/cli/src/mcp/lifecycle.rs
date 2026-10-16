//! MCP 就绪回执、健康监控与进程级生命周期所有权。

use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use kloop_core::tools::ToolSource;
use kloop_mcp::McpServerCapabilities;
use kloop_mcp::McpTransportFailure;
use kloop_mcp::McpTransportHealth;
use kloop_mcp::McpTransportState;
use kloop_server::McpServerStatus;
use kloop_server::McpTransportKind;
use sha2::Digest;
use sha2::Sha256;
use tokio_util::sync::CancellationToken;

use super::config::McpServerConfig;
use super::config::McpTransport;
use super::tools::McpToolSource;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct McpEndpointBinding {
    transport: McpTransportKind,
    digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum McpLifecycleState {
    Planned,
    Starting,
    Ready,
    Degraded,
    Failed,
    Stale,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum McpAuthAvailability {
    NotApplicable,
    StaticCredentialConfigured,
    StaticCredentialAvailable,
    StaticCredentialUnavailable,
    Unauthenticated,
    LoginRequired,
    OAuthAvailable,
    ReauthenticationRequired,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct McpCapabilitySummary {
    tools: Option<bool>,
    tools_list_changed: Option<bool>,
    pub(super) resources: Option<bool>,
    resources_list_changed: Option<bool>,
    pub(super) directory_read: Option<bool>,
}

impl From<&McpServerCapabilities> for McpCapabilitySummary {
    fn from(capabilities: &McpServerCapabilities) -> Self {
        Self {
            tools: Some(true),
            tools_list_changed: Some(capabilities.tools_list_changed),
            resources: Some(capabilities.resources),
            resources_list_changed: Some(capabilities.resources_list_changed),
            directory_read: Some(capabilities.directory_read),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum McpRefreshState {
    Unavailable,
    StartupFixed,
    Idle,
    Refreshing,
    Retrying { attempt: u8 },
    RetryExhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum McpHealthResult {
    Unknown,
    Healthy,
    Degraded,
    Failed,
    Stale,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum McpFailureKind {
    Startup,
    Authentication,
    TransportClosed,
    TransportRead,
    TransportWrite,
    RequestTimeout,
    RefreshInProgress,
    Refresh,
    Request,
    NotificationClosed,
    Shutdown,
}

impl McpFailureKind {
    pub(super) fn reason(self) -> &'static str {
        match self {
            Self::Startup => "startup handshake or tool discovery failed",
            Self::Authentication => "authentication is unavailable; reauthenticate before retrying",
            Self::TransportClosed => "transport connection closed",
            Self::TransportRead => "transport input failed",
            Self::TransportWrite => "transport output failed",
            Self::RequestTimeout => "the server did not respond before the request deadline",
            Self::RefreshInProgress => {
                "tool catalog refresh is in progress; retry after it settles"
            }
            Self::Refresh => {
                "tool catalog refresh failed; the last-known catalog is retained but stale"
            }
            Self::Request => {
                "an MCP request failed; the last-known catalog is retained but unavailable"
            }
            Self::NotificationClosed => "the transport notification channel closed",
            Self::Shutdown => "the MCP lifecycle owner shut the server down",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct McpReadinessReceipt {
    pub(super) server_identity: String,
    pub(super) endpoint: McpEndpointBinding,
    pub(super) auth: McpAuthAvailability,
    pub(super) capabilities: McpCapabilitySummary,
    pub(super) catalog_generation: u64,
    pub(super) readiness_revision: u64,
    pub(super) lifecycle: McpLifecycleState,
    pub(super) refresh: McpRefreshState,
    pub(super) last_health: McpHealthResult,
    pub(super) failure: Option<McpFailureKind>,
}

impl McpReadinessReceipt {
    pub(super) fn new(server: &McpServerConfig) -> Self {
        Self {
            server_identity: server.name.clone(),
            endpoint: endpoint_binding(server),
            auth: initial_auth_availability(server),
            capabilities: McpCapabilitySummary::default(),
            catalog_generation: 0,
            readiness_revision: 0,
            lifecycle: McpLifecycleState::Planned,
            refresh: McpRefreshState::Unavailable,
            last_health: McpHealthResult::Unknown,
            failure: None,
        }
    }

    pub(super) fn advance(&mut self) {
        self.readiness_revision = self
            .readiness_revision
            .checked_add(1)
            .expect("MCP readiness revision exhausted");
    }

    pub(super) fn is_ready(&self) -> bool {
        self.lifecycle == McpLifecycleState::Ready
    }
}

pub(super) fn endpoint_binding(server: &McpServerConfig) -> McpEndpointBinding {
    let mut digest = Sha256::new();
    let transport = server.transport.kind();
    match &server.transport {
        McpTransport::Stdio { command, env } => {
            digest.update(b"stdio\0");
            // Arguments may carry credentials. The executable identity and env
            // names bind the endpoint without creating a secret-derived oracle.
            if let Some(program) = command.first() {
                digest.update(program.len().to_be_bytes());
                digest.update(program.as_bytes());
            }
            for name in env.keys() {
                digest.update(name.len().to_be_bytes());
                digest.update(name.as_bytes());
            }
        }
        McpTransport::Http { url, .. } => {
            digest.update(b"http\0");
            if let Ok(parsed) = url::Url::parse(url) {
                digest.update(parsed.scheme().as_bytes());
                if let Some(host) = parsed.host_str() {
                    digest.update(host.as_bytes());
                }
                if let Some(port) = parsed.port_or_known_default() {
                    digest.update(port.to_be_bytes());
                }
                digest.update(b"http-origin\0");
            } else {
                digest.update(b"invalid-url");
            }
        }
    }
    McpEndpointBinding {
        transport,
        digest: digest.finalize().into(),
    }
}

fn initial_auth_availability(server: &McpServerConfig) -> McpAuthAvailability {
    match &server.transport {
        McpTransport::Stdio { .. } => McpAuthAvailability::NotApplicable,
        McpTransport::Http {
            bearer_token: Some(_),
            ..
        } => McpAuthAvailability::StaticCredentialConfigured,
        McpTransport::Http {
            bearer_token: None, ..
        } => McpAuthAvailability::Unauthenticated,
    }
}

struct McpLifecycleInner {
    cancel: CancellationToken,
    sources: Mutex<Vec<Arc<McpToolSource>>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Drop for McpLifecycleInner {
    fn drop(&mut self) {
        self.cancel.cancel();
        for task in self.tasks.get_mut().unwrap().drain(..) {
            task.abort();
        }
        for source in self.sources.get_mut().unwrap() {
            source.abort_client();
        }
    }
}

/// Process-level owner for every MCP monitor, refresh worker and transport.
/// Frontends call `shutdown`; Drop is only the synchronous error-path fallback.
#[derive(Clone)]
pub struct McpLifecycleOwner {
    inner: Arc<McpLifecycleInner>,
}

impl Default for McpLifecycleOwner {
    fn default() -> Self {
        Self {
            inner: Arc::new(McpLifecycleInner {
                cancel: CancellationToken::new(),
                sources: Mutex::new(Vec::new()),
                tasks: Mutex::new(Vec::new()),
            }),
        }
    }
}

impl McpLifecycleOwner {
    pub(super) fn register_source(&self, source: Arc<McpToolSource>) {
        self.inner.sources.lock().unwrap().push(source);
    }

    pub(super) fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.inner.tasks.lock().unwrap().push(tokio::spawn(future));
    }

    pub(super) fn cancel_token(&self) -> CancellationToken {
        self.inner.cancel.clone()
    }

    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        let sources = self.inner.sources.lock().unwrap().clone();
        for source in &sources {
            source.close(McpFailureKind::Shutdown);
        }
        for source in &sources {
            source.shutdown_client().await;
        }
        let tasks = std::mem::take(&mut *self.inner.tasks.lock().unwrap());
        for mut task in tasks {
            if tokio::time::timeout(Duration::from_secs(1), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
    }
}

/// Runtime tool sources plus the immutable startup-discovery snapshot exposed
/// by the native protocol. Failed configured servers stay visible in statuses
/// and retain an unavailable ToolSource route.
pub struct McpConnections {
    pub sources: Vec<Arc<dyn ToolSource>>,
    pub statuses: Vec<McpServerStatus>,
    pub lifecycle: McpLifecycleOwner,
}

pub(super) fn failure_kind(failure: McpTransportFailure) -> McpFailureKind {
    match failure {
        McpTransportFailure::RequestTimeout => McpFailureKind::RequestTimeout,
        McpTransportFailure::WriteFailed => McpFailureKind::TransportWrite,
        McpTransportFailure::ConnectionEof => McpFailureKind::TransportClosed,
        McpTransportFailure::ReadFailed | McpTransportFailure::MessageTooLarge => {
            McpFailureKind::TransportRead
        }
        McpTransportFailure::ExplicitShutdown => McpFailureKind::Shutdown,
    }
}

pub(super) fn spawn_health_monitor(
    lifecycle: &McpLifecycleOwner,
    source: Arc<McpToolSource>,
    mut health: tokio::sync::watch::Receiver<McpTransportHealth>,
    source_closed: CancellationToken,
) {
    let cancel = lifecycle.cancel_token();
    lifecycle.spawn(async move {
        let mut seen = McpTransportHealth {
            state: McpTransportState::Healthy,
            session_revision: 0,
            authentication_revision: 0,
        };
        loop {
            let current = *health.borrow_and_update();
            if current.session_revision > seen.session_revision {
                source.refresh_with_retry().await;
            }
            if current.authentication_revision > seen.authentication_revision {
                source.authentication_refreshed().await;
            }
            source.transport_health(current.state).await;
            seen = current;
            if matches!(current.state, McpTransportState::Closed(_)) {
                source_closed.cancel();
                source.shutdown_client().await;
                break;
            }

            let changed = tokio::select! {
                _ = cancel.cancelled() => break,
                changed = health.changed() => changed,
            };
            if changed.is_err() {
                source.notification_closed().await;
                source_closed.cancel();
                source.shutdown_client().await;
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use kloop_core::tools::SourceDefinitionState;
    use serde_json::json;

    use super::super::testutil::resource_pair;

    #[test]
    fn readiness_receipt_is_typed_monotonic_and_secret_free() {
        let server = McpServerConfig {
            name: "secure".into(),
            transport: McpTransport::Http {
                url: "https://user:URL_SECRET@example.com/mcp?token=QUERY_SECRET".into(),
                bearer_token: None,
                http_headers: BTreeMap::from([(
                    "Authorization".into(),
                    "Bearer HEADER_SECRET".into(),
                )]),
                oauth_client_id: None,
                oauth_scopes: Vec::new(),
            },
            readonly_tools: Vec::new(),
        };
        let source = McpToolSource::planned(&server);
        let planned = source.receipt();
        assert_eq!(planned.lifecycle, McpLifecycleState::Planned);
        assert_eq!(planned.readiness_revision, 0);
        assert_eq!(planned.endpoint.transport, McpTransportKind::Http);
        let rendered = format!("{planned:?}");
        for secret in ["URL_SECRET", "QUERY_SECRET", "HEADER_SECRET"] {
            assert!(
                !rendered.contains(secret),
                "receipt leaked {secret}: {rendered}"
            );
        }
        let mut other = server.clone();
        if let McpTransport::Http {
            url, http_headers, ..
        } = &mut other.transport
        {
            *url = "https://other:DIFFERENT_SECRET@example.com/mcp/OTHER_PATH_SECRET?token=OTHER"
                .into();
            *http_headers =
                BTreeMap::from([("Authorization".into(), "Bearer OTHER_HEADER_SECRET".into())]);
        }
        assert_eq!(
            endpoint_binding(&server),
            endpoint_binding(&other),
            "URL credentials/query/path and header values must not affect the binding digest"
        );
        let stdio_a = McpServerConfig {
            name: "stdio".into(),
            transport: McpTransport::Stdio {
                command: vec!["runner".into(), "--token=SECRET_A".into()],
                env: BTreeMap::from([("TOKEN".into(), "SECRET_A".into())]),
            },
            readonly_tools: Vec::new(),
        };
        let mut stdio_b = stdio_a.clone();
        if let McpTransport::Stdio { command, env } = &mut stdio_b.transport {
            command[1] = "--token=SECRET_B".into();
            env.insert("TOKEN".into(), "SECRET_B".into());
        }
        assert_eq!(endpoint_binding(&stdio_a), endpoint_binding(&stdio_b));

        source.starting();
        let starting = source.receipt();
        assert_eq!(starting.lifecycle, McpLifecycleState::Starting);
        assert_eq!(starting.readiness_revision, 1);
        source.startup_failed(
            Some(McpAuthAvailability::LoginRequired),
            McpFailureKind::Startup,
        );
        let failed = source.receipt();
        assert_eq!(failed.lifecycle, McpLifecycleState::Failed);
        assert_eq!(failed.auth, McpAuthAvailability::LoginRequired);
        assert_eq!(failed.failure, Some(McpFailureKind::Startup));
        assert_eq!(failed.readiness_revision, 2);
        match source.definition_state("secure__unknown") {
            SourceDefinitionState::Unavailable { reason } => {
                assert!(reason.contains("unavailable, not missing"), "{reason}");
                assert!(!reason.contains("URL_SECRET"), "{reason}");
            }
            state => panic!("unexpected route state: {state:?}"),
        }
        match source.definition_state("SECURE__UNKNOWN") {
            SourceDefinitionState::Unavailable { .. } => {}
            state => panic!("case-insensitive unavailable route was {state:?}"),
        }
    }

    #[tokio::test]
    async fn health_monitor_processes_initial_closed_snapshot() {
        let (resource_server, _wire) = resource_pair("initial-closed");
        let source = resource_server.source;
        let client = source.state.read().unwrap().client.clone().unwrap();
        let lifecycle = McpLifecycleOwner::default();
        lifecycle.register_source(source.clone());
        let (_health_tx, health_rx) = tokio::sync::watch::channel(McpTransportHealth {
            state: McpTransportState::Closed(McpTransportFailure::ConnectionEof),
            session_revision: 0,
            authentication_revision: 0,
        });
        spawn_health_monitor(
            &lifecycle,
            source.clone(),
            health_rx,
            CancellationToken::new(),
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while source.receipt().lifecycle != McpLifecycleState::Closed {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            source.receipt().failure,
            Some(McpFailureKind::TransportClosed)
        );
        assert!(client.call_tool("echo", &json!({})).await.is_err());
        lifecycle.shutdown().await;
    }

    #[tokio::test]
    async fn lifecycle_owner_explicit_shutdown_closes_routes() {
        let (resource_server, _wire) = resource_pair("shutdown");
        let source = resource_server.source;
        let lifecycle = McpLifecycleOwner::default();
        lifecycle.register_source(source.clone());
        assert_eq!(source.receipt().lifecycle, McpLifecycleState::Ready);

        lifecycle.shutdown().await;

        let closed = source.receipt();
        assert_eq!(closed.lifecycle, McpLifecycleState::Closed);
        assert_eq!(closed.last_health, McpHealthResult::Closed);
        assert_eq!(closed.failure, Some(McpFailureKind::Shutdown));
        match source.definition_state("shutdown__echo") {
            SourceDefinitionState::Unavailable { reason } => {
                assert!(reason.contains("shut the server down"), "{reason}");
            }
            state => panic!("shutdown route was exposed as {state:?}"),
        }
    }
}
