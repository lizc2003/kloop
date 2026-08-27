use kloop_protocol::ReasoningEffort;

use super::SlashResult;
use crate::provider_route::SessionProviderState;

pub const SUMMARY: &str = "show or set the session reasoning effort";

const USAGE: &str = "usage: /effort <level> | /effort off";

pub fn run(args: &str, state: &SessionProviderState) -> SlashResult {
    let mut parts = args.split_whitespace();
    let Some(requested) = parts.next() else {
        return SlashResult::message(format!("{}\n{USAGE}", status_line(state)));
    };
    if parts.next().is_some() {
        return SlashResult::message(USAGE);
    }
    // `off` is kloop's word for "send no effort field at all" — it is not a
    // provider level, so it is matched before the level vocabulary.
    let target = if requested.eq_ignore_ascii_case("off") {
        None
    } else {
        match requested.parse::<ReasoningEffort>() {
            Ok(effort) => Some(effort),
            Err(error) => {
                return SlashResult::message(format!("{error}\n{}", accepted_line(state)));
            }
        }
    };
    if state.effort() == target {
        return SlashResult::message(format!("{}\n{USAGE}", status_line(state)));
    }
    match state.set_effort(target) {
        // Sampling reads the effort off the frozen route, so the front-ends
        // must re-freeze `cfg` before the next turn — same signal a provider
        // switch raises.
        Ok(()) => SlashResult::route(
            status_line(state),
            /*changed*/ true,
            /*open_picker*/ false,
        ),
        Err(error) => SlashResult::message(error.to_string()),
    }
}

fn status_line(state: &SessionProviderState) -> String {
    let route = state.active_route();
    match state.effort() {
        Some(effort) => format!(
            "effort: {effort} (provider {}, accepted: {})",
            route.provider_id,
            ReasoningEffort::join(state.accepted_efforts())
        ),
        None => format!(
            "effort: off — no effort field is sent, the provider's own default applies (provider {}, accepted: {})",
            route.provider_id,
            ReasoningEffort::join(state.accepted_efforts())
        ),
    }
}

fn accepted_line(state: &SessionProviderState) -> String {
    format!(
        "provider '{}' accepts: {} (or 'off')",
        state.active_route().provider_id,
        ReasoningEffort::join(state.accepted_efforts())
    )
}
