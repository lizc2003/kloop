//! 协议客户端可见的配置、发现 DTO 与前端注入回调。

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;

use kloop_core::Config;
use kloop_core::interaction::Questioner;
use kloop_core::permissions::Approver;
use kloop_core::provider_route::ProviderCatalog;
use kloop_core::session_store::SessionStore;
use kloop_protocol::ActiveProviderRoute;

/// Out-of-band note sink handed to the config factory (rule persistence
/// messages etc.); the server routes these into `note` notifications.
pub type NoteFn = Arc<dyn Fn(&str) + Send + Sync>;

/// Runtime choices fixed when one thread is created. The cwd is always
/// canonicalized by the server before the factory sees it, so every cwd-relative
/// subsystem (permissions, sandbox, project context, skills) can share one anchor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadStartOptions {
    pub cwd: PathBuf,
    pub provider_id: Option<String>,
    pub model: Option<String>,
}

/// Builds one Config per thread. Called with the thread's resolved runtime
/// choices, approver (routes to `approval/request`), and note sink, so cwd-bound
/// policy and approval caches stay per-thread. `provider_id`/`model` are the
/// client's own `thread/start` choice and must be honored or refused; a reopened
/// thread carries neither, so it is built on the configured default and adopts
/// that as its route.
pub type ConfigFactory = Arc<
    dyn Fn(
            ThreadStartOptions,
            Arc<ProviderCatalog>,
            Arc<dyn Approver>,
            Option<Arc<dyn Questioner>>,
            NoteFn,
        ) -> Result<Config>
        + Send
        + Sync,
>;

/// Non-sensitive effective configuration shown to a local protocol client.
/// Provider credentials, MCP headers/env, hook commands, and permission-rule
/// bodies are intentionally absent from this allowlist DTO.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConfigSnapshot {
    pub cwd: String,
    pub route: Option<ActiveProviderRoute>,
    pub permission_mode: String,
    pub context_window: Option<u64>,
    pub defer_threshold: usize,
    pub sandbox: SandboxConfigInfo,
    pub worktree_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SandboxConfigInfo {
    pub enabled: bool,
    pub allow_network: bool,
    pub auto_allow: bool,
    pub escalate: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillScope {
    Project,
    User,
    /// Compiled into the binary (plan 119); no file on disk, so `path` is empty.
    Builtin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillContext {
    Inline,
    Fork,
}

/// Metadata only: a skill body and its allowed-tools list stay private until
/// the normal skill invocation path loads them into a model turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub path: String,
    pub scope: SkillScope,
    pub context: SkillContext,
    pub model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SkillsSnapshot {
    pub cwd: String,
    pub skills: Vec<SkillInfo>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransportKind {
    Stdio,
    Http,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerState {
    Connected,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
}

/// Startup discovery status, not a live health probe. Request handling returns
/// this immutable snapshot and never reconnects or performs network IO.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct McpServerStatus {
    pub name: String,
    pub transport: McpTransportKind,
    pub state: McpServerState,
    pub tools: Vec<McpToolInfo>,
    pub message: Option<String>,
}

pub type ConfigReader = Arc<dyn Fn(&Path) -> Result<ConfigSnapshot> + Send + Sync>;
pub type SkillsReader = Arc<dyn Fn(&Path) -> Result<SkillsSnapshot> + Send + Sync>;

pub struct ServerPaths {
    /// Sessions are partitioned by project, and each thread pins its own cwd,
    /// so the server resolves storage per thread rather than holding one pair
    /// of directories for the process.
    pub store: SessionStore,
}

/// Everything the protocol server needs. The CLI owns filesystem/env/network
/// assembly and injects one immutable provider catalog. Thread configs, catalog
/// reads, and switch validation all consume that same instance.
pub struct ServerConfig {
    pub factory: ConfigFactory,
    pub paths: ServerPaths,
    pub provider_catalog: Arc<ProviderCatalog>,
    pub mcp_servers: Vec<McpServerStatus>,
    pub config_reader: ConfigReader,
    pub skills_reader: SkillsReader,
}

impl ServerConfig {
    pub fn new(factory: ConfigFactory, paths: ServerPaths) -> Self {
        let (provider_catalog, _) = ProviderCatalog::from_provider(
            "mock",
            kloop_provider::Provider::mock(Vec::new()),
            "mock",
            vec!["mock".into()],
        )
        .expect("built-in server provider catalog is valid");
        Self {
            factory,
            paths,
            provider_catalog,
            mcp_servers: Vec::new(),
            config_reader: Arc::new(|cwd| {
                Ok(ConfigSnapshot {
                    cwd: cwd.to_string_lossy().to_string(),
                    route: None,
                    permission_mode: "manual".into(),
                    context_window: None,
                    defer_threshold: kloop_core::tools::TOOL_DEFER_THRESHOLD,
                    sandbox: SandboxConfigInfo {
                        enabled: false,
                        allow_network: false,
                        auto_allow: false,
                        escalate: false,
                    },
                    worktree_enabled: false,
                })
            }),
            skills_reader: Arc::new(|cwd| {
                Ok(SkillsSnapshot {
                    cwd: cwd.to_string_lossy().to_string(),
                    skills: Vec::new(),
                    warnings: Vec::new(),
                })
            }),
        }
    }
}
