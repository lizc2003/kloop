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
    /// The selected provider's declared window, if it declared one. Only the
    /// initial provider's matters: `/provider` switching mid-session would need
    /// the whole compaction budget re-derived, which is a separate change.
    initial_context_window: Option<u64>,
}

impl ResolvedProviderSettings {
    pub(crate) fn mock() -> Self {
        let (catalog, initial_route) = ProviderCatalog::from_provider(
            "mock",
            Provider::mock(Vec::new()),
            "mock",
            vec!["mock".into()],
        )
        .expect("built-in mock catalog is valid");
        Self {
            catalog,
            initial_provider: "mock".into(),
            initial_route,
            initial_context_window: None,
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

    pub(crate) fn initial_context_window(&self) -> Option<u64> {
        self.initial_context_window
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
    model: String,
    models: Vec<String>,
    cache: bool,
    thinking: ThinkingMode,
    effort: Option<ReasoningEffort>,
    /// The model's real context window, when the provider knows it. Only the
    /// provider can: the global default has to be conservative enough for the
    /// smallest model anyone routes to, and a window set too low costs nothing
    /// visible — it just compacts earlier than it had to, which is why it stays
    /// wrong for a long time.
    context_window: Option<u64>,
}

struct GlobalFile {
    initial_provider: String,
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
        .unwrap_or_else(|| profile.model.clone());
    if !profile.models.iter().any(|model| model == &initial_model) {
        bail!(
            "initial model '{initial_model}' is not in provider '{initial_provider}' models allowlist"
        );
    }
    // Read before the loop below moves the profiles out.
    let initial_context_window = profile.context_window;

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
        let default_effort = selected_effort(&profile, selected, env)?;
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
            default_model: profile.model,
            models: profile.models,
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
        initial_context_window,
    })
}

fn env_only_file(env: &dyn Fn(&str) -> Option<String>) -> Result<GlobalFile> {
    let id = nonempty_env(env, "KLOOP_PROVIDER")?.context(
        "no provider configured: set KLOOP_PROVIDER or declare provider in ~/.kloop/config.toml",
    )?;
    let wire = match id.as_str() {
        "anthropic" => Rail::Anthropic,
        "openai" | "openai-compat" => Rail::OpenAiChat,
        "openai-responses" => Rail::OpenAiResponses,
        _ => bail!("provider '{id}' requires a declared providers profile"),
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
        model: model.clone(),
        models: vec![model],
        cache: wire == Rail::Anthropic,
        thinking: ThinkingMode::Unset,
        // `KLOOP_EFFORT` is applied by `selected_effort` for the selected
        // provider — which, here, is the only one.
        effort: None,
        // No config file to declare it in; the global default applies.
        context_window: None,
    };
    Ok(GlobalFile {
        initial_provider: id.clone(),
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

/// The effort this provider starts a session at: `KLOOP_EFFORT` beats the
/// profile's own `effort`. Like the base URL and credential overrides, the env
/// source applies to the selected provider only — it must not silently retarget
/// the others. Only the spelling is checked (at parse time): which levels are
/// legal belongs to the model, not the wire, and the provider names its own
/// supported set on refusal.
fn selected_effort(
    profile: &Profile,
    selected: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<ReasoningEffort>> {
    Ok(if selected {
        parse_effort_env(env)?.or(profile.effort)
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
    let initial_provider =
        optional_string(table, "provider", "provider")?.context("provider is required")?;
    let providers = table
        .get("providers")
        .and_then(Value::as_table)
        .context("providers must be a table")?;
    let mut profiles = BTreeMap::new();
    for (id, value) in providers {
        let spec = value
            .as_table()
            .with_context(|| format!("providers.{id} must be a table"))?;
        profiles.insert(id.clone(), parse_profile(id, spec)?);
    }
    if !profiles.contains_key(&initial_provider) {
        bail!("provider '{initial_provider}' has no matching providers profile");
    }
    Ok(GlobalFile {
        initial_provider,
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
                | "model"
                | "models"
                | "cache"
                | "thinking"
                | "effort"
                | "context_window"
        ) {
            bail!("providers.{id} has unknown key '{key}'");
        }
    }
    let _display_name = optional_string(spec, "name", &format!("providers.{id}.name"))?;
    let wire = parse_wire(
        &required_string(spec, "wire_api", &format!("providers.{id}.wire_api"))?,
        id,
    )?;
    let model = required_string(spec, "model", &format!("providers.{id}.model"))?;
    let models = required_string_array(spec, "models", &format!("providers.{id}.models"))?;
    if !models.iter().any(|candidate| candidate == &model) {
        bail!("providers.{id}.model '{model}' is not in models allowlist");
    }
    let base_url = optional_string(spec, "base_url", &format!("providers.{id}.base_url"))?
        .unwrap_or_else(|| wire.default_base().to_string());
    let base_url = validate_base_url(&base_url, &format!("providers.{id}.base_url"))?;
    let cache = optional_bool(spec, "cache", &format!("providers.{id}.cache"))?
        .unwrap_or(wire == Rail::Anthropic);
    let effort = optional_string(spec, "effort", &format!("providers.{id}.effort"))?
        .map(|raw| {
            raw.parse::<ReasoningEffort>()
                .map_err(|e| anyhow!("providers.{id}.effort: {e}"))
        })
        .transpose()?;
    let thinking = match spec.get("thinking") {
        None => ThinkingMode::Unset,
        Some(Value::String(raw)) => {
            parse_thinking_string(raw, &format!("providers.{id}.thinking"))?
        }
        Some(Value::Integer(raw)) if *raw > 0 => ThinkingMode::Budget(*raw as u64),
        Some(_) => {
            bail!("providers.{id}.thinking must be 'off', 'adaptive', or a positive integer")
        }
    };
    if wire != Rail::Anthropic && (spec.contains_key("cache") || spec.contains_key("thinking")) {
        bail!("providers.{id}: cache/thinking are only valid for messages wire_api");
    }
    let context_window = optional_integer(
        spec,
        "context_window",
        &format!("providers.{id}.context_window"),
    )?;
    let headers = parse_headers(id, spec.get("http_headers"), wire)?;
    Ok(Profile {
        wire,
        base_url,
        headers,
        model,
        models,
        cache,
        thinking,
        effort,
        context_window,
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
        .with_context(|| format!("providers.{id}.http_headers must be a table"))?;
    for (header, value) in table {
        let folded = header.to_ascii_lowercase();
        let allowed = match wire {
            Rail::Anthropic => folded == "x-api-key",
            Rail::OpenAiChat | Rail::OpenAiResponses => folded == "authorization",
        };
        if !allowed {
            bail!("providers.{id}.http_headers contains unsupported header '{header}'");
        }
        let value = value
            .as_str()
            .with_context(|| format!("providers.{id}.http_headers.{header} must be a string"))?;
        let value = nonempty(value, &format!("providers.{id}.http_headers.{header}"))?;
        if headers.insert(folded, value).is_some() {
            bail!("providers.{id}.http_headers contains a duplicate header");
        }
    }
    if wire != Rail::Anthropic
        && let Some(value) = headers.get("authorization")
        && bearer_from_header(value).is_none()
    {
        bail!("providers.{id}.http_headers.Authorization must use Bearer authentication");
    }
    Ok(headers)
}

fn parse_wire(raw: &str, id: &str) -> Result<Rail> {
    match raw {
        "messages" => Ok(Rail::Anthropic),
        "chat" => Ok(Rail::OpenAiChat),
        "responses" => Ok(Rail::OpenAiResponses),
        _ => bail!("providers.{id}.wire_api must be messages | chat | responses"),
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

/// A positive token count. Zero is rejected rather than read as "unbounded":
/// `KLOOP_CONTEXT_WINDOW=0` means off, but a config file saying `0` is far more
/// likely a mistake than a request to disable the guard.
fn optional_integer(table: &toml::Table, key: &str, field: &str) -> Result<Option<u64>> {
    table
        .get(key)
        .map(|value| {
            let raw = value
                .as_integer()
                .with_context(|| format!("{field} must be an integer token count"))?;
            u64::try_from(raw)
                .ok()
                .filter(|count| *count > 0)
                .with_context(|| format!("{field} must be a positive token count, got {raw}"))
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
provider = "anthropic-a"

[providers.anthropic-a]
wire_api = "messages"
base_url = "https://anthropic-a.example"
http_headers = { x-api-key = "a-key" }
model = "claude-a"
models = ["claude-a", "claude-b", "claude-a"]
cache = false
thinking = "adaptive"

[providers.responses-b]
wire_api = "responses"
base_url = "https://responses-b.example/v1"
http_headers = { Authorization = "Bearer b-key" }
model = "gpt-a"
models = ["gpt-a", "gpt-b"]
effort = "high"

[providers.chat-c]
wire_api = "chat"
base_url = "https://chat-c.example/v1"
model = "chat-a"
models = ["chat-a", "shared"]
"#;

    /// Only the selected provider's window is read, and a provider that declares
    /// nothing yields `None` so the caller can fall back — the distinction the
    /// whole precedence chain rests on.
    #[test]
    fn context_window_comes_from_the_selected_provider_only() {
        let table: toml::Table = r#"
provider = "responses-b"

[providers.anthropic-a]
wire_api = "messages"
base_url = "https://anthropic-a.example"
http_headers = { x-api-key = "a-key" }
model = "claude-a"
models = ["claude-a"]
context_window = 111000

[providers.responses-b]
wire_api = "responses"
base_url = "https://responses-b.example/v1"
http_headers = { Authorization = "Bearer b-key" }
model = "gpt-a"
models = ["gpt-a"]
context_window = 258400
"#
        .parse()
        .unwrap();
        let selected = resolve_table(Some(&table), &env(&[])).unwrap();
        assert_eq!(selected.initial_context_window(), Some(258_400));

        // The other provider's 111000 must not leak in when it is selected away
        // from; and a profile without the key declares nothing.
        let other =
            resolve_table(Some(&table), &env(&[("KLOOP_PROVIDER", "anthropic-a")])).unwrap();
        assert_eq!(other.initial_context_window(), Some(111_000));

        let bare: toml::Table = CATALOG.parse().unwrap();
        assert_eq!(
            resolve_table(Some(&bare), &env(&[]))
                .unwrap()
                .initial_context_window(),
            None
        );
    }

    #[test]
    fn context_window_rejects_non_positive_and_non_integer() {
        for (raw, want) in [
            ("context_window = 0", "must be a positive token count"),
            ("context_window = -5", "must be a positive token count"),
            (
                "context_window = \"258400\"",
                "must be an integer token count",
            ),
        ] {
            let table: toml::Table = format!(
                r#"
provider = "responses-b"

[providers.responses-b]
wire_api = "responses"
base_url = "https://responses-b.example/v1"
http_headers = {{ Authorization = "Bearer b-key" }}
model = "gpt-a"
models = ["gpt-a"]
{raw}
"#
            )
            .parse()
            .unwrap();
            let error = resolve_table(Some(&table), &env(&[]))
                .err()
                .expect("invalid context_window must be rejected")
                .to_string();
            assert!(error.contains(want), "{raw} → {error}");
        }
    }

    #[test]
    fn canonical_catalog_preserves_ordered_allowlists_and_availability() {
        let settings = resolve(Some(CATALOG), &env(&[])).unwrap();
        assert_eq!(settings.initial_provider(), "anthropic-a");
        assert_eq!(settings.model(), "claude-a");
        let descriptors = settings.catalog().descriptors();
        assert_eq!(descriptors.len(), 3);
        assert_eq!(descriptors[0].models, ["claude-a", "claude-b"]);
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
    fn rejects_retired_default_model_key_and_invalid_membership() {
        let retired = r#"
provider = "x"
[providers.x]
wire_api = "responses"
default_model = "old"
http_headers = { Authorization = "Bearer key" }
"#;
        assert!(
            resolve(Some(retired), &env(&[]))
                .err()
                .unwrap()
                .to_string()
                .contains("unknown key 'default_model'")
        );

        let outside_allowlist = "provider = \"x\"\n[providers.x]\nwire_api = \"responses\"\n\
             http_headers = { Authorization = \"Bearer key\" }\n\
             model = \"missing\"\nmodels = [\"a\"]\n";
        assert!(resolve(Some(outside_allowlist), &env(&[])).is_err());
    }

    /// `wire_api` names the endpoint, not the vendor — the axis
    /// `ProviderApiFamily` uses. An unknown wire fails closed, and the refusal
    /// names every legal spelling rather than just rejecting the bad one.
    #[test]
    fn wire_api_names_the_endpoint_and_an_unknown_one_fails_closed() {
        let profile = |wire: &str| {
            format!(
                "provider = \"x\"\n[providers.x]\nwire_api = \"{wire}\"\n\
                 http_headers = {{ x-api-key = \"k\" }}\nmodel = \"m\"\nmodels = [\"m\"]\n"
            )
        };
        assert_eq!(
            resolve(Some(&profile("vendor")), &env(&[]))
                .map(|_| ())
                .unwrap_err()
                .to_string(),
            "providers.x.wire_api must be messages | chat | responses"
        );
        let resolved = resolve(Some(&profile("messages")), &env(&[])).unwrap();
        assert_eq!(
            resolved.catalog().descriptors()[0].api_family,
            ProviderApiFamily::AnthropicMessages
        );
    }

    #[test]
    fn errors_never_echo_credentials() {
        for raw in [
            "secret = \"SENTINEL\"\n",
            "provider = \"x\"\n[providers.x]\nwire_api = \"bad\"\nhttp_headers = { Authorization = \"Bearer SENTINEL\" }\nmodel = \"m\"\nmodels = [\"m\"]\n",
            "provider = \"x\"\n[providers.x]\nwire_api = \"responses\"\nbase_url = \"https://SENTINEL@example.test/v1\"\nhttp_headers = { Authorization = \"Bearer key\" }\nmodel = \"m\"\nmodels = [\"m\"]\n",
        ] {
            let error = resolve(Some(raw), &env(&[])).err().unwrap().to_string();
            assert!(!error.contains("SENTINEL"), "secret reflected: {error}");
        }
    }

    /// `effort` is a per-provider default on every rail now (not responses
    /// only), and `KLOOP_EFFORT` overrides it for the *selected* provider only.
    /// There is no scope above the profile: effort belongs to the provider that
    /// has to send it.
    #[test]
    fn effort_resolves_env_over_profile_for_the_selected_provider() {
        const WITH_EFFORT: &str = r#"
provider = "anthropic-a"

[providers.anthropic-a]
wire_api = "messages"
http_headers = { x-api-key = "a-key" }
model = "claude-a"
models = ["claude-a"]
effort = "low"

[providers.responses-b]
wire_api = "responses"
http_headers = { Authorization = "Bearer b-key" }
model = "gpt-a"
models = ["gpt-a"]
effort = "none"
"#;
        // Every profile keeps its own; nothing above it can retarget them.
        let configured = resolve(Some(WITH_EFFORT), &env(&[])).unwrap();
        assert_eq!(
            configured.catalog().default_effort("anthropic-a"),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(
            configured.catalog().default_effort("responses-b"),
            Some(ReasoningEffort::None)
        );

        // The environment beats the selected profile, and follows the selection.
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
provider = "responses-b"

[providers.responses-b]
wire_api = "responses"
http_headers = { Authorization = "Bearer b-key" }
model = "gpt-a"
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
provider = "anthropic-a"

[providers.anthropic-a]
wire_api = "messages"
http_headers = { x-api-key = "a-key" }
model = "claude-a"
models = ["claude-a"]
effort = "sky-high"
"#;
        assert_eq!(
            resolve(Some(TYPO), &env(&[]))
                .map(|_| ())
                .unwrap_err()
                .to_string(),
            "providers.anthropic-a.effort: unknown effort 'sky-high' \
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
