use std::sync::Arc;

use super::SlashResult;
use crate::config::Config;
use crate::history::History;
use crate::provider_route::SessionProviderState;
use crate::provider_route::SwitchOutcome;

pub const SUMMARY: &str = "show or switch the session provider and model";

pub fn run(
    args: &str,
    history: &mut History,
    cfg: &Arc<Config>,
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
        for descriptor in cfg.provider_catalog.descriptors() {
            lines.push(format!(
                "  {} [{}] default={} models={} availability={:?}",
                descriptor.id,
                api_family_label(descriptor.api_family),
                descriptor.default_model,
                descriptor.models.join(","),
                descriptor.availability,
            ));
        }
        lines.push("usage: /provider <provider> [model]".into());
        return SlashResult::provider(lines.join("\n"), false, true);
    };
    let model = parts.next();
    if parts.next().is_some() {
        return SlashResult::message("usage: /provider <provider> [model]");
    }
    let expected_revision = state.active_route().revision;
    match history.switch_provider(state, expected_revision, provider_id, model) {
        Ok(SwitchOutcome::NoOp(route)) => SlashResult::provider(
            format!(
                "provider unchanged: {} {} (revision {})",
                route.provider_id(),
                route.primary_model(),
                route.revision()
            ),
            false,
            false,
        ),
        Ok(SwitchOutcome::Changed { route, continuity }) => SlashResult::provider(
            format!(
                "provider switched: {} {} (revision {}, reasoning continuity: {continuity:?})",
                route.provider_id(),
                route.primary_model(),
                route.revision()
            ),
            true,
            false,
        ),
        Err(error) => SlashResult::message(format!("provider switch failed: {error}")),
    }
}

fn api_family_label(family: kloop_protocol::ProviderApiFamily) -> &'static str {
    match family {
        kloop_protocol::ProviderApiFamily::AnthropicMessages => "anthropic",
        kloop_protocol::ProviderApiFamily::OpenAiChatCompletions => "chat",
        kloop_protocol::ProviderApiFamily::OpenAiResponses => "responses",
        kloop_protocol::ProviderApiFamily::Mock => "mock",
    }
}
