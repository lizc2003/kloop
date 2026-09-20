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
use kloop_core::provider_route::ModelKnowledge;
use kloop_core::provider_route::ProviderCatalog;
use kloop_core::provider_route::ProviderCatalogEntry;
use kloop_protocol::ANTHROPIC_MIN_THINKING_BUDGET;
use kloop_protocol::ProviderApiFamily;
use kloop_protocol::ProviderAvailabilityCode;
use kloop_protocol::ReasoningEffort;
use kloop_provider::AuthScheme;
use kloop_provider::Credential;
use kloop_provider::Provider;

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

    /// What each wire's own vendor sends, used when a profile declares no
    /// `auth_header` and the credential arrives from the environment instead.
    /// A profile that does declare one outranks this: the spelling belongs to
    /// the endpoint, not to the rail and not to where the secret came from.
    fn default_auth(self) -> AuthScheme {
        match self {
            Self::Anthropic => AuthScheme::ApiKey,
            Self::OpenAiChat | Self::OpenAiResponses => AuthScheme::Bearer,
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
    /// The one credential this profile declared, with the header spelling that
    /// carries it. `None` is a profile whose secret can only come from the
    /// environment — it stays visible but bounded-unavailable until it does.
    auth: Option<Credential>,
    model: String,
    models: Vec<String>,
    prompt_cache: bool,
    /// Whether this endpoint takes the `thinking` request field. A statement
    /// about the gateway, not about reasoning — `effort` is the only knob for
    /// that — and the sibling of `prompt_cache`.
    thinking_param: bool,
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
    /// The `[models."<id>"]` tables: what is true about a model, as opposed to
    /// what the user picked. Keyed by model id and shared across providers.
    model_knowledge: BTreeMap<String, ModelKnowledge>,
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
        let prompt_cache = profile.prompt_cache;
        let sends_thinking = profile.thinking_param;
        let default_effort = selected_effort(&profile, selected, env)?;
        let factory = Arc::new(move || {
            let cred = credential
                .clone()
                .ok_or(ProviderAvailabilityCode::MissingCredential)?;
            Ok(match wire {
                Rail::Anthropic => Provider::Anthropic {
                    cred,
                    base: base.clone(),
                    prompt_cache,
                },
                Rail::OpenAiChat => Provider::OpenAiCompat {
                    cred,
                    base: base.clone(),
                },
                Rail::OpenAiResponses => Provider::OpenAiResponses {
                    cred,
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
            context_window: profile.context_window,
            availability,
            default_effort,
            sends_thinking,
            factory,
        });
    }
    let catalog = Arc::new(
        ProviderCatalog::new(entries)
            .map_err(anyhow::Error::msg)?
            .with_model_knowledge(file.model_knowledge),
    );
    // A declared effort set is a claim the user made after testing; contradicting
    // it is a config mistake, and finding it at startup beats finding it in a 400
    // halfway through the first turn.
    if catalog.default_effort(&initial_provider) == Some(ReasoningEffort::None)
        && !catalog.sends_thinking(&initial_provider)
    {
        bail!(
            "provider '{initial_provider}' omits the thinking request field \
             (thinking_param = false), so effort = 'none' cannot be expressed there"
        );
    }
    if let Some(effort) = catalog.default_effort(&initial_provider)
        && !catalog.effort_supported(&initial_model, effort)
    {
        let declared = catalog
            .declared_efforts(&initial_model)
            .unwrap_or_default()
            .iter()
            .map(|level| level.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "effort '{}' is not among the efforts declared for model '{initial_model}' \
             (declared: {declared})",
            effort.as_str()
        );
    }
    let initial_context_window = catalog.effective_window(&initial_provider, &initial_model);
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
    let profile = Profile {
        wire,
        base_url,
        auth: Some(Credential::new(wire.default_auth(), key)),
        model: model.clone(),
        models: vec![model],
        prompt_cache: wire == Rail::Anthropic,
        thinking_param: true,
        // `KLOOP_EFFORT` is applied by `selected_effort` for the selected
        // provider — which, here, is the only one.
        effort: None,
        // No config file to declare it in; the global default applies.
        context_window: None,
    };
    Ok(GlobalFile {
        initial_provider: id.clone(),
        profiles: BTreeMap::from([(id, profile)]),
        model_knowledge: BTreeMap::new(),
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

/// The env var replaces the secret, never the presentation: which header a
/// gateway wants is a property of the gateway, so a profile that declared
/// `auth_header` keeps its spelling even when the secret arrives from the
/// environment. Only a profile that declared nothing falls back to the rail's.
fn selected_credential(
    profile: &Profile,
    selected: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<Credential>> {
    let from_env = if selected {
        match profile.wire {
            Rail::Anthropic => nonempty_env(env, "ANTHROPIC_API_KEY")?,
            Rail::OpenAiChat | Rail::OpenAiResponses => nonempty_env(env, "OPENAI_API_KEY")?,
        }
    } else {
        None
    };
    let scheme = profile
        .auth
        .as_ref()
        .map_or_else(|| profile.wire.default_auth(), Credential::scheme);
    Ok(match from_env {
        Some(secret) => Some(Credential::new(scheme, secret)),
        None => profile.auth.clone(),
    })
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
        model_knowledge: parse_model_knowledge(table)?,
    })
}

/// The `[models."<id>"]` tables. Every field is optional: a model kloop has
/// never been told about simply has no entry, which is the normal case and not
/// an error — the conservative default window and an unrestricted effort set
/// apply, and the provider still gets to refuse.
fn parse_model_knowledge(table: &toml::Table) -> Result<BTreeMap<String, ModelKnowledge>> {
    let Some(models) = table.get("models") else {
        return Ok(BTreeMap::new());
    };
    let models = models.as_table().context("models must be a table")?;
    let mut knowledge = BTreeMap::new();
    for (id, value) in models {
        let id = nonempty(id, "models key")?;
        let spec = value
            .as_table()
            .with_context(|| format!("models.{id} must be a table"))?;
        for key in spec.keys() {
            if !matches!(
                key.as_str(),
                "context_window" | "efforts" | "thinking_budget"
            ) {
                bail!("models.{id} has unknown key '{key}'");
            }
        }
        let context_window = optional_integer(
            spec,
            "context_window",
            &format!("models.{id}.context_window"),
        )?;
        let thinking_budgets = parse_thinking_budget(spec, &id)?;
        if thinking_budgets.is_some() && spec.contains_key("efforts") {
            bail!(
                "models.{id} declares both efforts and thinking_budget; \
                 the budget table's keys are the accepted efforts"
            );
        }
        // The keys of a budget table are that model's accepted levels, so the
        // existing effort gate needs no second source to consult — and a model
        // that takes no effort field at all finally has a way to say so.
        let efforts = match &thinking_budgets {
            Some(budgets) => Some(budgets.keys().copied().collect()),
            None => parse_efforts(spec, &id)?,
        };
        knowledge.insert(
            id.clone(),
            ModelKnowledge {
                context_window,
                efforts,
                thinking_budgets,
            },
        );
    }
    Ok(knowledge)
}

/// The budget dialect: models whose only reasoning dial is `budget_tokens`
/// (Haiku 4.5 and older — `output_config.effort` is an error on exactly those).
/// One budget per effort level keeps `/effort` the single knob; kloop renders it
/// into whichever field the model actually reads.
///
/// The API floor is 1024, and the ceiling takes care of itself: a budget raises
/// `max_tokens` by its own size, so it can never exceed it.
fn parse_thinking_budget(
    spec: &toml::Table,
    id: &str,
) -> Result<Option<BTreeMap<ReasoningEffort, u64>>> {
    let Some(value) = spec.get("thinking_budget") else {
        return Ok(None);
    };
    let field = format!("models.{id}.thinking_budget");
    let table = value
        .as_table()
        .with_context(|| format!("{field} must be a table of effort = token budget"))?;
    let mut budgets = BTreeMap::new();
    for (level, budget) in table {
        let effort = level
            .parse::<ReasoningEffort>()
            .map_err(|e| anyhow!("{field}: {e}"))?;
        // `none` is "do no reasoning", which is a disabled thinking field on
        // every model — a budget for it would be a contradiction.
        if effort == ReasoningEffort::None {
            bail!("{field} must not give 'none' a budget (it means no reasoning at all)");
        }
        let budget = budget
            .as_integer()
            .and_then(|raw| u64::try_from(raw).ok())
            .with_context(|| format!("{field}.{level} must be a token count"))?;
        if budget < ANTHROPIC_MIN_THINKING_BUDGET {
            bail!("{field}.{level} must be at least {ANTHROPIC_MIN_THINKING_BUDGET} tokens");
        }
        budgets.insert(effort, budget);
    }
    if budgets.is_empty() {
        bail!("{field} must name at least one effort level");
    }
    Ok(Some(budgets))
}

/// `None` is "not declared" and means every level is allowed. An empty array is
/// rejected rather than read as "supports nothing": it is far more likely a slip,
/// and a model that should do no reasoning is spelled `["none"]`.
fn parse_efforts(spec: &toml::Table, id: &str) -> Result<Option<Vec<ReasoningEffort>>> {
    let Some(value) = spec.get("efforts") else {
        return Ok(None);
    };
    let items = value
        .as_array()
        .with_context(|| format!("models.{id}.efforts must be an array of effort levels"))?;
    if items.is_empty() {
        bail!(
            "models.{id}.efforts must not be empty (omit the key to declare nothing, \
             or write [\"none\"] for a model that does no reasoning)"
        );
    }
    let mut efforts = Vec::new();
    for item in items {
        let raw = item
            .as_str()
            .with_context(|| format!("models.{id}.efforts must contain only strings"))?;
        let effort = raw
            .parse::<ReasoningEffort>()
            .map_err(|e| anyhow!("models.{id}.efforts: {e}"))?;
        if !efforts.contains(&effort) {
            efforts.push(effort);
        }
    }
    Ok(Some(efforts))
}

fn parse_profile(id: &str, spec: &toml::Table) -> Result<Profile> {
    for key in spec.keys() {
        if !matches!(
            key.as_str(),
            "name"
                | "wire_api"
                | "base_url"
                | "auth_header"
                | "model"
                | "models"
                | "prompt_cache"
                | "thinking_param"
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
    // Omitting `models` is the single-model case spelled once: the catalog is
    // just `model` itself. Writing it out is for providers that route to several.
    let models = match spec.get("models") {
        Some(_) => required_string_array(spec, "models", &format!("providers.{id}.models"))?,
        None => vec![model.clone()],
    };
    if !models.iter().any(|candidate| candidate == &model) {
        bail!("providers.{id}.model '{model}' is not in models allowlist");
    }
    let base_url = optional_string(spec, "base_url", &format!("providers.{id}.base_url"))?
        .unwrap_or_else(|| wire.default_base().to_string());
    let base_url = validate_base_url(&base_url, &format!("providers.{id}.base_url"))?;
    let prompt_cache = optional_bool(
        spec,
        "prompt_cache",
        &format!("providers.{id}.prompt_cache"),
    )?
    .unwrap_or(wire == Rail::Anthropic);
    let effort = optional_string(spec, "effort", &format!("providers.{id}.effort"))?
        .map(|raw| {
            raw.parse::<ReasoningEffort>()
                .map_err(|e| anyhow!("providers.{id}.effort: {e}"))
        })
        .transpose()?;
    let thinking_param = optional_bool(
        spec,
        "thinking_param",
        &format!("providers.{id}.thinking_param"),
    )?
    .unwrap_or(true);
    if wire != Rail::Anthropic
        && (spec.contains_key("prompt_cache") || spec.contains_key("thinking_param"))
    {
        bail!("providers.{id}: prompt_cache/thinking_param are only valid for messages wire_api");
    }
    let context_window = optional_integer(
        spec,
        "context_window",
        &format!("providers.{id}.context_window"),
    )?;
    let auth = parse_auth_header(id, spec.get("auth_header"))?;
    Ok(Profile {
        wire,
        base_url,
        auth,
        model,
        models,
        prompt_cache,
        thinking_param,
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

/// `auth_header` is one credential written the way the endpoint wants it, not a
/// bag of request headers: kloop sends exactly the spelling declared here, and
/// nothing else. The set stays closed at the two spellings the wires actually
/// use, because everything downstream depends on there being exactly one known
/// secret — `redact_secret` strips it out of provider-authored text, and
/// availability is "did this profile get a credential at all". Which spelling
/// belongs to which rail is the gateway's business, not ours, so both are legal
/// on all three.
///
/// Widening this later means adding a spelling *and* the rule for finding the
/// bare secret inside its value, the way Bearer's prefix is stripped here.
fn parse_auth_header(id: &str, value: Option<&Value>) -> Result<Option<Credential>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let field = format!("providers.{id}.auth_header");
    let table = value
        .as_table()
        .with_context(|| format!("{field} must be a table"))?;
    let mut entries = table.iter();
    let (header, value) = entries
        .next()
        .with_context(|| format!("{field} must name one header"))?;
    if entries.next().is_some() {
        bail!("{field} holds exactly one header");
    }
    let raw = value
        .as_str()
        .with_context(|| format!("{field}.{header} must be a string"))?;
    let raw = nonempty(raw, &format!("{field}.{header}"))?;
    match header.to_ascii_lowercase().as_str() {
        "x-api-key" => Ok(Some(Credential::api_key(raw))),
        "authorization" => {
            // The stored secret is the bare token: the "Bearer " around it is
            // wire framing, and a redaction sentinel that carried the prefix
            // would miss the token echoed on its own.
            let token = bearer_from_header(&raw)
                .with_context(|| format!("{field}.{header} must use Bearer authentication"))?;
            Ok(Some(Credential::bearer(token)))
        }
        _ => bail!("{field} contains unsupported header '{header}' (x-api-key | Authorization)"),
    }
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
auth_header = { x-api-key = "a-key" }
model = "claude-a"
models = ["claude-a", "claude-b", "claude-a"]
prompt_cache = false
thinking_param = false

[providers.responses-b]
wire_api = "responses"
base_url = "https://responses-b.example/v1"
auth_header = { Authorization = "Bearer b-key" }
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
auth_header = { x-api-key = "a-key" }
model = "claude-a"
models = ["claude-a"]
context_window = 111000

[providers.responses-b]
wire_api = "responses"
base_url = "https://responses-b.example/v1"
auth_header = { Authorization = "Bearer b-key" }
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
auth_header = {{ Authorization = "Bearer b-key" }}
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
        let raw = CATALOG.replace("auth_header = { Authorization = \"Bearer b-key\" }", "");
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
auth_header = { Authorization = "Bearer key" }
"#;
        assert!(
            resolve(Some(retired), &env(&[]))
                .err()
                .unwrap()
                .to_string()
                .contains("unknown key 'default_model'")
        );

        let outside_allowlist = "provider = \"x\"\n[providers.x]\nwire_api = \"responses\"\n\
             auth_header = { Authorization = \"Bearer key\" }\n\
             model = \"missing\"\nmodels = [\"a\"]\n";
        assert!(resolve(Some(outside_allowlist), &env(&[])).is_err());
    }

    fn auth_of(raw: &str) -> Result<Option<Credential>> {
        let table: toml::Table = raw.parse().unwrap();
        parse_auth_header("x", table.get("auth_header"))
    }

    /// One credential, written the way the endpoint reads it. Both spellings are
    /// legal on every rail: which header a gateway wants is the gateway's
    /// business, and the Messages wire alone has both in the wild. The stored
    /// secret is always bare — `Bearer` is framing, and a redaction sentinel
    /// carrying the prefix would miss the token echoed on its own.
    #[test]
    fn auth_header_takes_either_spelling_on_any_rail() {
        assert_eq!(
            auth_of(r#"auth_header = { Authorization = "Bearer b-key" }"#).unwrap(),
            Some(Credential::bearer("b-key"))
        );
        assert_eq!(
            auth_of(r#"auth_header = { x-api-key = "a-key" }"#).unwrap(),
            Some(Credential::api_key("a-key"))
        );
        // Spelling is case-insensitive the way headers are.
        assert_eq!(
            auth_of(r#"auth_header = { X-Api-Key = "a-key" }"#).unwrap(),
            Some(Credential::api_key("a-key"))
        );
        // Declaring none is a profile whose secret can only come from the env.
        assert_eq!(auth_of("model = \"m\"").unwrap(), None);

        // The rail does not get a veto: the user's own gateway speaks Messages
        // and authenticates with Bearer.
        let gateway = "provider = \"x\"\n[providers.x]\nwire_api = \"messages\"\n\
             auth_header = { Authorization = \"Bearer k\" }\nmodel = \"m\"\n";
        let resolved = resolve(Some(gateway), &env(&[])).unwrap();
        assert_eq!(
            resolved.catalog().descriptors()[0].availability,
            ProviderAvailabilityCode::Ready
        );
    }

    /// Everything downstream assumes exactly one known secret — `redact_secret`
    /// has to match it in provider-authored text, and availability is "did this
    /// profile get a credential at all" — so the set stays closed and anything
    /// outside it fails closed rather than being sent unredacted.
    #[test]
    fn auth_header_is_exactly_one_known_spelling() {
        let refused = |raw: &str| auth_of(raw).map(|_| ()).unwrap_err().to_string();

        assert_eq!(
            refused(r#"auth_header = { X-Tenant = "acme" }"#),
            "providers.x.auth_header contains unsupported header 'X-Tenant' \
             (x-api-key | Authorization)"
        );
        assert_eq!(
            refused(r#"auth_header = { x-api-key = "a", Authorization = "Bearer b" }"#),
            "providers.x.auth_header holds exactly one header"
        );
        assert_eq!(
            refused(r#"auth_header = { Authorization = "a-key" }"#),
            "providers.x.auth_header.Authorization must use Bearer authentication"
        );
        assert_eq!(
            refused(r#"auth_header = { x-api-key = "" }"#),
            "providers.x.auth_header.x-api-key must not be empty"
        );
        assert_eq!(
            refused("auth_header = \"Bearer k\""),
            "providers.x.auth_header must be a table"
        );
        assert_eq!(
            refused("auth_header = {}"),
            "providers.x.auth_header must name one header"
        );

        // The old name is gone outright; no alias, just the closed key table.
        let retired = "provider = \"x\"\n[providers.x]\nwire_api = \"messages\"\n\
             http_headers = { x-api-key = \"k\" }\nmodel = \"m\"\n";
        assert_eq!(
            resolve(Some(retired), &env(&[]))
                .map(|_| ())
                .unwrap_err()
                .to_string(),
            "providers.x has unknown key 'http_headers'"
        );
    }

    /// An environment credential replaces the secret, never the presentation:
    /// the header a gateway reads is a property of the gateway, not of where the
    /// secret came from. A profile that declared nothing falls back to what the
    /// wire's own vendor sends.
    #[test]
    fn environment_credentials_replace_the_secret_not_the_spelling() {
        let profile = |wire: Rail, auth: Option<Credential>| Profile {
            wire,
            base_url: "https://gateway.test".into(),
            auth,
            model: "m".into(),
            models: vec!["m".into()],
            prompt_cache: false,
            thinking_param: true,
            effort: None,
            context_window: None,
        };
        let from_env = |profile: &Profile, selected: bool, var: &str| {
            selected_credential(profile, selected, &env(&[(var, "env-key")])).unwrap()
        };

        let declared = profile(Rail::Anthropic, Some(Credential::bearer("file-key")));
        assert_eq!(
            from_env(&declared, /*selected=*/ true, "ANTHROPIC_API_KEY"),
            Some(Credential::bearer("env-key"))
        );
        // Unselected profiles are never retargeted by the environment.
        assert_eq!(
            from_env(&declared, /*selected=*/ false, "ANTHROPIC_API_KEY"),
            Some(Credential::bearer("file-key"))
        );

        let undeclared = profile(Rail::Anthropic, None);
        assert_eq!(
            from_env(&undeclared, /*selected=*/ true, "ANTHROPIC_API_KEY"),
            Some(Credential::api_key("env-key"))
        );
        let chat = profile(Rail::OpenAiChat, None);
        assert_eq!(
            from_env(&chat, /*selected=*/ true, "OPENAI_API_KEY"),
            Some(Credential::bearer("env-key"))
        );
    }

    fn knowledge(raw: &str) -> Result<BTreeMap<String, ModelKnowledge>> {
        parse_model_knowledge(&raw.parse::<toml::Table>().unwrap())
    }

    /// Effort is the only reasoning knob the user turns. Models whose sole dial
    /// is a token budget declare one per level, and those keys *are* the levels
    /// they accept — which is how "this model reads no effort field at all"
    /// finally becomes expressible (an empty `efforts` array is refused).
    #[test]
    fn a_budget_table_declares_both_the_budgets_and_the_accepted_efforts() {
        let parsed = knowledge(
            "[models.\"claude-haiku-4-5\"]\n\
             thinking_budget = { low = 2048, high = 16384 }\n",
        )
        .unwrap();
        assert_eq!(
            parsed,
            BTreeMap::from([(
                "claude-haiku-4-5".to_string(),
                ModelKnowledge {
                    context_window: None,
                    efforts: Some(vec![ReasoningEffort::Low, ReasoningEffort::High]),
                    thinking_budgets: Some(BTreeMap::from([
                        (ReasoningEffort::Low, 2048),
                        (ReasoningEffort::High, 16384),
                    ])),
                },
            )])
        );
    }

    #[test]
    fn a_budget_table_fails_closed_on_contradictions_and_unusable_budgets() {
        let refused = |raw: &str| knowledge(raw).map(|_| ()).unwrap_err().to_string();
        let model = |body: &str| format!("[models.\"m\"]\n{body}\n");

        assert_eq!(
            refused(&model(
                "thinking_budget = { low = 2048 }\nefforts = [\"low\"]"
            )),
            "models.m declares both efforts and thinking_budget; \
             the budget table's keys are the accepted efforts"
        );
        assert_eq!(
            refused(&model("thinking_budget = { low = 512 }")),
            "models.m.thinking_budget.low must be at least 1024 tokens"
        );
        assert_eq!(
            refused(&model("thinking_budget = { none = 2048 }")),
            "models.m.thinking_budget must not give 'none' a budget \
             (it means no reasoning at all)"
        );
        assert_eq!(
            refused(&model("thinking_budget = {}")),
            "models.m.thinking_budget must name at least one effort level"
        );
        assert!(refused(&model("thinking_budget = { hgih = 2048 }")).contains("thinking_budget"));
    }

    /// Thinking is on by default on this rail. Omitting the field is not neutral
    /// — Opus 4.8/4.7/4.6 and Sonnet 4.6 read a missing `thinking` as "do not
    /// think", so a silent default would run them with no reasoning while still
    /// paying for a configured effort. `thinking_param = false` is the way out
    /// for a gateway that cannot take the field; it is a statement about the
    /// gateway, not a second reasoning dial, and the old `thinking` enum that
    /// overlapped `effort` is gone.
    #[test]
    fn the_messages_rail_thinks_unless_told_otherwise() {
        let profile = |line: &str| {
            let raw = format!(
                "provider = \"x\"\n[providers.x]\nwire_api = \"messages\"\n\
                 auth_header = {{ x-api-key = \"k\" }}\nmodel = \"m\"\n{line}\n"
            );
            raw.parse::<toml::Table>()
                .unwrap()
                .get("providers")
                .and_then(Value::as_table)
                .and_then(|providers| providers.get("x"))
                .and_then(Value::as_table)
                .map(|spec| parse_profile("x", spec))
                .unwrap()
        };

        assert!(profile("").unwrap().thinking_param);
        assert!(!profile("thinking_param = false").unwrap().thinking_param);
        // The retired enum overlapped `effort`: "off" was `effort = "none"` by
        // another name, "adaptive" was the default spelled out, and the pair
        // could be configured into a request that disabled thinking and asked
        // for xhigh at once.
        assert_eq!(
            profile("thinking = \"adaptive\"")
                .map(|_| ())
                .unwrap_err()
                .to_string(),
            "providers.x has unknown key 'thinking'"
        );
        // The other rails have no such field, and no default to pick either.
        let chat = "provider = \"x\"\n[providers.x]\nwire_api = \"chat\"\n\
             auth_header = { Authorization = \"Bearer k\" }\nmodel = \"m\"\n";
        let resolved = resolve(Some(chat), &env(&[])).unwrap();
        assert_eq!(
            resolved.catalog().descriptors()[0].api_family,
            ProviderApiFamily::OpenAiChatCompletions
        );
    }

    /// "Do no reasoning" is spelled with the very field this gateway cannot
    /// take, so the two cannot both be true. Refused at startup rather than
    /// rendered into a request that quietly asks for nothing.
    #[test]
    fn none_is_refused_where_the_thinking_field_cannot_be_sent() {
        let raw = |line: &str| {
            format!(
                "provider = \"x\"\n[providers.x]\nwire_api = \"messages\"\n\
                 auth_header = {{ x-api-key = \"k\" }}\nmodel = \"m\"\n{line}\n"
            )
        };
        assert_eq!(
            resolve(
                Some(&raw("thinking_param = false\neffort = \"none\"")),
                &env(&[])
            )
            .map(|_| ())
            .unwrap_err()
            .to_string(),
            "provider 'x' omits the thinking request field (thinking_param = false), \
             so effort = 'none' cannot be expressed there"
        );
        // Any other level is fine: it rides output_config, which this gateway
        // does take.
        assert!(
            resolve(
                Some(&raw("thinking_param = false\neffort = \"high\"")),
                &env(&[])
            )
            .is_ok()
        );
        // And `none` is fine on a gateway that does take the field.
        assert!(resolve(Some(&raw("effort = \"none\"")), &env(&[])).is_ok());
    }

    /// `wire_api` names the endpoint, not the vendor — the axis
    /// `ProviderApiFamily` uses. An unknown wire fails closed, and the refusal
    /// names every legal spelling rather than just rejecting the bad one.
    #[test]
    fn wire_api_names_the_endpoint_and_an_unknown_one_fails_closed() {
        let profile = |wire: &str| {
            format!(
                "provider = \"x\"\n[providers.x]\nwire_api = \"{wire}\"\n\
                 auth_header = {{ x-api-key = \"k\" }}\nmodel = \"m\"\nmodels = [\"m\"]\n"
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

    fn with_knowledge(gateway: &str, models: &str) -> String {
        format!(
            "provider = \"g\"\n[providers.g]\nwire_api = \"responses\"\n\
             auth_header = {{ Authorization = \"Bearer k\" }}\n\
             model = \"m\"\n{gateway}\n{models}"
        )
    }

    /// The budget is the smaller of what the model can take and what the gateway
    /// will give. The conservative default is deliberately NOT a third number in
    /// that `min`: if it were, declaring a real 1M window would still compact at
    /// 200k and the declaration would be silently pointless.
    #[test]
    fn the_effective_window_is_the_smaller_of_the_model_and_the_gateway() {
        let window = |gateway: &str, models: &str| {
            resolve(Some(&with_knowledge(gateway, models)), &env(&[]))
                .unwrap()
                .initial_context_window()
        };
        let declared = "[models.m]\ncontext_window = 400000\n";

        assert_eq!(window("context_window = 258400", declared), Some(258_400));
        assert_eq!(window("", declared), Some(400_000));
        assert_eq!(window("context_window = 258400", ""), Some(258_400));
        // Neither declared: the caller falls back on its own, so this stays None.
        assert_eq!(window("", ""), None);
    }

    /// `efforts` is a claim the user made after testing, so it is checked at
    /// startup rather than at the first 400. Declaring nothing means no limit;
    /// declaring a list means exactly that list, `none` included — on the OpenAI
    /// rails it is an ordinary wire value a model can reject.
    #[test]
    fn declared_efforts_gate_the_configured_effort_at_startup() {
        let with_effort = |effort: &str, models: &str| {
            let raw = format!(
                "provider = \"g\"\n[providers.g]\nwire_api = \"responses\"\n\
                 auth_header = {{ Authorization = \"Bearer k\" }}\n\
                 model = \"m\"\neffort = \"{effort}\"\n{models}"
            );
            resolve(Some(&raw), &env(&[])).map(|_| ())
        };
        let narrow = "[models.m]\nefforts = [\"low\", \"high\"]\n";

        assert_eq!(
            with_effort("max", narrow).unwrap_err().to_string(),
            "effort 'max' is not among the efforts declared for model 'm' (declared: low, high)"
        );
        assert!(with_effort("high", narrow).is_ok());
        // Undeclared model: nothing to check against, the provider still refuses.
        assert!(with_effort("max", "").is_ok());
        // `none` gets no exemption: a list that leaves it out leaves it out.
        assert_eq!(
            with_effort("none", narrow).unwrap_err().to_string(),
            "effort 'none' is not among the efforts declared for model 'm' (declared: low, high)"
        );
        assert!(with_effort("none", "[models.m]\nefforts = [\"none\", \"low\"]\n").is_ok());
    }

    #[test]
    fn model_knowledge_fails_closed_on_empty_and_malformed_declarations() {
        let error = |models: &str| {
            resolve(Some(&with_knowledge("", models)), &env(&[]))
                .map(|_| ())
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            error("[models.m]\nefforts = []\n"),
            "models.m.efforts must not be empty (omit the key to declare nothing, \
             or write [\"none\"] for a model that does no reasoning)"
        );
        assert_eq!(
            error("[models.m]\nefforts = [\"sky-high\"]\n"),
            "models.m.efforts: unknown effort 'sky-high' \
             (known: none, low, medium, high, xhigh, max)"
        );
        assert_eq!(
            error("[models.m]\nwindow = 1\n"),
            "models.m has unknown key 'window'"
        );
        assert!(error("[models.m]\ncontext_window = 0\n").contains("positive token count"));
    }

    /// Real model ids contain dots (`gpt-5.6-sol`, `gpt-4.1`), so the table key
    /// has to be quoted — bare `[models.gpt-5.6-sol]` is a nested table, not a
    /// model named "gpt-5.6-sol". Quoted, it matches the model exactly; bare, it
    /// fails closed rather than silently applying to nothing.
    #[test]
    fn dotted_model_ids_match_when_the_key_is_quoted() {
        let with_id = |key: &str| {
            format!(
                "provider = \"g\"\n[providers.g]\nwire_api = \"responses\"\n\
                 auth_header = {{ Authorization = \"Bearer k\" }}\n\
                 model = \"gpt-5.6-sol\"\n[models.{key}]\ncontext_window = 400000\n"
            )
        };
        assert_eq!(
            resolve(Some(&with_id("\"gpt-5.6-sol\"")), &env(&[]))
                .unwrap()
                .initial_context_window(),
            Some(400_000)
        );
        assert_eq!(
            resolve(Some(&with_id("gpt-5.6-sol")), &env(&[]))
                .map(|_| ())
                .unwrap_err()
                .to_string(),
            "models.gpt-5 has unknown key '6-sol'"
        );
    }

    #[test]
    fn errors_never_echo_credentials() {
        for raw in [
            "secret = \"SENTINEL\"\n",
            "provider = \"x\"\n[providers.x]\nwire_api = \"bad\"\nauth_header = { Authorization = \"Bearer SENTINEL\" }\nmodel = \"m\"\nmodels = [\"m\"]\n",
            "provider = \"x\"\n[providers.x]\nwire_api = \"responses\"\nbase_url = \"https://SENTINEL@example.test/v1\"\nauth_header = { Authorization = \"Bearer key\" }\nmodel = \"m\"\nmodels = [\"m\"]\n",
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
auth_header = { x-api-key = "a-key" }
model = "claude-a"
models = ["claude-a"]
effort = "low"

[providers.responses-b]
wire_api = "responses"
auth_header = { Authorization = "Bearer b-key" }
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
auth_header = { Authorization = "Bearer b-key" }
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
auth_header = { x-api-key = "a-key" }
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
