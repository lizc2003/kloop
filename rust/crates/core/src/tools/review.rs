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

#[derive(Clone, Debug, PartialEq, Eq)]
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

fn targets(text: &str) -> Vec<PullRequest> {
    text.split(|character: char| {
        character.is_whitespace() || matches!(character, '，' | ',' | '。' | ';' | '；' | '：')
    })
    .filter_map(|word| {
        PullRequest::parse(
            word.trim_matches(['`', '\'', '"', '(', ')', '[', ']', '<', '>', ',', '。']),
        )
    })
    .collect()
}

pub(crate) fn known_targets<'a>(
    messages: impl IntoIterator<Item = &'a kloop_protocol::Message>,
) -> Vec<(String, String)> {
    let mut current = None;
    let mut known = std::collections::BTreeMap::new();
    for message in messages {
        if message.role != kloop_protocol::Role::User || message.injected.is_some() {
            continue;
        }
        let [ContentBlock::Text { text }] = message.content.as_slice() else {
            continue;
        };
        let found = targets(text);
        match found.as_slice() {
            [target] => current = Some(target.clone()),
            [] => {}
            _ => {
                current = None;
                continue;
            }
        }
        let Some(pr) = &current else { continue };
        for clause in text.split(['\n', '，', ',', '。', ';', '；']) {
            let clause = clause.trim();
            let value = clause
                .strip_prefix("目标分支是")
                .or_else(|| clause.strip_prefix("目标分支为"))
                .or_else(|| clause.strip_prefix("target branch is "))
                .or_else(|| clause.strip_prefix("target branch: "))
                .or_else(|| {
                    clause
                        .strip_prefix("是")
                        .and_then(|value| value.strip_suffix("分支"))
                });
            if let Some(branch) = value.and_then(flow::branch) {
                known.insert(pr.api.to_string(), (pr.url.to_string(), branch));
            }
        }
    }
    known.into_values().collect()
}

async fn authenticated_metadata(target: &PullRequest, ctx: &ToolCtx) -> Result<Metadata> {
    let remote = flow::git(ctx, "git config --get remote.origin.url".into()).await?;
    if !flow::same_repository(&remote, target) {
        bail!("origin does not match the requested PR repository")
    }
    let text = flow::git(ctx, format!(
        "if command -v gh >/dev/null 2>&1; then GH_PROMPT_DISABLED=1 gh api --method GET {}; else printf '%s\\n' 'No authenticated metadata client is available.' >&2; exit 1; fi",
        flow::quote(target.api.as_str()),
    )).await?;
    Metadata::parse(text.trim())
}

pub(super) async fn prepare(arguments: &str, body: String, ctx: &ToolCtx) -> Result<String> {
    let targets = targets(arguments);
    let [target] = targets.as_slice() else {
        return Ok(body);
    };
    if let Some((_, branch)) = ctx
        .review_targets
        .iter()
        .find(|(pr, _)| PullRequest::parse(pr).is_some_and(|known| known.api == target.api))
    {
        *ctx.review_update
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(State::Resolve {
            pr: target.url.to_string(),
            branch: branch.clone(),
            revisions: None,
        });
        return Ok(format!(
            "{body}\n\nThe user already specified target branch {branch:?} for this PR. Continue native preparation from that fact; do not ask again or substitute the current branch."
        ));
    }
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
    let metadata = match metadata {
        Some(metadata) => Ok(metadata),
        None => authenticated_metadata(target, ctx).await,
    };
    if ctx.cancel.is_cancelled() || ctx.stop.is_cancelled() {
        bail!("PR metadata lookup stopped");
    }
    let failure = match metadata {
        Ok(metadata) => {
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
        Err(error) => error.to_string(),
    };
    *ctx.review_update
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(State::Awaiting {
        pr: target.url.to_string(),
    });
    ctx.return_to_user(format!(
        "已尝试 PR 元数据和现有认证访问，仍无法确认实际目标分支：{}。访问结果：{failure}。请提供这个 PR 的实际目标分支。",
        target.url
    ));
    Ok(format!(
        "{body}\n\nPR metadata did not establish the target branch. The turn has returned this missing-input question to the user; no review range has been selected. Resume from their answer, then resolve and freeze the head, target tip and merge-base."
    ))
}
