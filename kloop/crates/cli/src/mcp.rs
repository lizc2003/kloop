//! MCP glue: `[mcp.servers.<name>]` config parsing, startup connection with
//! degrade-to-warning, `{server}__{tool}` namespacing, and the `ToolSource`
//! adapter over [`kloop_mcp::McpClient`]. This module is the only place that
//! knows both core's tool seam and the MCP wire crate.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde_json::Value;

use kloop_core::tools::SourceOutput;
use kloop_core::tools::ToolSource;
use kloop_mcp::McpClient;
use kloop_protocol::ToolDef;

/// Model-visible tool names must satisfy the providers' `[a-zA-Z0-9_-]`
/// pattern AND kloop's permission-rule grammar (alnum + `_` only, so a
/// "p"-persisted allow rule parses back on the next start). 64 is the
/// stricter (OpenAI-compat) length limit.
const MAX_TOOL_NAME_LEN: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Raw (un-prefixed) tool names the user vouches are read-only: eligible
    /// for concurrent dispatch. Everything else runs serial.
    pub readonly: Vec<String>,
}

/// Parse `[mcp.servers.<name>]` tables from `.kloop/config.toml`. A missing
/// file or missing section is an empty list; a malformed section is an error
/// (silent misconfiguration would look like a vanished server).
pub fn load_mcp_servers(config_path: &Path) -> Result<Vec<McpServerConfig>> {
    let Ok(raw) = std::fs::read_to_string(config_path) else {
        return Ok(Vec::new());
    };
    let value: toml::Table = raw
        .parse()
        .with_context(|| format!("cannot parse {}", config_path.display()))?;
    let Some(servers) = value.get("mcp").and_then(|m| m.get("servers")) else {
        return Ok(Vec::new());
    };
    let servers = servers
        .as_table()
        .context("[mcp.servers] must be a table of server tables")?;
    let mut out = Vec::new();
    for (name, spec) in servers {
        let spec = spec
            .as_table()
            .with_context(|| format!("[mcp.servers.{name}] must be a table"))?;
        let str_list = |key: &str, required: bool| -> Result<Vec<String>> {
            let Some(entries) = spec.get(key) else {
                if required {
                    bail!("[mcp.servers.{name}] is missing '{key}'");
                }
                return Ok(Vec::new());
            };
            entries
                .as_array()
                .and_then(|list| {
                    list.iter()
                        .map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .with_context(|| format!("[mcp.servers.{name}].{key} must be a string array"))
        };
        let command = str_list("command", /*required*/ true)?;
        if command.is_empty() {
            bail!("[mcp.servers.{name}].command must not be empty");
        }
        let readonly = str_list("readonly", /*required*/ false)?;
        let mut env = BTreeMap::new();
        if let Some(env_spec) = spec.get("env") {
            let env_spec = env_spec
                .as_table()
                .with_context(|| format!("[mcp.servers.{name}].env must be a table"))?;
            for (k, v) in env_spec {
                let v = v
                    .as_str()
                    .with_context(|| format!("[mcp.servers.{name}].env.{k} must be a string"))?;
                env.insert(k.clone(), v.to_string());
            }
        }
        for key in spec.keys() {
            if !matches!(key.as_str(), "command" | "env" | "readonly") {
                bail!("[mcp.servers.{name}] has unknown key '{key}' (command | env | readonly)");
            }
        }
        out.push(McpServerConfig {
            name: name.clone(),
            command,
            env,
            readonly,
        });
    }
    Ok(out)
}

/// `{server}__{tool}` with every character outside `[A-Za-z0-9_]` replaced
/// by `_`. `-` is folded too (unlike cc): kloop's permission-rule parser
/// accepts only alnum + `_` tool names, and persisted allow rules must
/// round-trip.
pub fn qualified_name(server: &str, tool: &str) -> String {
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect()
    };
    format!("{}__{}", sanitize(server), sanitize(tool))
}

/// One connected server exposed through core's tool seam. Names in `defs`
/// are qualified; the map recovers the raw wire name per call.
struct McpToolSource {
    client: McpClient,
    defs: Vec<ToolDef>,
    raw_names: HashMap<String, String>,
    readonly: HashSet<String>,
}

impl ToolSource for McpToolSource {
    fn defs(&self) -> &[ToolDef] {
        &self.defs
    }

    fn is_readonly(&self, tool: &str) -> bool {
        self.readonly.contains(tool)
    }

    fn call<'a>(
        &'a self,
        tool: &'a str,
        input: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
        Box::pin(async move {
            let raw = self
                .raw_names
                .get(tool)
                .with_context(|| format!("unknown mcp tool: {tool}"))?;
            // One wire call: the structured CallToolResult for a program, its
            // flattened text for the model-facing tool_result, and — when the
            // result carries a usable image — content blocks so the model sees
            // the picture instead of an `[image: …]` tag.
            let structured = self.client.call_tool_structured(raw, input).await?;
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

/// Namespace a server's advertised tools, dropping (with a warning) any
/// whose qualified name is oversized or collides after sanitization.
fn build_source(
    server: &McpServerConfig,
    client: McpClient,
    advertised: Vec<ToolDef>,
    warn: &dyn Fn(&str),
) -> McpToolSource {
    let readonly_raw: HashSet<&str> = server.readonly.iter().map(String::as_str).collect();
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
    McpToolSource {
        client,
        defs,
        raw_names,
        readonly,
    }
}

/// Spawn + handshake + tool discovery for every configured server. A failing
/// server degrades to a warning and is skipped — MCP never blocks startup.
pub async fn connect_servers(
    servers: Vec<McpServerConfig>,
    warn: &dyn Fn(&str),
) -> Vec<Arc<dyn ToolSource>> {
    let mut sources: Vec<Arc<dyn ToolSource>> = Vec::new();
    for server in servers {
        let connect = async {
            let client = McpClient::spawn(&server.command, &server.env)?;
            client.initialize().await?;
            let advertised = client.list_tools().await?;
            anyhow::Ok((client, advertised))
        };
        match connect.await {
            Ok((client, advertised)) => {
                let count = advertised.len();
                let source = build_source(&server, client, advertised, warn);
                warn(&format!(
                    "mcp server '{}': connected, {count} tool(s)",
                    server.name
                ));
                sources.push(Arc::new(source));
            }
            Err(e) => warn(&format!(
                "mcp server '{}' unavailable, skipped: {e:#}",
                server.name
            )),
        }
    }
    sources
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(tag: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kloop-mcp-cfg-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn load_mcp_servers_full_round_trip() {
        let path = write_config(
            "full",
            r#"
[permissions]
allow = ["bash(ls *)"]

[mcp.servers.memory]
command = ["npx", "-y", "@modelcontextprotocol/server-memory"]
env = { NODE_ENV = "production" }
readonly = ["read_graph", "search_nodes"]

[mcp.servers.fs]
command = ["mcp-fs"]
"#,
        );
        let servers = load_mcp_servers(&path).unwrap();
        assert_eq!(
            servers,
            vec![
                McpServerConfig {
                    name: "fs".into(),
                    command: vec!["mcp-fs".into()],
                    env: BTreeMap::new(),
                    readonly: vec![],
                },
                McpServerConfig {
                    name: "memory".into(),
                    command: vec![
                        "npx".into(),
                        "-y".into(),
                        "@modelcontextprotocol/server-memory".into()
                    ],
                    env: BTreeMap::from([("NODE_ENV".into(), "production".into())]),
                    readonly: vec!["read_graph".into(), "search_nodes".into()],
                },
            ]
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn load_mcp_servers_missing_file_and_section_are_empty() {
        assert_eq!(
            load_mcp_servers(Path::new("/nonexistent/kloop.toml")).unwrap(),
            vec![]
        );
        let path = write_config("nosection", "[permissions]\nallow = []\n");
        assert_eq!(load_mcp_servers(&path).unwrap(), vec![]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn load_mcp_servers_rejects_malformed_sections() {
        for (tag, bad) in [
            ("nocmd", "[mcp.servers.x]\nenv = {}\n"),
            ("emptycmd", "[mcp.servers.x]\ncommand = []\n"),
            ("cmdstr", "[mcp.servers.x]\ncommand = \"npx\"\n"),
            (
                "unknown",
                "[mcp.servers.x]\ncommand = [\"a\"]\ntimeout = 5\n",
            ),
            (
                "badenv",
                "[mcp.servers.x]\ncommand = [\"a\"]\nenv = { K = 1 }\n",
            ),
        ] {
            let path = write_config(tag, bad);
            assert!(load_mcp_servers(&path).is_err(), "{tag} should fail");
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }
    }

    #[test]
    fn qualified_name_sanitizes_to_rule_safe_charset() {
        assert_eq!(qualified_name("memory", "read_graph"), "memory__read_graph");
        // '-' and '.' fold to '_' so persisted allow rules parse back.
        assert_eq!(
            qualified_name("github.com", "create-issue"),
            "github_com__create_issue"
        );
        assert_eq!(qualified_name("a b", "t!"), "a_b__t_");
    }
}
