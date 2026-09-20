use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use kloop_protocol::ActiveProviderRoute;
use kloop_protocol::ProviderApiFamily;
use kloop_protocol::ProviderAttemptIdentity;
use kloop_protocol::ProviderAvailabilityCode;
use kloop_protocol::ProviderDescriptor;
use kloop_protocol::ProviderResponseProvenance;
use kloop_protocol::ReasoningContinuity;
use kloop_protocol::ReasoningEffort;
use kloop_protocol::RoutePickerStage;
use kloop_provider::Provider;
use kloop_provider::Reasoning;
use kloop_provider::ThinkingMode;

pub type ProviderFactory =
    Arc<dyn Fn() -> Result<Provider, ProviderAvailabilityCode> + Send + Sync + 'static>;

pub struct ProviderCatalogEntry {
    pub id: String,
    pub api_family: ProviderApiFamily,
    pub endpoint_fingerprint: String,
    pub default_model: String,
    pub models: Vec<String>,
    /// What this gateway caps the context at, when it caps it lower than the
    /// model itself. Only the gateway operator knows this — it is configuration,
    /// not a fact about the model.
    pub context_window: Option<u64>,
    pub availability: ProviderAvailabilityCode,
    /// The configured effort this provider starts a session at. It seeds
    /// [`SessionProviderState`]; `/effort` then owns the value for the rest of
    /// the session (the provider itself bakes in nothing).
    pub default_effort: Option<ReasoningEffort>,
    /// Whether this endpoint can take the `thinking` request field at all. Not a
    /// reasoning setting — `effort` is the only one of those — but a statement
    /// about the gateway, the sibling of `prompt_cache`. False omits the field
    /// entirely, which also makes "do no reasoning" inexpressible here.
    pub sends_thinking: bool,
    pub factory: ProviderFactory,
}

struct CatalogEntry {
    descriptor: ProviderDescriptor,
    endpoint_fingerprint: String,
    context_window: Option<u64>,
    default_effort: Option<ReasoningEffort>,
    sends_thinking: bool,
    factory: ProviderFactory,
    provider: OnceLock<Result<Arc<Provider>, ProviderAvailabilityCode>>,
}

/// What is true about a model rather than chosen by the user: how much context
/// it takes and which reasoning levels it accepts. It is knowledge, written once
/// and then left alone, which is why it lives in its own `[models."<id>"]` table
/// instead of being repeated inside every provider that can route to the model.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelKnowledge {
    pub context_window: Option<u64>,
    /// Absent means "not declared", which is not the same as "supports nothing":
    /// an undeclared model accepts every level and lets the provider refuse.
    pub efforts: Option<Vec<ReasoningEffort>>,
    /// Models whose only reasoning dial is a token budget (Haiku 4.5 and older:
    /// `output_config.effort` is an error there, and thinking happens only with
    /// an explicit `budget_tokens`). One budget per effort level, so `/effort`
    /// stays the single knob and renders into whichever field the model reads.
    /// Declaring it also declares the accepted levels — its keys are the answer
    /// [`ProviderCatalog::declared_efforts`] gives, which is how a model that
    /// takes no effort field at all becomes expressible.
    pub thinking_budgets: Option<BTreeMap<ReasoningEffort, u64>>,
}

pub struct ProviderCatalog {
    entries: BTreeMap<String, CatalogEntry>,
    model_knowledge: BTreeMap<String, ModelKnowledge>,
}

impl fmt::Debug for ProviderCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCatalog")
            .field("descriptors", &self.descriptors())
            .finish()
    }
}

impl ProviderCatalog {
    pub fn new(entries: Vec<ProviderCatalogEntry>) -> Result<Self, String> {
        if entries.is_empty() {
            return Err("provider catalog must contain at least one provider".into());
        }
        let mut catalog = BTreeMap::new();
        for entry in entries {
            let id = checked_nonempty(&entry.id, "provider id")?;
            let default_model = checked_nonempty(&entry.default_model, "default model")?;
            let endpoint_fingerprint =
                checked_nonempty(&entry.endpoint_fingerprint, "endpoint fingerprint")?;
            let mut models = Vec::new();
            for model in entry.models {
                let model = checked_nonempty(&model, "provider model")?;
                if !models.contains(&model) {
                    models.push(model);
                }
            }
            if models.is_empty() {
                return Err(format!("provider '{id}' must declare at least one model"));
            }
            if !models.contains(&default_model) {
                return Err(format!(
                    "provider '{id}' default model '{default_model}' is not in its models allowlist"
                ));
            }
            let descriptor = ProviderDescriptor {
                id: id.clone(),
                api_family: entry.api_family,
                default_model,
                models,
                availability: entry.availability,
                default_effort: entry.default_effort,
            };
            if catalog
                .insert(
                    id.clone(),
                    CatalogEntry {
                        descriptor,
                        endpoint_fingerprint,
                        context_window: entry.context_window,
                        default_effort: entry.default_effort,
                        sends_thinking: entry.sends_thinking,
                        factory: entry.factory,
                        provider: OnceLock::new(),
                    },
                )
                .is_some()
            {
                return Err(format!("duplicate provider id '{id}'"));
            }
        }
        Ok(Self {
            entries: catalog,
            model_knowledge: BTreeMap::new(),
        })
    }

    /// Attach the `[models."<id>"]` table. Separate from `new` so every existing
    /// construction site keeps working with an empty knowledge base — an unknown
    /// model is the normal case, not an error.
    pub fn with_model_knowledge(mut self, knowledge: BTreeMap<String, ModelKnowledge>) -> Self {
        self.model_knowledge = knowledge;
        self
    }

    /// The window to budget compaction against for one (provider, model) pair.
    /// The model says what it can take, the gateway says what it will give, and
    /// the smaller of the two wins. `None` means neither was declared, so the
    /// caller falls back to its own conservative default — the fallback must not
    /// take part in the `min`, or declaring a real 1M window would still compact
    /// at the default.
    pub fn effective_window(&self, provider_id: &str, model: &str) -> Option<u64> {
        let gateway = self
            .entries
            .get(provider_id)
            .and_then(|entry| entry.context_window);
        let declared = self
            .model_knowledge
            .get(model)
            .and_then(|knowledge| knowledge.context_window);
        match (declared, gateway) {
            (Some(model_window), Some(cap)) => Some(model_window.min(cap)),
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        }
    }

    /// Whether this model accepts that reasoning level. Declaring nothing means
    /// no limit — every level passes and the provider still gets to refuse.
    /// Declaring a list means exactly that list, **`none` included**: on the two
    /// OpenAI rails `none` is an ordinary wire value (`"effort": "none"`) that a
    /// model can reject like any other, so exempting it would wave through the
    /// one value the list was written to exclude. One rule, no exceptions —
    /// a model that should be allowed to stop reasoning lists `none`.
    ///
    /// `/effort unset` is a different thing and never reaches here: it sends no
    /// effort field at all, so there is nothing for a model to accept or reject.
    pub fn effort_supported(&self, model: &str, effort: ReasoningEffort) -> bool {
        match self
            .model_knowledge
            .get(model)
            .and_then(|knowledge| knowledge.efforts.as_deref())
        {
            Some(declared) => declared.contains(&effort),
            None => true,
        }
    }

    /// Whether this provider sends the `thinking` request field. A gateway that
    /// cannot take it also cannot be told to stop reasoning, since that is what
    /// the field says — so the effort gates consult this before accepting
    /// `none`. Unknown ids answer true: they fail later, by name.
    pub fn sends_thinking(&self, provider_id: &str) -> bool {
        self.entries
            .get(provider_id)
            .is_none_or(|entry| entry.sends_thinking)
    }

    pub fn declared_efforts(&self, model: &str) -> Option<&[ReasoningEffort]> {
        self.model_knowledge.get(model)?.efforts.as_deref()
    }

    /// Every declared effort list, by model. Only models that declared one
    /// appear — absence is what "no limit" looks like, so an empty entry would
    /// read as the opposite. Handed to a front-end whole, because the picker
    /// walks models the session has not switched to yet.
    pub fn declared_effort_table(&self) -> BTreeMap<String, Vec<ReasoningEffort>> {
        self.model_knowledge
            .iter()
            .filter_map(|(model, knowledge)| Some((model.clone(), knowledge.efforts.clone()?)))
            .collect()
    }

    pub fn from_provider(
        id: impl Into<String>,
        provider: Provider,
        default_model: impl Into<String>,
        models: Vec<String>,
    ) -> Result<(Arc<Self>, FrozenProviderRoute), String> {
        let id = id.into();
        let default_model = default_model.into();
        let api_family = provider.api_family();
        let endpoint_fingerprint = provider.endpoint_fingerprint();
        let provider = Mutex::new(Some(provider));
        let entry = ProviderCatalogEntry {
            id: id.clone(),
            api_family,
            endpoint_fingerprint,
            default_model: default_model.clone(),
            models,
            context_window: None,
            availability: ProviderAvailabilityCode::Ready,
            default_effort: None,
            sends_thinking: true,
            factory: Arc::new(move || {
                provider
                    .lock()
                    .unwrap()
                    .take()
                    .ok_or(ProviderAvailabilityCode::InvalidConfiguration)
            }),
        };
        let catalog = Arc::new(Self::new(vec![entry])?);
        let route = catalog
            .initial_route(&id, Some(&default_model))
            .map_err(|error| error.to_string())?;
        Ok((catalog, route))
    }

    fn validate_receipt(
        &self,
        receipt: &kloop_protocol::ProviderRouteReceipt,
    ) -> Result<(), SwitchError> {
        let entry = self
            .entries
            .get(&receipt.provider_id)
            .ok_or_else(|| SwitchError::UnknownProvider(receipt.provider_id.clone()))?;
        if entry.descriptor.api_family != receipt.api_family
            || entry.endpoint_fingerprint != receipt.endpoint_fingerprint
            || !entry
                .descriptor
                .models
                .iter()
                .any(|model| model == &receipt.primary_model)
        {
            return Err(SwitchError::RouteDrift(receipt.provider_id.clone()));
        }
        Ok(())
    }

    pub fn restore_route(
        self: &Arc<Self>,
        receipt: &kloop_protocol::ProviderRouteReceipt,
    ) -> Result<FrozenProviderRoute, SwitchError> {
        if receipt.revision == 0 {
            return Err(SwitchError::InvalidRevision);
        }
        self.validate_receipt(receipt)?;
        let resolved = self.resolve(&receipt.provider_id, &receipt.primary_model)?;
        // Effort is session-local and absent from the durable timeline, so a
        // restored route re-seeds it from configuration (plan 102).
        Ok(FrozenProviderRoute::with_continuity(
            receipt.revision,
            resolved,
            receipt.continuity,
            self.default_effort(&receipt.provider_id),
        ))
    }
    pub fn descriptors(&self) -> Vec<ProviderDescriptor> {
        self.entries
            .values()
            .map(|entry| entry.descriptor.clone())
            .collect()
    }

    pub fn descriptor(&self, id: &str) -> Option<ProviderDescriptor> {
        self.entries.get(id).map(|entry| entry.descriptor.clone())
    }

    /// The configured effort a session starts at on this provider. Unknown ids
    /// answer None — the callers below have already resolved the route.
    pub fn default_effort(&self, id: &str) -> Option<ReasoningEffort> {
        self.entries.get(id).and_then(|entry| entry.default_effort)
    }

    fn resolve(&self, provider_id: &str, model: &str) -> Result<ResolvedRoute, SwitchError> {
        let entry = self
            .entries
            .get(provider_id)
            .ok_or_else(|| SwitchError::UnknownProvider(provider_id.to_string()))?;
        if !entry
            .descriptor
            .models
            .iter()
            .any(|allowed| allowed == model)
        {
            return Err(SwitchError::UnknownModel {
                provider_id: provider_id.to_string(),
                model: model.to_string(),
            });
        }
        if entry.descriptor.availability != ProviderAvailabilityCode::Ready {
            return Err(SwitchError::Unavailable {
                provider_id: provider_id.to_string(),
                code: entry.descriptor.availability,
            });
        }
        let provider = entry
            .provider
            .get_or_init(|| (entry.factory)().map(Arc::new))
            .clone()
            .map_err(|code| SwitchError::Unavailable {
                provider_id: provider_id.to_string(),
                code,
            })?;
        Ok(ResolvedRoute {
            provider_id: provider_id.to_string(),
            api_family: entry.descriptor.api_family,
            endpoint_fingerprint: entry.endpoint_fingerprint.clone(),
            primary_model: model.to_string(),
            allowed_models: entry.descriptor.models.clone(),
            provider,
            thinking: ThinkingRouting {
                send_param: entry.sends_thinking,
                budgets: entry
                    .descriptor
                    .models
                    .iter()
                    .filter_map(|model| {
                        let budgets = self.model_knowledge.get(model)?.thinking_budgets.clone()?;
                        Some((model.clone(), budgets))
                    })
                    .collect(),
            },
        })
    }

    pub fn initial_route(
        self: &Arc<Self>,
        provider_id: &str,
        model: Option<&str>,
    ) -> Result<FrozenProviderRoute, SwitchError> {
        let descriptor = self
            .descriptor(provider_id)
            .ok_or_else(|| SwitchError::UnknownProvider(provider_id.to_string()))?;
        let model = model.unwrap_or(&descriptor.default_model);
        Ok(FrozenProviderRoute::new(
            1,
            self.resolve(provider_id, model)?,
            self.default_effort(provider_id),
        ))
    }
}

pub(crate) fn validate_timeline(
    timeline: &[kloop_protocol::ProviderRouteReceipt],
) -> Result<(), SwitchError> {
    let mut previous: Option<&kloop_protocol::ProviderRouteReceipt> = None;
    for receipt in timeline {
        if receipt.revision == 0
            || receipt.boundary == 0
            || receipt.provider_id.trim().is_empty()
            || receipt.endpoint_fingerprint.trim().is_empty()
            || receipt.primary_model.trim().is_empty()
        {
            return Err(SwitchError::InvalidTimeline);
        }
        match previous {
            None if receipt.revision == 1
                && receipt.source == kloop_protocol::ProviderRouteSource::Initial => {}
            Some(previous)
                if matches!(
                    receipt.source,
                    kloop_protocol::ProviderRouteSource::ExplicitSwitch
                        | kloop_protocol::ProviderRouteSource::Reopened
                ) && previous
                    .revision
                    .checked_add(1)
                    .is_some_and(|revision| revision == receipt.revision)
                    && receipt.boundary > previous.boundary => {}
            _ => return Err(SwitchError::InvalidTimeline),
        }
        previous = Some(receipt);
    }
    if previous.is_none() {
        return Err(SwitchError::InvalidTimeline);
    }
    Ok(())
}

/// What reasoning a message carries, which decides whether its block shape has
/// to agree with the API family that produced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningShape {
    /// No reasoning blocks: there is no shape to disagree about.
    None,
    /// Reasoning, none of it redacted.
    Plain,
    /// Reasoning including at least one redacted block.
    Redacted,
}

impl ReasoningShape {
    pub fn of(message: &kloop_protocol::Message) -> Self {
        if !message.has_reasoning() {
            Self::None
        } else if message
            .content
            .iter()
            .any(kloop_protocol::ContentBlock::has_redacted_reasoning)
        {
            Self::Redacted
        } else {
            Self::Plain
        }
    }
}

/// Why a recorded provenance does not line up with the route timeline it claims
/// to have come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProvenanceMismatch {
    UnknownRevision,
    OriginOutsideInterval,
    OriginNotOnItsLine,
    IdentityMismatch,
    BlockShapeMismatch,
}

/// Check one provider response provenance against the timeline that should
/// contain it; returns the index of the route it resolved to.
///
/// Two callers need exactly this, in exactly this order: the rollout validator
/// reading a session off disk, and the request-view projection deciding whether
/// reasoning may replay. Each used to spell the rules out for itself — same
/// conditions, same order, different wording — so a change to one side was
/// invisible to the other, while both decide whether opaque reasoning is safe to
/// send back. What stays theirs is the wording, and `origin_line_boundary`:
/// only the on-disk validator can say which line the provenance was read from,
/// and only there does a message have to sit at the boundary it names.
pub fn validate_provenance(
    source: &ProviderResponseProvenance,
    routes: &[kloop_protocol::ProviderRouteReceipt],
    reasoning: ReasoningShape,
    origin_line_boundary: Option<u64>,
) -> Result<usize, ProvenanceMismatch> {
    let index = routes
        .iter()
        .position(|route| route.revision == source.route_revision)
        .ok_or(ProvenanceMismatch::UnknownRevision)?;
    let route = &routes[index];
    let interval_end = routes.get(index + 1).map_or(u64::MAX, |next| next.boundary);
    if source.origin_boundary <= route.boundary || source.origin_boundary >= interval_end {
        return Err(ProvenanceMismatch::OriginOutsideInterval);
    }
    if origin_line_boundary.is_some_and(|boundary| source.origin_boundary != boundary) {
        return Err(ProvenanceMismatch::OriginNotOnItsLine);
    }
    let model_matches = source.model == route.primary_model;
    if source.provider_id != route.provider_id
        || source.api_family != route.api_family
        || source.endpoint_fingerprint != route.endpoint_fingerprint
        || !model_matches
    {
        return Err(ProvenanceMismatch::IdentityMismatch);
    }
    // Only Anthropic has redacted reasoning: Responses carries reasoning as an
    // encrypted blob, and Chat as signature-less thinking of its own —
    // DeepSeek- and GLM-shaped models stream `reasoning_content`, which the
    // chat adapter keeps for the transcript and this projection strips before
    // the request. Chat reasoning is therefore a real shape, not a corrupt one;
    // what Chat must not do is replay it.
    let shape_agrees = match reasoning {
        ReasoningShape::None | ReasoningShape::Plain => true,
        ReasoningShape::Redacted => !matches!(
            source.api_family,
            ProviderApiFamily::OpenAiChatCompletions | ProviderApiFamily::OpenAiResponses
        ),
    };
    if !shape_agrees {
        return Err(ProvenanceMismatch::BlockShapeMismatch);
    }
    Ok(index)
}

fn checked_nonempty(value: &str, field: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    Ok(value.to_string())
}

/// A child may select only a model declared by the frozen parent's provider.
/// The string form remains the configuration and frontmatter contract; this
/// wrapper makes that inherited-provider constraint explicit after parsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InheritedProviderModelOverride(String);

impl InheritedProviderModelOverride {
    pub fn parse(value: &str) -> Result<Self, SwitchError> {
        let model = value.trim();
        if model.is_empty() {
            return Err(SwitchError::InvalidInheritedProviderModel);
        }
        Ok(Self(model.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone)]
struct ResolvedRoute {
    provider_id: String,
    api_family: ProviderApiFamily,
    endpoint_fingerprint: String,
    primary_model: String,
    allowed_models: Vec<String>,
    provider: Arc<Provider>,
    /// Everything needed to turn (model, effort) into a `thinking` field without
    /// the catalog in hand: a frozen route outlives the lookup that made it, and
    /// `child_route` can mint an attempt for any allowed model.
    thinking: ThinkingRouting,
}

/// The `thinking` half of the reasoning knob, carried by a resolved route.
#[derive(Clone)]
struct ThinkingRouting {
    /// Whether the endpoint takes the field at all (`thinking_param`). False is
    /// a gateway's limitation, not a choice about reasoning, and it silences
    /// every mode below.
    send_param: bool,
    /// Per-model effort -> token budget, for the models that read no effort
    /// field. Only the route's allowed models appear.
    budgets: BTreeMap<String, BTreeMap<ReasoningEffort, u64>>,
}

impl ThinkingRouting {
    /// Effort is the only knob the user turns; this is where it lands on the
    /// wire for one model. `none` means "do not reason" on every model and so
    /// outranks both the budget table and the profile default.
    fn resolve(&self, model: &str, effort: Option<ReasoningEffort>) -> ThinkingMode {
        // A gateway that cannot take the field gets none of these. `none` never
        // reaches here on such a provider: both the startup check and `/effort`
        // refuse it, because "do no reasoning" is spelled with this very field.
        if !self.send_param {
            return ThinkingMode::Unset;
        }
        if effort == Some(ReasoningEffort::None) {
            return ThinkingMode::Off;
        }
        let Some(budgets) = self.budgets.get(model) else {
            return ThinkingMode::Adaptive;
        };
        // A budget-dialect model with no effort selected gets no thinking field,
        // which is exactly that model's own API default.
        match effort.and_then(|effort| budgets.get(&effort)) {
            Some(budget) => ThinkingMode::Budget(*budget),
            None => ThinkingMode::Unset,
        }
    }
}

struct SessionState {
    revision: u64,
    active: ResolvedRoute,
    continuity: ReasoningContinuity,
    remembered_models: BTreeMap<String, String>,
    /// Session-local reasoning effort (`/effort`). Sticky across provider
    /// switches unless the new rail refuses it — see [`SessionProviderState::switch_with`].
    effort: Option<ReasoningEffort>,
    /// Whether `effort` came from `/effort` rather than from configuration.
    /// An unpinned session follows each provider's configured default across
    /// switches; a pinned one keeps the user's choice wherever it is accepted.
    effort_pinned: bool,
}

pub struct SessionProviderState {
    catalog: Arc<ProviderCatalog>,
    state: Mutex<SessionState>,
}

impl fmt::Debug for SessionProviderState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionProviderState")
            .field("active", &self.active_route())
            .finish_non_exhaustive()
    }
}

impl SessionProviderState {
    pub fn new(
        catalog: Arc<ProviderCatalog>,
        provider_id: &str,
        model: Option<&str>,
    ) -> Result<Self, SwitchError> {
        let route = catalog.initial_route(provider_id, model)?;
        Ok(Self::from_route(catalog, route))
    }

    pub fn from_timeline(
        catalog: Arc<ProviderCatalog>,
        timeline: &[kloop_protocol::ProviderRouteReceipt],
    ) -> Result<Self, SwitchError> {
        validate_timeline(timeline)?;
        // Only the route the session continues on has to exist in today's
        // catalog. The earlier receipts record what a past turn actually ran on;
        // re-checking them against current configuration would make renaming a
        // provider retroactively invalidate every session that ever used it.
        let latest = timeline.last().ok_or(SwitchError::InvalidTimeline)?;
        let route = catalog.restore_route(latest)?;
        let remembered_models = timeline
            .iter()
            .map(|receipt| (receipt.provider_id.clone(), receipt.primary_model.clone()))
            .collect();
        Ok(Self {
            catalog,
            state: Mutex::new(SessionState {
                revision: route.revision,
                active: route.route,
                continuity: route.continuity,
                remembered_models,
                effort: route.effort,
                effort_pinned: false,
            }),
        })
    }
    pub fn from_route(catalog: Arc<ProviderCatalog>, route: FrozenProviderRoute) -> Self {
        let remembered_models = BTreeMap::from([(
            route.provider_id().to_string(),
            route.primary_model().to_string(),
        )]);
        Self {
            catalog,
            state: Mutex::new(SessionState {
                revision: route.revision,
                active: route.route,
                continuity: route.continuity,
                remembered_models,
                effort: route.effort,
                effort_pinned: false,
            }),
        }
    }

    pub fn catalog(&self) -> &Arc<ProviderCatalog> {
        &self.catalog
    }

    pub fn freeze(&self) -> FrozenProviderRoute {
        let state = self.state.lock().unwrap();
        FrozenProviderRoute::with_continuity(
            state.revision,
            state.active.clone(),
            state.continuity,
            state.effort,
        )
    }

    pub fn effort(&self) -> Option<ReasoningEffort> {
        self.state.lock().unwrap().effort
    }

    /// Set (or with `None`, clear) the session reasoning effort. Deliberately
    /// unvalidated beyond kloop's own vocabulary: which levels a model accepts
    /// is the model's contract, not the rail's, and the provider states it
    /// precisely in its own error. The next frozen route carries the value to
    /// every attempt, children included.
    pub fn set_effort(&self, effort: Option<ReasoningEffort>) {
        let mut state = self.state.lock().unwrap();
        state.effort = effort;
        state.effort_pinned = true;
    }

    pub fn active_route(&self) -> ActiveProviderRoute {
        self.freeze().public_route()
    }

    pub fn remembered_models(&self) -> BTreeMap<String, String> {
        self.state.lock().unwrap().remembered_models.clone()
    }

    /// Which model a switch to `provider_id` would land on: the one named, else
    /// the one this session last used there, else the provider's default. None
    /// when no such provider is configured. Callers that need the model *before*
    /// committing — the effort check is one, since the target model's declared
    /// list is the one that governs — ask here rather than re-deriving the rule.
    pub fn target_model(&self, provider_id: &str, model: Option<&str>) -> Option<String> {
        let state = self.state.lock().unwrap();
        let descriptor = self.catalog.descriptor(provider_id)?;
        Some(
            model
                .map(str::to_string)
                .or_else(|| state.remembered_models.get(provider_id).cloned())
                .unwrap_or(descriptor.default_model),
        )
    }

    pub fn preflight(&self, provider_id: &str, model: Option<&str>) -> Result<(), SwitchError> {
        let target_model = self
            .target_model(provider_id, model)
            .ok_or_else(|| SwitchError::UnknownProvider(provider_id.to_string()))?;
        self.catalog.resolve(provider_id, &target_model).map(|_| ())
    }

    /// The picker payload for one entry stage: the catalog, what every model
    /// declared, and where the session sits right now. Built in one shot because
    /// the picker walks providers and models the session is not on.
    pub fn route_picker(&self, stage: RoutePickerStage) -> RoutePicker {
        RoutePicker {
            stage,
            providers: self.catalog.descriptors(),
            declared_efforts: self.catalog.declared_effort_table(),
            active: self.active_route(),
        }
    }

    /// Move the session onto a route, with the reasoning effort decided in the
    /// same step. One call and therefore **one revision**: `/provider a m high`
    /// splits into a switch and an effort change only on a timeline, where it
    /// would read months later as a user who changed their mind twice.
    ///
    /// `commit` writes the receipt and may refuse; nothing lands unless it
    /// succeeds. It sees both routes, so it can tell an effort-only revision
    /// (same route) from a real switch and pick the continuity accordingly.
    pub fn switch_with<E>(
        &self,
        expected_revision: u64,
        provider_id: &str,
        model: Option<&str>,
        effort: EffortRequest,
        commit: impl FnOnce(
            &FrozenProviderRoute,
            &FrozenProviderRoute,
        ) -> Result<ReasoningContinuity, E>,
    ) -> Result<SwitchOutcome, SwitchCommitError<E>> {
        let target_model = self.target_model(provider_id, model).ok_or_else(|| {
            SwitchCommitError::Switch(SwitchError::UnknownProvider(provider_id.to_string()))
        })?;
        let target = self
            .catalog
            .resolve(provider_id, &target_model)
            .map_err(SwitchCommitError::Switch)?;

        let mut state = self.state.lock().unwrap();
        if state.revision != expected_revision {
            return Err(SwitchCommitError::Switch(SwitchError::StaleRevision {
                expected: expected_revision,
                actual: state.revision,
            }));
        }
        let same_route = state.active.provider_id == target.provider_id
            && state.active.primary_model == target.primary_model
            && state.active.api_family == target.api_family
            && state.active.endpoint_fingerprint == target.endpoint_fingerprint;
        // A named level wins outright. Otherwise: staying on the same route
        // changes nothing, and crossing to another provider keeps a pinned
        // choice (once `/effort` has spoken it travels with the session) or
        // follows the target's configured value. A model that refuses the level
        // says so on the next turn — kloop does not second-guess it here (see
        // `set_effort`).
        let next_effort = match effort {
            EffortRequest::Set(level) => level,
            EffortRequest::Inherit if same_route || state.effort_pinned => state.effort,
            EffortRequest::Inherit => self.catalog.default_effort(provider_id),
        };
        let pins = matches!(effort, EffortRequest::Set(_));
        if same_route && next_effort == state.effort {
            state.effort_pinned |= pins;
            return Ok(SwitchOutcome::NoOp(FrozenProviderRoute::with_continuity(
                state.revision,
                state.active.clone(),
                state.continuity,
                state.effort,
            )));
        }
        let Some(next_revision) = state.revision.checked_add(1) else {
            // Out of revisions is not a reason to lose the user's choice when
            // only the effort moves: apply it in memory, unrecorded. A real
            // switch cannot do that — the route requests go to has to be the
            // route the last receipt names.
            if same_route {
                state.effort = next_effort;
                state.effort_pinned |= pins;
                return Ok(SwitchOutcome::NoOp(FrozenProviderRoute::with_continuity(
                    state.revision,
                    state.active.clone(),
                    state.continuity,
                    state.effort,
                )));
            }
            return Err(SwitchCommitError::Switch(SwitchError::RevisionExhausted));
        };
        let previous = FrozenProviderRoute::with_continuity(
            state.revision,
            state.active.clone(),
            state.continuity,
            state.effort,
        );
        let tentative = FrozenProviderRoute::new(next_revision, target.clone(), next_effort);
        let continuity = commit(&previous, &tentative).map_err(SwitchCommitError::Commit)?;
        let next = FrozenProviderRoute::with_continuity(
            next_revision,
            target.clone(),
            continuity,
            next_effort,
        );
        state.revision = next_revision;
        state.active = target;
        state.continuity = continuity;
        state.effort = next_effort;
        state.effort_pinned |= pins;
        state
            .remembered_models
            .insert(provider_id.to_string(), target_model);
        Ok(SwitchOutcome::Changed {
            route: next,
            continuity,
        })
    }

    pub fn restore(
        catalog: Arc<ProviderCatalog>,
        revision: u64,
        provider_id: &str,
        model: &str,
        remembered_models: BTreeMap<String, String>,
    ) -> Result<Self, SwitchError> {
        if revision == 0 {
            return Err(SwitchError::InvalidRevision);
        }
        let active = catalog.resolve(provider_id, model)?;
        let effort = catalog.default_effort(provider_id);
        Ok(Self {
            catalog,
            state: Mutex::new(SessionState {
                revision,
                active,
                continuity: ReasoningContinuity::Preserved,
                remembered_models,
                effort,
                effort_pinned: false,
            }),
        })
    }
}

#[derive(Clone)]
pub struct FrozenProviderRoute {
    revision: u64,
    route: ResolvedRoute,
    continuity: ReasoningContinuity,
    /// The session's reasoning effort at freeze time. Rides the frozen route
    /// (so every attempt minted from it, children included, samples at the
    /// same effort) but never enters the durable receipt — it is not part of
    /// route identity and does not affect reasoning replay.
    effort: Option<ReasoningEffort>,
}

impl fmt::Debug for FrozenProviderRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FrozenProviderRoute")
            .field("revision", &self.revision)
            .field("provider_id", &self.route.provider_id)
            .field("api_family", &self.route.api_family)
            .field("primary_model", &self.route.primary_model)
            .finish()
    }
}

impl FrozenProviderRoute {
    fn new(revision: u64, route: ResolvedRoute, effort: Option<ReasoningEffort>) -> Self {
        Self {
            revision,
            route,
            continuity: ReasoningContinuity::Preserved,
            effort,
        }
    }

    fn with_continuity(
        revision: u64,
        route: ResolvedRoute,
        continuity: ReasoningContinuity,
        effort: Option<ReasoningEffort>,
    ) -> Self {
        Self {
            revision,
            route,
            continuity,
            effort,
        }
    }

    pub fn effort(&self) -> Option<ReasoningEffort> {
        self.effort
    }

    /// Whether both sides name the same route — provider, model, rail and
    /// endpoint. Revision and effort are not route identity, so a revision that
    /// moves only the effort answers true here, which is how a commit tells an
    /// effort change from a switch.
    pub fn same_route(&self, other: &Self) -> bool {
        self.route.provider_id == other.route.provider_id
            && self.route.primary_model == other.route.primary_model
            && self.route.api_family == other.route.api_family
            && self.route.endpoint_fingerprint == other.route.endpoint_fingerprint
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn at_revision(&self, revision: u64) -> Result<Self, SwitchError> {
        self.at_revision_with_continuity(revision, self.continuity)
    }

    /// The same route at a stated revision, carrying the reasoning continuity
    /// the change is being recorded with — the shape a route change takes once
    /// the projection has said what it costs.
    pub fn at_revision_with_continuity(
        &self,
        revision: u64,
        continuity: ReasoningContinuity,
    ) -> Result<Self, SwitchError> {
        if revision == 0 {
            return Err(SwitchError::InvalidRevision);
        }
        Ok(Self::with_continuity(
            revision,
            self.route.clone(),
            continuity,
            self.effort,
        ))
    }

    pub fn provider_id(&self) -> &str {
        &self.route.provider_id
    }

    pub fn api_family(&self) -> ProviderApiFamily {
        self.route.api_family
    }

    pub fn endpoint_fingerprint(&self) -> &str {
        &self.route.endpoint_fingerprint
    }

    pub fn primary_model(&self) -> &str {
        &self.route.primary_model
    }

    pub fn allowed_models(&self) -> &[String] {
        &self.route.allowed_models
    }

    pub fn primary_attempt(&self) -> FrozenProviderAttempt {
        self.attempt(self.route.primary_model.clone())
    }

    fn attempt(&self, model: String) -> FrozenProviderAttempt {
        let model_for_thinking = model.clone();
        FrozenProviderAttempt {
            identity: ProviderAttemptIdentity {
                route_revision: self.revision,
                provider_id: self.route.provider_id.clone(),
                api_family: self.route.api_family,
                endpoint_fingerprint: self.route.endpoint_fingerprint.clone(),
                model,
            },
            reasoning: Reasoning::new(
                self.effort,
                self.route
                    .thinking
                    .resolve(&model_for_thinking, self.effort),
            ),
            provider: Arc::clone(&self.route.provider),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_models(&self, models: &[&str]) -> Self {
        let mut route = self.route.clone();
        route.primary_model = models[0].to_string();
        route.allowed_models = models.iter().map(|model| model.to_string()).collect();
        Self::new(self.revision, route, self.effort)
    }

    pub fn child_route(
        &self,
        model: Option<&InheritedProviderModelOverride>,
    ) -> Result<Self, SwitchError> {
        let model = model
            .map(InheritedProviderModelOverride::as_str)
            .unwrap_or(&self.route.primary_model);
        if !self
            .route
            .allowed_models
            .iter()
            .any(|allowed| allowed == model)
        {
            return Err(SwitchError::UnknownModel {
                provider_id: self.route.provider_id.clone(),
                model: model.to_string(),
            });
        }
        let mut route = self.route.clone();
        route.primary_model = model.to_string();
        // A child inherits the session effort: the same rail, so always accepted.
        Ok(Self::with_continuity(
            1,
            route,
            self.continuity,
            self.effort,
        ))
    }

    pub fn receipt(
        &self,
        boundary: u64,
        source: kloop_protocol::ProviderRouteSource,
        continuity: ReasoningContinuity,
    ) -> kloop_protocol::ProviderRouteReceipt {
        kloop_protocol::ProviderRouteReceipt {
            revision: self.revision,
            boundary,
            source,
            provider_id: self.route.provider_id.clone(),
            api_family: self.route.api_family,
            endpoint_fingerprint: self.route.endpoint_fingerprint.clone(),
            primary_model: self.route.primary_model.clone(),
            effort: self.effort,
            continuity,
        }
    }

    pub fn continuity(&self) -> ReasoningContinuity {
        self.continuity
    }

    pub fn public_route(&self) -> ActiveProviderRoute {
        ActiveProviderRoute {
            revision: self.revision,
            provider_id: self.route.provider_id.clone(),
            api_family: self.route.api_family,
            model: self.route.primary_model.clone(),
            continuity: self.continuity,
            effort: self.effort,
        }
    }
}

#[derive(Clone)]
pub struct FrozenProviderAttempt {
    identity: ProviderAttemptIdentity,
    /// The session effort together with what it rendered to for this attempt's
    /// model. Resolved here rather than inside the provider because only the
    /// route knows the model's dialect, and only the route is still holding the
    /// session effort.
    reasoning: Reasoning,
    provider: Arc<Provider>,
}

impl fmt::Debug for FrozenProviderAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FrozenProviderAttempt")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl FrozenProviderAttempt {
    pub fn identity(&self) -> &ProviderAttemptIdentity {
        &self.identity
    }

    pub fn provider(&self) -> &Arc<Provider> {
        &self.provider
    }

    pub fn model(&self) -> &str {
        &self.identity.model
    }

    /// The reasoning effort this attempt samples at. Deliberately outside
    /// [`ProviderAttemptIdentity`]: identity drives provenance and reasoning
    /// replay matching, and a response stays replayable when the effort changes.
    pub fn effort(&self) -> Option<ReasoningEffort> {
        self.reasoning.effort
    }

    /// The whole reasoning setting for this attempt: the chosen effort and the
    /// `thinking` field it rendered to for this model.
    pub fn reasoning(&self) -> Reasoning {
        self.reasoning
    }

    pub fn provenance(&self, origin_boundary: u64) -> ProviderResponseProvenance {
        ProviderResponseProvenance {
            route_revision: self.identity.route_revision,
            origin_boundary,
            provider_id: self.identity.provider_id.clone(),
            api_family: self.identity.api_family,
            endpoint_fingerprint: self.identity.endpoint_fingerprint.clone(),
            model: self.identity.model.clone(),
        }
    }
}

/// What a route change does with the session's reasoning effort.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffortRequest {
    /// No level was named: the session keeps its own, or adopts the target
    /// provider's configured value when `/effort` has never spoken.
    Inherit,
    /// A level — or `unset`, the `None` inside — named in the same command, so
    /// it lands in the same revision as the route it came with.
    Set(Option<ReasoningEffort>),
}

/// Everything the route picker needs to run all three of its stages without
/// asking the session again. Built when a command opens the picker: the front-end
/// walks providers and models the session is not on, so it cannot read the
/// answers off the active route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutePicker {
    /// Where this entry point starts — and, because Esc there closes the panel,
    /// how far back it can go.
    pub stage: RoutePickerStage,
    pub providers: Vec<ProviderDescriptor>,
    /// `[models."<id>"].efforts`, by model; absent means undeclared, which means
    /// no limit (see [`ProviderCatalog::effort_supported`]).
    pub declared_efforts: BTreeMap<String, Vec<ReasoningEffort>>,
    /// Where the session sits now: the provider and model `/model` and `/effort`
    /// start inside, and the effort the last stage's cursor opens on.
    pub active: ActiveProviderRoute,
}

impl RoutePicker {
    /// What the effort stage lists for one model: `unset`, then what the model
    /// declared — everything when it declared nothing.
    pub fn effort_choices(&self, model: &str) -> Vec<Option<ReasoningEffort>> {
        ReasoningEffort::choices(self.declared_efforts.get(model).map(Vec::as_slice))
    }
}

#[derive(Clone, Debug)]
pub enum SwitchOutcome {
    NoOp(FrozenProviderRoute),
    Changed {
        route: FrozenProviderRoute,
        continuity: ReasoningContinuity,
    },
}

/// A session that opened on a different route than the one it was last written
/// on, and where it went. Carried back to whichever front-end opened the session
/// so the hop is stated once, in words, rather than left for the user to notice
/// in a bill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteReopened {
    pub from_provider: String,
    pub from_model: String,
    pub to_provider: String,
    pub to_model: String,
    pub continuity: ReasoningContinuity,
}

impl fmt::Display for RouteReopened {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            from_provider,
            from_model,
            to_provider,
            to_model,
            continuity,
        } = self;
        write!(
            formatter,
            "session was written on {from_provider}/{from_model}; reopening on \
             {to_provider}/{to_model}"
        )?;
        if *continuity == ReasoningContinuity::Filtered {
            formatter.write_str(" — earlier reasoning is dropped from the request")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwitchError {
    UnknownProvider(String),
    UnknownModel {
        provider_id: String,
        model: String,
    },
    Unavailable {
        provider_id: String,
        code: ProviderAvailabilityCode,
    },
    RouteDrift(String),
    StaleRevision {
        expected: u64,
        actual: u64,
    },
    RevisionExhausted,
    InvalidRevision,
    InvalidTimeline,
    InvalidInheritedProviderModel,
}

impl fmt::Display for SwitchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownProvider(provider) => write!(formatter, "unknown provider '{provider}'"),
            Self::UnknownModel { provider_id, model } => {
                write!(
                    formatter,
                    "unknown model '{model}' for provider '{provider_id}'"
                )
            }
            Self::Unavailable { provider_id, code } => {
                write!(
                    formatter,
                    "provider '{provider_id}' is unavailable ({code:?})"
                )
            }
            Self::RouteDrift(provider_id) => write!(
                formatter,
                "provider '{provider_id}' no longer matches the persisted route identity"
            ),
            Self::StaleRevision { expected, actual } => write!(
                formatter,
                "provider route revision changed: expected {expected}, active {actual}"
            ),
            Self::RevisionExhausted => formatter.write_str("provider route revision exhausted"),
            Self::InvalidRevision => {
                formatter.write_str("provider route revision must be positive")
            }
            Self::InvalidTimeline => formatter.write_str("provider route timeline is invalid"),
            Self::InvalidInheritedProviderModel => {
                formatter.write_str("inherited provider model override must be a non-blank string")
            }
        }
    }
}

impl std::error::Error for SwitchError {}

#[derive(Debug)]
pub enum SwitchCommitError<E> {
    Switch(SwitchError),
    Commit(E),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_entry(id: &str, default_model: &str, models: &[&str]) -> ProviderCatalogEntry {
        ProviderCatalogEntry {
            id: id.into(),
            api_family: ProviderApiFamily::Mock,
            endpoint_fingerprint: format!("mock:{id}"),
            default_model: default_model.into(),
            models: models.iter().map(|model| (*model).to_string()).collect(),
            context_window: None,
            availability: ProviderAvailabilityCode::Ready,
            default_effort: None,
            sends_thinking: true,
            factory: Arc::new(|| Ok(Provider::mock(Vec::new()))),
        }
    }

    /// Effort is the only knob the user turns; which field it lands in is the
    /// model's business. A budget-dialect model turns a level into a token
    /// count, every other model leaves the level on `output_config` and takes
    /// the profile's own thinking mode, and `none` means "do not reason" on all
    /// of them — so it outranks both.
    #[test]
    fn effort_renders_into_whichever_reasoning_field_the_model_reads() {
        let entry = mock_entry("p", "budget-model", &["budget-model", "effort-model"]);

        let catalog = Arc::new(
            ProviderCatalog::new(vec![entry])
                .unwrap()
                .with_model_knowledge(BTreeMap::from([(
                    "budget-model".to_string(),
                    ModelKnowledge {
                        context_window: None,
                        efforts: Some(vec![ReasoningEffort::Low, ReasoningEffort::High]),
                        thinking_budgets: Some(BTreeMap::from([
                            (ReasoningEffort::Low, 2048),
                            (ReasoningEffort::High, 16384),
                        ])),
                    },
                )])),
        );
        let thinking = |model: &str, effort: Option<ReasoningEffort>| {
            let route = catalog.initial_route("p", Some(model)).unwrap();
            let state = SessionProviderState::from_route(Arc::clone(&catalog), route);
            state.set_effort(effort);
            state.freeze().primary_attempt().reasoning().thinking
        };

        assert_eq!(
            thinking("budget-model", Some(ReasoningEffort::High)),
            ThinkingMode::Budget(16384)
        );
        // No level selected on a budget model sends no field, which is that
        // model's own API default — not the profile's adaptive.
        assert_eq!(thinking("budget-model", None), ThinkingMode::Unset);
        assert_eq!(
            thinking("effort-model", Some(ReasoningEffort::High)),
            ThinkingMode::Adaptive
        );
        assert_eq!(thinking("effort-model", None), ThinkingMode::Adaptive);
        for model in ["budget-model", "effort-model"] {
            assert_eq!(
                thinking(model, Some(ReasoningEffort::None)),
                ThinkingMode::Off,
                "{model}"
            );
        }
    }

    /// A gateway that cannot take the `thinking` field gets none of it — not the
    /// default, not a model's budget. That is a limitation of the endpoint, not
    /// a reasoning choice, which is why `effort` is untouched and only the field
    /// disappears.
    #[test]
    fn a_gateway_that_omits_the_thinking_field_sends_no_mode_at_all() {
        let mut entry = mock_entry("p", "budget-model", &["budget-model", "effort-model"]);
        entry.sends_thinking = false;
        let catalog = Arc::new(
            ProviderCatalog::new(vec![entry])
                .unwrap()
                .with_model_knowledge(BTreeMap::from([(
                    "budget-model".to_string(),
                    ModelKnowledge {
                        context_window: None,
                        efforts: Some(vec![ReasoningEffort::High]),
                        thinking_budgets: Some(BTreeMap::from([(ReasoningEffort::High, 16384)])),
                    },
                )])),
        );
        assert!(!catalog.sends_thinking("p"));
        for model in ["budget-model", "effort-model"] {
            let route = catalog.initial_route("p", Some(model)).unwrap();
            let state = SessionProviderState::from_route(Arc::clone(&catalog), route);
            state.set_effort(Some(ReasoningEffort::High));
            let attempt = state.freeze().primary_attempt();
            assert_eq!(attempt.reasoning().thinking, ThinkingMode::Unset, "{model}");
            assert_eq!(attempt.effort(), Some(ReasoningEffort::High), "{model}");
        }
    }

    fn receipt(
        revision: u64,
        boundary: u64,
        source: kloop_protocol::ProviderRouteSource,
        provider_id: &str,
        model: &str,
    ) -> kloop_protocol::ProviderRouteReceipt {
        kloop_protocol::ProviderRouteReceipt {
            revision,
            boundary,
            source,
            provider_id: provider_id.into(),
            api_family: ProviderApiFamily::Mock,
            endpoint_fingerprint: format!("mock:{provider_id}"),
            primary_model: model.into(),
            effort: None,
            continuity: ReasoningContinuity::Preserved,
        }
    }

    /// A past revision names what a past turn ran on. Only the route the
    /// session continues on has to exist today — otherwise renaming a provider
    /// in configuration would retroactively lock every session that ever
    /// touched it (plan 132).
    #[test]
    fn from_timeline_judges_only_the_route_the_session_continues_on() {
        let catalog =
            Arc::new(ProviderCatalog::new(vec![mock_entry("kept", "k1", &["k1"])]).unwrap());
        let recovered = [
            receipt(
                1,
                1,
                kloop_protocol::ProviderRouteSource::Initial,
                "gone",
                "g1",
            ),
            receipt(
                2,
                4,
                kloop_protocol::ProviderRouteSource::Reopened,
                "kept",
                "k1",
            ),
        ];
        let state = SessionProviderState::from_timeline(Arc::clone(&catalog), &recovered).unwrap();
        assert_eq!(state.active_route().revision, 2);
        assert_eq!(state.active_route().provider_id, "kept");
        assert_eq!(
            state.remembered_models(),
            BTreeMap::from([("gone".into(), "g1".into()), ("kept".into(), "k1".into())]),
            "a model remembered for a provider that is gone is harmless: switching to it resolves"
        );

        // The last receipt is the one that still has to resolve.
        let stranded = [
            receipt(
                1,
                1,
                kloop_protocol::ProviderRouteSource::Initial,
                "kept",
                "k1",
            ),
            receipt(
                2,
                4,
                kloop_protocol::ProviderRouteSource::ExplicitSwitch,
                "gone",
                "g1",
            ),
        ];
        assert_eq!(
            SessionProviderState::from_timeline(catalog, &stranded).unwrap_err(),
            SwitchError::UnknownProvider("gone".into())
        );
    }

    #[test]
    fn from_timeline_restores_latest_route_and_remembered_models() {
        let catalog = Arc::new(
            ProviderCatalog::new(vec![
                mock_entry("a", "a1", &["a1", "a2"]),
                mock_entry("b", "b1", &["b1"]),
            ])
            .unwrap(),
        );
        let timeline = [
            receipt(
                1,
                1,
                kloop_protocol::ProviderRouteSource::Initial,
                "a",
                "a1",
            ),
            receipt(
                2,
                3,
                kloop_protocol::ProviderRouteSource::ExplicitSwitch,
                "b",
                "b1",
            ),
            receipt(
                3,
                5,
                kloop_protocol::ProviderRouteSource::ExplicitSwitch,
                "a",
                "a2",
            ),
        ];

        let state = SessionProviderState::from_timeline(catalog, &timeline).unwrap();
        assert_eq!(state.active_route().revision, 3);
        assert_eq!(state.active_route().provider_id, "a");
        assert_eq!(state.active_route().model, "a2");
        assert_eq!(
            state.remembered_models(),
            BTreeMap::from([("a".into(), "a2".into()), ("b".into(), "b1".into())])
        );
    }

    #[test]
    fn from_timeline_rejects_malformed_receipt_sequence_before_restore() {
        let catalog = Arc::new(
            ProviderCatalog::new(vec![
                mock_entry("a", "a1", &["a1"]),
                mock_entry("b", "b1", &["b1"]),
            ])
            .unwrap(),
        );
        let initial = receipt(
            1,
            1,
            kloop_protocol::ProviderRouteSource::Initial,
            "a",
            "a1",
        );
        let valid_switch = receipt(
            2,
            3,
            kloop_protocol::ProviderRouteSource::ExplicitSwitch,
            "b",
            "b1",
        );
        let cases = [
            vec![],
            vec![valid_switch.clone()],
            vec![
                initial.clone(),
                receipt(
                    2,
                    1,
                    kloop_protocol::ProviderRouteSource::ExplicitSwitch,
                    "b",
                    "b1",
                ),
            ],
            vec![
                initial.clone(),
                receipt(
                    2,
                    3,
                    kloop_protocol::ProviderRouteSource::ExplicitSwitch,
                    "b",
                    "b1",
                ),
                receipt(
                    3,
                    3,
                    kloop_protocol::ProviderRouteSource::ExplicitSwitch,
                    "a",
                    "a1",
                ),
            ],
            vec![
                initial.clone(),
                receipt(
                    2,
                    3,
                    kloop_protocol::ProviderRouteSource::ExplicitSwitch,
                    "b",
                    "b1",
                ),
                receipt(
                    3,
                    2,
                    kloop_protocol::ProviderRouteSource::ExplicitSwitch,
                    "a",
                    "a1",
                ),
            ],
            vec![
                initial.clone(),
                receipt(
                    3,
                    3,
                    kloop_protocol::ProviderRouteSource::ExplicitSwitch,
                    "b",
                    "b1",
                ),
            ],
            vec![
                initial,
                receipt(
                    2,
                    3,
                    kloop_protocol::ProviderRouteSource::Initial,
                    "b",
                    "b1",
                ),
            ],
        ];
        for timeline in cases {
            assert!(matches!(
                SessionProviderState::from_timeline(Arc::clone(&catalog), &timeline),
                Err(SwitchError::InvalidTimeline)
            ));
        }
    }

    #[test]
    fn catalog_deduplicates_models_and_validates_defaults() {
        let catalog =
            ProviderCatalog::new(vec![mock_entry("a", "m1", &["m1", "m2", "m1"])]).unwrap();
        assert_eq!(catalog.descriptors()[0].models, ["m1", "m2"]);

        let error = ProviderCatalog::new(vec![mock_entry("a", "missing", &["m1"])]).unwrap_err();
        assert!(error.contains("default model"));
    }

    #[test]
    fn child_route_freezes_parent_and_restarts_revision() {
        let catalog =
            Arc::new(ProviderCatalog::new(vec![mock_entry("a", "m1", &["m1", "m2"])]).unwrap());
        let route = catalog.initial_route("a", None).unwrap();
        let child = route
            .child_route(Some(&InheritedProviderModelOverride::parse("m2").unwrap()))
            .unwrap();
        assert_eq!(child.revision(), 1);
        assert_eq!(child.primary_model(), "m2");
        assert_eq!(route.primary_model(), "m1");

        let error = route
            .child_route(Some(
                &InheritedProviderModelOverride::parse("missing").unwrap(),
            ))
            .unwrap_err();
        assert_eq!(
            error,
            SwitchError::UnknownModel {
                provider_id: "a".into(),
                model: "missing".into(),
            }
        );
        assert_eq!(
            InheritedProviderModelOverride::parse(" \t "),
            Err(SwitchError::InvalidInheritedProviderModel)
        );
    }

    #[test]
    fn successful_switch_updates_only_after_commit_and_remembers_model() {
        let catalog = Arc::new(
            ProviderCatalog::new(vec![
                mock_entry("a", "a1", &["a1"]),
                mock_entry("b", "b1", &["b1", "b2"]),
            ])
            .unwrap(),
        );
        let state = SessionProviderState::new(catalog, "a", None).unwrap();
        let outcome = state
            .switch_with(
                1,
                "b",
                Some("b2"),
                EffortRequest::Inherit,
                |previous, next| {
                    assert_eq!(previous.primary_model(), "a1");
                    assert_eq!(next.revision(), 2);
                    Ok::<_, ()>(ReasoningContinuity::Filtered)
                },
            )
            .unwrap();
        assert!(matches!(outcome, SwitchOutcome::Changed { .. }));
        assert_eq!(state.active_route().model, "b2");
        assert_eq!(state.remembered_models()["b"], "b2");
    }

    #[test]
    fn failed_commit_and_noop_are_zero_mutation() {
        let catalog = Arc::new(
            ProviderCatalog::new(vec![
                mock_entry("a", "a1", &["a1"]),
                mock_entry("b", "b1", &["b1"]),
            ])
            .unwrap(),
        );
        let state = SessionProviderState::new(catalog, "a", None).unwrap();
        let result = state.switch_with(1, "b", None, EffortRequest::Inherit, |_, _| {
            Err::<ReasoningContinuity, _>("persist")
        });
        assert!(matches!(result, Err(SwitchCommitError::Commit("persist"))));
        assert_eq!(state.active_route().revision, 1);
        assert_eq!(state.active_route().provider_id, "a");

        let mut called = false;
        let outcome = state
            .switch_with(1, "a", None, EffortRequest::Inherit, |_, _| {
                called = true;
                Ok::<_, ()>(ReasoningContinuity::Preserved)
            })
            .unwrap();
        assert!(!called);
        assert!(matches!(outcome, SwitchOutcome::NoOp(_)));
    }

    fn rail_entry(
        id: &str,
        api_family: ProviderApiFamily,
        default_effort: Option<ReasoningEffort>,
    ) -> ProviderCatalogEntry {
        ProviderCatalogEntry {
            api_family,
            default_effort,
            ..mock_entry(id, "m1", &["m1"])
        }
    }

    /// The session knob is set and read as given: kloop bounds the spelling, the
    /// model bounds the levels. What matters here is that the value reaches every
    /// attempt minted from the frozen route.
    #[test]
    fn set_effort_reaches_every_attempt_minted_from_the_route() {
        let catalog = Arc::new(
            ProviderCatalog::new(vec![rail_entry(
                "responses",
                ProviderApiFamily::OpenAiResponses,
                None,
            )])
            .unwrap(),
        );
        let state = SessionProviderState::new(catalog, "responses", None).unwrap();
        assert_eq!(state.effort(), None);
        // The level is the model's contract, not ours: kloop stores what it is
        // told and lets the model object on the next turn.
        state.set_effort(Some(ReasoningEffort::Max));
        assert_eq!(state.effort(), Some(ReasoningEffort::Max));
        let route = state.freeze();
        assert_eq!(route.primary_attempt().effort(), Some(ReasoningEffort::Max));
        assert_eq!(
            route.child_route(None).unwrap().effort(),
            Some(ReasoningEffort::Max)
        );
        state.set_effort(None);
        assert_eq!(state.freeze().primary_attempt().effort(), None);
    }

    /// Across a switch: an untouched session follows each provider's configured
    /// effort, while a session that has run `/effort` carries its choice along —
    /// including an explicit `off`, which a switch must not quietly revive.
    #[test]
    fn switching_provider_keeps_a_pinned_effort_and_otherwise_follows_configuration() {
        let catalog = || {
            Arc::new(
                ProviderCatalog::new(vec![
                    rail_entry(
                        "claude",
                        ProviderApiFamily::AnthropicMessages,
                        Some(ReasoningEffort::XHigh),
                    ),
                    rail_entry(
                        "responses",
                        ProviderApiFamily::OpenAiResponses,
                        Some(ReasoningEffort::Low),
                    ),
                ])
                .unwrap(),
            )
        };
        let switch = |state: &SessionProviderState, to: &str, revision: u64| {
            state
                .switch_with(revision, to, None, EffortRequest::Inherit, |_, _| {
                    Ok::<_, ()>(ReasoningContinuity::Filtered)
                })
                .unwrap()
        };

        let unpinned = SessionProviderState::new(catalog(), "claude", None).unwrap();
        assert_eq!(unpinned.effort(), Some(ReasoningEffort::XHigh));
        switch(&unpinned, "responses", 1);
        assert_eq!(unpinned.effort(), Some(ReasoningEffort::Low));

        let pinned = SessionProviderState::new(catalog(), "claude", None).unwrap();
        pinned.set_effort(Some(ReasoningEffort::Medium));
        switch(&pinned, "responses", 1);
        assert_eq!(pinned.effort(), Some(ReasoningEffort::Medium));
        assert_eq!(
            pinned.active_route().effort,
            Some(ReasoningEffort::Medium),
            "the published route reports what the next turn will sample at"
        );

        let cleared = SessionProviderState::new(catalog(), "claude", None).unwrap();
        cleared.set_effort(None);
        switch(&cleared, "responses", 1);
        assert_eq!(cleared.effort(), None);
    }
}
