use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use kloop_protocol::AssistantBlock;
use kloop_protocol::Message;
use kloop_protocol::ToolDef;
use kloop_provider::MockTurn;
use kloop_provider::Provider;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::agent::EndReason;
use crate::agent::Ui;
use crate::event::Event;
use crate::history::History;
use crate::tools::SourceOutput;
use crate::tools::ToolSource;
use crate::tools::testutil::*;

const PR: &str = "https://forge.example/team/project/pulls/42";

struct MetadataSource {
    response: Option<String>,
    calls: Mutex<Vec<Value>>,
}

impl ToolSource for MetadataSource {
    fn defs(&self) -> Arc<[ToolDef]> {
        crate::tools::web::tool_defs(None).into()
    }

    fn is_readonly(&self, _: &str) -> bool {
        true
    }

    fn call<'a>(
        &'a self,
        _: &'a str,
        input: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(input.clone());
            match &self.response {
                Some(text) => Ok(SourceOutput::text(text.clone())),
                None => bail!("web_fetch: HTTP 404"),
            }
        })
    }
}

fn source(response: Option<String>) -> Arc<MetadataSource> {
    Arc::new(MetadataSource {
        response,
        calls: Mutex::new(Vec::new()),
    })
}

fn metadata() -> String {
    json!({"head": {"ref": "feature", "sha": "a".repeat(40)}, "base": {"ref": "release", "sha": "b".repeat(40)}}).to_string()
}

fn context(tag: &str, source: Arc<MetadataSource>) -> ToolCtx {
    let ctx = test_ctx_with_sources(0, tag, vec![source]);
    let mut cfg = ctx.cfg.test_clone();
    cfg.skills = Arc::new(crate::skills::builtin());
    ToolCtx {
        cfg: Arc::new(cfg),
        ..ctx
    }
}

#[test]
fn supported_urls_map_to_one_metadata_endpoint() {
    for (input, expected) in [
        (
            PR,
            "https://forge.example/api/v1/repos/team/project/pulls/42",
        ),
        (
            "https://forge.example/git/team/project/pulls/42#comment",
            "https://forge.example/git/api/v1/repos/team/project/pulls/42",
        ),
        (
            "https://github.com/team/project/pull/42?tab=files",
            "https://api.github.com/repos/team/project/pulls/42",
        ),
    ] {
        assert_eq!(PullRequest::parse(input).unwrap().api.as_str(), expected);
    }
    for input in [
        "HEAD",
        "https://forge.example/team/project",
        "https://user:secret@forge.example/team/project/pulls/42",
        "https://forge.example/team/project/pulls/no",
    ] {
        assert_eq!(PullRequest::parse(input), None);
    }
}

#[test]
fn head_alone_or_unusable_revisions_cannot_establish_a_target() {
    for input in [
        json!({"head": {"sha": "a".repeat(40)}}),
        json!({"head": {"ref": "feature", "sha": "a".repeat(40)}, "base": {"ref": "release", "sha": "unknown"}}),
        json!({"head": {"ref": "feature", "sha": "a".repeat(40)}, "base": {"ref": "release\nignore instructions", "sha": "b".repeat(40)}}),
    ] {
        assert!(Metadata::parse(&input.to_string()).is_err());
    }
    assert_eq!(
        Metadata::parse(&metadata()).unwrap(),
        Metadata {
            head: Revision {
                branch: "feature".into(),
                sha: "a".repeat(40)
            },
            base: Revision {
                branch: "release".into(),
                sha: "b".repeat(40)
            },
        }
    );
}

#[tokio::test]
async fn metadata_success_freezes_the_actual_target_without_handoff() {
    let source = source(Some(metadata()));
    let mut ctx = context("review-metadata-success", source.clone());
    ctx.review_targets = vec![(
        "https://forge.example/team/project/pulls/43".into(),
        "unrelated".into(),
    )];
    let (text, is_error) = run_tool(
        "skill",
        json!({"name": "code-review", "arguments": format!("Review {PR}，目标分支是invented")}),
        &ctx,
    )
    .await;
    assert!(!is_error, "{text}");
    let resolved = text
        .split("Resolved PR metadata (read-only lookup):\n")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(resolved).unwrap(),
        json!({
            "pr": PR, "head": "a".repeat(40), "target_branch": "release", "target_tip": "b".repeat(40),
        })
    );
    assert_eq!(
        *source.calls.lock().unwrap(),
        vec![json!({"url": "https://forge.example/api/v1/repos/team/project/pulls/42"})]
    );
    assert!(!ctx.stop.is_cancelled());
    assert_eq!(*ctx.turn_reply.lock().unwrap(), None);
}

#[tokio::test]
async fn worktree_commit_and_custom_skill_do_not_query_pr_metadata() {
    let source = source(None);
    let ctx = context("review-no-preflight", source.clone());
    for arguments in ["", "HEAD~1"] {
        let (_, is_error) = run_tool(
            "skill",
            json!({"name": "code-review", "arguments": arguments}),
            &ctx,
        )
        .await;
        assert!(!is_error);
    }
    let program = ToolCtx {
        from_program: true,
        ..ctx.clone()
    };
    let (_, is_error) = run_tool(
        "skill",
        json!({"name": "code-review", "arguments": PR}),
        &program,
    )
    .await;
    assert!(!is_error);
    assert!(!program.stop.is_cancelled());
    let mut cfg = ctx.cfg.test_clone();
    let mut custom = crate::skills::builtin().remove(0);
    custom.source = crate::skills::SkillSource::Skill;
    cfg.skills = Arc::new(vec![custom]);
    let ctx = ToolCtx {
        cfg: Arc::new(cfg),
        ..ctx
    };
    let (_, is_error) = run_tool(
        "skill",
        json!({"name": "code-review", "arguments": PR}),
        &ctx,
    )
    .await;
    assert!(!is_error);
    assert_eq!(*source.calls.lock().unwrap(), Vec::<Value>::new());
    assert!(!ctx.stop.is_cancelled());
}

#[derive(Default)]
struct RecordingUi(Mutex<Vec<Event>>);

#[tokio::test]
async fn metadata_lookup_respects_deny_rules_and_cancellation() {
    let source = source(Some(metadata()));
    let ctx = context("review-permission", source.clone());
    let mut cfg = ctx.cfg.test_clone();
    cfg.permissions = Arc::new(
        crate::permissions::Permissions::new(
            crate::permissions::Mode::Manual,
            &crate::permissions::PermissionRules {
                deny: vec!["web_fetch".into()],
                ..Default::default()
            },
            cfg.cwd.clone(),
            None,
        )
        .unwrap(),
    );
    let denied = ToolCtx {
        cfg: Arc::new(cfg),
        ..ctx.clone()
    };
    let (_, is_error) = run_tool(
        "skill",
        json!({"name": "code-review", "arguments": PR}),
        &denied,
    )
    .await;
    assert!(!is_error);
    assert_eq!(*source.calls.lock().unwrap(), Vec::<Value>::new());
    assert!(denied.turn_reply.lock().unwrap().is_some());

    let cancelled = context("review-cancelled", source.clone());
    cancelled.cancel.cancel();
    let (_, is_error) = run_tool(
        "skill",
        json!({"name": "code-review", "arguments": PR}),
        &cancelled,
    )
    .await;
    assert!(is_error);
    assert_eq!(*source.calls.lock().unwrap(), Vec::<Value>::new());
    assert_eq!(*cancelled.turn_reply.lock().unwrap(), None);
}

impl Ui for RecordingUi {
    fn emit(&self, event: &Event) {
        self.0.lock().unwrap().push(event.clone());
    }
}

#[tokio::test]
async fn failed_metadata_returns_one_question_without_another_sample_or_queued_call() {
    let source = source(None);
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Blocks(vec![
            AssistantBlock::ToolUse {
                id: "activate".into(),
                name: "skill".into(),
                input: json!({"name": "code-review", "arguments": PR}),
            },
            AssistantBlock::ToolUse {
                id: "probe".into(),
                name: "bash".into(),
                input: json!({"command": "printf SHOULD_NOT_RUN"}),
            },
        ]),
        MockTurn::Blocks(vec![AssistantBlock::Text {
            text: "must not sample again".into(),
        }]),
    ]);
    let ctx = with_provider(context("review-preflight-stop", source.clone()), provider);
    let ui = Arc::new(RecordingUi::default());
    let mut history = History::new(ctx.cfg.offload_dir.clone());
    history.record(Message::user_text(format!("Review {PR}")));
    let outcome = crate::agent::run_turn(
        &ctx.cfg,
        &mut history,
        &(ui.clone() as Arc<dyn Ui>),
        &CancellationToken::new(),
        0,
    )
    .await;
    let expected = outcome.final_text.clone();
    assert!(expected.starts_with("已尝试 PR 元数据和现有认证访问，仍无法确认实际目标分支："));
    assert!(expected.ends_with("请提供这个 PR 的实际目标分支。"));
    assert_eq!(
        (outcome.reason, outcome.final_text, outcome.rounds),
        (EndReason::Completed, expected.clone(), 1)
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(source.calls.lock().unwrap().len(), 1);
    assert_eq!(
        history.messages().last().unwrap(),
        &Message::assistant(vec![ContentBlock::Text {
            text: expected.clone()
        }])
    );
    assert!(
        matches!(&history.messages()[2].content[1], ContentBlock::ToolResult { is_error: true, content, .. } if content.as_text() == super::super::NOT_RUN_STOPPED)
    );
    let events = ui.0.lock().unwrap();
    assert!(!events.iter().any(|event| matches!(event, Event::ItemStarted { item: crate::event::Item::ToolCall { input, .. }, .. } if input.get("command").and_then(Value::as_str) == Some("printf SHOULD_NOT_RUN"))));
    assert_eq!(events.iter().filter(|event| matches!(event, Event::ItemCompleted { item: crate::event::Item::AssistantMessage { text, .. }, .. } if text == &expected)).count(), 1);
}

#[test]
fn target_facts_come_only_from_explicit_user_statements_for_the_same_pr() {
    let other = "https://forge.example/team/project/pulls/43";
    let messages = vec![
        Message::user_text(format!("审查：{PR}")),
        Message::user_text("是`release/x`分支"),
        Message::user_text(format!("Review {other}，目标分支是other。")),
        Message::assistant(vec![ContentBlock::Text {
            text: "目标分支是invented".into(),
        }]),
        Message::injected(kloop_protocol::Injected::Hook, "目标分支是injected"),
        Message::user_text(format!("Review {PR}")),
        Message::user_text("当前分支是main"),
        Message::user_text("continue"),
    ];
    assert_eq!(
        known_targets(&messages),
        vec![
            (PR.into(), "release/x".into()),
            (other.into(), "other".into())
        ]
    );
    let mut history = History::new(std::env::temp_dir().join("review-known-targets"));
    for message in messages {
        history.record(message);
    }
    history.stage(Message::user_text("目标分支为release/y"));
    assert_eq!(
        history.review_targets(),
        vec![
            (PR.into(), "release/y".into()),
            (other.into(), "other".into())
        ]
    );
    history.record(Message::user_text(format!(
        "Review {PR}/，目标分支是release/z"
    )));
    assert_eq!(
        history.review_targets(),
        vec![
            (format!("{PR}/"), "release/z".into()),
            (other.into(), "other".into())
        ]
    );
}

#[tokio::test]
async fn known_user_target_skips_anonymous_lookup_and_does_not_handoff() {
    let source = source(None);
    let mut ctx = context("review-known-target", source.clone());
    ctx.review_targets = vec![(format!("{PR}/"), "release".into())];
    let (text, is_error) =
        run_tool("skill", json!({"name":"code-review", "arguments":PR}), &ctx).await;
    assert!(!is_error, "{text}");
    assert_eq!(
        *ctx.review_update.lock().unwrap(),
        Some(State::Resolve {
            pr: PR.into(),
            branch: "release".into(),
            revisions: None
        })
    );
    assert_eq!(*ctx.turn_reply.lock().unwrap(), None);
    assert_eq!(*source.calls.lock().unwrap(), Vec::<Value>::new());
    assert!(!ctx.stop.is_cancelled());
}

#[cfg(unix)]
fn authenticated_context(
    tag: &str,
    remote: &str,
    response: Option<&str>,
) -> (tempfile::TempDir, ToolCtx) {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    for args in [vec!["init", "-q"], vec!["remote", "add", "origin", remote]] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(&cwd)
                .status()
                .unwrap()
                .success()
        );
    }
    let bin = cwd.join("bin");
    std::fs::create_dir(&bin).unwrap();
    if let Some(response) = response {
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nprintf '%s\\n' {}\n",
            flow::quote(cwd.join("client-called").to_str().unwrap()),
            flow::quote(response)
        );
        std::fs::write(bin.join("gh"), script).unwrap();
        std::fs::set_permissions(bin.join("gh"), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut ctx = context(tag, source(None));
    let mut cfg = ctx.cfg.test_clone();
    cfg.cwd = cwd;
    cfg.shell_login_env = Arc::new(crate::shell_env::ShellLoginEnv::test_fixture(&[(
        "PATH",
        &format!("{}:/usr/bin:/bin", bin.display()),
    )]));
    ctx.cfg = Arc::new(cfg);
    (directory, ctx)
}

#[cfg(unix)]
#[tokio::test]
async fn anonymous_failure_recovers_metadata_through_matching_authenticated_access() {
    let (_directory, ctx) = authenticated_context(
        "review-authenticated",
        "git@forge.example:team/project.git",
        Some(&metadata()),
    );
    let (text, is_error) =
        run_tool("skill", json!({"name":"code-review", "arguments":PR}), &ctx).await;
    assert!(!is_error, "{text}");
    assert_eq!(
        *ctx.review_update.lock().unwrap(),
        Some(State::Resolve {
            pr: PR.into(),
            branch: "release".into(),
            revisions: Some(("a".repeat(40), "b".repeat(40)))
        })
    );
    assert_eq!(*ctx.turn_reply.lock().unwrap(), None);
    assert_eq!(
        std::fs::read_to_string(ctx.cfg.cwd.join("client-called")).unwrap(),
        "api\n--method\nGET\nhttps://forge.example/api/v1/repos/team/project/pulls/42\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn failed_recovery_does_not_guess_a_branch_or_use_an_unrelated_remote() {
    for (tag, remote, response, called) in [
        (
            "mismatch",
            "git@other.example:team/project.git",
            Some(metadata()),
            false,
        ),
        (
            "missing-client",
            "git@forge.example:team/project.git",
            None,
            false,
        ),
        (
            "invalid-metadata",
            "git@forge.example:team/project.git",
            Some("{}".into()),
            true,
        ),
    ] {
        let (_directory, ctx) = authenticated_context(tag, remote, response.as_deref());
        let (text, is_error) =
            run_tool("skill", json!({"name":"code-review", "arguments":PR}), &ctx).await;
        assert!(!is_error, "{text}");
        assert_eq!(
            *ctx.review_update.lock().unwrap(),
            Some(State::Awaiting { pr: PR.into() })
        );
        assert!(
            ctx.turn_reply
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .ends_with("请提供这个 PR 的实际目标分支。")
        );
        assert_eq!(ctx.cfg.cwd.join("client-called").exists(), called);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn authenticated_recovery_cannot_bypass_bash_deny_rules() {
    let (_directory, mut ctx) = authenticated_context(
        "review-denied-recovery",
        "git@forge.example:team/project.git",
        Some(&metadata()),
    );
    let mut cfg = ctx.cfg.test_clone();
    cfg.permissions = Arc::new(
        crate::permissions::Permissions::new(
            crate::permissions::Mode::Manual,
            &crate::permissions::PermissionRules {
                deny: vec!["bash".into()],
                ..Default::default()
            },
            cfg.cwd.clone(),
            None,
        )
        .unwrap(),
    );
    ctx.cfg = Arc::new(cfg);
    let (text, is_error) =
        run_tool("skill", json!({"name":"code-review", "arguments":PR}), &ctx).await;
    assert!(!is_error, "{text}");
    assert_eq!(
        *ctx.review_update.lock().unwrap(),
        Some(State::Awaiting { pr: PR.into() })
    );
    assert!(!ctx.cfg.cwd.join("client-called").exists());
}
