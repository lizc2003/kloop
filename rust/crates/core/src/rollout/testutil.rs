use std::path::{Path, PathBuf};

use crate::usage::ProviderUsageRecord;
use kloop_protocol::{
    ContentBlock, ProviderRouteReceipt, ProviderRouteSource, ReasoningContinuity,
};
use serde_json::{Value, json};

pub(super) fn temp_file(tag: &str) -> PathBuf {
    std::env::temp_dir()
        .join(format!("kloop-rollout-{}-{tag}", std::process::id()))
        .join("session.jsonl")
}

pub(super) fn tool_use(id: &str) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.into(),
        name: "bash".into(),
        input: json!({"command": "ls"}),
    }
}

pub(super) fn tool_result(id: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id.into(),
        content: "ok".into(),
        is_error: false,
    }
}

pub(super) fn usage_record(_model: &str, input_tokens: u64) -> ProviderUsageRecord {
    ProviderUsageRecord {
        provider_id: "test".into(),
        api_family: kloop_protocol::ProviderApiFamily::Mock,
        route_revision: 1,
        model: "mock".into(),
        operation: crate::usage::UsageOperation::Sampling,
        usage: kloop_protocol::Usage {
            input_tokens,
            output_tokens: 2,
            cache_read_input_tokens: 3,
            cache_creation_input_tokens: 4,
        },
    }
}

pub(super) fn fixture_route(route_boundary: u64) -> ProviderRouteReceipt {
    ProviderRouteReceipt {
        route_revision: 1,
        route_boundary,
        source: ProviderRouteSource::Initial,
        provider_id: "test".into(),
        api_family: kloop_protocol::ProviderApiFamily::Mock,
        endpoint_fingerprint: kloop_provider::Provider::endpoint_fingerprint_for(
            kloop_protocol::ProviderApiFamily::Mock,
            "mock",
        ),
        model: "mock".into(),
        effort: None,
        continuity: ReasoningContinuity::Preserved,
    }
}

pub(super) fn cleanup(path: &Path) {
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

pub(super) fn raw_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
