use kloop_protocol::ReasoningEffort;
use kloop_protocol::RoutePickerStage;

use super::SlashResult;
use super::provider;
use crate::config::Config;
use crate::history::History;
use crate::provider_route::EffortRequest;
use crate::provider_route::SessionProviderState;
use crate::provider_route::SwitchOutcome;

pub const SUMMARY: &str = "show or set the session reasoning effort";

const USAGE: &str = "usage: /effort <level> | /effort unset (send no effort field at all)";

pub fn run(
    args: &str,
    history: &mut History,
    cfg: &Config,
    state: &SessionProviderState,
) -> SlashResult {
    let mut parts = args.split_whitespace();
    let Some(requested) = parts.next() else {
        return SlashResult::route(
            format!("{}\n{}\n{USAGE}", status_line(state), levels_line(state)),
            /*changed*/ false,
            Some(RoutePickerStage::Effort),
        );
    };
    if parts.next().is_some() {
        return SlashResult::message(USAGE);
    }
    let target = match provider::parse_level(requested) {
        Ok(level) => level,
        Err(error) => return SlashResult::message(format!("{error}\n{USAGE}")),
    };
    // The same commit every route change goes through, aimed at the route the
    // session is already on: the effort is the only thing that moves, so it
    // lands as one revision — the transcript says which effort each stretch of
    // the session ran at instead of only how it opened.
    let active = state.active_route();
    match provider::apply(
        history,
        cfg,
        state,
        &active.provider_id,
        Some(&active.model),
        EffortRequest::Set(target),
    ) {
        Ok(SwitchOutcome::NoOp(_)) => {
            SlashResult::message(format!("{}\n{USAGE}", status_line(state)))
        }
        // Sampling reads the effort off the frozen route, so the front-ends must
        // re-freeze `cfg` before the next turn — the same signal a provider
        // switch raises.
        Ok(SwitchOutcome::Changed { .. }) => {
            SlashResult::route(status_line(state), /*changed*/ true, None)
        }
        Err(reason) => SlashResult::message(format!("effort not changed: {reason}")),
    }
}

fn status_line(state: &SessionProviderState) -> String {
    let route = state.active_route();
    match state.effort() {
        Some(effort) => format!("effort: {effort} (provider {})", route.provider_id),
        None => format!(
            "effort: unset — no effort field is sent, the provider's own default applies (provider {})",
            route.provider_id
        ),
    }
}

/// What this model's picker would list, in words, for the front-ends that have
/// no picker. A declared list is the whole list — an undeclared level is refused
/// locally, so printing the other five would be advertising a local error.
fn levels_line(state: &SessionProviderState) -> String {
    let model = state.active_route().model;
    let declared = state.catalog().declared_efforts(&model);
    let levels = ReasoningEffort::choices(declared)
        .into_iter()
        .map(ReasoningEffort::choice_str)
        .collect::<Vec<_>>()
        .join(", ");
    match declared {
        Some(_) => format!("levels: {levels} — declared by model '{model}'"),
        None => format!(
            "levels: {levels} — '{model}' declares none, so a model accepts its own subset and names it if you miss"
        ),
    }
}
