use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use kloop_protocol::ActiveProviderRoute;
use kloop_protocol::ProviderApiFamily;
use kloop_protocol::ProviderAttemptIdentity;
use kloop_protocol::ProviderAttemptKind;
use kloop_protocol::ProviderAvailabilityCode;
use kloop_protocol::ProviderDescriptor;
use kloop_protocol::ProviderResponseProvenance;
use kloop_protocol::ReasoningContinuity;
use kloop_protocol::ReasoningEffort;
use kloop_provider::Provider;

pub type ProviderFactory =
    Arc<dyn Fn() -> Result<Provider, ProviderAvailabilityCode> + Send + Sync + 'static>;

pub struct ProviderCatalogEntry {
    pub id: String,
    pub api_family: ProviderApiFamily,
    pub endpoint_fingerprint: String,
    pub default_model: String,
    pub models: Vec<String>,
    pub fallback_model: Option<String>,
    pub availability: ProviderAvailabilityCode,
    /// The configured effort this provider starts a session at. It seeds
    /// [`SessionProviderState`]; `/effort` then owns the value for the rest of
    /// the session (the provider itself bakes in nothing).
    pub default_effort: Option<ReasoningEffort>,
    pub factory: ProviderFactory,
}

struct CatalogEntry {
    descriptor: ProviderDescriptor,
    endpoint_fingerprint: String,
    default_effort: Option<ReasoningEffort>,
    factory: ProviderFactory,
    provider: OnceLock<Result<Arc<Provider>, ProviderAvailabilityCode>>,
}

pub struct ProviderCatalog {
    entries: BTreeMap<String, CatalogEntry>,
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
            if let Some(fallback) = entry.fallback_model.as_ref()
                && !models.contains(fallback)
            {
                return Err(format!(
                    "provider '{id}' fallback model '{fallback}' is not in its models allowlist"
                ));
            }
            let descriptor = ProviderDescriptor {
                id: id.clone(),
                api_family: entry.api_family,
                default_model,
                models,
                fallback_model: entry.fallback_model,
                availability: entry.availability,
            };
            if catalog
                .insert(
                    id.clone(),
                    CatalogEntry {
                        descriptor,
                        endpoint_fingerprint,
                        default_effort: entry.default_effort,
                        factory: entry.factory,
                        provider: OnceLock::new(),
                    },
                )
                .is_some()
            {
                return Err(format!("duplicate provider id '{id}'"));
            }
        }
        Ok(Self { entries: catalog })
    }

    pub fn from_provider(
        id: impl Into<String>,
        provider: Provider,
        default_model: impl Into<String>,
        models: Vec<String>,
        fallback_model: Option<String>,
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
            fallback_model,
            availability: ProviderAvailabilityCode::Ready,
            default_effort: None,
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
            || entry.descriptor.fallback_model != receipt.fallback_model
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
            fallback_model: entry.descriptor.fallback_model.clone(),
            provider,
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
            || receipt
                .fallback_model
                .as_deref()
                .is_some_and(|fallback| fallback.trim().is_empty())
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
    let model_matches = match source.attempt_kind {
        ProviderAttemptKind::Primary => source.model == route.primary_model,
        ProviderAttemptKind::Fallback => {
            route.fallback_model.as_deref() == Some(source.model.as_str())
        }
    };
    if source.provider_id != route.provider_id
        || source.api_family != route.api_family
        || source.endpoint_fingerprint != route.endpoint_fingerprint
        || !model_matches
    {
        return Err(ProvenanceMismatch::IdentityMismatch);
    }
    // Chat carries no reasoning at all; Responses carries it as an encrypted
    // blob and never as a redacted block.
    let shape_agrees = match reasoning {
        ReasoningShape::None => true,
        ReasoningShape::Plain => source.api_family != ProviderApiFamily::OpenAiChatCompletions,
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
    fallback_model: Option<String>,
    provider: Arc<Provider>,
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

    pub fn preflight(&self, provider_id: &str, model: Option<&str>) -> Result<(), SwitchError> {
        let target_model = {
            let state = self.state.lock().unwrap();
            let descriptor = self
                .catalog
                .descriptor(provider_id)
                .ok_or_else(|| SwitchError::UnknownProvider(provider_id.to_string()))?;
            model
                .map(str::to_string)
                .or_else(|| state.remembered_models.get(provider_id).cloned())
                .unwrap_or(descriptor.default_model)
        };
        self.catalog.resolve(provider_id, &target_model).map(|_| ())
    }

    /// Change the session's reasoning effort as a route revision. `/effort` is a
    /// user decision that changes what every later request looks like, so it
    /// belongs on the same timeline as a provider switch rather than mutating
    /// state invisibly — a transcript that recorded only the opening effort
    /// would state it with confidence and be wrong. `commit` writes the receipt
    /// and may refuse; nothing lands unless it succeeds. `Ok(None)` means the
    /// value was already this.
    pub fn commit_effort<E>(
        &self,
        effort: Option<ReasoningEffort>,
        commit: impl FnOnce(&FrozenProviderRoute) -> Result<(), E>,
    ) -> Result<Option<FrozenProviderRoute>, E> {
        let mut state = self.state.lock().unwrap();
        if state.effort == effort {
            state.effort_pinned = true;
            return Ok(None);
        }
        let Some(next_revision) = state.revision.checked_add(1) else {
            // Out of revisions is not a reason to lose the user's choice: apply
            // it in memory, unrecorded, exactly as before this method existed.
            state.effort = effort;
            state.effort_pinned = true;
            return Ok(None);
        };
        // Same provider, same model: nothing about reasoning replay changes, so
        // the continuity this route already carries rides forward untouched.
        let next = FrozenProviderRoute::with_continuity(
            next_revision,
            state.active.clone(),
            state.continuity,
            effort,
        );
        commit(&next)?;
        state.revision = next_revision;
        state.effort = effort;
        state.effort_pinned = true;
        Ok(Some(next))
    }

    pub fn switch_with<E>(
        &self,
        expected_revision: u64,
        provider_id: &str,
        model: Option<&str>,
        commit: impl FnOnce(
            &FrozenProviderRoute,
            &FrozenProviderRoute,
        ) -> Result<ReasoningContinuity, E>,
    ) -> Result<SwitchOutcome, SwitchCommitError<E>> {
        let target_model = {
            let state = self.state.lock().unwrap();
            let descriptor = self.catalog.descriptor(provider_id).ok_or_else(|| {
                SwitchCommitError::Switch(SwitchError::UnknownProvider(provider_id.to_string()))
            })?;
            model
                .map(str::to_string)
                .or_else(|| state.remembered_models.get(provider_id).cloned())
                .unwrap_or(descriptor.default_model)
        };
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
        if state.active.provider_id == target.provider_id
            && state.active.primary_model == target.primary_model
            && state.active.api_family == target.api_family
            && state.active.endpoint_fingerprint == target.endpoint_fingerprint
        {
            return Ok(SwitchOutcome::NoOp(FrozenProviderRoute::with_continuity(
                state.revision,
                state.active.clone(),
                state.continuity,
                state.effort,
            )));
        }
        let next_revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| SwitchCommitError::Switch(SwitchError::RevisionExhausted))?;
        let previous = FrozenProviderRoute::with_continuity(
            state.revision,
            state.active.clone(),
            state.continuity,
            state.effort,
        );
        // An unpinned session follows the target's configured effort; once
        // `/effort` has spoken, the user's choice travels with the session. A
        // model that refuses the level says so on the next turn — kloop does not
        // second-guess it here (see `set_effort`).
        let next_effort = if state.effort_pinned {
            state.effort
        } else {
            self.catalog.default_effort(provider_id)
        };
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
            .field("fallback_model", &self.route.fallback_model)
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

    pub fn fallback_model(&self) -> Option<&str> {
        self.route.fallback_model.as_deref()
    }

    pub fn primary_attempt(&self) -> FrozenProviderAttempt {
        self.attempt(
            self.route.primary_model.clone(),
            ProviderAttemptKind::Primary,
        )
    }

    pub fn fallback_attempt(&self) -> Option<FrozenProviderAttempt> {
        self.route
            .fallback_model
            .as_ref()
            .filter(|model| **model != self.route.primary_model)
            .map(|model| self.attempt(model.clone(), ProviderAttemptKind::Fallback))
    }

    fn attempt(&self, model: String, attempt_kind: ProviderAttemptKind) -> FrozenProviderAttempt {
        FrozenProviderAttempt {
            identity: ProviderAttemptIdentity {
                route_revision: self.revision,
                provider_id: self.route.provider_id.clone(),
                api_family: self.route.api_family,
                endpoint_fingerprint: self.route.endpoint_fingerprint.clone(),
                model,
                attempt_kind,
            },
            effort: self.effort,
            provider: Arc::clone(&self.route.provider),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_models(&self, primary: &str, fallback: Option<&str>) -> Self {
        let mut route = self.route.clone();
        route.primary_model = primary.to_string();
        route.allowed_models = vec![primary.to_string()];
        route.fallback_model = fallback.map(str::to_string);
        if let Some(fallback) = fallback
            && !route.allowed_models.iter().any(|model| model == fallback)
        {
            route.allowed_models.push(fallback.to_string());
        }
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
            fallback_model: self.route.fallback_model.clone(),
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
    effort: Option<ReasoningEffort>,
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
        self.effort
    }

    pub fn provenance(&self, origin_boundary: u64) -> ProviderResponseProvenance {
        ProviderResponseProvenance {
            route_revision: self.identity.route_revision,
            origin_boundary,
            provider_id: self.identity.provider_id.clone(),
            api_family: self.identity.api_family,
            endpoint_fingerprint: self.identity.endpoint_fingerprint.clone(),
            model: self.identity.model.clone(),
            attempt_kind: self.identity.attempt_kind,
        }
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
            fallback_model: None,
            availability: ProviderAvailabilityCode::Ready,
            default_effort: None,
            factory: Arc::new(|| Ok(Provider::mock(Vec::new()))),
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
            fallback_model: None,
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
            .switch_with(1, "b", Some("b2"), |previous, next| {
                assert_eq!(previous.primary_model(), "a1");
                assert_eq!(next.revision(), 2);
                Ok::<_, ()>(ReasoningContinuity::Filtered)
            })
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
        let result = state.switch_with(1, "b", None, |_, _| {
            Err::<ReasoningContinuity, _>("persist")
        });
        assert!(matches!(result, Err(SwitchCommitError::Commit("persist"))));
        assert_eq!(state.active_route().revision, 1);
        assert_eq!(state.active_route().provider_id, "a");

        let mut called = false;
        let outcome = state
            .switch_with(1, "a", None, |_, _| {
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
                .switch_with(revision, to, None, |_, _| {
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
