//! MCP 门面与启动编排；配置、生命周期、工具和资源适配各在子模块中。

mod config;
mod lifecycle;
mod resources;
mod tools;

#[cfg(test)]
mod testutil;

use std::sync::Arc;

use anyhow::Result;
use kloop_core::tools::ToolSource;
use kloop_mcp::McpAuthenticationError;
use kloop_mcp::McpClient;
use kloop_mcp::McpNotification;
use kloop_mcp::McpServerCapabilities;
use kloop_mcp::McpSessionReinitialized;
use kloop_mcp::McpTransportHealth;
use kloop_mcp::oauth::OAuthSession;
use kloop_protocol::ToolDef;
use kloop_server::McpServerState;
use kloop_server::McpServerStatus;
use kloop_server::McpToolInfo;
use tokio_util::sync::CancellationToken;

pub use config::McpServerConfig;
pub use config::McpTransport;
pub(crate) use config::load_defer_threshold;
pub use config::load_mcp_servers;
pub use config::qualified_name;
pub use lifecycle::McpConnections;
pub use lifecycle::McpLifecycleOwner;

use config::http_headers_for;
use lifecycle::McpAuthAvailability;
use lifecycle::McpFailureKind;
use lifecycle::spawn_health_monitor;
use resources::McpResourceServer;
use resources::McpResourceSource;
use tools::McpToolSource;
use tools::spawn_tool_refresh;

use crate::mcp_auth::CredentialStore;

const DYNAMIC_CATALOG_UNSUPPORTED: &str = "server advertises tools/list_changed, but this transport has no notification stream; the startup tool catalog will remain fixed";

fn dynamic_catalog_message(
    capabilities: &McpServerCapabilities,
    has_notifications: bool,
) -> Option<&'static str> {
    (capabilities.tools_list_changed && !has_notifications).then_some(DYNAMIC_CATALOG_UNSUPPORTED)
}

async fn initial_tool_catalog(
    client: &McpClient,
    capabilities: &mut McpServerCapabilities,
) -> Result<Vec<ToolDef>> {
    match client.list_tools().await {
        Ok(advertised) => Ok(advertised),
        Err(error) => match error.downcast_ref::<McpSessionReinitialized>() {
            Some(session) => {
                *capabilities = session.capabilities.clone();
                client.list_tools().await
            }
            None => Err(error),
        },
    }
}

fn classify_startup_failure(
    server: &McpServerConfig,
    error: &anyhow::Error,
    oauth_status_session: Option<&Arc<OAuthSession>>,
    startup_auth: Option<McpAuthAvailability>,
) -> (Option<McpAuthAvailability>, McpFailureKind) {
    let typed_auth = error.downcast_ref::<McpAuthenticationError>().is_some();
    let oauth_reauthentication = oauth_status_session.is_some_and(|oauth| oauth.needs_relogin());
    let failed_auth = if typed_auth {
        // `StaticCredentialUnavailable` used to mean "the env var was not
        // set", which could be known before connecting. A token written in
        // the config is always present, so the state now means the only thing
        // left it can mean: the server rejected the one we have.
        Some(match &server.transport {
            McpTransport::Http {
                bearer_token: Some(_),
                ..
            } => McpAuthAvailability::StaticCredentialUnavailable,
            McpTransport::Http {
                bearer_token: None, ..
            } if oauth_status_session.is_some() => McpAuthAvailability::ReauthenticationRequired,
            McpTransport::Http {
                bearer_token: None, ..
            } => McpAuthAvailability::LoginRequired,
            McpTransport::Stdio { .. } => McpAuthAvailability::NotApplicable,
        })
    } else if oauth_reauthentication {
        Some(McpAuthAvailability::ReauthenticationRequired)
    } else {
        startup_auth
    };
    let failure = if typed_auth || oauth_reauthentication {
        McpFailureKind::Authentication
    } else {
        McpFailureKind::Startup
    };
    (failed_auth, failure)
}

/// Planning state for one server plus the result of its concurrent connect.
struct ServerStartup {
    server: McpServerConfig,
    source: Arc<McpToolSource>,
    oauth_capable: bool,
    startup_auth: Option<McpAuthAvailability>,
    oauth_status_session: Option<Arc<OAuthSession>>,
    outcome: Result<ConnectedServer>,
}

/// What a successful connection hands to the ordered post-processing pass.
struct ConnectedServer {
    client: Arc<McpClient>,
    oauth: Option<Arc<OAuthSession>>,
    capabilities: McpServerCapabilities,
    notifications: Option<tokio::sync::broadcast::Receiver<McpNotification>>,
    health: Option<tokio::sync::watch::Receiver<McpTransportHealth>>,
    advertised: Vec<ToolDef>,
}

/// Spawn + handshake + tool discovery for one planned server.
async fn connect_one(
    server: McpServerConfig,
    source: Arc<McpToolSource>,
    oauth_capable: bool,
    startup_auth: Option<McpAuthAvailability>,
    oauth_status_session: Option<Arc<OAuthSession>>,
    oauth_session: Result<Option<Arc<OAuthSession>>>,
) -> ServerStartup {
    let outcome = async {
        let (client, oauth) = match &server.transport {
            McpTransport::Stdio { command, env } => (McpClient::spawn(command, env)?, None),
            McpTransport::Http {
                url,
                bearer_token,
                http_headers,
                ..
            } => {
                let headers = http_headers_for(bearer_token, http_headers);
                let oauth = oauth_session?;
                (McpClient::http(url.clone(), headers, oauth.clone())?, oauth)
            }
        };
        let client = Arc::new(client);
        let health = client.subscribe_health();
        let mut capabilities = client.initialize().await?;
        let advertised = initial_tool_catalog(&client, &mut capabilities).await?;
        let notifications = capabilities
            .tools_list_changed
            .then(|| client.subscribe_notifications())
            .flatten();
        anyhow::Ok(ConnectedServer {
            client,
            oauth,
            capabilities,
            notifications,
            health,
            advertised,
        })
    }
    .await;
    ServerStartup {
        server,
        source,
        oauth_capable,
        startup_auth,
        oauth_status_session,
        outcome,
    }
}

/// Spawn + handshake + tool discovery for every configured server. Servers are
/// connected concurrently, so a slow or hanging one costs only its own timeout
/// instead of every later server's too. Results are folded back in
/// configuration order because tool-name collision precedence and the warning
/// stream depend on it. A failing server degrades to a warning but retains a
/// source-owned unavailable route.
pub async fn connect_servers(
    servers: Vec<McpServerConfig>,
    warn: &dyn Fn(&str),
) -> Result<McpConnections> {
    let store = Arc::new(CredentialStore::default_path()?);
    let lifecycle = McpLifecycleOwner::default();
    let mut resource_servers = Vec::new();
    let mut startups = Vec::new();
    for server in servers {
        let source = McpToolSource::planned(&server);
        lifecycle.register_source(source.clone());
        source.starting();
        resource_servers.push(McpResourceServer {
            name: server.name.clone(),
            source: source.clone(),
        });
        // A remote server with no static bearer takes the OAuth path: use a
        // stored token if the user has logged in; else connect unauthenticated
        // and, if that's rejected, point them at the login command.
        let oauth_capable = matches!(
            &server.transport,
            McpTransport::Http {
                bearer_token: None,
                ..
            }
        );
        let oauth_session = match &server.transport {
            McpTransport::Http {
                url,
                bearer_token: None,
                ..
            } => store.session_for(&server.name, url),
            _ => Ok(None),
        };
        let startup_auth = if oauth_capable {
            Some(match &oauth_session {
                Ok(Some(_)) => McpAuthAvailability::OAuthAvailable,
                Ok(None) | Err(_) => McpAuthAvailability::LoginRequired,
            })
        } else {
            None
        };
        let oauth_status_session = oauth_session
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .cloned();
        startups.push(connect_one(
            server,
            source,
            oauth_capable,
            startup_auth,
            oauth_status_session,
            oauth_session,
        ));
    }
    let mut sources: Vec<Arc<dyn ToolSource>> = Vec::new();
    let mut statuses = Vec::new();
    for startup in futures::future::join_all(startups).await {
        let ServerStartup {
            server,
            source,
            oauth_capable,
            startup_auth,
            oauth_status_session,
            outcome,
        } = startup;
        let transport = server.transport.kind();
        match outcome {
            Ok(ConnectedServer {
                client,
                oauth,
                capabilities,
                notifications,
                health,
                advertised,
            }) => {
                let count = advertised.len();
                let has_notifications = notifications.is_some();
                source.ready(
                    client,
                    oauth,
                    capabilities.clone(),
                    advertised,
                    has_notifications,
                    warn,
                );
                let tools = source
                    .state
                    .read()
                    .unwrap()
                    .catalog
                    .defs
                    .iter()
                    .map(|def| McpToolInfo {
                        name: def.name.clone(),
                        description: def.description.clone(),
                    })
                    .collect();
                let dynamic_message =
                    dynamic_catalog_message(&capabilities, has_notifications).map(str::to_string);
                if let Some(message) = &dynamic_message {
                    warn(&format!("mcp server '{}': {message}", server.name));
                }
                warn(&format!(
                    "mcp server '{}': connected, {count} tool(s)",
                    server.name
                ));
                statuses.push(McpServerStatus {
                    name: server.name.clone(),
                    transport,
                    state: McpServerState::Connected,
                    tools,
                    message: dynamic_message,
                });
                let source_closed = CancellationToken::new();
                if let Some(health) = health {
                    spawn_health_monitor(&lifecycle, source.clone(), health, source_closed.clone());
                }
                if let Some(notifications) = notifications {
                    spawn_tool_refresh(&lifecycle, source.clone(), notifications, source_closed);
                }
            }
            Err(error) => {
                let (failed_auth, failure) = classify_startup_failure(
                    &server,
                    &error,
                    oauth_status_session.as_ref(),
                    startup_auth,
                );
                source.startup_failed(failed_auth, failure);
                let hint = if oauth_capable {
                    format!("; if it needs OAuth, run: kloop mcp login {}", server.name)
                } else {
                    String::new()
                };
                warn(&format!("mcp server '{}' unavailable{hint}", server.name));
                statuses.push(McpServerStatus {
                    name: server.name.clone(),
                    transport,
                    state: McpServerState::Unavailable,
                    tools: Vec::new(),
                    // Never reflect the raw anyhow chain onto the protocol or
                    // warning stream: Desktop forwards stderr into the WebView,
                    // and errors may quote URL credentials, commands, or env.
                    message: Some("connection or tool discovery failed; see engine log".into()),
                });
            }
        }
        sources.push(source);
    }
    if !resource_servers.is_empty() {
        sources.push(Arc::new(McpResourceSource::new(resource_servers)));
    }
    Ok(McpConnections {
        sources,
        statuses,
        lifecycle,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use kloop_core::tools::SourceDefinitionState;
    use kloop_server::McpTransportKind;
    #[cfg(unix)]
    use serde_json::json;

    #[tokio::test]
    async fn failed_connections_remain_visible_without_leaking_details() {
        let warnings = std::sync::Mutex::new(Vec::new());
        let warn = |warning: &str| warnings.lock().unwrap().push(warning.to_string());
        let connections = connect_servers(
            vec![McpServerConfig {
                name: "broken".into(),
                transport: McpTransport::Stdio {
                    command: vec!["/definitely/missing/kloop-mcp-secret".into()],
                    env: BTreeMap::from([("TOKEN".into(), "SUPER-SECRET".into())]),
                },
                readonly_tools: Vec::new(),
            }],
            &warn,
        )
        .await
        .unwrap();

        assert_eq!(connections.sources.len(), 2);
        match connections.sources[0].definition_state("broken__echo") {
            SourceDefinitionState::Unavailable { reason } => {
                assert!(reason.contains("unavailable, not missing"), "{reason}");
            }
            state => panic!("failed source was not retained: {state:?}"),
        }
        assert_eq!(
            connections.statuses,
            vec![McpServerStatus {
                name: "broken".into(),
                transport: McpTransportKind::Stdio,
                state: McpServerState::Unavailable,
                tools: Vec::new(),
                message: Some("connection or tool discovery failed; see engine log".into()),
            }]
        );
        let wire = serde_json::to_string(&connections.statuses).unwrap();
        assert!(!wire.contains("SUPER-SECRET"));
        assert!(!wire.contains("kloop-mcp-secret"));
        let warnings = warnings.lock().unwrap();
        assert_eq!(
            *warnings,
            vec!["mcp server 'broken' unavailable".to_string()]
        );
        assert!(!warnings[0].contains("SUPER-SECRET"));
        assert!(!warnings[0].contains("kloop-mcp-secret"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_tool_list_recovers_reinitialized_http_session() {
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::body_partial_json;
        use wiremock::matchers::header;
        use wiremock::matchers::method;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "initialize"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .insert_header("mcp-session-id", "sess-1")
                    .set_body_json(json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "serverInfo": {"name": "fixture", "version": "1"}
                        }
                    })),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "initialize"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .insert_header("mcp-session-id", "sess-2")
                    .set_body_json(json!({
                        "jsonrpc": "2.0",
                        "id": 3,
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {"resources": {}},
                            "serverInfo": {"name": "fixture", "version": "2"}
                        }
                    })),
            )
            .with_priority(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "notifications/initialized"}),
            ))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "tools/list"})))
            .and(header("mcp-session-id", "sess-1"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "tools/list"})))
            .and(header("mcp-session-id", "sess-2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({
                        "jsonrpc": "2.0",
                        "id": 4,
                        "result": {
                            "tools": [{
                                "name": "echo",
                                "description": "echo",
                                "inputSchema": {"type": "object"}
                            }]
                        }
                    })),
            )
            .mount(&server)
            .await;

        let client = McpClient::http(server.uri(), BTreeMap::new(), None).unwrap();
        let mut capabilities = client.initialize().await.unwrap();
        let tools = initial_tool_catalog(&client, &mut capabilities)
            .await
            .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert!(capabilities.resources);
    }

    #[cfg(unix)]
    async fn delayed_http_server(delay: std::time::Duration) -> wiremock::MockServer {
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::body_partial_json;
        use wiremock::matchers::method;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "initialize"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(delay)
                    .insert_header("content-type", "application/json")
                    .insert_header("mcp-session-id", "sess-1")
                    .set_body_json(json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "serverInfo": {"name": "fixture", "version": "1"}
                        }
                    })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "notifications/initialized"}),
            ))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "tools/list"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(delay)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "result": {"tools": []}
                    })),
            )
            .mount(&server)
            .await;
        server
    }

    #[cfg(unix)]
    fn static_bearer_server(name: &str, url: String) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            transport: McpTransport::Http {
                url,
                bearer_token: Some("fixture-token".into()),
                http_headers: BTreeMap::new(),
                oauth_client_id: None,
                oauth_scopes: Vec::new(),
            },
            readonly_tools: Vec::new(),
        }
    }

    /// Each server's handshake is `initialize` + `tools/list`, both delayed, so
    /// serializing two servers costs four delays. Running them at once costs
    /// two; the ordering assertions pin the deterministic fold-back.
    #[cfg(unix)]
    #[tokio::test]
    async fn servers_connect_concurrently_and_report_in_config_order() {
        use std::time::Duration;
        use std::time::Instant;

        const DELAY: Duration = Duration::from_millis(750);
        let first = delayed_http_server(DELAY).await;
        let second = delayed_http_server(DELAY).await;
        let warnings = std::sync::Mutex::new(Vec::new());
        let warn = |warning: &str| warnings.lock().unwrap().push(warning.to_string());

        let started = Instant::now();
        let connections = connect_servers(
            vec![
                static_bearer_server("alpha", first.uri()),
                static_bearer_server("beta", second.uri()),
            ],
            &warn,
        )
        .await
        .unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed < DELAY * 3,
            "two {DELAY:?}-delay servers took {elapsed:?}; startup still looks serial"
        );
        assert_eq!(
            connections
                .statuses
                .iter()
                .map(|status| status.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
        assert_eq!(
            *warnings.lock().unwrap(),
            vec![
                "mcp server 'alpha': connected, 0 tool(s)".to_string(),
                "mcp server 'beta': connected, 0 tool(s)".to_string(),
            ]
        );
        connections.lifecycle.shutdown().await;
    }

    #[test]
    fn dynamic_catalog_boundary_is_explicit_without_notifications() {
        let capabilities = McpServerCapabilities {
            tools_list_changed: true,
            ..Default::default()
        };
        assert_eq!(
            dynamic_catalog_message(&capabilities, false),
            Some(DYNAMIC_CATALOG_UNSUPPORTED)
        );
        assert_eq!(dynamic_catalog_message(&capabilities, true), None);
        assert_eq!(
            dynamic_catalog_message(&McpServerCapabilities::default(), false),
            None
        );
    }
}
