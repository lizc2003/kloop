//! Wire-contract tests: an in-process mock MCP server on a duplex pipe
//! speaks scripted newline-delimited JSON-RPC, and every assertion is on the
//! exact JSON the client put on (or accepted from) the wire.

use serde_json::json;
use serde_json::Value;
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
                    "capabilities": {},
                    "serverInfo": {"name": "mock", "version": "0"},
                }),
            )
            .await;
        let initialized = server.recv().await;
        (init, initialized)
    });

    client.initialize().await.unwrap();

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
