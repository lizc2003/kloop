use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::Result;
use anyhow::bail;
use kloop_protocol::ContentBlock;
use serde::Deserialize;
use serde_json::json;
use url::Url;

use super::ToolCtx;

mod flow;
pub(crate) use flow::{Flow, State, carry, latest_user_text};

#[cfg(test)]
mod tests;

static NEXT_LOOKUP: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, PartialEq, Eq)]
struct PullRequest {
    url: Url,
    api: Url,
}

impl PullRequest {
    fn parse(text: &str) -> Option<Self> {
        let mut url = Url::parse(text).ok()?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return None;
        }
        let parts: Vec<_> = url
            .path_segments()?
            .filter(|part| !part.is_empty())
            .collect();
        let [prefix @ .., owner, repo, kind, number] = parts.as_slice() else {
            return None;
        };
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let api = match (*kind, url.host_str()?) {
            ("pull", "github.com") if prefix.is_empty() => Url::parse(&format!(
                "https://api.github.com/repos/{owner}/{repo}/pulls/{number}"
            ))
            .ok()?,
            ("pulls", _) => {
                let mut api = url.clone();
                let prefix = prefix.join("/");
                let prefix = if prefix.is_empty() {
                    prefix
                } else {
                    format!("/{prefix}")
                };
                api.set_path(&format!(
                    "{prefix}/api/v1/repos/{owner}/{repo}/pulls/{number}"
                ));
                api.set_query(None);
                api.set_fragment(None);
                api
            }
            _ => return None,
        };
        url.set_query(None);
        url.set_fragment(None);
        Some(Self { url, api })
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Revision {
    #[serde(rename = "ref")]
    branch: String,
    sha: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Metadata {
    head: Revision,
    base: Revision,
}

impl Metadata {
    fn parse(text: &str) -> Result<Self> {
        let value: Self = serde_json::from_str(text)?;
        for revision in [&value.head, &value.base] {
            if !matches!(revision.sha.len(), 40 | 64)
                || !revision.sha.bytes().all(|byte| byte.is_ascii_hexdigit())
                || revision.branch.is_empty()
                || revision
                    .branch
                    .chars()
                    .any(|character| character.is_control() || character.is_whitespace())
            {
                bail!("PR metadata contains an invalid revision");
            }
        }
        Ok(value)
    }
}

pub(super) async fn prepare(arguments: &str, body: String, ctx: &ToolCtx) -> Result<String> {
    let targets: Vec<_> = arguments
        .split_whitespace()
        .filter_map(|word| {
            PullRequest::parse(
                word.trim_matches(['`', '\'', '"', '(', ')', '[', ']', '<', '>', ',', '。']),
            )
        })
        .collect();
    let [target] = targets.as_slice() else {
        return Ok(body);
    };
    let result = super::run_one(
        format!(
            "review-metadata-{}",
            NEXT_LOOKUP.fetch_add(1, Ordering::Relaxed)
        ),
        "web_fetch".into(),
        json!({"url": target.api.as_str()}),
        ctx.clone(),
        None,
    )
    .await;
    if ctx.cancel.is_cancelled() || ctx.stop.is_cancelled() {
        bail!("PR metadata lookup stopped");
    }
    let metadata = match result {
        ContentBlock::ToolResult {
            content,
            is_error: false,
            ..
        } => Metadata::parse(&content.as_text()).ok(),
        _ => None,
    };
    if let Some(metadata) = metadata {
        *ctx.review_update
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(State::Resolve {
            pr: target.url.to_string(),
            branch: metadata.base.branch.clone(),
            revisions: Some((metadata.head.sha.clone(), metadata.base.sha.clone())),
        });
        let resolved = json!({
            "pr": target.url.as_str(),
            "head": metadata.head.sha,
            "target_branch": metadata.base.branch,
            "target_tip": metadata.base.sha,
        });
        return Ok(format!(
            "{body}\n\nResolved PR metadata (read-only lookup):\n{resolved}\nVerify the repository remote matches this PR, compute the merge-base against these frozen SHAs, and read that revision throughout."
        ));
    }
    *ctx.review_update
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(State::Awaiting {
        pr: target.url.to_string(),
    });
    ctx.return_to_user(format!(
        "无法从 PR 元数据确认实际目标分支：{}。这个 PR 的目标分支是哪个？",
        target.url,
    ));
    Ok(format!(
        "{body}\n\nPR metadata did not establish the target branch. The turn has returned this missing-input question to the user; no review range has been selected. Resume from their answer, then resolve and freeze the head, target tip and merge-base."
    ))
}
