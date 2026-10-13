use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use kloop_protocol::AssistantBlock;
use kloop_provider::{MockTurn, Provider};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::agent::EndReason;
use crate::tools::testutil::*;

const PR: &str = "https://forge.example/team/project/pulls/42";

struct Fixture {
    _dir: tempfile::TempDir,
    cwd: PathBuf,
    base: String,
    head: String,
    tip: String,
}

fn run(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        run(&cwd, &["init", "-b", "main"]);
        run(&cwd, &["config", "user.email", "fixture@example.invalid"]);
        run(&cwd, &["config", "user.name", "Fixture"]);
        std::fs::write(cwd.join("old.txt"), "old\n").unwrap();
        run(&cwd, &["add", "."]);
        run(&cwd, &["commit", "-m", "base"]);
        let base = run(&cwd, &["rev-parse", "HEAD"]);
        run(&cwd, &["checkout", "-b", "feature"]);
        for path in ["code.rs", "code_test.rs", "quote'$(touch escaped).txt"] {
            std::fs::write(cwd.join(path), format!("new content in {path}\n")).unwrap();
        }
        run(&cwd, &["add", "."]);
        run(&cwd, &["commit", "-m", "feature"]);
        let head = run(&cwd, &["rev-parse", "HEAD"]);
        run(&cwd, &["update-ref", "refs/pull/42/head", &head]);
        run(&cwd, &["checkout", "-b", "release", &base]);
        std::fs::write(cwd.join("target.txt"), "target advanced\n").unwrap();
        run(&cwd, &["add", "."]);
        run(&cwd, &["commit", "-m", "target advanced"]);
        let tip = run(&cwd, &["rev-parse", "HEAD"]);
        run(&cwd, &["checkout", "main"]);
        run(&cwd, &["update-ref", "refs/remotes/origin/release", &base]);
        let remote = "https://forge.example/team/project";
        run(&cwd, &["remote", "add", "origin", remote]);
        run(
            &cwd,
            &[
                "config",
                &format!("url.{}.insteadOf", cwd.display()),
                remote,
            ],
        );
        Self {
            _dir: dir,
            cwd,
            base,
            head,
            tip,
        }
    }

    fn ctx(&self, tag: &str, provider: Provider, limit: Option<usize>) -> ToolCtx {
        let cfg = TestConfig::new(tag)
            .provider(provider)
            .max_rounds(limit)
            .dirs(self._dir.path())
            .build();
        let mut cfg = cfg.test_clone();
        cfg.cwd = self.cwd.clone();
        cfg.skills = Arc::new(crate::skills::builtin());
        test_ctx_with_cfg(0, Arc::new(cfg))
    }
}

#[test]
fn target_input_and_repository_identity_are_bounded() {
    assert_eq!(branch("是`release/x`分支"), Some("release/x".into()));
    assert_eq!(branch("目标分支是 test。"), Some("test".into()));
    assert_eq!(branch("switch tasks now"), None);
    assert_eq!(branch("--help"), None);
    let target = PullRequest::parse(PR).unwrap();
    assert!(same_repository(
        "git@forge.example:team/project.git\n",
        &target
    ));
    assert!(same_repository(
        "ssh://git@forge.example/team/project.git",
        &target
    ));
    assert!(!same_repository(
        "https://other.example/team/project",
        &target
    ));
    assert!(!same_repository(
        "git@forge.example:team/different.git",
        &target
    ));
    for remote in [
        "git@forge.example:team/project.git.git",
        "https://forge.example/team/project.git.git/",
    ] {
        assert!(!same_repository(remote, &target));
        assert!(same_repository(
            remote,
            &PullRequest::parse("https://forge.example/team/project.git/pulls/42").unwrap()
        ));
    }
}

#[tokio::test]
async fn trailing_slash_pr_url_resolves_the_same_frozen_range() {
    let fixture = Fixture::new();
    let ctx = fixture.ctx("review-trailing-slash", Provider::mock(vec![]), None);
    let (prepared, _) = packet(&ctx, &format!("{PR}/"), "release", None)
        .await
        .unwrap();
    assert_eq!(
        (prepared.head, prepared.tip, prepared.base),
        (fixture.head, fixture.tip, fixture.base)
    );
}

#[tokio::test]
async fn fresh_remote_target_and_every_file_are_prepared_without_checkout() {
    let fixture = Fixture::new();
    let ctx = fixture.ctx("review-packet-fresh", Provider::mock(vec![]), None);
    let (prepared, text) = packet(&ctx, PR, "release", None).await.unwrap();
    assert_eq!(
        (&prepared.head, &prepared.tip, &prepared.base),
        (&fixture.head, &fixture.tip, &fixture.base)
    );
    assert_eq!(
        prepared
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.gap.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            ("code.rs", None),
            ("code_test.rs", None),
            ("quote'$(touch escaped).txt", None)
        ]
    );
    assert!(text.contains("new content in code_test.rs"));
    assert_eq!(run(&fixture.cwd, &["rev-parse", "HEAD"]), fixture.base);
    assert_eq!(
        run(&fixture.cwd, &["rev-parse", "origin/release"]),
        fixture.base
    );
    assert_eq!(run(&fixture.cwd, &["status", "--porcelain"]), "");
    assert!(!fixture.cwd.join("escaped").exists());
}

#[tokio::test]
async fn truncated_diff_is_a_gap_and_other_files_still_arrive() {
    let fixture = Fixture::new();
    run(&fixture.cwd, &["checkout", "feature"]);
    std::fs::write(fixture.cwd.join("big.txt"), "x".repeat(40_000)).unwrap();
    run(&fixture.cwd, &["add", "."]);
    run(&fixture.cwd, &["commit", "-m", "large diff"]);
    let head = run(&fixture.cwd, &["rev-parse", "HEAD"]);
    run(&fixture.cwd, &["update-ref", "refs/pull/42/head", &head]);
    run(&fixture.cwd, &["checkout", "main"]);
    let ctx = fixture.ctx("review-packet-truncated", Provider::mock(vec![]), None);
    let (prepared, text) = packet(&ctx, PR, "release", None).await.unwrap();
    assert_eq!(prepared.files.len(), 4);
    assert_eq!(
        prepared
            .files
            .iter()
            .filter(|file| file.gap.is_some())
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["big.txt"]
    );
    assert!(text.contains("new content in code_test.rs"));
    assert!(!text.contains(&"x".repeat(1000)));
}

fn report() -> MockTurn {
    MockTurn::Blocks(vec![AssistantBlock::Text {
        text: "Review report: no confirmed finding; supplied changes checked.".into(),
    }])
}

#[tokio::test]
async fn file_reads_preserve_source_line_numbers_and_blank_lines() {
    let fixture = Fixture::new();
    run(&fixture.cwd, &["checkout", "feature"]);
    std::fs::write(fixture.cwd.join("code.rs"), "first\nsecond\n\nfourth\n").unwrap();
    run(&fixture.cwd, &["add", "."]);
    run(&fixture.cwd, &["commit", "-m", "numbered source"]);
    let head = run(&fixture.cwd, &["rev-parse", "HEAD"]);
    run(&fixture.cwd, &["update-ref", "refs/pull/42/head", &head]);
    run(&fixture.cwd, &["checkout", "main"]);
    let ctx = fixture.ctx("review-numbered-file", Provider::mock(vec![]), None);
    let (prepared, _) = packet(&ctx, PR, "release", None).await.unwrap();
    let mut flow = Flow {
        state: State::Prepared(prepared),
    };
    for (revision, path, start, count, expected) in [
        ("head", "code.rs", 2, 2, vec!["2\tsecond", "3\t"]),
        ("base", "old.txt", 1, 1, vec!["1\told"]),
    ] {
        let query = flow
            .command(
                "review_read",
                &json!({"operation":"file","revision":revision,"path":path,"start_line":start,"line_count":count}),
                &fixture.cwd,
            )
            .unwrap();
        let (text, error) = run_tool("bash", query, &ctx).await;
        assert!(!error, "{text}");
        assert_eq!(text.lines().collect::<Vec<_>>(), expected);
    }
}

#[tokio::test]
async fn previously_confirmed_target_prepares_without_a_question_or_unpinned_queued_reads() {
    let fixture = Fixture::new();
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Blocks(vec![
            AssistantBlock::ToolUse {
                id: "activate".into(),
                name: "skill".into(),
                input: json!({"name":"code-review", "arguments":PR}),
            },
            AssistantBlock::ToolUse {
                id: "queued-write".into(),
                name: "bash".into(),
                input: json!({"command":"printf WRONG > unpinned-read"}),
            },
            AssistantBlock::ToolUse {
                id: "queued-read".into(),
                name: "read".into(),
                input: json!({"file_path":"old.txt"}),
            },
        ]),
        report(),
    ]);
    let ctx = fixture.ctx("review-prior-target", provider, Some(4));
    let mut history = History::new(ctx.cfg.offload_dir.clone());
    history.record(Message::user_text(format!("Review {PR}")));
    history.record(Message::user_text("是release分支"));
    history.stage(Message::user_text(format!("Review {PR}")));
    let outcome = crate::agent::run_turn(&ctx.cfg, &mut history, &ctx.ui, &ctx.cancel, 0).await;
    assert_eq!(outcome.reason, EndReason::Completed);
    assert!(!fixture.cwd.join("unpinned-read").exists());
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["review_read"]
    );
    assert_eq!(state(history.messages()), State::Idle);
    assert!(outcome.final_text.contains(&fixture.head));
    let skipped = history
        .messages()
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error: true,
            } if tool_use_id.starts_with("queued-") => {
                Some((tool_use_id.as_str(), content.as_text().into_owned()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let reason = "Not run: native PR preparation must finish before further tools.";
    assert_eq!(
        skipped,
        vec![
            ("queued-write", reason.into()),
            ("queued-read", reason.into())
        ]
    );
}

#[tokio::test]
async fn persisted_target_answer_prepares_before_sampling_and_finishes_with_facts() {
    let fixture = Fixture::new();
    let (provider, seen) = Provider::mock_recording(vec![report()]);
    let ctx = fixture.ctx("review-persisted-answer", provider, Some(3));
    let path = fixture._dir.path().join("session.jsonl");
    let mut history = History::new(ctx.cfg.offload_dir.clone());
    history.attach_rollout(crate::rollout::Rollout::new(path.clone()));
    Flow {
        state: State::Awaiting { pr: PR.into() },
    }
    .save(&mut history);
    drop(history);
    let resumed = crate::rollout::resume_session(&path).unwrap();
    let mut history = History::resume(ctx.cfg.offload_dir.clone(), resumed);
    history.stage(Message::user_text("是release分支"));
    let outcome = crate::agent::run_turn(&ctx.cfg, &mut history, &ctx.ui, &ctx.cancel, 0).await;
    assert_eq!(outcome.reason, EndReason::Completed);
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["review_read"]
    );
    let view = format!("{:?}", requests[0].messages);
    assert!(view.contains(&fixture.tip));
    assert!(view.contains("new content in code_test.rs"));
    assert!(outcome.final_text.contains("3/3"));
    assert_eq!(Flow::load(&history).state, State::Idle);
    drop(history);
    let resumed = crate::rollout::resume_session(&path).unwrap();
    assert_eq!(
        Flow::load(&History::resume(ctx.cfg.offload_dir.clone(), resumed)).state,
        State::Idle
    );
}

#[tokio::test]
async fn last_round_has_no_tools_and_unauthorized_calls_never_execute() {
    let fixture = Fixture::new();
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Blocks(vec![AssistantBlock::ToolUse {
            id: "escape".into(),
            name: "bash".into(),
            input: json!({"command":"touch escaped"}),
        }]),
        report(),
    ]);
    let ctx = fixture.ctx("review-last-round", provider, Some(2));
    let (prepared, text) = packet(&ctx, PR, "release", None).await.unwrap();
    let mut history = History::new(ctx.cfg.offload_dir.clone());
    history.record(Message::injected(Injected::Harness, text));
    Flow {
        state: State::Prepared(prepared),
    }
    .save(&mut history);
    history.record(Message::user_text("继续"));
    let outcome = crate::agent::run_turn(&ctx.cfg, &mut history, &ctx.ui, &ctx.cancel, 0).await;
    assert_eq!(outcome.reason, EndReason::Completed);
    assert!(!fixture.cwd.join("escaped").exists());
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].tools.is_empty());
    assert!(outcome.final_text.contains("补查失败 / 截断 1 次"));
}

#[tokio::test]
async fn supplementary_queries_pin_both_revisions_and_exhaust_in_a_batch() {
    let fixture = Fixture::new();
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Blocks(
            (0..17)
                .map(|index| AssistantBlock::ToolUse {
                    id: format!("read-{index}"),
                    name: "review_read".into(),
                    input: json!({"operation":"file","path":"old.txt","revision":"base"}),
                })
                .collect(),
        ),
        report(),
    ]);
    let ctx = fixture.ctx("review-followup-budget", provider, None);
    let (prepared, text) = packet(&ctx, PR, "release", None).await.unwrap();
    let mut flow = Flow {
        state: State::Prepared(prepared),
    };
    let query = flow
        .command(
            "review_read",
            &json!({"operation":"file","path":"quote'$(touch escaped).txt"}),
            &fixture.cwd,
        )
        .unwrap();
    let (text_read, error) = run_tool("bash", query, &ctx).await;
    assert!(!error, "{text_read}");
    assert!(text_read.contains("new content"));
    assert!(!fixture.cwd.join("escaped").exists());
    let mut history = History::new(ctx.cfg.offload_dir.clone());
    history.record(Message::injected(Injected::Harness, text));
    flow.save(&mut history);
    history.replace_all(vec![Message::injected(Injected::ContextSummary, "summary")]);
    assert!(format!("{:?}", history.messages()).contains("new content in code_test.rs"));
    history.record(Message::user_text("继续"));
    let outcome = crate::agent::run_turn(
        &ctx.cfg,
        &mut history,
        &ctx.ui,
        &CancellationToken::new(),
        0,
    )
    .await;
    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert!(seen.lock().unwrap()[1].tools.is_empty());
    assert!(outcome.final_text.contains("补查失败 / 截断 2 次"));
}

#[tokio::test]
async fn unreadable_arguments_exhaust_the_followup_budget() {
    let fixture = Fixture::new();
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Blocks(
            (0..16)
                .map(|index| AssistantBlock::InvalidToolUse {
                    id: format!("invalid-{index}"),
                    name: "review_read".into(),
                    raw: "{".into(),
                    error: "unreadable JSON".into(),
                })
                .collect(),
        ),
        report(),
    ]);
    let ctx = fixture.ctx("review-unreadable-budget", provider, None);
    let (prepared, _) = packet(&ctx, PR, "release", None).await.unwrap();
    let mut history = History::new(ctx.cfg.offload_dir.clone());
    Flow {
        state: State::Prepared(prepared),
    }
    .save(&mut history);
    history.record(Message::user_text("继续"));
    let outcome = crate::agent::run_turn(&ctx.cfg, &mut history, &ctx.ui, &ctx.cancel, 0).await;
    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert!(seen.lock().unwrap()[1].tools.is_empty());
    assert!(outcome.final_text.contains("补查失败 / 截断 16 次"));
}

#[tokio::test]
async fn git_denial_and_cancel_stop_before_any_provider_request() {
    let fixture = Fixture::new();
    for cancelled in [false, true] {
        let (provider, seen) = Provider::mock_recording(vec![report()]);
        let mut ctx = fixture.ctx("review-denied-packet", provider, None);
        if cancelled {
            ctx.cancel.cancel();
        } else {
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
        }
        let mut history = History::new(ctx.cfg.offload_dir.clone());
        Flow {
            state: State::Awaiting { pr: PR.into() },
        }
        .save(&mut history);
        history.record(Message::user_text("release"));
        let outcome = crate::agent::run_turn(&ctx.cfg, &mut history, &ctx.ui, &ctx.cancel, 0).await;
        assert!(matches!(
            outcome.reason,
            EndReason::Error(_) | EndReason::Aborted
        ));
        assert!(seen.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn missing_objects_are_fetched_without_updating_tracking_refs_or_fetch_head() {
    let fixture = Fixture::new();
    let checkout = tempfile::tempdir().unwrap();
    let cwd = checkout.path().join("work");
    run(
        checkout.path(),
        &[
            "clone",
            "--no-local",
            "--single-branch",
            "--branch",
            "main",
            fixture.cwd.to_str().unwrap(),
            cwd.to_str().unwrap(),
        ],
    );
    let remote = "https://forge.example/team/project";
    run(&cwd, &["remote", "set-url", "origin", remote]);
    run(
        &cwd,
        &[
            "config",
            &format!("url.{}.insteadOf", fixture.cwd.display()),
            remote,
        ],
    );
    let ctx = fixture.ctx("review-fetch-missing", Provider::mock(vec![]), None);
    let mut cfg = ctx.cfg.test_clone();
    cfg.cwd = cwd.canonicalize().unwrap();
    let ctx = ToolCtx {
        cfg: Arc::new(cfg),
        ..ctx
    };
    let fetch_head = std::fs::read(cwd.join(".git/FETCH_HEAD")).ok();
    let tracking_refs = run(
        &cwd,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/remotes/origin",
        ],
    );
    let (prepared, _) = packet(&ctx, PR, "release", None).await.unwrap();
    assert_eq!(
        (prepared.head, prepared.tip, prepared.base),
        (fixture.head, fixture.tip, fixture.base.clone())
    );
    assert_eq!(run(&cwd, &["rev-parse", "HEAD"]), fixture.base);
    assert_eq!(
        run(
            &cwd,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/remotes/origin",
            ]
        ),
        tracking_refs
    );
    assert_eq!(std::fs::read(cwd.join(".git/FETCH_HEAD")).ok(), fetch_head);
}

#[tokio::test]
async fn closed_investigation_rejects_a_tool_call_without_more_sampling() {
    let fixture = Fixture::new();
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Blocks(vec![AssistantBlock::ToolUse {
            id: "closed-call".into(),
            name: "bash".into(),
            input: json!({"command":"touch escaped"}),
        }]),
        report(),
    ]);
    let ctx = fixture.ctx("review-closed-no-call", provider, None);
    let (mut prepared, _) = packet(&ctx, PR, "release", None).await.unwrap();
    prepared.remaining = 0;
    let mut history = History::new(ctx.cfg.offload_dir.clone());
    Flow {
        state: State::Prepared(prepared),
    }
    .save(&mut history);
    history.record(Message::user_text("继续"));
    let outcome = crate::agent::run_turn(&ctx.cfg, &mut history, &ctx.ui, &ctx.cancel, 0).await;
    assert!(matches!(outcome.reason, EndReason::Error(_)));
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert!(!fixture.cwd.join("escaped").exists());
    assert!(history.messages().iter().any(|message| message.content.iter().any(|block| matches!(block, ContentBlock::ToolResult { tool_use_id, is_error:true, .. } if tool_use_id == "closed-call"))));
}

#[tokio::test]
async fn new_task_and_steering_leave_the_review_stage() {
    let fixture = Fixture::new();
    let ctx = fixture.ctx("review-task-switch", Provider::mock(vec![]), None);
    let (prepared, packet) = packet(&ctx, PR, "release", None).await.unwrap();
    let mut flow = Flow {
        state: State::Prepared(prepared),
    };
    let mut history = History::new(ctx.cfg.offload_dir.clone());
    history.record(Message::injected(Injected::Harness, packet));
    flow.save(&mut history);
    history.record(Message::injected(
        Injected::Steering,
        "switch to implementing a feature",
    ));
    assert!(flow.tools(&mut history, None).is_none());
    assert_eq!(flow.state, State::Idle);
    history.replace_all(vec![Message::injected(
        Injected::ContextSummary,
        "new task",
    )]);
    assert!(!format!("{:?}", history.messages()).contains("new content in code_test.rs"));
    let mut pending = Flow {
        state: State::Awaiting { pr: PR.into() },
    };
    pending.input(Some("help with a different task"), &mut history);
    assert_eq!(pending.state, State::Idle);
}
