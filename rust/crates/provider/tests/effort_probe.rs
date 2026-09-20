//! Temporary diagnostic (plan 102): ask each live endpoint which effort levels
//! it actually accepts, instead of guessing a per-rail table.
use std::sync::Arc;

use kloop_protocol::Message;
use kloop_protocol::ProviderApiFamily;
use kloop_provider::Credential;
use kloop_provider::Provider;
use kloop_provider::Reasoning;

fn label(level: Option<kloop_protocol::ReasoningEffort>) -> &'static str {
    level.map_or("(nofield)", kloop_protocol::ReasoningEffort::as_str)
}

fn env(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| panic!("set {name}"))
}

#[tokio::test]
#[ignore = "diagnostic against a live endpoint"]
async fn probe_effort_levels() {
    let rail = env("KLOOP_PROVIDER");
    let (family, key, base, model) = match rail.as_str() {
        "anthropic" => (
            ProviderApiFamily::AnthropicMessages,
            env("ANTHROPIC_API_KEY"),
            env("ANTHROPIC_BASE_URL"),
            env("ANTHROPIC_MODEL"),
        ),
        "openai" => (
            ProviderApiFamily::OpenAiChatCompletions,
            env("OPENAI_API_KEY"),
            env("OPENAI_BASE_URL"),
            env("OPENAI_MODEL"),
        ),
        _ => (
            ProviderApiFamily::OpenAiResponses,
            env("OPENAI_API_KEY"),
            env("OPENAI_BASE_URL"),
            env("OPENAI_MODEL"),
        ),
    };
    let base = base.trim_end_matches('/').to_string();
    println!("== {rail} / {model} ==");
    let mut rows: Vec<Option<kloop_protocol::ReasoningEffort>> = vec![None];
    rows.extend(
        kloop_protocol::ReasoningEffort::ALL
            .iter()
            .copied()
            .map(Some),
    );
    for level in rows {
        // A throttled proxy answers 429 for every row and tells us nothing;
        // space the probes out and keep a no-field control at the top.
        tokio::time::sleep(std::time::Duration::from_secs(45)).await;
        let provider = Arc::new(match family {
            ProviderApiFamily::AnthropicMessages => Provider::Anthropic {
                cred: Credential::api_key(key.clone()),
                base: base.clone(),
                prompt_cache: false,
            },
            ProviderApiFamily::OpenAiChatCompletions => Provider::OpenAiCompat {
                cred: Credential::bearer(key.clone()),
                base: base.clone(),
            },
            _ => Provider::OpenAiResponses {
                cred: Credential::bearer(key.clone()),
                base: base.clone(),
            },
        });
        let attempt = provider.attempt_identity("probe", 1, &model);
        let mut rx = provider.stream_attempt(
            &attempt,
            Reasoning::new(level, kloop_provider::ThinkingMode::Unset),
            /*cache_key*/ None,
            "Reply with OK.",
            &[Message::user_text("hi")],
            &[],
        );
        let mut failure = None;
        while let Some(event) = rx.recv().await {
            if let Err(error) = event {
                failure = Some(error.to_string());
                break;
            }
        }
        match failure {
            None => println!("  {:<8} ACCEPTED", label(level)),
            Some(error) => {
                let error = error.replace('\n', " ");
                println!(
                    "  {:<8} REJECTED {}",
                    label(level),
                    &error[..error.len().min(240)]
                );
            }
        }
    }
}
