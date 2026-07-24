//! Process-global provider configuration from `~/.kloop/config.toml`.
//!
//! The schema intentionally follows the portable core of Codex's config
//! (`model`, `model_provider`, `model_reasoning_effort`, and named
//! `model_providers`). Project `.kloop/config.toml` remains cwd-scoped policy;
//! credentials never flow through its permission-rule persistence path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use toml::Value;
use url::Url;

use kloop_provider::Provider;
use kloop_provider::ThinkingMode;
use kloop_server::ModelInfo;

pub(crate) const GLOBAL_CONFIG: &str = ".kloop/config.toml";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rail {
    Mock,
    Anthropic,
    OpenAiChat,
    OpenAiResponses,
}

/// Fully resolved process-wide provider state. Deliberately no Debug/Serialize:
/// both would make accidental credential projection too easy.
pub(crate) struct ResolvedProviderSettings {
    rail: Rail,
    key: String,
    base: String,
    model: String,
    cache: bool,
    thinking: ThinkingMode,
    effort: Option<String>,
}

impl ResolvedProviderSettings {
    pub(crate) fn mock() -> Self {
        Self {
            rail: Rail::Mock,
            key: String::new(),
            base: String::new(),
            model: "mock".into(),
            cache: false,
            thinking: ThinkingMode::Unset,
            effort: None,
        }
    }

    pub(crate) fn provider(&self) -> Provider {
        match self.rail {
            Rail::Mock => Provider::mock(Vec::new()),
            Rail::Anthropic => Provider::Anthropic {
                key: self.key.clone(),
                base: self.base.clone(),
                cache: self.cache,
                thinking: self.thinking,
            },
            Rail::OpenAiChat => Provider::OpenAiCompat {
                key: self.key.clone(),
                base: self.base.clone(),
            },
            Rail::OpenAiResponses => Provider::OpenAiResponses {
                key: self.key.clone(),
                base: self.base.clone(),
                effort: self.effort.clone(),
            },
        }
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn model_info(&self) -> ModelInfo {
        let provider = match self.rail {
            Rail::Mock => "mock",
            Rail::Anthropic => "anthropic",
            Rail::OpenAiChat => "openai",
            Rail::OpenAiResponses => "openaiResponses",
        };
        ModelInfo {
            id: self.model.clone(),
            display_name: self.model.clone(),
            provider: provider.into(),
            is_default: true,
        }
    }
}

struct Profile {
    wire: Rail,
    base_url: Option<String>,
    headers: BTreeMap<String, String>,
    model: Option<String>,
    cache: Option<bool>,
    thinking: Option<ThinkingMode>,
    effort: Option<String>,
}

struct GlobalFile {
    model: Option<String>,
    model_provider: Option<String>,
    effort: Option<String>,
    profiles: BTreeMap<String, Profile>,
}

pub(crate) fn global_config_path() -> Result<PathBuf> {
    let home =
        std::env::home_dir().context("cannot determine home directory for ~/.kloop/config.toml")?;
    Ok(home.join(GLOBAL_CONFIG))
}

/// Read and resolve the process-global provider once. `--mock` calls this with
/// `mock=true` and remains hermetic: no HOME, file, or provider env access.
pub(crate) fn load(mock: bool) -> Result<ResolvedProviderSettings> {
    if mock {
        return Ok(ResolvedProviderSettings::mock());
    }
    let path = global_config_path()?;
    let raw = read_global_file(&path)?;
    resolve(raw.as_deref(), &|name| std::env::var(name).ok())
}

fn read_global_file(path: &Path) -> Result<Option<String>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("cannot inspect ~/.kloop/config.toml"),
    };
    if metadata.file_type().is_symlink() {
        bail!("~/.kloop/config.toml must be a regular file, not a symlink");
    }
    if !metadata.is_file() {
        bail!("~/.kloop/config.toml must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("~/.kloop/config.toml contains credentials; run: chmod 600 ~/.kloop/config.toml");
        }
    }
    std::fs::read_to_string(path)
        .map(Some)
        .map_err(|_| anyhow!("cannot read ~/.kloop/config.toml"))
}

fn resolve(
    raw: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<ResolvedProviderSettings> {
    let file = match raw {
        Some(raw) => parse_global_file(raw)?,
        None => GlobalFile {
            model: None,
            model_provider: None,
            effort: None,
            profiles: BTreeMap::new(),
        },
    };

    let env_provider = nonempty_env(env, "KLOOP_PROVIDER")?;
    let selected = env_provider.as_deref().or(file.model_provider.as_deref());
    let (rail, profile) = match selected {
        Some(name) => match file.profiles.get(name) {
            Some(profile) => (profile.wire, Some(profile)),
            None => (
                infer_builtin_rail(name).with_context(|| {
                    format!("provider profile '{name}' is not defined in ~/.kloop/config.toml")
                })?,
                None,
            ),
        },
        None if env("ANTHROPIC_API_KEY").is_some() => {
            (Rail::Anthropic, file.profiles.get("anthropic"))
        }
        None if env("OPENAI_API_KEY").is_some() => (Rail::OpenAiChat, file.profiles.get("openai")),
        None => {
            bail!(
                "no provider configured: set model_provider in ~/.kloop/config.toml, \
                 set KLOOP_PROVIDER with provider credentials, or run with --mock"
            )
        }
    };

    let specific_model = match rail {
        Rail::Anthropic => nonempty_env(env, "ANTHROPIC_MODEL")?,
        Rail::OpenAiChat | Rail::OpenAiResponses => nonempty_env(env, "OPENAI_MODEL")?,
        Rail::Mock => None,
    };
    let model = specific_model
        .or(nonempty_env(env, "KLOOP_MODEL")?)
        .or_else(|| profile.and_then(|profile| profile.model.clone()))
        .or_else(|| file.model.clone())
        .or_else(|| (rail == Rail::Anthropic).then(|| "claude-sonnet-5".into()))
        .context(
            "openai providers need model in ~/.kloop/config.toml or OPENAI_MODEL/KLOOP_MODEL",
        )?;

    match rail {
        Rail::Anthropic => {
            let key = nonempty_env(env, "ANTHROPIC_API_KEY")?
                .or_else(|| profile.and_then(|profile| profile.headers.get("x-api-key").cloned()))
                .context("anthropic credentials missing: set x-api-key in ~/.kloop/config.toml or ANTHROPIC_API_KEY")?;
            let base = resolve_base(
                nonempty_env(env, "ANTHROPIC_BASE_URL")?,
                profile.and_then(|profile| profile.base_url.clone()),
                "https://api.anthropic.com",
                "model_providers.<selected>.base_url",
            )?;
            let cache = match nonempty_env(env, "KLOOP_CACHE")? {
                Some(raw) => parse_bool_switch(&raw, "KLOOP_CACHE")?,
                None => profile.and_then(|profile| profile.cache).unwrap_or(true),
            };
            let thinking = match nonempty_env(env, "KLOOP_THINKING")? {
                Some(raw) => parse_thinking_string(&raw, "KLOOP_THINKING")?,
                None => profile
                    .and_then(|profile| profile.thinking)
                    .unwrap_or(ThinkingMode::Unset),
            };
            Ok(ResolvedProviderSettings {
                rail,
                key,
                base,
                model,
                cache,
                thinking,
                effort: None,
            })
        }
        Rail::OpenAiChat | Rail::OpenAiResponses => {
            let key = nonempty_env(env, "OPENAI_API_KEY")?
                .or_else(|| profile.and_then(|profile| bearer_key(&profile.headers)))
                .context("openai credentials missing: set Bearer Authorization in ~/.kloop/config.toml or OPENAI_API_KEY")?;
            let base = resolve_base(
                nonempty_env(env, "OPENAI_BASE_URL")?,
                profile.and_then(|profile| profile.base_url.clone()),
                "https://api.openai.com/v1",
                "model_providers.<selected>.base_url",
            )?;
            let effort = if rail == Rail::OpenAiResponses {
                nonempty_env(env, "KLOOP_EFFORT")?
                    .or_else(|| profile.and_then(|profile| profile.effort.clone()))
                    .or_else(|| file.effort.clone())
            } else {
                None
            };
            Ok(ResolvedProviderSettings {
                rail,
                key,
                base,
                model,
                cache: false,
                thinking: ThinkingMode::Unset,
                effort,
            })
        }
        Rail::Mock => unreachable!("mock returns before resolver"),
    }
}

fn parse_global_file(raw: &str) -> Result<GlobalFile> {
    let table: toml::Table = raw
        .parse()
        .map_err(|_| anyhow!("cannot parse ~/.kloop/config.toml (TOML syntax error)"))?;
    for key in table.keys() {
        if !matches!(
            key.as_str(),
            "model" | "model_provider" | "model_reasoning_effort" | "model_providers"
        ) {
            bail!("~/.kloop/config.toml has unknown top-level key '{key}'");
        }
    }
    let model = optional_string(&table, "model", "model")?;
    let model_provider = optional_string(&table, "model_provider", "model_provider")?;
    let effort = optional_string(&table, "model_reasoning_effort", "model_reasoning_effort")?;
    let mut profiles = BTreeMap::new();
    if let Some(value) = table.get("model_providers") {
        let providers = value
            .as_table()
            .context("model_providers must be a table")?;
        for (name, value) in providers {
            let spec = value
                .as_table()
                .with_context(|| format!("model_providers.{name} must be a table"))?;
            profiles.insert(name.clone(), parse_profile(name, spec)?);
        }
    }
    if let Some(selected) = &model_provider {
        if !profiles.contains_key(selected) && infer_builtin_rail(selected).is_none() {
            bail!("model_provider '{selected}' has no matching model_providers profile");
        }
    }
    Ok(GlobalFile {
        model,
        model_provider,
        effort,
        profiles,
    })
}

fn parse_profile(name: &str, spec: &toml::Table) -> Result<Profile> {
    for key in spec.keys() {
        if !matches!(
            key.as_str(),
            "name"
                | "wire_api"
                | "base_url"
                | "http_headers"
                | "model"
                | "cache"
                | "thinking"
                | "effort"
        ) {
            bail!("model_providers.{name} has unknown key '{key}'");
        }
    }
    let _display_name = optional_string(spec, "name", &format!("model_providers.{name}.name"))?;
    let wire = match optional_string(
        spec,
        "wire_api",
        &format!("model_providers.{name}.wire_api"),
    )? {
        Some(raw) => parse_wire(&raw, name)?,
        None => infer_builtin_rail(name).with_context(|| {
            format!("model_providers.{name}.wire_api is required for a custom provider")
        })?,
    };
    let base_url = optional_string(
        spec,
        "base_url",
        &format!("model_providers.{name}.base_url"),
    )?
    .map(|base| validate_base_url(&base, &format!("model_providers.{name}.base_url")))
    .transpose()?;
    let model = optional_string(spec, "model", &format!("model_providers.{name}.model"))?;
    let cache = optional_bool(spec, "cache", &format!("model_providers.{name}.cache"))?;
    let effort = optional_string(spec, "effort", &format!("model_providers.{name}.effort"))?;
    let thinking = match spec.get("thinking") {
        None => None,
        Some(Value::String(raw)) => Some(parse_thinking_string(
            raw,
            &format!("model_providers.{name}.thinking"),
        )?),
        Some(Value::Integer(raw)) if *raw > 0 => Some(ThinkingMode::Budget(*raw as u64)),
        Some(_) => bail!(
            "model_providers.{name}.thinking must be 'off', 'adaptive', or a positive integer"
        ),
    };
    if wire != Rail::Anthropic && (cache.is_some() || thinking.is_some()) {
        bail!("model_providers.{name}: cache/thinking are only valid for anthropic wire_api");
    }
    if wire != Rail::OpenAiResponses && effort.is_some() {
        bail!("model_providers.{name}.effort is only valid for responses wire_api");
    }
    let headers = parse_headers(name, spec.get("http_headers"), wire)?;
    Ok(Profile {
        wire,
        base_url,
        headers,
        model,
        cache,
        thinking,
        effort,
    })
}

fn parse_headers(
    name: &str,
    value: Option<&Value>,
    wire: Rail,
) -> Result<BTreeMap<String, String>> {
    let mut headers = BTreeMap::new();
    let Some(value) = value else {
        return Ok(headers);
    };
    let table = value
        .as_table()
        .with_context(|| format!("model_providers.{name}.http_headers must be a table"))?;
    for (header, value) in table {
        let folded = header.to_ascii_lowercase();
        let allowed = match wire {
            Rail::Anthropic => folded == "x-api-key",
            Rail::OpenAiChat | Rail::OpenAiResponses => folded == "authorization",
            Rail::Mock => false,
        };
        if !allowed {
            bail!("model_providers.{name}.http_headers contains unsupported header '{header}'");
        }
        let value = value.as_str().with_context(|| {
            format!("model_providers.{name}.http_headers.{header} must be a string")
        })?;
        let value = nonempty(
            value,
            &format!("model_providers.{name}.http_headers.{header}"),
        )?;
        if headers.insert(folded, value).is_some() {
            bail!("model_providers.{name}.http_headers contains a duplicate header");
        }
    }
    if wire != Rail::Anthropic {
        if let Some(value) = headers.get("authorization") {
            if bearer_from_header(value).is_none() {
                bail!("model_providers.{name}.http_headers.Authorization must use Bearer authentication");
            }
        }
    }
    Ok(headers)
}

fn parse_wire(raw: &str, name: &str) -> Result<Rail> {
    match raw {
        "anthropic" => Ok(Rail::Anthropic),
        "chat" => Ok(Rail::OpenAiChat),
        "responses" => Ok(Rail::OpenAiResponses),
        _ => bail!("model_providers.{name}.wire_api must be anthropic | chat | responses"),
    }
}

fn infer_builtin_rail(name: &str) -> Option<Rail> {
    match name {
        "anthropic" => Some(Rail::Anthropic),
        "openai" | "openai-compat" => Some(Rail::OpenAiChat),
        "openai-responses" => Some(Rail::OpenAiResponses),
        _ => None,
    }
}

fn resolve_base(
    env_base: Option<String>,
    profile_base: Option<String>,
    default: &str,
    field: &str,
) -> Result<String> {
    validate_base_url(
        env_base.or(profile_base).as_deref().unwrap_or(default),
        field,
    )
}

fn validate_base_url(raw: &str, field: &str) -> Result<String> {
    let parsed = Url::parse(raw).map_err(|_| anyhow!("{field} must be an absolute http(s) URL"))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        bail!(
            "{field} must be an absolute http(s) base URL without credentials, query, or fragment"
        );
    }
    let path = parsed.path().trim_end_matches('/');
    if path.ends_with("/responses")
        || path.ends_with("/chat/completions")
        || path.ends_with("/v1/messages")
    {
        bail!("{field} must be a base URL; kloop appends the provider endpoint");
    }
    Ok(raw.trim_end_matches('/').to_string())
}

fn optional_string(table: &toml::Table, key: &str, field: &str) -> Result<Option<String>> {
    table
        .get(key)
        .map(|value| {
            value
                .as_str()
                .with_context(|| format!("{field} must be a string"))
                .and_then(|value| nonempty(value, field))
        })
        .transpose()
}

fn optional_bool(table: &toml::Table, key: &str, field: &str) -> Result<Option<bool>> {
    table
        .get(key)
        .map(|value| {
            value
                .as_bool()
                .with_context(|| format!("{field} must be a boolean"))
        })
        .transpose()
}

fn nonempty(raw: &str, field: &str) -> Result<String> {
    let value = raw.trim();
    if value.is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(value.to_string())
}

fn nonempty_env(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Result<Option<String>> {
    env(name).map(|value| nonempty(&value, name)).transpose()
}

fn parse_bool_switch(raw: &str, field: &str) -> Result<bool> {
    match raw {
        "off" | "0" | "false" => Ok(false),
        "on" | "1" | "true" => Ok(true),
        _ => bail!("{field} must be on | off | true | false | 1 | 0"),
    }
}

fn parse_thinking_string(raw: &str, field: &str) -> Result<ThinkingMode> {
    match raw {
        "off" => Ok(ThinkingMode::Off),
        "adaptive" => Ok(ThinkingMode::Adaptive),
        value => value
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .map(ThinkingMode::Budget)
            .with_context(|| format!("{field} must be off | adaptive | a positive token budget")),
    }
}

fn bearer_key(headers: &BTreeMap<String, String>) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|value| bearer_from_header(value))
}

fn bearer_from_header(value: &str) -> Option<String> {
    let (scheme, key) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !key.trim().is_empty()).then(|| key.trim().into())
}

/// Fail closed when process-wide provider keys are placed in the cwd-scoped
/// policy file. Besides preventing accidental commits, this keeps
/// `persist_allow_rules` from ever rewriting a credential-bearing file.
pub(crate) fn reject_provider_keys_in_project_config(path: &Path) -> Result<()> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => bail!("cannot read project .kloop/config.toml"),
    };
    let table: toml::Table = raw
        .parse()
        .map_err(|_| anyhow!("cannot parse project .kloop/config.toml"))?;
    for key in [
        "model",
        "model_provider",
        "model_reasoning_effort",
        "model_providers",
    ] {
        if table.contains_key(key) {
            bail!(
                "project .kloop/config.toml contains provider key '{key}'; move provider settings to ~/.kloop/config.toml"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(entries: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            entries
                .iter()
                .find_map(|(key, value)| (*key == name).then(|| (*value).to_string()))
        }
    }

    #[test]
    fn codex_style_custom_responses_profile_resolves() {
        let raw = r#"
model = "gpt-5.6-sol"
model_provider = "gw_router"
model_reasoning_effort = "xhigh"

[model_providers.gw_router]
name = "gateway"
wire_api = "responses"
base_url = "https://router.example/v1/"
http_headers = { Authorization = "Bearer sentinel-key" }
"#;
        let settings = resolve(Some(raw), &env(&[])).unwrap();
        assert_eq!(settings.rail, Rail::OpenAiResponses);
        assert_eq!(settings.model(), "gpt-5.6-sol");
        assert_eq!(settings.base, "https://router.example/v1");
        assert_eq!(settings.effort.as_deref(), Some("xhigh"));
        assert_eq!(settings.model_info().provider, "openaiResponses");
    }

    #[test]
    fn all_three_rails_and_profile_models_resolve() {
        let anthropic = r#"
model_provider = "anthropic"
[model_providers.anthropic]
base_url = "https://anthropic.example"
http_headers = { x-api-key = "a-key" }
model = "claude-test"
cache = false
thinking = "adaptive"
"#;
        let settings = resolve(Some(anthropic), &env(&[])).unwrap();
        assert_eq!(settings.rail, Rail::Anthropic);
        assert_eq!(settings.model(), "claude-test");
        assert!(!settings.cache);
        assert_eq!(settings.thinking, ThinkingMode::Adaptive);

        let chat = r#"
model_provider = "openai"
[model_providers.openai]
http_headers = { Authorization = "Bearer chat-key" }
model = "chat-test"
"#;
        let settings = resolve(Some(chat), &env(&[])).unwrap();
        assert_eq!(settings.rail, Rail::OpenAiChat);
        assert_eq!(settings.model(), "chat-test");
    }

    #[test]
    fn environment_overrides_profile_without_cross_provider_fallback() {
        let raw = r#"
model = "file-model"
model_provider = "sky"
[model_providers.sky]
wire_api = "responses"
base_url = "https://file.example/v1"
http_headers = { Authorization = "Bearer file-key" }
"#;
        let settings = resolve(
            Some(raw),
            &env(&[
                ("OPENAI_API_KEY", "env-key"),
                ("OPENAI_BASE_URL", "https://env.example/v1"),
                ("OPENAI_MODEL", "env-model"),
                ("KLOOP_EFFORT", "high"),
            ]),
        )
        .unwrap();
        assert_eq!(settings.key, "env-key");
        assert_eq!(settings.base, "https://env.example/v1");
        assert_eq!(settings.model(), "env-model");
        assert_eq!(settings.effort.as_deref(), Some("high"));

        let error = resolve(
            Some(raw),
            &env(&[
                ("KLOOP_PROVIDER", "anthropic"),
                ("OPENAI_API_KEY", "wrong-rail"),
            ]),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("anthropic credentials missing"), "{error}");
        assert!(!error.contains("wrong-rail"));
    }

    #[test]
    fn pure_environment_compatibility_keeps_existing_precedence() {
        let settings = resolve(
            None,
            &env(&[
                ("OPENAI_API_KEY", "key"),
                ("OPENAI_MODEL", "specific"),
                ("KLOOP_MODEL", "shared"),
            ]),
        )
        .unwrap();
        assert_eq!(settings.rail, Rail::OpenAiChat);
        assert_eq!(settings.model(), "specific");

        let settings = resolve(
            None,
            &env(&[("ANTHROPIC_API_KEY", "key"), ("KLOOP_MODEL", "shared")]),
        )
        .unwrap();
        assert_eq!(settings.rail, Rail::Anthropic);
        assert_eq!(settings.model(), "shared");
    }

    #[test]
    fn strict_schema_and_errors_do_not_echo_secrets() {
        for raw in [
            "secret = \"SENTINEL\"\n",
            "model_provider = \"x\"\n[model_providers.x]\nwire_api = \"bad\"\nhttp_headers = { Authorization = \"Bearer SENTINEL\" }\n",
            "model_provider = \"x\"\n[model_providers.x]\nwire_api = \"responses\"\nhttp_headers = { X-Secret = \"SENTINEL\" }\n",
            "model_provider = \"x\"\n[model_providers.x]\nwire_api = \"responses\"\nbase_url = \"https://SENTINEL@example.test/v1\"\nhttp_headers = { Authorization = \"Bearer key\" }\n",
        ] {
            let error = resolve(Some(raw), &env(&[])).err().unwrap().to_string();
            assert!(!error.contains("SENTINEL"), "secret reflected: {error}");
        }
    }

    #[test]
    fn rejects_endpoint_urls_and_custom_profiles_without_wire() {
        let no_wire = r#"
model_provider = "custom"
[model_providers.custom]
http_headers = { Authorization = "Bearer key" }
"#;
        assert!(resolve(Some(no_wire), &env(&[]))
            .err()
            .unwrap()
            .to_string()
            .contains("wire_api"));

        for base in [
            "https://example.test/v1/responses",
            "https://example.test/v1?token=x",
            "file:///tmp/api",
        ] {
            let raw = format!(
                "model = \"m\"\nmodel_provider = \"x\"\n[model_providers.x]\nwire_api = \"responses\"\nbase_url = \"{base}\"\nhttp_headers = {{ Authorization = \"Bearer key\" }}\n"
            );
            assert!(resolve(Some(&raw), &env(&[])).is_err(), "accepted {base}");
        }
    }

    #[test]
    fn project_config_rejects_provider_keys() {
        let dir = std::env::temp_dir().join(format!(
            "kloop-project-provider-config-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[permissions]\nallow = []\n").unwrap();
        reject_provider_keys_in_project_config(&path).unwrap();
        std::fs::write(&path, "model = \"secret-model\"\n").unwrap();
        assert!(reject_provider_keys_in_project_config(&path).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn global_file_requires_private_regular_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir =
            std::env::temp_dir().join(format!("kloop-global-provider-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "model = \"m\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_global_file(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            read_global_file(&path).unwrap().as_deref(),
            Some("model = \"m\"\n")
        );

        let link = dir.join("link.toml");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_global_file(&link).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
