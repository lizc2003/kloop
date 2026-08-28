//! Immutable process-wide provider catalog resolution.
//!
//! The user config declares every logical provider and its ordered model
//! allowlist. Environment variables may select a new session's initial route,
//! but never add an undeclared provider or model.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use toml::Value;
use url::Url;

use kloop_core::provider_route::FrozenProviderRoute;
use kloop_core::provider_route::ProviderCatalog;
use kloop_core::provider_route::ProviderCatalogEntry;
use kloop_protocol::ProviderApiFamily;
use kloop_protocol::ProviderAvailabilityCode;
use kloop_protocol::ReasoningEffort;
use kloop_provider::Provider;
use kloop_provider::ThinkingMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rail {
    Anthropic,
    OpenAiChat,
    OpenAiResponses,
}

impl Rail {
    fn api_family(self) -> ProviderApiFamily {
        match self {
            Self::Anthropic => ProviderApiFamily::AnthropicMessages,
            Self::OpenAiChat => ProviderApiFamily::OpenAiChatCompletions,
            Self::OpenAiResponses => ProviderApiFamily::OpenAiResponses,
        }
    }

    fn default_base(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com",
            Self::OpenAiChat | Self::OpenAiResponses => "https://api.openai.com/v1",
        }
    }
}

/// Secret-free catalog plus one validated initial route. Deliberately no
/// Debug/Serialize: its catalog owns lazy factories that capture credentials.
pub(crate) struct ResolvedProviderSettings {
    catalog: Arc<ProviderCatalog>,
    initial_provider: String,
    initial_route: FrozenProviderRoute,
}

impl ResolvedProviderSettings {
    pub(crate) fn mock() -> Self {
        let (catalog, initial_route) = ProviderCatalog::from_provider(
            "mock",
            Provider::mock(Vec::new()),
            "mock",
            vec!["mock".into()],
            None,
        )
        .expect("built-in mock catalog is valid");
        Self {
            catalog,
            initial_provider: "mock".into(),
            initial_route,
        }
    }

    pub(crate) fn catalog(&self) -> Arc<ProviderCatalog> {
        Arc::clone(&self.catalog)
    }

    pub(crate) fn initial_route(&self) -> FrozenProviderRoute {
        self.initial_route.clone()
    }

    pub(crate) fn initial_provider(&self) -> &str {
        &self.initial_provider
    }

    #[cfg(test)]
    pub(crate) fn model(&self) -> &str {
        self.initial_route.primary_model()
    }
}

struct Profile {
    wire: Rail,
    base_url: String,
    headers: BTreeMap<String, String>,
    default_model: String,
    models: Vec<String>,
    fallback_model: Option<String>,
    cache: bool,
    thinking: ThinkingMode,
    effort: Option<ReasoningEffort>,
}

struct GlobalFile {
    initial_model: Option<String>,
    initial_provider: String,
    /// Root `effort`: the rail-agnostic level a session starts at, overriding
    /// the selected profile's own `effort` (same key, wider scope).
    initial_effort: Option<ReasoningEffort>,
    profiles: BTreeMap<String, Profile>,
}

pub(crate) fn load(mock: bool, table: &toml::Table) -> Result<ResolvedProviderSettings> {
    if mock {
        return Ok(ResolvedProviderSettings::mock());
    }
    resolve_table(Some(table), &|name| std::env::var(name).ok())
}

#[cfg(test)]
fn resolve(
    raw: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<ResolvedProviderSettings> {
    let table = raw
        .map(|raw| {
            raw.parse::<toml::Table>()
                .map_err(|_| anyhow!("cannot parse ~/.kloop/config.toml (TOML syntax error)"))
        })
        .transpose()?;
    resolve_table(table.as_ref(), env)
}

fn resolve_table(
    table: Option<&toml::Table>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<ResolvedProviderSettings> {
    let file = match table {
        Some(table) if !table.is_empty() => parse_global_file(table)?,
        _ => env_only_file(env)?,
    };
    let initial_provider =
        nonempty_env(env, "KLOOP_PROVIDER")?.unwrap_or_else(|| file.initial_provider.clone());
    let profile = file.profiles.get(&initial_provider).with_context(|| {
        format!("provider profile '{initial_provider}' is not defined in ~/.kloop/config.toml")
    })?;
    let rail_model = match profile.wire {
        Rail::Anthropic => nonempty_env(env, "ANTHROPIC_MODEL")?,
        Rail::OpenAiChat | Rail::OpenAiResponses => nonempty_env(env, "OPENAI_MODEL")?,
    };
    let initial_model = rail_model
        .or(nonempty_env(env, "KLOOP_MODEL")?)
        .or(file.initial_model)
        .unwrap_or_else(|| profile.default_model.clone());
    if !profile.models.iter().any(|model| model == &initial_model) {
        bail!(
            "initial model '{initial_model}' is not in provider '{initial_provider}' models allowlist"
        );
    }

    let root_effort = file.initial_effort;
    let mut entries = Vec::with_capacity(file.profiles.len());
    for (id, profile) in file.profiles {
        let selected = id == initial_provider;
        let base = selected_base(&profile, selected, env)?;
        let credential = selected_credential(&profile, selected, env)?;
        let availability = if credential.is_some() {
            ProviderAvailabilityCode::Ready
        } else {
            ProviderAvailabilityCode::MissingCredential
        };
        let api_family = profile.wire.api_family();
        let endpoint_fingerprint = Provider::endpoint_fingerprint_for(api_family, &base);
        let wire = profile.wire;
        let cache = profile.cache;
        let thinking = profile.thinking;
        let default_effort = selected_effort(&profile, selected, root_effort, env)?;
        let factory = Arc::new(move || {
            let key = credential
                .clone()
                .ok_or(ProviderAvailabilityCode::MissingCredential)?;
            Ok(match wire {
                Rail::Anthropic => Provider::Anthropic {
                    key,
                    base: base.clone(),
                    cache,
                    thinking,
                },
                Rail::OpenAiChat => Provider::OpenAiCompat {
                    key,
                    base: base.clone(),
                },
                Rail::OpenAiResponses => Provider::OpenAiResponses {
                    key,
                    base: base.clone(),
                },
            })
        });
        entries.push(ProviderCatalogEntry {
            id,
            api_family,
            endpoint_fingerprint,
            default_model: profile.default_model,
            models: profile.models,
            fallback_model: profile.fallback_model,
            availability,
            default_effort,
            factory,
        });
    }
    let catalog = Arc::new(ProviderCatalog::new(entries).map_err(anyhow::Error::msg)?);
    let initial_route = catalog
        .initial_route(&initial_provider, Some(&initial_model))
        .map_err(anyhow::Error::new)?;
    Ok(ResolvedProviderSettings {
        catalog,
        initial_provider,
        initial_route,
    })
}

fn env_only_file(env: &dyn Fn(&str) -> Option<String>) -> Result<GlobalFile> {
    let id = nonempty_env(env, "KLOOP_PROVIDER")?
        .context("no provider configured: set KLOOP_PROVIDER or declare model_provider in ~/.kloop/config.toml")?;
    let wire = match id.as_str() {
        "anthropic" => Rail::Anthropic,
        "openai" | "openai-compat" => Rail::OpenAiChat,
        "openai-responses" => Rail::OpenAiResponses,
        _ => bail!("provider '{id}' requires a declared model_providers profile"),
    };
    let model = match wire {
        Rail::Anthropic => nonempty_env(env, "ANTHROPIC_MODEL")?,
        Rail::OpenAiChat | Rail::OpenAiResponses => nonempty_env(env, "OPENAI_MODEL")?,
    }
    .or(nonempty_env(env, "KLOOP_MODEL")?)
    .unwrap_or_else(|| {
        if wire == Rail::Anthropic {
            "claude-sonnet-5".into()
        } else {
            "gpt-5.6-sol".into()
        }
    });
    let key = match wire {
        Rail::Anthropic => nonempty_env(env, "ANTHROPIC_API_KEY")?,
        Rail::OpenAiChat | Rail::OpenAiResponses => nonempty_env(env, "OPENAI_API_KEY")?,
    }
    .context("provider credentials missing; set the provider API key environment variable")?;
    let base = match wire {
        Rail::Anthropic => nonempty_env(env, "ANTHROPIC_BASE_URL")?,
        Rail::OpenAiChat | Rail::OpenAiResponses => nonempty_env(env, "OPENAI_BASE_URL")?,
    }
    .unwrap_or_else(|| wire.default_base().into());
    let base_url = validate_base_url(&base, "provider base URL")?;
    let mut headers = BTreeMap::new();
    match wire {
        Rail::Anthropic => {
            headers.insert("x-api-key".into(), key);
        }
        Rail::OpenAiChat | Rail::OpenAiResponses => {
            headers.insert("authorization".into(), format!("Bearer {key}"));
        }
    }
    let profile = Profile {
        wire,
        base_url,
        headers,
        default_model: model.clone(),
        models: vec![model.clone()],
        fallback_model: None,
        cache: wire == Rail::Anthropic,
        thinking: ThinkingMode::Unset,
        // `KLOOP_EFFORT` is applied by `selected_effort` for the selected
        // provider — which, here, is the only one.
        effort: None,
    };
    Ok(GlobalFile {
        initial_model: Some(model),
        initial_provider: id.clone(),
        initial_effort: None,
        profiles: BTreeMap::from([(id, profile)]),
    })
}

fn selected_base(
    profile: &Profile,
    selected: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<String> {
    let override_name = match profile.wire {
        Rail::Anthropic => "ANTHROPIC_BASE_URL",
        Rail::OpenAiChat | Rail::OpenAiResponses => "OPENAI_BASE_URL",
    };
    if selected && let Some(base) = nonempty_env(env, override_name)? {
        return validate_base_url(&base, override_name);
    }
    Ok(profile.base_url.clone())
}

/// The effort this provider starts a session at: `KLOOP_EFFORT` beats the root
/// root `effort`, which beats the profile's own `effort`. Like the
/// base URL and credential overrides, the two global sources apply only to the
/// selected provider — they must not silently retarget the others. Only the
/// spelling is checked (at parse time): which levels are legal belongs to the
/// model, not the wire, and the provider names its own supported set on refusal.
fn selected_effort(
    profile: &Profile,
    selected: bool,
    root: Option<ReasoningEffort>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<ReasoningEffort>> {
    Ok(if selected {
        parse_effort_env(env)?.or(root).or(profile.effort)
    } else {
        profile.effort
    })
}

fn parse_effort_env(env: &dyn Fn(&str) -> Option<String>) -> Result<Option<ReasoningEffort>> {
    nonempty_env(env, "KLOOP_EFFORT")?
        .map(|raw| {
            raw.parse::<ReasoningEffort>()
                .map_err(|e| anyhow!("KLOOP_EFFORT: {e}"))
        })
        .transpose()
}

fn selected_credential(
    profile: &Profile,
    selected: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<String>> {
    let from_env = if selected {
        match profile.wire {
            Rail::Anthropic => nonempty_env(env, "ANTHROPIC_API_KEY")?,
            Rail::OpenAiChat | Rail::OpenAiResponses => nonempty_env(env, "OPENAI_API_KEY")?,
        }
    } else {
        None
    };
    Ok(from_env.or_else(|| match profile.wire {
        Rail::Anthropic => profile.headers.get("x-api-key").cloned(),
        Rail::OpenAiChat | Rail::OpenAiResponses => bearer_key(&profile.headers),
    }))
}

fn parse_global_file(table: &toml::Table) -> Result<GlobalFile> {
    let initial_model = optional_string(table, "model", "model")?;
    let initial_effort = optional_string(table, "effort", "effort")?
        .map(|raw| {
            raw.parse::<ReasoningEffort>()
                .map_err(|e| anyhow!("effort: {e}"))
        })
        .transpose()?;
    let initial_provider = optional_string(table, "model_provider", "model_provider")?
        .context("model_provider is required")?;
    let providers = table
        .get("model_providers")
        .and_then(Value::as_table)
        .context("model_providers must be a table")?;
    let mut profiles = BTreeMap::new();
    for (id, value) in providers {
        let spec = value
            .as_table()
            .with_context(|| format!("model_providers.{id} must be a table"))?;
        profiles.insert(id.clone(), parse_profile(id, spec)?);
    }
    if !profiles.contains_key(&initial_provider) {
        bail!("model_provider '{initial_provider}' has no matching model_providers profile");
    }
    Ok(GlobalFile {
        initial_model,
        initial_provider,
        initial_effort,
        profiles,
    })
}

fn parse_profile(id: &str, spec: &toml::Table) -> Result<Profile> {
    for key in spec.keys() {
        if !matches!(
            key.as_str(),
            "name"
                | "wire_api"
                | "base_url"
                | "http_headers"
                | "default_model"
                | "models"
                | "fallback_model"
                | "cache"
                | "thinking"
                | "effort"
        ) {
            bail!("model_providers.{id} has unknown key '{key}'");
        }
    }
    let _display_name = optional_string(spec, "name", &format!("model_providers.{id}.name"))?;
    let wire = parse_wire(
        &required_string(spec, "wire_api", &format!("model_providers.{id}.wire_api"))?,
        id,
    )?;
    let default_model = required_string(
        spec,
        "default_model",
        &format!("model_providers.{id}.default_model"),
    )?;
    let models = required_string_array(spec, "models", &format!("model_providers.{id}.models"))?;
    if !models.iter().any(|model| model == &default_model) {
        bail!("model_providers.{id}.default_model '{default_model}' is not in models allowlist");
    }
    let fallback_model = optional_string(
        spec,
        "fallback_model",
        &format!("model_providers.{id}.fallback_model"),
    )?;
    if let Some(fallback) = fallback_model.as_ref()
        && !models.iter().any(|model| model == fallback)
    {
        bail!("model_providers.{id}.fallback_model '{fallback}' is not in models allowlist");
    }
    let base_url = optional_string(spec, "base_url", &format!("model_providers.{id}.base_url"))?
        .unwrap_or_else(|| wire.default_base().to_string());
    let base_url = validate_base_url(&base_url, &format!("model_providers.{id}.base_url"))?;
    let cache = optional_bool(spec, "cache", &format!("model_providers.{id}.cache"))?
        .unwrap_or(wire == Rail::Anthropic);
    let effort = optional_string(spec, "effort", &format!("model_providers.{id}.effort"))?
        .map(|raw| {
            raw.parse::<ReasoningEffort>()
                .map_err(|e| anyhow!("model_providers.{id}.effort: {e}"))
        })
        .transpose()?;
    let thinking = match spec.get("thinking") {
        None => ThinkingMode::Unset,
        Some(Value::String(raw)) => {
            parse_thinking_string(raw, &format!("model_providers.{id}.thinking"))?
        }
        Some(Value::Integer(raw)) if *raw > 0 => ThinkingMode::Budget(*raw as u64),
        Some(_) => {
            bail!("model_providers.{id}.thinking must be 'off', 'adaptive', or a positive integer")
        }
    };
    if wire != Rail::Anthropic && (spec.contains_key("cache") || spec.contains_key("thinking")) {
        bail!("model_providers.{id}: cache/thinking are only valid for anthropic wire_api");
    }
    let headers = parse_headers(id, spec.get("http_headers"), wire)?;
    Ok(Profile {
        wire,
        base_url,
        headers,
        default_model,
        models,
        fallback_model,
        cache,
        thinking,
        effort,
    })
}

fn required_string(table: &toml::Table, key: &str, field: &str) -> Result<String> {
    optional_string(table, key, field)?.with_context(|| format!("{field} is required"))
}

fn required_string_array(table: &toml::Table, key: &str, field: &str) -> Result<Vec<String>> {
    let values = table
        .get(key)
        .and_then(Value::as_array)
        .with_context(|| format!("{field} must be an array of strings"))?;
    let mut models = Vec::new();
    for value in values {
        let model = value
            .as_str()
            .with_context(|| format!("{field} must contain only strings"))?;
        let model = nonempty(model, field)?;
        if !models.contains(&model) {
            models.push(model);
        }
    }
    if models.is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(models)
}

fn parse_headers(id: &str, value: Option<&Value>, wire: Rail) -> Result<BTreeMap<String, String>> {
    let mut headers = BTreeMap::new();
    let Some(value) = value else {
        return Ok(headers);
    };
    let table = value
        .as_table()
        .with_context(|| format!("model_providers.{id}.http_headers must be a table"))?;
    for (header, value) in table {
        let folded = header.to_ascii_lowercase();
        let allowed = match wire {
            Rail::Anthropic => folded == "x-api-key",
            Rail::OpenAiChat | Rail::OpenAiResponses => folded == "authorization",
        };
        if !allowed {
            bail!("model_providers.{id}.http_headers contains unsupported header '{header}'");
        }
        let value = value.as_str().with_context(|| {
            format!("model_providers.{id}.http_headers.{header} must be a string")
        })?;
        let value = nonempty(
            value,
            &format!("model_providers.{id}.http_headers.{header}"),
        )?;
        if headers.insert(folded, value).is_some() {
            bail!("model_providers.{id}.http_headers contains a duplicate header");
        }
    }
    if wire != Rail::Anthropic
        && let Some(value) = headers.get("authorization")
        && bearer_from_header(value).is_none()
    {
        bail!("model_providers.{id}.http_headers.Authorization must use Bearer authentication");
    }
    Ok(headers)
}

fn parse_wire(raw: &str, id: &str) -> Result<Rail> {
    match raw {
        "anthropic" => Ok(Rail::Anthropic),
        "chat" => Ok(Rail::OpenAiChat),
        "responses" => Ok(Rail::OpenAiResponses),
        _ => bail!("model_providers.{id}.wire_api must be anthropic | chat | responses"),
    }
}

fn validate_base_url(raw: &str, field: &str) -> Result<String> {
    if raw == "mock" {
        return Ok(raw.into());
    }
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

    const CATALOG: &str = r#"
model_provider = "anthropic-a"

[model_providers.anthropic-a]
wire_api = "anthropic"
base_url = "https://anthropic-a.example"
http_headers = { x-api-key = "a-key" }
default_model = "claude-a"
models = ["claude-a", "claude-b", "claude-a"]
fallback_model = "claude-b"
cache = false
thinking = "adaptive"

[model_providers.responses-b]
wire_api = "responses"
base_url = "https://responses-b.example/v1"
http_headers = { Authorization = "Bearer b-key" }
default_model = "gpt-a"
models = ["gpt-a", "gpt-b"]
effort = "high"

[model_providers.chat-c]
wire_api = "chat"
base_url = "https://chat-c.example/v1"
default_model = "chat-a"
models = ["chat-a", "shared"]
"#;

    #[test]
    fn canonical_catalog_preserves_ordered_allowlists_and_availability() {
        let settings = resolve(Some(CATALOG), &env(&[])).unwrap();
        assert_eq!(settings.initial_provider(), "anthropic-a");
        assert_eq!(settings.model(), "claude-a");
        let descriptors = settings.catalog().descriptors();
        assert_eq!(descriptors.len(), 3);
        assert_eq!(descriptors[0].models, ["claude-a", "claude-b"]);
        assert_eq!(descriptors[0].fallback_model.as_deref(), Some("claude-b"));
        assert_eq!(
            descriptors[1].availability,
            ProviderAvailabilityCode::MissingCredential
        );
    }

    #[test]
    fn environment_selects_only_declared_initial_routes() {
        let settings = resolve(
            Some(CATALOG),
            &env(&[("KLOOP_PROVIDER", "responses-b"), ("KLOOP_MODEL", "gpt-b")]),
        )
        .unwrap();
        assert_eq!(settings.initial_provider(), "responses-b");
        assert_eq!(settings.model(), "gpt-b");

        let error = resolve(
            Some(CATALOG),
            &env(&[("KLOOP_PROVIDER", "responses-b"), ("KLOOP_MODEL", "raw")]),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("allowlist"));
    }

    #[test]
    fn selected_environment_credentials_do_not_make_other_profiles_ready() {
        let raw = CATALOG.replace("http_headers = { Authorization = \"Bearer b-key\" }", "");
        let settings = resolve(
            Some(&raw),
            &env(&[
                ("KLOOP_PROVIDER", "responses-b"),
                ("OPENAI_API_KEY", "selected-key"),
            ]),
        )
        .unwrap();
        let descriptors = settings.catalog().descriptors();
        assert_eq!(descriptors[2].availability, ProviderAvailabilityCode::Ready);
        assert_eq!(
            descriptors[1].availability,
            ProviderAvailabilityCode::MissingCredential
        );
    }

    #[test]
    fn rejects_legacy_profile_model_and_invalid_membership() {
        let legacy = r#"
model_provider = "x"
[model_providers.x]
wire_api = "responses"
model = "old"
http_headers = { Authorization = "Bearer key" }
"#;
        assert!(
            resolve(Some(legacy), &env(&[]))
                .err()
                .unwrap()
                .to_string()
                .contains("unknown key 'model'")
        );

        for field in [
            "default_model = \"missing\"\nmodels = [\"a\"]",
            "default_model = \"a\"\nmodels = [\"a\"]\nfallback_model = \"missing\"",
        ] {
            let raw = format!(
                "model_provider = \"x\"\n[model_providers.x]\nwire_api = \"responses\"\nhttp_headers = {{ Authorization = \"Bearer key\" }}\n{field}\n"
            );
            assert!(resolve(Some(&raw), &env(&[])).is_err());
        }
    }

    #[test]
    fn errors_never_echo_credentials() {
        for raw in [
            "secret = \"SENTINEL\"\n",
            "model_provider = \"x\"\n[model_providers.x]\nwire_api = \"bad\"\nhttp_headers = { Authorization = \"Bearer SENTINEL\" }\ndefault_model = \"m\"\nmodels = [\"m\"]\n",
            "model_provider = \"x\"\n[model_providers.x]\nwire_api = \"responses\"\nbase_url = \"https://SENTINEL@example.test/v1\"\nhttp_headers = { Authorization = \"Bearer key\" }\ndefault_model = \"m\"\nmodels = [\"m\"]\n",
        ] {
            let error = resolve(Some(raw), &env(&[])).err().unwrap().to_string();
            assert!(!error.contains("SENTINEL"), "secret reflected: {error}");
        }
    }

    /// `effort` is a per-provider default on every rail now (not responses
    /// only), the root `effort` and `KLOOP_EFFORT` override it
    /// for the *selected* provider only, and every source is checked against
    /// the rail that would have to send it.
    #[test]
    fn effort_resolves_env_over_root_over_profile_for_the_selected_provider() {
        const WITH_EFFORT: &str = r#"
model_provider = "anthropic-a"
effort = "max"

[model_providers.anthropic-a]
wire_api = "anthropic"
http_headers = { x-api-key = "a-key" }
default_model = "claude-a"
models = ["claude-a"]
effort = "low"

[model_providers.responses-b]
wire_api = "responses"
http_headers = { Authorization = "Bearer b-key" }
default_model = "gpt-a"
models = ["gpt-a"]
effort = "none"
"#;
        // Root beats the selected profile; an unselected profile keeps its own.
        let rooted = resolve(Some(WITH_EFFORT), &env(&[])).unwrap();
        assert_eq!(
            rooted.catalog().default_effort("anthropic-a"),
            Some(ReasoningEffort::Max)
        );
        assert_eq!(
            rooted.catalog().default_effort("responses-b"),
            Some(ReasoningEffort::None)
        );

        // The environment beats the root key, and follows the selection.
        let from_env = resolve(
            Some(WITH_EFFORT),
            &env(&[("KLOOP_PROVIDER", "responses-b"), ("KLOOP_EFFORT", "high")]),
        )
        .unwrap();
        assert_eq!(
            from_env.catalog().default_effort("responses-b"),
            Some(ReasoningEffort::High)
        );
        assert_eq!(
            from_env.catalog().default_effort("anthropic-a"),
            Some(ReasoningEffort::Low)
        );
    }

    /// Only kloop's own spelling is enforced at load time. Which levels a model
    /// takes is the model's contract, so a wire never vetoes a level.
    #[test]
    fn effort_rejects_only_unknown_spellings() {
        const XHIGH_ON_RESPONSES: &str = r#"
model_provider = "responses-b"

[model_providers.responses-b]
wire_api = "responses"
http_headers = { Authorization = "Bearer b-key" }
default_model = "gpt-a"
models = ["gpt-a"]
effort = "xhigh"
"#;
        assert_eq!(
            resolve(Some(XHIGH_ON_RESPONSES), &env(&[]))
                .unwrap()
                .catalog()
                .default_effort("responses-b"),
            Some(ReasoningEffort::XHigh)
        );

        const TYPO: &str = r#"
model_provider = "anthropic-a"

[model_providers.anthropic-a]
wire_api = "anthropic"
http_headers = { x-api-key = "a-key" }
default_model = "claude-a"
models = ["claude-a"]
effort = "sky-high"
"#;
        assert_eq!(
            resolve(Some(TYPO), &env(&[]))
                .map(|_| ())
                .unwrap_err()
                .to_string(),
            "model_providers.anthropic-a.effort: unknown effort 'sky-high' \
             (known: none, low, medium, high, xhigh, max)"
        );
        assert_eq!(
            resolve(Some(XHIGH_ON_RESPONSES), &env(&[("KLOOP_EFFORT", "hgih")]))
                .map(|_| ())
                .unwrap_err()
                .to_string(),
            "KLOOP_EFFORT: unknown effort 'hgih' \
             (known: none, low, medium, high, xhigh, max)"
        );
    }
}
