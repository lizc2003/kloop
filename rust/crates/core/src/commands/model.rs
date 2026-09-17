//! `/model` — the middle entry point into the route picker: change the model
//! without leaving the provider, and optionally the effort in the same breath.

use kloop_protocol::ReasoningEffort;
use kloop_protocol::RoutePickerStage;

use super::SlashResult;
use super::provider;
use crate::config::Config;
use crate::history::History;
use crate::provider_route::EffortRequest;
use crate::provider_route::SessionProviderState;
use crate::provider_route::SwitchOutcome;

pub const SUMMARY: &str = "show or switch the model on the current provider";

const USAGE: &str = "usage: /model <model> [effort]";

pub fn run(
    args: &str,
    history: &mut History,
    cfg: &Config,
    state: &SessionProviderState,
) -> SlashResult {
    let active = state.active_route();
    let mut parts = args.split_whitespace();
    let Some(model) = parts.next() else {
        // A front-end without a picker ignores the stage and shows this, so the
        // list has to be here rather than a "pick one in the TUI" apology.
        let mut lines = vec![format!(
            "active: {} {} (revision {})",
            active.provider_id, active.model, active.revision
        )];
        match state.catalog().descriptor(&active.provider_id) {
            Some(descriptor) => {
                lines.push(format!("models on provider '{}':", descriptor.id));
                for model in &descriptor.models {
                    let mut row = format!("  {model}");
                    if model == &descriptor.default_model {
                        row.push_str(" (default)");
                    }
                    lines.push(row);
                }
            }
            None => lines.push(format!(
                "provider '{}' is no longer in the catalog",
                active.provider_id
            )),
        }
        lines.push(USAGE.into());
        return SlashResult::route(
            lines.join("\n"),
            /*changed*/ false,
            Some(RoutePickerStage::Model),
        );
    };
    let effort = match parts.next().map(provider::parse_level).transpose() {
        Ok(level) => level.map_or(EffortRequest::Inherit, EffortRequest::Set),
        Err(error) => return SlashResult::message(format!("{error}\n{USAGE}")),
    };
    if parts.next().is_some() {
        return SlashResult::message(USAGE);
    }
    match provider::apply(
        history,
        cfg,
        state,
        &active.provider_id,
        Some(model),
        effort,
    ) {
        Ok(SwitchOutcome::NoOp(route)) => SlashResult::route(
            format!(
                "model unchanged: {} (provider {}, revision {})",
                route.primary_model(),
                route.provider_id(),
                route.revision()
            ),
            /*changed*/ false,
            None,
        ),
        Ok(SwitchOutcome::Changed { route, continuity }) => SlashResult::route(
            format!(
                "model switched: {} (provider {}, revision {}, reasoning continuity: {continuity:?}, effort: {})",
                route.primary_model(),
                route.provider_id(),
                route.revision(),
                ReasoningEffort::choice_str(route.effort()),
            ),
            /*changed*/ true,
            None,
        ),
        Err(reason) => SlashResult::message(format!("model switch failed: {reason}")),
    }
}
