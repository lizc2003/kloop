//! MCP 配置解析、传输配置脱敏与工具名归一化。

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use kloop_server::McpTransportKind;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    /// Raw (un-prefixed) tool names the user vouches are read-only: eligible
    /// for concurrent dispatch. Everything else runs serial.
    pub readonly_tools: Vec<String>,
}

/// How to reach a server: a local child over stdio, or a remote endpoint over
/// streamable HTTP. `command` vs `url` in the config selects between them
/// (untagged, like codex).
#[derive(Clone, PartialEq, Eq)]
pub enum McpTransport {
    Stdio {
        command: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        /// A static bearer token, written here in full. `~/.kloop/config.toml`
        /// is where kloop's credentials live — the provider's `auth_header`
        /// and `[web].api_key` are already in this same 0600 file — and a
        /// secret reachable only through an exported variable is a working
        /// setup for whoever knows the variable's name and a silent failure
        /// for everyone else. Mutually exclusive with the OAuth path below.
        bearer_token: Option<String>,
        /// Static extra request headers.
        http_headers: BTreeMap<String, String>,
        /// OAuth (plan 34b): a preconfigured client_id to skip dynamic client
        /// registration. Only meaningful on the OAuth path (no static bearer).
        oauth_client_id: Option<String>,
        /// OAuth: scopes to request at login, overriding those advertised by
        /// discovery. Empty = let discovery/the server decide.
        oauth_scopes: Vec<String>,
    },
}

/// Never prints a secret. Three fields here can hold one — the bearer token,
/// a static header's value, and a stdio child's environment — and a transport
/// reaches test failures and any future log through `{:?}`. Keys and names
/// stay: *what* is configured is the diagnostic, what it is set to is not.
impl fmt::Debug for McpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpTransport::Stdio { command, env } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("env", &RedactedValues(env))
                .finish(),
            McpTransport::Http {
                url,
                bearer_token,
                http_headers,
                oauth_client_id,
                oauth_scopes,
            } => f
                .debug_struct("Http")
                .field("url", url)
                .field("bearer_token", &bearer_token.as_ref().map(|_| REDACTED))
                .field("http_headers", &RedactedValues(http_headers))
                .field("oauth_client_id", oauth_client_id)
                .field("oauth_scopes", oauth_scopes)
                .finish(),
        }
    }
}

const REDACTED: &str = "<redacted>";

/// A map printed as its keys, every value replaced.
struct RedactedValues<'a>(&'a BTreeMap<String, String>);

impl fmt::Debug for RedactedValues<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.keys().map(|key| (key, REDACTED)))
            .finish()
    }
}

impl McpTransport {
    pub(super) fn kind(&self) -> McpTransportKind {
        match self {
            McpTransport::Stdio { .. } => McpTransportKind::Stdio,
            McpTransport::Http { .. } => McpTransportKind::Http,
        }
    }
}

const STDIO_ONLY_KEYS: &[&str] = &["command", "env"];
const HTTP_ONLY_KEYS: &[&str] = &[
    "url",
    "bearer_token",
    "http_headers",
    "oauth_client_id",
    "oauth_scopes",
];

/// Parse `[mcp.servers.<name>]` tables from the global user config. A missing
/// section is an empty list; malformed or unknown keys are errors because a
/// silent drop would look like a vanished server.
pub fn load_mcp_servers(root: &toml::Table) -> Result<Vec<McpServerConfig>> {
    let Some(mcp) = root.get("mcp") else {
        return Ok(Vec::new());
    };
    let mcp = mcp.as_table().context("[mcp] must be a table")?;
    for key in mcp.keys() {
        if !matches!(key.as_str(), "servers" | "defer_threshold") {
            bail!("[mcp] has unknown key '{key}' (servers | defer_threshold)");
        }
    }
    let Some(servers) = mcp.get("servers") else {
        return Ok(Vec::new());
    };
    let servers = servers
        .as_table()
        .context("[mcp.servers] must be a table of server tables")?;
    let mut out = Vec::new();
    let mut route_names = HashMap::new();
    for (name, spec) in servers {
        let component = qualified_name_component(name);
        if let Some(previous) = route_names.insert(component.clone(), name.clone()) {
            bail!(
                "[mcp.servers] names '{previous}' and '{name}' both normalize to '{component}'; rename one server so qualified tool routes stay unique"
            );
        }
        let spec = spec
            .as_table()
            .with_context(|| format!("[mcp.servers.{name}] must be a table"))?;
        out.push(parse_server(name, spec)?);
    }
    Ok(out)
}

fn parse_server(name: &str, spec: &toml::Table) -> Result<McpServerConfig> {
    let str_list = |key: &str| -> Result<Vec<String>> {
        let Some(entries) = spec.get(key) else {
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
    let str_table = |key: &str| -> Result<BTreeMap<String, String>> {
        let mut map = BTreeMap::new();
        if let Some(table) = spec.get(key) {
            let table = table
                .as_table()
                .with_context(|| format!("[mcp.servers.{name}].{key} must be a table"))?;
            for (k, v) in table {
                let v = v
                    .as_str()
                    .with_context(|| format!("[mcp.servers.{name}].{key}.{k} must be a string"))?;
                map.insert(k.clone(), v.to_string());
            }
        }
        Ok(map)
    };

    let has_command = spec.contains_key("command");
    let has_url = spec.contains_key("url");
    let (transport, allowed_extra): (McpTransport, &[&str]) = match (has_command, has_url) {
        (true, true) => {
            bail!("[mcp.servers.{name}] sets both 'command' and 'url'; pick one transport")
        }
        (false, false) => {
            bail!("[mcp.servers.{name}] must set 'command' (stdio) or 'url' (remote http)")
        }
        (true, false) => {
            let command = str_list("command")?;
            if command.is_empty() {
                bail!("[mcp.servers.{name}].command must not be empty");
            }
            (
                McpTransport::Stdio {
                    command,
                    env: str_table("env")?,
                },
                HTTP_ONLY_KEYS,
            )
        }
        (false, true) => {
            let url = spec["url"]
                .as_str()
                .with_context(|| format!("[mcp.servers.{name}].url must be a string"))?
                .to_string();
            let bearer_token = match spec.get("bearer_token") {
                Some(v) => {
                    let token = v.as_str().with_context(|| {
                        format!("[mcp.servers.{name}].bearer_token must be a string")
                    })?;
                    if token.is_empty() {
                        bail!("[mcp.servers.{name}].bearer_token must not be empty");
                    }
                    Some(token.to_string())
                }
                None => None,
            };
            let oauth_client_id = match spec.get("oauth_client_id") {
                Some(v) => Some(
                    v.as_str()
                        .with_context(|| {
                            format!("[mcp.servers.{name}].oauth_client_id must be a string")
                        })?
                        .to_string(),
                ),
                None => None,
            };
            let oauth_scopes = str_list("oauth_scopes")?;
            // Static bearer and OAuth are two different auth modes; OAuth keys
            // under a static-bearer server would silently do nothing.
            if bearer_token.is_some() && (oauth_client_id.is_some() || !oauth_scopes.is_empty()) {
                bail!(
                    "[mcp.servers.{name}] mixes bearer_token (static bearer) with \
                     oauth_client_id/oauth_scopes; pick one auth mode"
                );
            }
            (
                McpTransport::Http {
                    url,
                    bearer_token,
                    http_headers: str_table("http_headers")?,
                    oauth_client_id,
                    oauth_scopes,
                },
                STDIO_ONLY_KEYS,
            )
        }
    };

    for key in spec.keys() {
        if allowed_extra.contains(&key.as_str()) {
            bail!("[mcp.servers.{name}] has '{key}', which does not apply to this transport",);
        }
        let known = STDIO_ONLY_KEYS.contains(&key.as_str())
            || HTTP_ONLY_KEYS.contains(&key.as_str())
            || key == "readonly_tools";
        if !known {
            bail!("[mcp.servers.{name}] has unknown key '{key}'");
        }
    }

    Ok(McpServerConfig {
        name: name.to_string(),
        transport,
        readonly_tools: str_list("readonly_tools")?,
    })
}

/// Build the extra HTTP request headers for a remote server: the static
/// `http_headers` plus, if a `bearer_token` was configured, the
/// `Authorization: Bearer <token>` it spells. Nothing here can fail any more —
/// the token is in the config or it is not.
pub(super) fn http_headers_for(
    bearer_token: &Option<String>,
    http_headers: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut headers = http_headers.clone();
    if let Some(token) = bearer_token {
        headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    }
    headers
}

/// `[mcp].defer_threshold`: the total tool count above which MCP tool
/// definitions are deferred behind `tool_search`. Lower it to exercise
/// deferral against a small server; raise it to effectively switch deferral
/// off. Absent, the built-in threshold applies.
pub(crate) fn load_defer_threshold(root: &toml::Table) -> Result<usize> {
    let Some(value) = root
        .get("mcp")
        .and_then(toml::Value::as_table)
        .and_then(|mcp| mcp.get("defer_threshold"))
    else {
        return Ok(kloop_core::tools::TOOL_DEFER_THRESHOLD);
    };
    let count = value
        .as_integer()
        .filter(|&number| number >= 0)
        .context("[mcp].defer_threshold must be a tool count")?;
    Ok(count as usize)
}

/// `{server}__{tool}` with every character outside `[A-Za-z0-9_]` replaced
/// by `_`. `-` is folded too (unlike cc): kloop's permission-rule parser
/// accepts only alnum + `_` tool names, and persisted allow rules must
/// round-trip.
pub(super) fn qualified_name_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect()
}

pub fn qualified_name(server: &str, tool: &str) -> String {
    format!(
        "{}__{}",
        qualified_name_component(server),
        qualified_name_component(tool)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(content: &str) -> toml::Table {
        content.parse().unwrap()
    }

    #[test]
    fn load_mcp_servers_full_round_trip() {
        let root = config(
            r#"
[permissions]
allow = ["bash(ls *)"]

[mcp.servers.memory]
command = ["npx", "-y", "@modelcontextprotocol/server-memory"]
env = { NODE_ENV = "production" }
readonly_tools = ["read_graph", "search_nodes"]

[mcp.servers.fs]
command = ["mcp-fs"]

[mcp.servers.remote]
url = "https://mcp.example.com/mcp"
bearer_token = "sk-example-token"
http_headers = { X-Tenant = "acme" }
readonly_tools = ["search"]

[mcp.servers.oauthy]
url = "https://oauth.example.com/mcp"
oauth_client_id = "preconfigured-123"
oauth_scopes = ["mcp.read", "mcp.write"]
"#,
        );
        let servers = load_mcp_servers(&root).unwrap();
        assert_eq!(
            servers,
            vec![
                McpServerConfig {
                    name: "fs".into(),
                    transport: McpTransport::Stdio {
                        command: vec!["mcp-fs".into()],
                        env: BTreeMap::new(),
                    },
                    readonly_tools: vec![],
                },
                McpServerConfig {
                    name: "memory".into(),
                    transport: McpTransport::Stdio {
                        command: vec![
                            "npx".into(),
                            "-y".into(),
                            "@modelcontextprotocol/server-memory".into()
                        ],
                        env: BTreeMap::from([("NODE_ENV".into(), "production".into())]),
                    },
                    readonly_tools: vec!["read_graph".into(), "search_nodes".into()],
                },
                McpServerConfig {
                    name: "oauthy".into(),
                    transport: McpTransport::Http {
                        url: "https://oauth.example.com/mcp".into(),
                        bearer_token: None,
                        http_headers: BTreeMap::new(),
                        oauth_client_id: Some("preconfigured-123".into()),
                        oauth_scopes: vec!["mcp.read".into(), "mcp.write".into()],
                    },
                    readonly_tools: vec![],
                },
                McpServerConfig {
                    name: "remote".into(),
                    transport: McpTransport::Http {
                        url: "https://mcp.example.com/mcp".into(),
                        bearer_token: Some("sk-example-token".into()),
                        http_headers: BTreeMap::from([("X-Tenant".into(), "acme".into())]),
                        oauth_client_id: None,
                        oauth_scopes: vec![],
                    },
                    readonly_tools: vec!["search".into()],
                },
            ]
        );
    }

    #[test]
    fn load_mcp_servers_missing_section_is_empty() {
        assert_eq!(load_mcp_servers(&toml::Table::new()).unwrap(), vec![]);
        let root = config("[permissions]\nallow = []\n");
        assert_eq!(load_mcp_servers(&root).unwrap(), vec![]);
    }

    #[test]
    fn load_mcp_servers_rejects_malformed_sections() {
        for (tag, bad) in [
            ("mcp-section", "mcp = 3\n"),
            ("mcp-unknown", "[mcp]\nother = true\n"),
            ("notransport", "[mcp.servers.x]\nenv = {}\n"),
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
            // command + url together, and stdio-only/http-only key crossover.
            (
                "both",
                "[mcp.servers.x]\ncommand = [\"a\"]\nurl = \"https://h/mcp\"\n",
            ),
            (
                "envonhttp",
                "[mcp.servers.x]\nurl = \"https://h/mcp\"\nenv = { K = \"v\" }\n",
            ),
            (
                "headersonstdio",
                "[mcp.servers.x]\ncommand = [\"a\"]\nhttp_headers = { X = \"y\" }\n",
            ),
            (
                "normalized-name-collision",
                "[mcp.servers.foo-bar]\ncommand = [\"a\"]\n[mcp.servers.foo_bar]\ncommand = [\"b\"]\n",
            ),
            // A token written but left empty is a typo, not "no auth".
            (
                "emptytoken",
                "[mcp.servers.x]\nurl = \"https://h/mcp\"\nbearer_token = \"\"\n",
            ),
            (
                "tokentype",
                "[mcp.servers.x]\nurl = \"https://h/mcp\"\nbearer_token = 3\n",
            ),
            // The retired env-var spelling is simply unknown now.
            (
                "tokenenvvar",
                "[mcp.servers.x]\nurl = \"https://h/mcp\"\nbearer_token_env_var = \"T\"\n",
            ),
            // Static bearer + OAuth keys is a contradiction.
            (
                "bearerplusoauth",
                "[mcp.servers.x]\nurl = \"https://h/mcp\"\nbearer_token = \"T\"\noauth_client_id = \"c\"\n",
            ),
        ] {
            let root = config(bad);
            assert!(load_mcp_servers(&root).is_err(), "{tag} should fail");
        }
    }

    #[test]
    fn defer_threshold_defaults_and_parses_from_the_mcp_section() {
        assert_eq!(
            load_defer_threshold(&toml::Table::new()).unwrap(),
            kloop_core::tools::TOOL_DEFER_THRESHOLD
        );
        assert_eq!(
            load_defer_threshold(&config("[mcp]\ndefer_threshold = 3\n")).unwrap(),
            3
        );
        // Zero is meaningful: defer everything.
        assert_eq!(
            load_defer_threshold(&config("[mcp]\ndefer_threshold = 0\n")).unwrap(),
            0
        );
        for bad in [
            "[mcp]\ndefer_threshold = -1\n",
            "[mcp]\ndefer_threshold = \"many\"\n",
        ] {
            assert!(load_defer_threshold(&config(bad)).is_err(), "{bad}");
        }
    }

    /// The transport holds the token in plaintext now, so its Debug is what
    /// stands between a failed assertion and a leaked credential.
    #[test]
    fn transport_debug_prints_names_but_never_values() {
        let http = McpTransport::Http {
            url: "https://example.com/mcp".into(),
            bearer_token: Some("sk-SECRET".into()),
            http_headers: BTreeMap::from([("X-Tenant".into(), "TENANT_SECRET".into())]),
            oauth_client_id: Some("client-123".into()),
            oauth_scopes: vec!["mcp.read".into()],
        };
        assert_eq!(
            format!("{http:?}"),
            "Http { url: \"https://example.com/mcp\", bearer_token: Some(\"<redacted>\"), \
             http_headers: {\"X-Tenant\": \"<redacted>\"}, oauth_client_id: Some(\"client-123\"), \
             oauth_scopes: [\"mcp.read\"] }"
        );

        let stdio = McpTransport::Stdio {
            command: vec!["mcp-fs".into()],
            env: BTreeMap::from([("API_KEY".into(), "ENV_SECRET".into())]),
        };
        assert_eq!(
            format!("{stdio:?}"),
            "Stdio { command: [\"mcp-fs\"], env: {\"API_KEY\": \"<redacted>\"} }"
        );
    }

    /// The token is spelled into a header here and nowhere else, so this is
    /// the whole of what the config's `bearer_token` does.
    #[test]
    fn a_configured_bearer_token_becomes_the_authorization_header() {
        assert_eq!(
            http_headers_for(
                &Some("sk-abc".into()),
                &BTreeMap::from([("X-Tenant".into(), "acme".into())]),
            ),
            BTreeMap::from([
                ("Authorization".into(), "Bearer sk-abc".into()),
                ("X-Tenant".into(), "acme".into()),
            ])
        );
        // No token ⇒ just the static headers.
        assert_eq!(
            http_headers_for(&None, &BTreeMap::from([("A".into(), "b".into())])),
            BTreeMap::from([("A".into(), "b".into())])
        );
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
