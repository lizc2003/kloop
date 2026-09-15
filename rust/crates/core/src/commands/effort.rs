use kloop_protocol::ReasoningEffort;

use super::SlashResult;
use crate::config::Config;
use crate::history::History;
use crate::provider_route::SessionProviderState;

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
    // A revision can only follow one, and setting the effort before the first
    // turn is the normal way to start a session — so open the timeline here if
    // sampling has not already done it. Guarded on emptiness rather than left to
    // `ensure`'s own idempotence: once the timeline has moved past `cfg`'s
    // frozen revision, `ensure` reads that as a mismatch and refuses.
    if !history.has_provider_route()
        && let Err(error) = history.ensure_initial_provider_route(&cfg.provider_route)
    {
        return SlashResult::message(format!("effort not changed: {error}"));
    }
    // Recorded as a route revision, so the transcript says which effort each
    // stretch of the session ran at instead of only how it opened.
    let committed = state.commit_effort(target, |next| {
        history
            .append_provider_route_changed(
                next,
                kloop_protocol::ProviderRouteSource::ExplicitSwitch,
                next.continuity(),
            )
            .map(|_| ())
    });
    if let Err(error) = committed {
        return SlashResult::message(format!("effort not changed: {error}"));
    }
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
