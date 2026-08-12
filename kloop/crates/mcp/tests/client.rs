//! Wire-contract tests: an in-process mock MCP server on a duplex pipe
//! speaks scripted newline-delimited JSON-RPC, and every assertion is on the
//! exact JSON the client put on (or accepted from) the wire.

use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::DuplexStream;
use tokio::io::ReadHalf;
use tokio::io::WriteHalf;

use kloop_mcp::McpClient;
use kloop_protocol::ToolDef;

/// The server end of the pipe: reads one JSON message per line, writes
/// replies as single lines.
struct ServerEnd {
    lines: tokio::io::Lines<BufReader<ReadHalf<DuplexStream>>>,
    writer: WriteHalf<DuplexStream>,
}

impl ServerEnd {
    fn new(io: DuplexStream) -> Self {
        let (reader, writer) = tokio::io::split(io);
        ServerEnd {
            lines: BufReader::new(reader).lines(),
            writer,
        }
    }

    async fn recv(&mut self) -> Value {
        let line = self
            .lines
            .next_line()
            .await
            .expect("server read failed")
            .expect("client closed unexpectedly");
        serde_json::from_str(&line).expect("client sent invalid JSON")
    }

    async fn send(&mut self, msg: Value) {
        let mut line = msg.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    async fn respond(&mut self, id: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await;
    }
}

fn pair() -> (McpClient, ServerEnd) {
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let (reader, writer) = tokio::io::split(client_io);
    (
        McpClient::over(reader, writer, None),
        ServerEnd::new(server_io),
    )
}

/// The full handshake contract: exact initialize params (protocol version,
/// empty capabilities, clientInfo), then the REQUIRED initialized
/// notification as its own id-less message.
#[tokio::test]
async fn handshake_sends_initialize_then_initialized_notification() {
    let (client, mut server) = pair();
    let server_task = tokio::spawn(async move {
        let init = server.recv().await;
        server
            .respond(
                &init["id"],
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {
                        "tools": {"listChanged": true},
                        "resources": {"listChanged": true},
                        "extensions": {
                            "io.modelcontextprotocol/skills": {"directoryRead": true}
                        }
                    },
                    "serverInfo": {"name": "mock", "version": "0"},
                }),
            )
            .await;
        let initialized = server.recv().await;
        (init, initialized)
    });

    let capabilities = client.initialize().await.unwrap();
    assert_eq!(
        capabilities,
        kloop_mcp::McpServerCapabilities {
            tools_list_changed: true,
            resources: true,
            resources_list_changed: true,
            directory_read: true,
        }
    );

    let (init, initialized) = server_task.await.unwrap();
    assert_eq!(init["method"], "initialize");
    assert_eq!(
        init["params"],
        json!({
            "protocolVersion": kloop_mcp::PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "kloop", "version": env!("CARGO_PKG_VERSION")},
        })
    );
    assert_eq!(
        initialized,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        "initialized must be an id-less notification"
    );
}

/// tools/list follows nextCursor pagination; the inputSchema lands verbatim
/// as ToolDef.schema (whole-object assertion), a missing schema defaults to
/// an empty object schema, and a missing description to "".
#[tokio::test]
async fn list_tools_paginates_and_passes_schema_through() {
    let (client, mut server) = pair();
    let echo_schema = json!({
        "type": "object",
        "properties": {"text": {"type": "string", "description": "what to echo"}},
        "required": ["text"],
        "additionalProperties": false,
    });
    let schema_for_page2 = echo_schema.clone();
    let server_task = tokio::spawn(async move {
        let page1 = server.recv().await;
        server
            .respond(
                &page1["id"],
                json!({
                    "tools": [{"name": "bare"}],
                    "nextCursor": "page-2",
                }),
            )
            .await;
        let page2 = server.recv().await;
        server
            .respond(
                &page2["id"],
                json!({
                    "tools": [{
                        "name": "echo",
                        "description": "Echo the input back.",
                        "inputSchema": schema_for_page2,
                    }],
                }),
            )
            .await;
        (page1, page2)
    });

    let tools = client.list_tools().await.unwrap();

    let (page1, page2) = server_task.await.unwrap();
    assert_eq!(page1["method"], "tools/list");
    assert_eq!(page1["params"], json!({}));
    assert_eq!(
        page2["params"],
        json!({"cursor": "page-2"}),
        "the second page must carry the server's cursor"
    );
    assert_eq!(
        tools,
        vec![
            ToolDef {
                name: "bare".into(),
                description: "".into(),
                schema: json!({"type": "object"}),
            },
            ToolDef {
                name: "echo".into(),
                description: "Echo the input back.".into(),
                schema: echo_schema,
            },
        ]
    );
}

#[tokio::test]
async fn catalog_paginators_reject_repeated_cursors() {
    let (client, mut server) = pair();
    let server_task = tokio::spawn(async move {
        for method in [
            "tools/list",
            "tools/list",
            "resources/list",
            "resources/list",
        ] {
            let request = server.recv().await;
            assert_eq!(request["method"], method);
            let result = if method == "tools/list" {
                json!({"tools": [], "nextCursor": "same-cursor"})
            } else {
                json!({"resources": [], "nextCursor": "same-cursor"})
            };
            server.respond(&request["id"], result).await;
        }
    });

    let error = client.list_tools().await.unwrap_err();
    assert!(error.to_string().contains("repeated nextCursor"));
    let error = client.list_resources().await.unwrap_err();
    assert!(error.to_string().contains("repeated nextCursor"));
    server_task.await.unwrap();
}

#[tokio::test]
async fn resource_reads_reject_invalid_base64_and_oversized_content_lists() {
    let (client, mut server) = pair();
    let server_task = tokio::spawn(async move {
        let invalid = server.recv().await;
        server
            .respond(
                &invalid["id"],
                json!({
                    "contents": [{
                        "uri": "fixture://blob",
                        "mimeType": "application/octet-stream",
                        "blob": "not-base64!"
                    }]
                }),
            )
            .await;

        let oversized = server.recv().await;
        let contents = (0..257)
            .map(|index| {
                json!({
                    "uri": format!("fixture://item/{index}"),
                    "mimeType": "text/plain",
                    "text": "x"
                })
            })
            .collect::<Vec<_>>();
        server
            .respond(&oversized["id"], json!({"contents": contents}))
            .await;
    });

    let error = client.read_resource("fixture://blob").await.unwrap_err();
    assert!(error.to_string().contains("not valid base64"));
    let error = client.read_resource("fixture://many").await.unwrap_err();
    assert!(error.to_string().contains("resource content budget"));
    server_task.await.unwrap();
}

#[tokio::test]
async fn resources_list_read_and_directory_preserve_wire_data() {
    let (client, mut server) = pair();
    let server_task = tokio::spawn(async move {
        let list_page1 = server.recv().await;
        server
            .respond(
                &list_page1["id"],
                json!({
                    "resources": [{
                        "uri": "fixture://root",
                        "name": "root",
                        "description": "root dir",
                        "mimeType": "inode/directory",
                        "size": 54,
                    }],
                    "nextCursor": "resources-2",
                }),
            )
            .await;
        let list_page2 = server.recv().await;
        server
            .respond(
                &list_page2["id"],
                json!({
                    "resources": [{
                        "uri": "fixture://note",
                        "name": "note",
                        "mimeType": "text/plain",
                        "_meta": {"fixture": true},
                    }]
                }),
            )
            .await;
        let read = server.recv().await;
        server
            .respond(
                &read["id"],
                json!({
                    "contents": [{
                        "uri": "fixture://note",
                        "mimeType": "text/plain",
                        "text": "resource body",
                    }]
                }),
            )
            .await;
        let directory_page1 = server.recv().await;
        server
            .respond(
                &directory_page1["id"],
                json!({
                    "resources": [{
                        "uri": "fixture://root/nested",
                        "name": "nested",
                        "mimeType": "inode/directory",
                    }],
                    "nextCursor": "directory-2",
                }),
            )
            .await;
        let directory_page2 = server.recv().await;
        server
            .respond(
                &directory_page2["id"],
                json!({
                    "resources": [{
                        "uri": "fixture://note",
                        "name": "note",
                        "mimeType": "text/plain",
                    }]
                }),
            )
            .await;
        (
            list_page1,
            list_page2,
            read,
            directory_page1,
            directory_page2,
        )
    });

    let resources = client.list_resources().await.unwrap();
    assert_eq!(resources.len(), 2);
    assert_eq!(resources[0].uri, "fixture://root");
    assert_eq!(resources[0].name, "root");
    assert_eq!(resources[0].description.as_deref(), Some("root dir"));
    assert_eq!(resources[0].mime_type.as_deref(), Some("inode/directory"));
    assert_eq!(resources[0].raw["size"], 54);
    assert_eq!(resources[1].raw["_meta"], json!({"fixture": true}));

    let read = client.read_resource("fixture://note").await.unwrap();
    assert_eq!(read.contents.len(), 1);
    assert_eq!(read.contents[0].uri, "fixture://note");
    assert_eq!(read.contents[0].text.as_deref(), Some("resource body"));
    assert_eq!(read.contents[0].blob, None);

    let children = client
        .read_resource_directory("fixture://root")
        .await
        .unwrap();
    assert_eq!(
        children
            .iter()
            .map(|resource| resource.uri.as_str())
            .collect::<Vec<_>>(),
        vec!["fixture://root/nested", "fixture://note"]
    );

    let (list_page1, list_page2, read, directory_page1, directory_page2) =
        server_task.await.unwrap();
    assert_eq!(list_page1["method"], "resources/list");
    assert_eq!(list_page1["params"], json!({}));
    assert_eq!(list_page2["params"], json!({"cursor": "resources-2"}));
    assert_eq!(read["method"], "resources/read");
    assert_eq!(read["params"], json!({"uri": "fixture://note"}));
    assert_eq!(directory_page1["method"], "resources/directory/read");
    assert_eq!(directory_page1["params"], json!({"uri": "fixture://root"}));
    assert_eq!(
        directory_page2["params"],
        json!({"uri": "fixture://root", "cursor": "directory-2"})
    );
}

#[tokio::test]
async fn list_changed_notifications_are_published_without_stealing_responses() {
    let (client, mut server) = pair();
    let mut notifications = client.subscribe_notifications().unwrap();
    tokio::spawn(async move {
        let list = server.recv().await;
        server
            .send(json!({
                "jsonrpc": "2.0",
                "method": "notifications/tools/list_changed",
            }))
            .await;
        server
            .send(json!({
                "jsonrpc": "2.0",
                "method": "notifications/resources/list_changed",
            }))
            .await;
        server
            .send(json!({
                "jsonrpc": "2.0",
                "method": "notifications/resources/updated",
                "params": {"uri": "fixture://note"},
            }))
            .await;
        server.respond(&list["id"], json!({"tools": []})).await;
    });

    assert!(client.list_tools().await.unwrap().is_empty());
    assert_eq!(
        notifications.recv().await.unwrap(),
        kloop_mcp::McpNotification::ToolsListChanged
    );
    assert_eq!(
        notifications.recv().await.unwrap(),
        kloop_mcp::McpNotification::ResourcesListChanged
    );
    assert_eq!(
        notifications.recv().await.unwrap(),
        kloop_mcp::McpNotification::ResourceUpdated {
            uri: "fixture://note".into()
        }
    );
}

/// tools/call sends the raw name + arguments verbatim; a multi-block content
/// array renders as joined text, with resource/image blocks degraded to
/// tagged text.
#[tokio::test]
async fn call_tool_round_trip_renders_content_blocks() {
    let (client, mut server) = pair();
    let server_task = tokio::spawn(async move {
        let call = server.recv().await;
        server
            .respond(
                &call["id"],
                json!({
                    "content": [
                        {"type": "text", "text": "first"},
                        {"type": "image", "data": "aGk=", "mimeType": "image/png"},
                        {"type": "resource", "resource": {"uri": "file:///a.txt", "text": "file body"}},
                        {"type": "resource_link", "uri": "https://example.com/doc"},
                    ],
                    "isError": false,
                }),
            )
            .await;
        call
    });

    let out = client
        .call_tool("echo", &json!({"text": "hi", "n": 2}))
        .await
        .unwrap();

    let call = server_task.await.unwrap();
    assert_eq!(call["method"], "tools/call");
    assert_eq!(
        call["params"],
        json!({"name": "echo", "arguments": {"text": "hi", "n": 2}})
    );
    assert_eq!(
        out,
        "first\n[image: image/png]\n[resource file:///a.txt]\nfile body\n[resource link: https://example.com/doc]"
    );
}

/// content_blocks lifts an MCP result that carries a usable image into
/// canonical blocks (so the model SEES it), while a text-only result stays on
/// the flattened-text path (None). Non-image items around the image fold into
/// Text blocks, preserving order.
#[test]
fn content_blocks_lifts_images_and_leaves_text_alone() {
    use kloop_protocol::ContentBlock;
    use kloop_protocol::ImageSource;

    // Text-only → None (the text path stays byte-identical).
    assert_eq!(
        kloop_mcp::content_blocks(&json!([{"type": "text", "text": "just text"}])),
        None
    );
    // An unsupported image mime is not lifted (a bad block would fail the whole
    // request) — it stays on the text path.
    assert_eq!(
        kloop_mcp::content_blocks(&json!([
            {"type": "image", "data": "aGk=", "mimeType": "image/svg+xml"}
        ])),
        None
    );
    assert_eq!(
        kloop_mcp::content_blocks(&json!([
            {"type": "image", "data": "not-base64!", "mimeType": "image/png"}
        ])),
        None
    );
    // A usable image is lifted; surrounding text/resource items fold into Text
    // blocks around it, in order.
    assert_eq!(
        kloop_mcp::content_blocks(&json!([
            {"type": "text", "text": "before"},
            {"type": "image", "data": "aGk=", "mimeType": "image/png"},
            {"type": "resource_link", "uri": "https://example.com/doc"},
        ])),
        Some(vec![
            ContentBlock::Text {
                text: "before".into()
            },
            ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "aGk=".into(),
                },
            },
            ContentBlock::Text {
                text: "[resource link: https://example.com/doc]".into()
            },
        ])
    );
}

/// isError: true is a successful JSON-RPC response that marks a FAILED tool
/// run — it must surface as Err with the content as the message.
#[tokio::test]
async fn call_tool_is_error_result_maps_to_err() {
    let (client, mut server) = pair();
    tokio::spawn(async move {
        let call = server.recv().await;
        server
            .respond(
                &call["id"],
                json!({
                    "content": [{"type": "text", "text": "no such entity"}],
                    "isError": true,
                }),
            )
            .await;
    });

    let err = client.call_tool("lookup", &json!({})).await.unwrap_err();
    assert_eq!(err.to_string(), "no such entity");
}

#[tokio::test]
async fn call_tool_rejects_invalid_image_payloads() {
    let (client, mut server) = pair();
    let server_task = tokio::spawn(async move {
        let invalid = server.recv().await;
        server
            .respond(
                &invalid["id"],
                json!({
                    "content": [{
                        "type": "image",
                        "mimeType": "image/png",
                        "data": "not-base64!"
                    }]
                }),
            )
            .await;
        let missing = server.recv().await;
        server
            .respond(
                &missing["id"],
                json!({
                    "content": [{"type": "image", "mimeType": "image/png"}]
                }),
            )
            .await;
        let oversized = server.recv().await;
        let content = (0..257)
            .map(|index| json!({"type": "text", "text": format!("item-{index}")}))
            .collect::<Vec<_>>();
        server
            .respond(&oversized["id"], json!({"content": content}))
            .await;
    });

    let error = client.call_tool("image", &json!({})).await.unwrap_err();
    assert!(error.to_string().contains("not valid base64"));
    let error = client.call_tool("image", &json!({})).await.unwrap_err();
    assert!(error.to_string().contains("has no base64 data"));
    let error = client.call_tool("many", &json!({})).await.unwrap_err();
    assert!(error.to_string().contains("content item budget"));
    server_task.await.unwrap();
}

/// A JSON-RPC error object (protocol-level failure) maps to Err carrying
/// code and message.
#[tokio::test]
async fn rpc_error_maps_to_err_with_code_and_message() {
    let (client, mut server) = pair();
    tokio::spawn(async move {
        let call = server.recv().await;
        server
            .send(json!({
                "jsonrpc": "2.0",
                "id": call["id"],
                "error": {"code": -32602, "message": "invalid params"},
            }))
            .await;
    });

    let err = client.call_tool("echo", &json!({})).await.unwrap_err();
    assert_eq!(err.to_string(), "mcp error -32602: invalid params");
}

/// EOF with a request in flight resolves that request with a definite error
/// instead of hanging until the timeout.
#[tokio::test]
async fn server_closing_fails_pending_requests() {
    let (client, mut server) = pair();
    tokio::spawn(async move {
        let _ = server.recv().await; // read the request, then hang up
        drop(server);
    });

    let err = client.call_tool("echo", &json!({})).await.unwrap_err();
    assert!(
        err.to_string().contains("closed the connection"),
        "got: {err}"
    );
}

/// Server-initiated requests are refused with -32601 (we advertise no
/// capabilities); notifications and stray non-JSON stderr-ish lines must not
/// derail the response routing for the request in flight.
#[tokio::test]
async fn server_requests_are_refused_and_noise_is_skipped() {
    let (client, mut server) = pair();
    let server_task = tokio::spawn(async move {
        let call = server.recv().await;
        // Interleave noise before answering: a notification, garbage, and a
        // server->client request that demands a -32601 reply.
        server
            .send(json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}}))
            .await;
        server.writer.write_all(b"not json at all\n").await.unwrap();
        server
            .send(json!({"jsonrpc": "2.0", "id": "srv-1", "method": "roots/list"}))
            .await;
        let refusal = server.recv().await;
        server
            .respond(
                &call["id"],
                json!({"content": [{"type": "text", "text": "ok"}]}),
            )
            .await;
        refusal
    });

    let out = client.call_tool("echo", &json!({})).await.unwrap();
    assert_eq!(out, "ok");
    let refusal = server_task.await.unwrap();
    assert_eq!(refusal["id"], "srv-1");
    assert_eq!(refusal["error"]["code"], -32601);
}

/// Concurrent calls multiplex on one connection: out-of-order responses
/// land on the right callers by id.
#[tokio::test]
async fn concurrent_calls_route_responses_by_id() {
    let (client, mut server) = pair();
    tokio::spawn(async move {
        let first = server.recv().await;
        let second = server.recv().await;
        // Answer in reverse order.
        server
            .respond(
                &second["id"],
                json!({"content": [{"type": "text", "text": second["params"]["name"]}]}),
            )
            .await;
        server
            .respond(
                &first["id"],
                json!({"content": [{"type": "text", "text": first["params"]["name"]}]}),
            )
            .await;
    });

    let no_args = json!({});
    let (a, b) = tokio::join!(
        client.call_tool("alpha", &no_args),
        client.call_tool("beta", &no_args),
    );
    assert_eq!(a.unwrap(), "alpha");
    assert_eq!(b.unwrap(), "beta");
}
