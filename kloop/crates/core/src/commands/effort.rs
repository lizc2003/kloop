use kloop_protocol::ReasoningEffort;

use super::SlashResult;
use crate::provider_route::SessionProviderState;

pub const SUMMARY: &str = "show or set the session reasoning effort";

const USAGE: &str = "usage: /effort <level> | /effort unset (send no effort field at all)";

pub fn run(args: &str, state: &SessionProviderState) -> SlashResult {
    let mut parts = args.split_whitespace();
    let Some(requested) = parts.next() else {
        return SlashResult::message(format!(
            "{}\n{}\n{USAGE}",
            status_line(state),
            levels_line()
        ));
    };
    if parts.next().is_some() {
        return SlashResult::message(USAGE);
    }
    // `unset` is kloop's word for "send no effort field at all" — not a wire
    // value, so it is matched before the level vocabulary. Deliberately not
    // spelled `off`: `none` is a real level meaning "do no reasoning", and the
    // two would read as synonyms.
    let target = if requested.eq_ignore_ascii_case("unset") {
        None
    } else {
        match requested.parse::<ReasoningEffort>() {
            Ok(effort) => Some(effort),
            Err(error) => {
                return SlashResult::message(format!("{error}\n{USAGE}"));
            }
        }
    };
    if state.effort() == target {
        return SlashResult::message(format!("{}\n{USAGE}", status_line(state)));
    }
    state.set_effort(target);
    // Sampling reads the effort off the frozen route, so the front-ends must
    // re-freeze `cfg` before the next turn — the same signal a provider switch
    // raises.
    SlashResult::route(
        status_line(state),
        /*changed*/ true,
        /*open_picker*/ false,
    )
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

/// Which levels a model actually takes is the model's own contract, not the
/// rail's, so kloop lists its vocabulary and lets the provider's error (which
/// names the supported values) settle the rest.
fn levels_line() -> String {
    format!(
        "levels: {} — a model accepts its own subset and names it if you miss",
        ReasoningEffort::join(ReasoningEffort::ALL)
    )
}
