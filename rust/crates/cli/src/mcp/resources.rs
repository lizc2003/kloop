//! MCP 资源工具：目录聚合、已广告 URI 的读取与目录读取。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use kloop_core::tools::SourceOutput;
use kloop_core::tools::ToolSource;
use kloop_mcp::McpClient;
use kloop_mcp::McpResource;
use kloop_mcp::McpRpcError;
use kloop_mcp::McpServerCapabilities;
use kloop_protocol::ContentBlock;
use kloop_protocol::ImageSource;
use kloop_protocol::ToolDef;
use serde_json::Value;
use serde_json::json;

use super::tools::McpToolSource;

const MAX_AGGREGATE_RESOURCE_ITEMS: usize = 10_000;
const MAX_AGGREGATE_RESOURCE_BYTES: usize = 4 * 1024 * 1024;

const LIST_MCP_RESOURCES: &str = "list_mcp_resources";
const READ_MCP_RESOURCE: &str = "read_mcp_resource";
const READ_MCP_RESOURCE_DIR: &str = "read_mcp_resource_dir";

#[derive(Clone)]
pub(super) struct McpResourceServer {
    pub(super) name: String,
    pub(super) source: Arc<McpToolSource>,
}

impl McpResourceServer {
    fn availability(&self) -> Result<McpServerCapabilities> {
        self.source.available_capabilities()
    }

    fn preflight(&self, directory: bool) -> Result<()> {
        self.source.resource_preflight(directory)
    }

    async fn begin_request(
        &self,
    ) -> Result<(
        tokio::sync::RwLockReadGuard<'_, ()>,
        Arc<McpClient>,
        McpServerCapabilities,
    )> {
        let guard = self.source.refresh_gate.read().await;
        let (client, capabilities) = self.source.ready_client()?;
        Ok((guard, client, capabilities))
    }

    fn classify<T>(&self, result: Result<T>) -> Result<T> {
        self.source.classify_request(result)
    }
}

pub(super) struct McpResourceSource {
    defs: Arc<[ToolDef]>,
    servers: Vec<McpResourceServer>,
}

impl McpResourceSource {
    pub(super) fn new(servers: Vec<McpResourceServer>) -> Self {
        let defs = vec![
            ToolDef {
                name: LIST_MCP_RESOURCES.into(),
                description: "List resources advertised by configured MCP servers. Pass server to filter; omit it to aggregate every resources-capable server. Returned entries preserve standard MCP fields and add server.".into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "server": {"type": "string", "description": "Optional MCP server name"}
                    },
                    "additionalProperties": false
                }),
            },
            ToolDef {
                name: READ_MCP_RESOURCE.into(),
                description: "Read one currently advertised MCP resource by server and URI. The call requires normal external-tool approval. Text is returned inline; supported image blobs become image blocks; other blobs are reported without injecting base64 into the model context.".into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "server": {"type": "string"},
                        "uri": {"type": "string"}
                    },
                    "required": ["server", "uri"],
                    "additionalProperties": false
                }),
            },
            ToolDef {
                name: READ_MCP_RESOURCE_DIR.into(),
                description: "List direct children of a currently advertised MCP directory resource through the io.modelcontextprotocol/skills directoryRead extension. The call requires normal external-tool approval and the listing is not recursive.".into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "server": {"type": "string"},
                        "uri": {"type": "string"}
                    },
                    "required": ["server", "uri"],
                    "additionalProperties": false
                }),
            },
        ];
        McpResourceSource {
            defs: Arc::from(defs),
            servers,
        }
    }

    fn server(&self, requested: &str) -> Result<McpResourceServer> {
        let normalized = normalize_server_name(requested);
        self.servers
            .iter()
            .find(|server| {
                server.name == requested || normalize_server_name(&server.name) == normalized
            })
            .cloned()
            .with_context(|| {
                let available = self
                    .servers
                    .iter()
                    .map(|server| server.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Server \"{requested}\" not found. Available servers: {available}")
            })
    }

    async fn list(&self, requested: Option<&str>) -> Result<SourceOutput> {
        let selected: Vec<McpResourceServer> = match requested {
            Some(name) => vec![self.server(name)?],
            None => self.servers.clone(),
        };
        let explicit_server = requested.is_some();
        let mut tasks = tokio::task::JoinSet::new();
        for (index, server) in selected.into_iter().enumerate() {
            tasks.spawn(async move {
                let name = server.name.clone();
                let result = async {
                    let (_guard, client, capabilities) = server.begin_request().await?;
                    if !capabilities.resources {
                        if explicit_server {
                            bail!("Server \"{name}\" does not advertise MCP resource support.");
                        }
                        return Ok(None);
                    }
                    server.classify(client.list_resources().await).map(Some)
                }
                .await;
                (index, name, result)
            });
        }
        let mut groups = Vec::new();
        let mut failures = Vec::new();
        loop {
            let next = tasks.join_next().await;
            let Some(result) = next else {
                break;
            };
            let (index, server, result) = result.context("MCP resource listing task failed")?;
            match result {
                Ok(Some(resources)) => groups.push((index, server, resources)),
                Ok(None) => {}
                Err(error) => failures.push((index, server, error)),
            }
        }
        groups.sort_by_key(|(index, _, _)| *index);
        failures.sort_by_key(|(index, _, _)| *index);
        if groups.is_empty() && failures.len() == 1 {
            let (_, server, error) = failures.pop().unwrap();
            return Err(error)
                .with_context(|| format!("MCP resource listing failed for server \"{server}\""));
        }
        if groups.is_empty() && !failures.is_empty() {
            let names = failures
                .iter()
                .map(|(_, server, _)| server.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("MCP resource listing failed for every selected server: {names}");
        }

        let mut values = Vec::new();
        let mut total_bytes = 0;
        for (_, server, resources) in groups {
            for resource in &resources {
                let value = resource_value(resource, &server);
                total_bytes += serde_json::to_vec(&value)?.len();
                if values.len() >= MAX_AGGREGATE_RESOURCE_ITEMS
                    || total_bytes > MAX_AGGREGATE_RESOURCE_BYTES
                {
                    bail!("aggregated MCP resource catalog exceeds the tool result budget");
                }
                values.push(value);
            }
        }
        let failed_servers = failures
            .iter()
            .map(|(_, server, _)| server.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let partial_failure = (!failed_servers.is_empty())
            .then(|| format!("\n\nResource listing failed for server(s): {failed_servers}"));
        if values.is_empty() {
            return Ok(SourceOutput {
                text: format!(
                    "No resources found. MCP servers may still provide tools even if they have no resources.{}",
                    partial_failure.as_deref().unwrap_or("")
                ),
                blocks: None,
                structured: Some(Value::Array(Vec::new())),
            });
        }
        Ok(SourceOutput {
            text: format!(
                "{}{}",
                serde_json::to_string(&values)?,
                partial_failure.as_deref().unwrap_or("")
            ),
            blocks: None,
            structured: Some(Value::Array(values)),
        })
    }

    async fn require_advertised_uri(
        &self,
        server: &McpResourceServer,
        client: &McpClient,
        uri: &str,
    ) -> Result<()> {
        let resources = server
            .classify(client.list_resources().await)
            .with_context(|| format!("cannot refresh resources from server \"{}\"", server.name))?;
        if resources.iter().any(|resource| resource.uri == uri) {
            return Ok(());
        }
        bail!(
            "Resource URI is not currently advertised by server \"{}\": {uri}. Re-run list_mcp_resources to refresh.",
            server.name
        )
    }

    async fn read(&self, server_name: &str, uri: &str) -> Result<SourceOutput> {
        let server = self.server(server_name)?;
        let (_guard, client, capabilities) = server.begin_request().await?;
        if !capabilities.resources {
            bail!(
                "Server \"{}\" does not advertise MCP resource support.",
                server.name
            );
        }
        self.require_advertised_uri(&server, &client, uri).await?;
        let read = match server.classify(client.read_resource(uri).await) {
            Ok(read) => read,
            Err(error) => match rpc_code(&error) {
                Some(-32601) => bail!(
                    "Server \"{}\" advertises resource support but does not implement resource reads.",
                    server.name
                ),
                Some(-32002 | -32602) => {
                    let directory_hint = capabilities.directory_read.then_some(
                        " If the URI is a directory resource, use read_mcp_resource_dir instead.",
                    );
                    bail!(
                        "Resource not found: {uri} — it may have been deleted or the URI is stale. Re-run list_mcp_resources to refresh.{}",
                        directory_hint.unwrap_or("")
                    )
                }
                _ => return Err(error),
            },
        };
        let mut rendered = Vec::new();
        let mut images = Vec::new();
        for content in &read.contents {
            if let Some(text) = &content.text {
                rendered.push(json!({
                    "uri": content.uri,
                    "mimeType": content.mime_type,
                    "text": text,
                }));
                continue;
            }
            let mime = content
                .mime_type
                .as_deref()
                .unwrap_or("application/octet-stream");
            if supported_image_mime(mime) {
                if let Some(blob) = &content.blob {
                    images.push(ContentBlock::Image {
                        source: ImageSource::Base64 {
                            media_type: mime.to_string(),
                            data: blob.clone(),
                        },
                    });
                }
                rendered.push(json!({
                    "uri": content.uri,
                    "mimeType": content.mime_type,
                    "text": format!("[binary image resource from {} at {}]", server.name, content.uri),
                }));
            } else {
                rendered.push(json!({
                    "uri": content.uri,
                    "mimeType": content.mime_type,
                    "text": format!("Binary resource from {} at {} was not injected into model context", server.name, content.uri),
                }));
            }
        }
        let value = json!({"contents": rendered});
        let text = serde_json::to_string(&value)?;
        let blocks = if images.is_empty() {
            None
        } else {
            let mut blocks = vec![ContentBlock::Text { text: text.clone() }];
            blocks.extend(images);
            Some(blocks)
        };
        Ok(SourceOutput {
            text,
            blocks,
            structured: Some(read.raw),
        })
    }

    async fn directory(&self, server_name: &str, uri: &str) -> Result<SourceOutput> {
        let server = self.server(server_name)?;
        let (_guard, client, capabilities) = server.begin_request().await?;
        if !capabilities.resources {
            bail!(
                "Server \"{}\" does not advertise MCP resource support.",
                server.name
            );
        }
        if !capabilities.directory_read {
            bail!(
                "Server \"{}\" does not support directory listing.",
                server.name
            );
        }
        self.require_advertised_uri(&server, &client, uri).await?;
        let resources = match server.classify(client.read_resource_directory(uri).await) {
            Ok(resources) => resources,
            Err(error) if rpc_code(&error) == Some(-32602) => {
                bail!(
                    "Not a directory resource: {uri}. If it is a file resource, use read_mcp_resource instead."
                )
            }
            Err(error) => return Err(error),
        };
        let values: Vec<Value> = resources
            .iter()
            .map(|resource| {
                json!({
                    "uri": clean_resource_text(&resource.uri),
                    "name": clean_resource_text(&resource.name),
                    "mimeType": resource.mime_type.as_deref().map(clean_resource_text),
                })
            })
            .collect();
        let value = json!({"resources": values});
        let listing = if resources.is_empty() {
            "Directory is empty.".to_string()
        } else {
            let names = resources
                .iter()
                .map(|resource| {
                    let suffix = if resource.mime_type.as_deref() == Some("inode/directory") {
                        "/"
                    } else {
                        ""
                    };
                    format!("{}{suffix}", clean_resource_text(&resource.name))
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!("Directory listing ({} entries):\n{names}", resources.len())
        };
        Ok(SourceOutput {
            text: format!("{listing}\n\n{}", serde_json::to_string(&value)?),
            blocks: None,
            structured: Some(value),
        })
    }
}

impl ToolSource for McpResourceSource {
    fn defs(&self) -> Arc<[ToolDef]> {
        self.defs.clone()
    }

    fn should_defer(&self, _tool: &str) -> bool {
        true
    }

    fn preflight(&self, tool: &str, input: &Value) -> Result<()> {
        match tool {
            LIST_MCP_RESOURCES => {
                if let Some(server) = input.get("server").and_then(Value::as_str) {
                    return self.server(server)?.preflight(/*directory=*/ false);
                }
                let mut first_unavailable = None;
                for server in &self.servers {
                    match server.availability() {
                        Ok(capabilities) if capabilities.resources => return Ok(()),
                        Ok(_) => {}
                        Err(error) => {
                            first_unavailable.get_or_insert(error);
                        }
                    }
                }
                match first_unavailable {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            }
            READ_MCP_RESOURCE => {
                let server = required_resource_arg(input, "server", tool)?;
                required_resource_arg(input, "uri", tool)?;
                self.server(server)?.preflight(/*directory=*/ false)
            }
            READ_MCP_RESOURCE_DIR => {
                let server = required_resource_arg(input, "server", tool)?;
                required_resource_arg(input, "uri", tool)?;
                self.server(server)?.preflight(/*directory=*/ true)
            }
            _ => bail!("unknown MCP resource tool: {tool}"),
        }
    }

    fn is_readonly(&self, _tool: &str) -> bool {
        true
    }

    fn call<'a>(
        &'a self,
        tool: &'a str,
        input: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
        Box::pin(async move {
            match tool {
                LIST_MCP_RESOURCES => {
                    let server = input.get("server").and_then(Value::as_str);
                    self.list(server).await
                }
                READ_MCP_RESOURCE => {
                    let server = required_resource_arg(input, "server", tool)?;
                    let uri = required_resource_arg(input, "uri", tool)?;
                    self.read(server, uri).await
                }
                READ_MCP_RESOURCE_DIR => {
                    let server = required_resource_arg(input, "server", tool)?;
                    let uri = required_resource_arg(input, "uri", tool)?;
                    self.directory(server, uri).await
                }
                _ => bail!("unknown MCP resource tool: {tool}"),
            }
        })
    }
}

fn required_resource_arg<'a>(input: &'a Value, key: &str, tool: &str) -> Result<&'a str> {
    input[key]
        .as_str()
        .with_context(|| format!("{tool}: missing required string argument '{key}'"))
}

fn normalize_server_name(name: &str) -> String {
    name.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn resource_value(resource: &McpResource, server: &str) -> Value {
    let mut value = resource.raw.clone();
    if let Some(object) = value.as_object_mut() {
        object.insert("server".into(), Value::String(server.to_string()));
    }
    value
}

fn rpc_code(error: &anyhow::Error) -> Option<i64> {
    error
        .downcast_ref::<McpRpcError>()
        .map(|rpc_error| rpc_error.code)
}

fn supported_image_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

fn clean_resource_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    *character,
                    '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}'
                )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use super::super::config::McpServerConfig;
    use super::super::config::McpTransport;
    use super::super::lifecycle::McpFailureKind;
    use super::super::testutil::resource_pair;

    #[tokio::test]
    async fn resource_reads_require_a_currently_advertised_uri() {
        let (server, mut wire) = resource_pair("fixture");
        let source = McpResourceSource::new(vec![server]);
        let server_task = tokio::spawn(async move {
            let request = wire.recv().await;
            assert_eq!(request["method"], "resources/list");
            wire.respond(
                &request["id"],
                json!({
                    "resources": [{
                        "uri": "fixture://note",
                        "name": "note",
                        "mimeType": "text/plain"
                    }]
                }),
            )
            .await;
        });

        let error = match source.read("fixture", "fixture://secret").await {
            Ok(_) => panic!("unadvertised URI unexpectedly reached resources/read"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("not currently advertised"));
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn resource_listing_propagates_single_server_errors() {
        let (server, mut wire) = resource_pair("broken");
        let source = McpResourceSource::new(vec![server]);
        let server_task = tokio::spawn(async move {
            let request = wire.recv().await;
            assert_eq!(request["method"], "resources/list");
            wire.respond_error(&request["id"], -32601, "not implemented")
                .await;
        });

        let error = match source.list(Some("broken")).await {
            Ok(_) => panic!("server failure was reported as an empty catalog"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("resource listing failed for server \"broken\"")
        );
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn aggregate_resource_listing_preserves_partial_failures() {
        let (good, mut good_wire) = resource_pair("good");
        let (bad, mut bad_wire) = resource_pair("bad");
        let source = McpResourceSource::new(vec![good, bad]);
        let good_task = tokio::spawn(async move {
            let request = good_wire.recv().await;
            good_wire
                .respond(
                    &request["id"],
                    json!({
                        "resources": [{
                            "uri": "fixture://note",
                            "name": "note",
                            "mimeType": "text/plain"
                        }]
                    }),
                )
                .await;
        });
        let bad_task = tokio::spawn(async move {
            let request = bad_wire.recv().await;
            bad_wire
                .respond_error(&request["id"], -32000, "temporarily unavailable")
                .await;
        });

        let output = source.list(None).await.unwrap();
        assert!(output.text.contains("\"server\":\"good\""));
        assert!(
            output
                .text
                .contains("Resource listing failed for server(s): bad")
        );
        assert_eq!(
            output.structured,
            Some(Value::Array(vec![json!({
                "uri": "fixture://note",
                "name": "note",
                "mimeType": "text/plain",
                "server": "good"
            })]))
        );
        good_task.await.unwrap();
        bad_task.await.unwrap();
    }

    #[tokio::test]
    async fn configured_failed_resource_server_is_unavailable_not_missing() {
        let server = McpServerConfig {
            name: "offline".into(),
            transport: McpTransport::Stdio {
                command: vec!["SECRET_COMMAND".into()],
                env: BTreeMap::new(),
            },
            readonly_tools: Vec::new(),
        };
        let source = McpToolSource::planned(&server);
        source.starting();
        source.startup_failed(None, McpFailureKind::Startup);
        let resources = McpResourceSource::new(vec![McpResourceServer {
            name: server.name,
            source,
        }]);
        let preflight = resources
            .preflight(LIST_MCP_RESOURCES, &json!({}))
            .unwrap_err();
        assert!(
            preflight.to_string().contains("unavailable, not missing"),
            "{preflight}"
        );
        let (ready_without_resources, _wire) = resource_pair("tool-only");
        {
            let mut state = ready_without_resources.source.state.write().unwrap();
            state.capabilities.resources = false;
            state.receipt.capabilities.resources = Some(false);
        }
        let mixed =
            McpResourceSource::new(vec![ready_without_resources, resources.servers[0].clone()]);
        let mixed_error = mixed.preflight(LIST_MCP_RESOURCES, &json!({})).unwrap_err();
        assert!(
            mixed_error.to_string().contains("unavailable, not missing"),
            "{mixed_error}"
        );

        let error = match resources.list(Some("offline")).await {
            Ok(_) => panic!("failed server was reported as an empty resource catalog"),
            Err(error) => error,
        };
        let text = format!("{error:#}");
        assert!(text.contains("unavailable, not missing"), "{text}");
        assert!(!text.contains("Server \"offline\" not found"), "{text}");
        assert!(!text.contains("SECRET_COMMAND"), "{text}");

        for error in [
            match resources.read("offline", "fixture://item").await {
                Ok(_) => panic!("failed server allowed resource read"),
                Err(error) => error,
            },
            match resources.directory("offline", "fixture://dir").await {
                Ok(_) => panic!("failed server allowed directory read"),
                Err(error) => error,
            },
        ] {
            let text = format!("{error:#}");
            assert!(text.contains("unavailable, not missing"), "{text}");
            assert!(!text.contains("SECRET_COMMAND"), "{text}");
        }
    }
}
