//! MCP 工具、资源与生命周期测试共用的双工服务端夹具。

use std::collections::BTreeMap;
use std::sync::Arc;

use kloop_mcp::McpClient;
use kloop_mcp::McpServerCapabilities;
use serde_json::Value;
use serde_json::json;

use super::config::McpServerConfig;
use super::config::McpTransport;
use super::resources::McpResourceServer;
use super::tools::McpToolSource;

pub(super) struct ResourceServerEnd {
    lines: tokio::io::Lines<tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
}

impl ResourceServerEnd {
    pub(super) async fn recv(&mut self) -> Value {
        let line = self
            .lines
            .next_line()
            .await
            .unwrap()
            .expect("client closed unexpectedly");
        serde_json::from_str(&line).unwrap()
    }

    async fn send(&mut self, message: Value) {
        use tokio::io::AsyncWriteExt;
        let mut line = message.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    pub(super) async fn respond(&mut self, id: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await;
    }

    pub(super) async fn respond_error(&mut self, id: &Value, code: i64, message: &str) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message}
        }))
        .await;
    }
}

pub(super) fn resource_pair(name: &str) -> (McpResourceServer, ResourceServerEnd) {
    use tokio::io::AsyncBufReadExt;
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let (reader, writer) = tokio::io::split(client_io);
    let (server_reader, server_writer) = tokio::io::split(server_io);
    let client = Arc::new(McpClient::over(reader, writer, None));
    let server = McpServerConfig {
        name: name.into(),
        transport: McpTransport::Stdio {
            command: vec!["fixture".into()],
            env: BTreeMap::new(),
        },
        readonly_tools: Vec::new(),
    };
    let source = McpToolSource::planned(&server);
    source.starting();
    source.ready(
        client,
        None,
        McpServerCapabilities {
            resources: true,
            ..Default::default()
        },
        Vec::new(),
        /*has_notifications=*/ false,
        &|_| {},
    );
    (
        McpResourceServer {
            name: name.into(),
            source,
        },
        ResourceServerEnd {
            lines: tokio::io::BufReader::new(server_reader).lines(),
            writer: server_writer,
        },
    )
}
