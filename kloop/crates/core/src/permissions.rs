//! permissions — the rule-then-ask gate run before every tool execution.
//!
//! Layered verdict: read-only tools and read-only bash pass outright (the
//! same classification concurrency batching uses), then the allowlist, then
//! the per-session approval cache, and only what remains is put to the
//! [`Approver`]. A denial becomes an is_error tool_result — the model can
//! take another approach; the turn does not end.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::bail;
use anyhow::Result;
use serde_json::Value;

use crate::tools::bash_segments;
use crate::tools::segment_is_readonly;

/// An approver's answer to one confirmation request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Allow, and stop asking for this signature (tool name, or per bash
    /// command head) for the rest of the session.
    AllowSession,
    Deny,
}

/// The asking seam, separate from `Ui` so the streaming-output trait stays
/// synchronous. The type-erased future shape mirrors `execute_tool`: it keeps
/// the trait object-safe without an async-trait dependency.
pub trait Approver: Send + Sync {
    fn confirm(&self, description: String) -> Pin<Box<dyn Future<Output = Decision> + Send + '_>>;
}

/// One allowlist entry: a bare tool name pre-approves the whole tool, and
/// `bash(<pattern>)` pre-approves bash command segments whose leading tokens
/// equal the pattern's (a trailing `*` matches any remainder).
#[derive(Clone, Debug, PartialEq, Eq)]
enum AllowRule {
    Tool(String),
    Bash { tokens: Vec<String>, wildcard: bool },
}

impl AllowRule {
    fn matches_tool(&self, name: &str) -> bool {
        matches!(self, AllowRule::Tool(t) if t == name)
    }

    fn matches_bash_segment(&self, segment: &str) -> bool {
        match self {
            AllowRule::Tool(t) => t == "bash",
            AllowRule::Bash { tokens, wildcard } => {
                let seg_tokens: Vec<&str> = segment.split_whitespace().collect();
                if *wildcard {
                    seg_tokens.len() >= tokens.len()
                        && tokens.iter().zip(&seg_tokens).all(|(p, s)| p == s)
                } else {
                    seg_tokens.len() == tokens.len()
                        && tokens.iter().zip(&seg_tokens).all(|(p, s)| p == s)
                }
            }
        }
    }
}

fn parse_rule(entry: &str) -> Result<AllowRule> {
    if let Some(inner) = entry
        .strip_prefix("bash(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let mut tokens: Vec<String> = inner.split_whitespace().map(str::to_string).collect();
        let wildcard = tokens.last().is_some_and(|t| t == "*");
        if wildcard {
            tokens.pop();
        }
        if tokens.is_empty() {
            bail!("allow rule 'bash({inner})': empty command pattern");
        }
        Ok(AllowRule::Bash { tokens, wildcard })
    } else if !entry.is_empty() && entry.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(AllowRule::Tool(entry.to_string()))
    } else {
        bail!("allow rule '{entry}': expected a tool name or bash(<pattern>)");
    }
}

pub struct Permissions {
    allow_everything: bool,
    rules: Vec<AllowRule>,
    session: Mutex<HashSet<String>>,
    approver: Option<Arc<dyn Approver>>,
}

impl Permissions {
    /// No gating at all (`--yolo`, `--mock`, tests).
    pub fn allow_all() -> Self {
        Permissions {
            allow_everything: true,
            rules: Vec::new(),
            session: Mutex::new(HashSet::new()),
            approver: None,
        }
    }

    /// `allow` is a comma-separated rule list (the `AGENT_ALLOW` format),
    /// e.g. `"write_file,bash(cargo *)"`; empty entries are skipped.
    pub fn new(allow: &str, approver: Arc<dyn Approver>) -> Result<Self> {
        let rules = allow
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(parse_rule)
            .collect::<Result<Vec<_>>>()?;
        Ok(Permissions {
            allow_everything: false,
            rules,
            session: Mutex::new(HashSet::new()),
            approver: Some(approver),
        })
    }

    /// Whether this tool call may run. Rules first, then the session cache,
    /// then the approver; without an approver whatever is left is denied.
    pub async fn check(&self, name: &str, input: &Value, depth: u8) -> bool {
        if self.allow_everything || self.preapproved(name, input) {
            return true;
        }
        let signatures = signatures(name, input);
        if self.cached(&signatures) {
            return true;
        }
        let Some(approver) = &self.approver else {
            return false;
        };
        match approver.confirm(describe(name, input, depth)).await {
            Decision::Allow => true,
            Decision::AllowSession => {
                self.session.lock().unwrap().extend(signatures);
                true
            }
            Decision::Deny => false,
        }
    }

    fn preapproved(&self, name: &str, input: &Value) -> bool {
        match name {
            "read_file" | "read_offloaded" => true,
            // task itself touches nothing; every tool call the sub-agent
            // makes passes through this same gate.
            "task" => true,
            "bash" => {
                let Some(cmd) = input["command"].as_str() else {
                    return false;
                };
                let segments = bash_segments(cmd);
                !segments.is_empty()
                    && segments.iter().all(|seg| {
                        segment_is_readonly(seg)
                            || self.rules.iter().any(|r| r.matches_bash_segment(seg))
                    })
            }
            other => self.rules.iter().any(|r| r.matches_tool(other)),
        }
    }

    fn cached(&self, signatures: &[String]) -> bool {
        let session = self.session.lock().unwrap();
        signatures.iter().all(|s| session.contains(s))
    }
}

/// AllowSession granularity: the tool name, except bash which caches one
/// entry per command-segment head so `cargo build && rm x` never rides on a
/// remembered `cargo`. Never empty — an empty list would vacuously pass
/// `cached()`.
fn signatures(name: &str, input: &Value) -> Vec<String> {
    if name == "bash" {
        if let Some(cmd) = input["command"].as_str() {
            let heads: Vec<String> = bash_segments(cmd)
                .iter()
                .filter_map(|seg| seg.split_whitespace().next())
                .map(|head| format!("bash:{head}"))
                .collect();
            if !heads.is_empty() {
                return heads;
            }
        }
    }
    vec![name.to_string()]
}

fn describe(name: &str, input: &Value, depth: u8) -> String {
    let prefix = if depth > 0 { "[sub-agent] " } else { "" };
    let detail: String = match name {
        "bash" => input["command"].as_str().unwrap_or("?").to_string(),
        "write_file" | "edit_file" => input["path"].as_str().unwrap_or("?").to_string(),
        _ => input.to_string(),
    }
    .chars()
    .take(200)
    .collect();
    format!("{prefix}{name}: {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Pops one scripted decision per confirm (Deny once exhausted) and
    /// records every description it was asked.
    struct ScriptedApprover {
        script: Mutex<Vec<Decision>>,
        asked: Mutex<Vec<String>>,
    }

    impl ScriptedApprover {
        fn new(script: Vec<Decision>) -> Arc<Self> {
            Arc::new(ScriptedApprover {
                script: Mutex::new(script),
                asked: Mutex::new(Vec::new()),
            })
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Approver for ScriptedApprover {
        fn confirm(
            &self,
            description: String,
        ) -> Pin<Box<dyn Future<Output = Decision> + Send + '_>> {
            self.asked.lock().unwrap().push(description);
            let mut script = self.script.lock().unwrap();
            let decision = if script.is_empty() {
                Decision::Deny
            } else {
                script.remove(0)
            };
            Box::pin(async move { decision })
        }
    }

    fn gate(allow: &str, approver: Arc<ScriptedApprover>) -> Permissions {
        Permissions::new(allow, approver).unwrap()
    }

    #[tokio::test]
    async fn read_only_tools_and_bash_skip_the_approver() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate("", approver.clone());
        assert!(p.check("read_file", &json!({"path": "x"}), 0).await);
        assert!(p.check("read_offloaded", &json!({"id": "off-1"}), 0).await);
        assert!(p.check("task", &json!({"prompt": "go"}), 0).await);
        assert!(
            p.check("bash", &json!({"command": "git status && ls | wc -l"}), 0)
                .await
        );
        assert_eq!(approver.asked(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn allowlist_matches_tools_and_bash_prefixes() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate("write_file, bash(cargo *)", approver.clone());
        // Bare tool name pre-approves the whole tool.
        assert!(p.check("write_file", &json!({"path": "x"}), 0).await);
        // Prefix pattern; chaining with a read-only segment stays allowed.
        assert!(
            p.check("bash", &json!({"command": "cargo test --all"}), 0)
                .await
        );
        assert!(
            p.check("bash", &json!({"command": "cargo build && ls"}), 0)
                .await
        );
        assert_eq!(approver.asked(), Vec::<String>::new());

        // One unlisted segment poisons the whole command; unlisted tools ask.
        assert!(
            !p.check("bash", &json!({"command": "cargo build && rm -rf x"}), 0)
                .await
        );
        assert!(!p.check("edit_file", &json!({"path": "x"}), 0).await);
        assert_eq!(approver.asked().len(), 2);
    }

    #[tokio::test]
    async fn bash_rule_without_wildcard_is_exact() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate("bash(make)", approver.clone());
        assert!(p.check("bash", &json!({"command": "make"}), 0).await);
        assert!(!p.check("bash", &json!({"command": "make clean"}), 0).await);
    }

    #[tokio::test]
    async fn bare_bash_rule_allows_any_command() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate("bash", approver.clone());
        assert!(
            p.check("bash", &json!({"command": "rm -rf /tmp/x"}), 0)
                .await
        );
        assert_eq!(approver.asked(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn allow_session_caches_by_command_head() {
        let approver = ScriptedApprover::new(vec![Decision::AllowSession]);
        let p = gate("", approver.clone());
        assert!(p.check("bash", &json!({"command": "cargo build"}), 0).await);
        // Same head: served from the cache, no second question.
        assert!(p.check("bash", &json!({"command": "cargo test"}), 0).await);
        assert_eq!(approver.asked().len(), 1);
        // Different head: asked again (script exhausted → deny).
        assert!(!p.check("bash", &json!({"command": "rm x"}), 0).await);
        // A chain mixing a cached head with an uncached one is not covered.
        assert!(
            !p.check("bash", &json!({"command": "cargo build && rm x"}), 0)
                .await
        );
        assert_eq!(approver.asked().len(), 3);
    }

    #[tokio::test]
    async fn plain_allow_does_not_cache() {
        let approver = ScriptedApprover::new(vec![Decision::Allow, Decision::Allow]);
        let p = gate("", approver.clone());
        assert!(p.check("write_file", &json!({"path": "x"}), 0).await);
        assert!(p.check("write_file", &json!({"path": "x"}), 0).await);
        assert_eq!(approver.asked().len(), 2);
    }

    #[tokio::test]
    async fn description_carries_depth_and_detail() {
        let approver = ScriptedApprover::new(vec![Decision::Deny, Decision::Deny]);
        let p = gate("", approver.clone());
        p.check("bash", &json!({"command": "rm x"}), 1).await;
        p.check("write_file", &json!({"path": "a.txt", "content": "hi"}), 0)
            .await;
        assert_eq!(
            approver.asked(),
            vec!["[sub-agent] bash: rm x", "write_file: a.txt"]
        );
    }

    #[tokio::test]
    async fn malformed_bash_input_asks_instead_of_passing() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate("bash", approver.clone());
        // No command string: rules can't vouch for it, so it goes to ask.
        assert!(!p.check("bash", &json!({}), 0).await);
        assert_eq!(approver.asked().len(), 1);
    }

    #[tokio::test]
    async fn allow_all_never_consults_anything() {
        let p = Permissions::allow_all();
        assert!(p.check("bash", &json!({"command": "rm -rf /"}), 0).await);
        assert!(p.check("write_file", &json!({"path": "x"}), 0).await);
    }

    #[test]
    fn rule_parsing_accepts_valid_and_rejects_malformed() {
        assert_eq!(
            parse_rule("write_file").unwrap(),
            AllowRule::Tool("write_file".into())
        );
        assert_eq!(
            parse_rule("bash(cargo *)").unwrap(),
            AllowRule::Bash {
                tokens: vec!["cargo".into()],
                wildcard: true,
            }
        );
        assert_eq!(
            parse_rule("bash(git push origin)").unwrap(),
            AllowRule::Bash {
                tokens: vec!["git".into(), "push".into(), "origin".into()],
                wildcard: false,
            }
        );
        assert!(parse_rule("bash()").is_err());
        assert!(parse_rule("bash(*)").is_err());
        assert!(parse_rule("write file").is_err());
        assert!(parse_rule("edit_file(x)").is_err());

        assert!(
            Permissions::new("write_file, ,bash(cargo *)", ScriptedApprover::new(vec![])).is_ok()
        );
        assert!(Permissions::new("nope!", ScriptedApprover::new(vec![])).is_err());
    }
}
