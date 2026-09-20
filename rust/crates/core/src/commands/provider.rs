use kloop_protocol::ReasoningEffort;
use kloop_protocol::RoutePickerStage;
use kloop_protocol::UnknownReasoningEffort;

use super::SlashResult;
use crate::config::Config;
use crate::history::History;
use crate::provider_route::EffortRequest;
use crate::provider_route::SessionProviderState;
use crate::provider_route::SwitchOutcome;

pub const SUMMARY: &str = "show or switch the session provider, model and effort";

const USAGE: &str = "usage: /provider <provider> [model] [effort]";

pub fn run(
    args: &str,
    history: &mut History,
    cfg: &Config,
    state: &SessionProviderState,
) -> SlashResult {
    let mut parts = args.split_whitespace();
    let Some(provider_id) = parts.next() else {
        let active = state.active_route();
        let mut lines = vec![format!(
            "active: {} {} (revision {})",
            active.provider_id, active.model, active.revision
        )];
        lines.push("providers:".into());
        // The session's own catalog, not `cfg`'s: it is the one a switch
        // resolves against, so it is the one whose contents are true here.
        for descriptor in state.catalog().descriptors() {
            lines.push(format!(
                "  {} [{}] default={} models={} availability={:?}",
                descriptor.id,
                api_family_label(descriptor.api_family),
                descriptor.default_model,
                descriptor.models.join(","),
                descriptor.availability,
            ));
        }
        lines.push(USAGE.into());
        return SlashResult::route(
            lines.join("\n"),
            /*changed*/ false,
            Some(RoutePickerStage::Provider),
        );
    };
    let model = parts.next();
    let effort = match parts.next().map(parse_level).transpose() {
        Ok(level) => level.map_or(EffortRequest::Inherit, EffortRequest::Set),
        Err(error) => return SlashResult::message(format!("{error}\n{USAGE}")),
    };
    if parts.next().is_some() {
        return SlashResult::message(USAGE);
    }
    match apply(history, cfg, state, provider_id, model, effort) {
        Ok(SwitchOutcome::NoOp(route)) => SlashResult::route(
            format!(
                "provider unchanged: {} {} (revision {})",
                route.provider_id(),
                route.primary_model(),
                route.revision()
            ),
            /*changed*/ false,
            None,
        ),
        // The effort is reported because a switch can change it: it is kept
        // when the new rail accepts it, and otherwise falls back to the new
        // provider's configured value.
        Ok(SwitchOutcome::Changed { route, continuity }) => SlashResult::route(
            format!(
                "provider switched: {} {} (revision {}, reasoning continuity: {continuity:?}, effort: {})",
                route.provider_id(),
                route.primary_model(),
                route.revision(),
                ReasoningEffort::choice_str(route.effort()),
            ),
            /*changed*/ true,
            None,
        ),
        Err(reason) => SlashResult::message(format!("provider switch failed: {reason}")),
    }
}

/// One level as a command line spells it. `unset` is kloop's word for "send no
/// effort field at all" — not a wire value, so it is matched before the level
/// vocabulary. Deliberately not spelled `off`: `none` is a real level meaning
/// "do no reasoning", and the two would read as synonyms.
pub(super) fn parse_level(raw: &str) -> Result<Option<ReasoningEffort>, UnknownReasoningEffort> {
    if raw.eq_ignore_ascii_case("unset") {
        return Ok(None);
    }
    raw.parse::<ReasoningEffort>().map(Some)
}

/// Apply one route decision — provider, model and effort together — as a single
/// revision. `/provider`, `/model` and `/effort` are three entry points into the
/// same change, and the picker walks all three stages before it sends anything;
/// committing the switch and the effort separately would leave two revisions on
/// the timeline for one decision the user made once, which reads months later as
/// someone who changed their mind twice.
///
/// Refusals come back as the bare reason. Each command adds its own prefix,
/// because "provider switch failed" is the wrong sentence for `/effort`.
pub(super) fn apply(
    history: &mut History,
    cfg: &Config,
    state: &SessionProviderState,
    provider_id: &str,
    model: Option<&str>,
    effort: EffortRequest,
) -> Result<SwitchOutcome, String> {
    // A declared `efforts` list is a claim the user made after testing the model;
    // refusing here costs one line, while letting it through costs a request that
    // comes back 400 halfway into the turn. An undeclared model accepts anything.
    // The list that counts is the *target* model's: a switch can land on a model
    // whose list is narrower than the one being left behind.
    if let EffortRequest::Set(Some(level)) = effort {
        let target = state
            .target_model(provider_id, model)
            .ok_or_else(|| format!("unknown provider '{provider_id}'"))?;
        // "Do no reasoning" is the `thinking` field on this rail, so a gateway
        // that cannot take that field cannot be told it either. Refusing beats
        // accepting a level that would quietly render to nothing.
        if level == ReasoningEffort::None && !state.catalog().sends_thinking(provider_id) {
            return Err(format!(
                "provider '{provider_id}' omits the thinking request field \
                 (thinking_param = false), so 'none' cannot be expressed there"
            ));
        }
        if !state.catalog().effort_supported(&target, level) {
            return Err(format!(
                "'{}' is not among the efforts declared for model '{target}' ({})",
                level.as_str(),
                ReasoningEffort::join(
                    state
                        .catalog()
                        .declared_efforts(&target)
                        .unwrap_or_default()
                ),
            ));
        }
    }
    // A revision can only follow one, and choosing a route before the first turn
    // is the normal way to start a session — so open the timeline here if
    // sampling has not already done it. Guarded on emptiness rather than left to
    // `ensure`'s own idempotence: once the timeline has moved past `cfg`'s frozen
    // revision, `ensure` reads that as a mismatch and refuses.
    if !history.has_provider_route() {
        history
            .ensure_initial_provider_route(&cfg.provider_route)
            .map_err(|error| error.to_string())?;
    }
    let expected_revision = state.active_route().revision;
    history
        .switch_provider(state, expected_revision, provider_id, model, effort)
        .map_err(|error| error.to_string())
}

fn api_family_label(family: kloop_protocol::ProviderApiFamily) -> &'static str {
    match family {
        kloop_protocol::ProviderApiFamily::AnthropicMessages => "messages",
        kloop_protocol::ProviderApiFamily::OpenAiChatCompletions => "chat",
        kloop_protocol::ProviderApiFamily::OpenAiResponses => "responses",
        kloop_protocol::ProviderApiFamily::Mock => "mock",
    }
}
