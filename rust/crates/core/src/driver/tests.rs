//! The driver's own rules: what is staged, what comes back, and how one
//! operation decides the idle-inbox debt. The front-end projections are tested
//! where they live.

use std::sync::Arc;
use std::time::Duration;

use kloop_protocol::Message;
use tokio_util::sync::CancellationToken;

use crate::agent::EndReason;
use crate::agent::Ui;
use crate::config::Config;
use crate::driver::Input;
use crate::driver::Report;
use crate::driver::SessionDriver;
use crate::event::Event;
use crate::history::History;
use crate::history::ProviderSwitchError;
use crate::inbox::InboxItem;
use crate::provider_route::ProviderCatalog;
use crate::provider_route::SwitchError;
use crate::skills::Skill;
use crate::skills::SkillContext;
use crate::skills::SkillSource;
use kloop_protocol::AssistantBlock;
use kloop_protocol::ContentBlock;
use kloop_protocol::ImageSource;
use kloop_provider::MockTurn;
use kloop_provider::Provider;

struct NoUi;

impl Ui for NoUi {
    fn emit(&self, _event: &Event) {}
}

fn text_block(text: &str) -> AssistantBlock {
    AssistantBlock::Text { text: text.into() }
}

fn mock_config(provider: Provider) -> Config {
    let (provider_catalog, provider_route) =
        ProviderCatalog::from_provider("mock", provider, "mock", vec!["mock".into()]).unwrap();
    let inbox = Arc::new(crate::inbox::Inbox::default());
    Config {
        provider_catalog,
        provider_route,
        context_budget: crate::config::ContextBudgetSource::Pinned,
        system: "test".into(),
        project_instructions: None,
        max_rounds: Some(10),
        cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        offload_dir: std::env::temp_dir().join("kloop-driver-offload"),
        // Empty: these sessions are ephemeral, so no rollout file is created.
        sessions_dir: std::path::PathBuf::new(),
        context_window: None,
        permissions: Arc::new(crate::permissions::Permissions::allow_all()),
        questioner: None,
        file_state: Default::default(),
        tool_sources: Vec::new(),
        session_id: "driver".into(),
        local_agent: crate::agent_mailbox::LocalAgentContext::root(Arc::clone(&inbox)),
        hooks: Arc::new(crate::hooks::Hooks::none()),
        background_shells: crate::tools::BackgroundShells::new(),
        shell_programs: Arc::new(crate::shell_programs::ShellPrograms::test_fixture()),
        shell_login_env: Arc::new(crate::shell_env::ShellLoginEnv::none()),
        powershell_execution_gate: Default::default(),
        background_executions: crate::tools::BackgroundExecutions::new(),
        sandbox: None,
        agent_types: Arc::new(Vec::new()),
        tool_allowlist: None,
        defer_threshold: 30,
        unlocked_tools: Default::default(),
        todos: Default::default(),
        inbox: Arc::clone(&inbox),
        scheduler: crate::scheduler::Scheduler::in_memory(inbox),
        cleanup_tasks: Default::default(),
        program_limits: Default::default(),
        request_reduction: true,
        skills: Default::default(),
        active_worktree: Arc::new(crate::worktree::ActiveWorktreeState::default()),
        surface: Default::default(),
    }
}

fn driver(provider: Provider) -> SessionDriver {
    let cfg = Arc::new(mock_config(provider));
    SessionDriver::new(
        cfg,
        History::new(std::env::temp_dir().join("kloop-driver-offload")),
        Vec::new(),
    )
    .expect("a fresh history starts from the config's route")
}

fn test_skill(name: &str, body: &str) -> Skill {
    Skill {
        name: name.into(),
        description: "test skill".into(),
        body: body.into(),
        dir: String::new(),
        context: SkillContext::Inline,
        model: None,
        allowed_tools: None,
        source: SkillSource::Skill,
    }
}

/// An interrupt before the model produced anything is a turn that never
/// happened: the input comes back, and neither history nor a session file
/// learned about it.
#[tokio::test]
async fn an_interrupted_message_comes_back_and_leaves_no_trace() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (_release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (provider, _seen) = Provider::mock_recording(vec![MockTurn::Gate {
        started: started_tx,
        release: release_rx,
        blocks: vec![text_block("too late")],
    }]);
    let cfg = Arc::new(mock_config(provider));
    let cancel = CancellationToken::new();
    let child_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        let ui: Arc<dyn Ui> = Arc::new(NoUi);
        let mut driver = SessionDriver::new(
            Arc::clone(&cfg),
            History::new(cfg.offload_dir.clone()),
            Vec::new(),
        )
        .unwrap();
        let report = driver
            .dispatch(
                Input::Message {
                    text: "teh quick brown fox".into(),
                    images: Vec::new(),
                },
                &ui,
                &child_cancel,
            )
            .await;
        (report, driver)
    });
    tokio::time::timeout(Duration::from_secs(2), started_rx)
        .await
        .expect("sampling did not start")
        .expect("sampling gate dropped");
    cancel.cancel();
    let (report, driver) = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("the turn ignored cancellation")
        .expect("the turn panicked");

    let Report::Turn(Some(turn)) = report else {
        panic!("a message input must report a turn");
    };
    assert_eq!(turn.outcome.reason, EndReason::Aborted);
    assert_eq!(
        turn.returned,
        Some(Message::user_text("teh quick brown fox"))
    );
    assert_eq!(driver.history().messages(), &[]);
}

/// A skill's expanded prompt is the turn's staged input too: interrupted
/// before the first reply, it comes back instead of being committed up front.
#[tokio::test]
async fn an_interrupted_skill_expansion_comes_back_staged() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (_release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Gate {
            started: started_tx,
            release: release_rx,
            blocks: vec![text_block("too late")],
        },
        MockTurn::Blocks(vec![text_block("answered")]),
    ]);
    let mut cfg = mock_config(provider);
    cfg.skills = Arc::new(vec![test_skill("review", "review the diff")]);
    let cfg = Arc::new(cfg);
    let cancel = CancellationToken::new();
    let child_cancel = cancel.clone();
    let image = ContentBlock::Image {
        source: ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "AA==".into(),
        },
    };
    let launch_image = image.clone();
    let handle = tokio::spawn(async move {
        let ui: Arc<dyn Ui> = Arc::new(NoUi);
        let mut driver = SessionDriver::new(
            Arc::clone(&cfg),
            History::new(cfg.offload_dir.clone()),
            vec![launch_image],
        )
        .unwrap();
        let report = driver
            .dispatch(
                Input::Command {
                    line: "/review".into(),
                },
                &ui,
                &child_cancel,
            )
            .await;
        (report, driver)
    });
    tokio::time::timeout(Duration::from_secs(2), started_rx)
        .await
        .expect("sampling did not start")
        .expect("sampling gate dropped");
    cancel.cancel();
    let (report, mut driver) = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("the turn ignored cancellation")
        .expect("the turn panicked");

    let Report::Command(command) = report else {
        panic!("a command input must report a command");
    };
    assert!(command.output.is_empty());
    let turn = command.turn.expect("the skill expanded into a turn");
    assert_eq!(turn.outcome.reason, EndReason::Aborted);
    assert_eq!(
        turn.returned,
        Some(Message::user_text("/review")),
        "the command line comes back, not the skill's expanded prompt"
    );
    assert_eq!(driver.history().messages(), &[]);

    // The `--image` blocks stayed with the driver: a retried command has no
    // composer text to put them in, so whichever input comes next carries them.
    let ui: Arc<dyn Ui> = Arc::new(NoUi);
    let report = driver
        .dispatch(
            Input::Message {
                text: "next".into(),
                images: Vec::new(),
            },
            &ui,
            &CancellationToken::new(),
        )
        .await;
    assert!(matches!(
        report,
        Report::Turn(Some(turn)) if turn.outcome.reason == EndReason::Completed
    ));
    let requests = seen.lock().unwrap();
    let next = requests.last().expect("the next message sampled");
    assert!(
        next.messages.iter().any(|message| message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. }))),
        "the launch image rides the next input: {:?}",
        next.messages
    );
}

/// An interrupt during a slow expansion cancels the turn the expansion would
/// have started. Starting it with an already-cancelled token would still run the
/// pre-turn hook, after the front-end has given up waiting.
#[tokio::test]
async fn a_cancelled_expansion_does_not_start_its_turn() {
    let ui: Arc<dyn Ui> = Arc::new(NoUi);
    let mut cfg = mock_config(Provider::mock(Vec::new()));
    cfg.skills = Arc::new(vec![test_skill("review", "review the diff")]);
    let cfg = Arc::new(cfg);
    let mut driver = SessionDriver::new(
        Arc::clone(&cfg),
        History::new(cfg.offload_dir.clone()),
        Vec::new(),
    )
    .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let report = driver
        .dispatch(
            Input::Command {
                line: "/review".into(),
            },
            &ui,
            &cancel,
        )
        .await;
    let Report::Command(command) = report else {
        panic!("a command input must report a command");
    };
    let turn = command
        .turn
        .expect("the cancelled expansion reports an aborted turn");
    assert_eq!(turn.outcome.reason, EndReason::Aborted);
    assert_eq!(turn.outcome.rounds, 0);
    assert_eq!(turn.returned, Some(Message::user_text("/review")));
    assert_eq!(driver.history().messages(), &[]);
}

/// A wake that finds the inbox empty is not a turn: it reports `None` and
/// hands the busy state straight back to the front-end.
#[tokio::test]
async fn a_wake_on_an_empty_inbox_runs_no_turn() {
    let ui: Arc<dyn Ui> = Arc::new(NoUi);
    let mut driver = driver(Provider::mock(Vec::new()));
    let report = driver
        .dispatch(Input::Wake, &ui, &CancellationToken::new())
        .await;
    assert!(matches!(report, Report::Turn(None)));
    assert_eq!(driver.history().messages(), &[]);
}

/// A command never drains the inbox, so it always owes a re-announcement. A
/// model turn drains its own batch, and only work that arrived during it is
/// new — otherwise a turn that failed before its first request would be
/// retried forever.
#[tokio::test]
async fn reannouncement_distinguishes_a_command_from_a_drained_turn() {
    let ui: Arc<dyn Ui> = Arc::new(NoUi);
    let mut driver = driver(Provider::mock(Vec::new()));
    driver.cfg().inbox.push(InboxItem::Steer {
        text: "steer".into(),
        images: Vec::new(),
    });
    let activity = driver.cfg().inbox.subscribe_activity();
    let report = driver
        .dispatch(
            Input::Command {
                line: "/help".into(),
            },
            &ui,
            &CancellationToken::new(),
        )
        .await;
    assert!(matches!(report, Report::Command(_)));
    assert!(!driver.cfg().inbox.is_empty());

    // Nothing arrived during the operation, so a model turn owes nothing.
    driver.reannounce_if_owed(true);
    assert!(!activity.has_changed().unwrap());
    // The command itself never drained the queue, so it does.
    driver.reannounce_if_owed(false);
    assert!(activity.has_changed().unwrap());
}

/// The idle provider transaction reports a refusal without touching the
/// session's route, and reports a no-op as a no-op.
#[tokio::test]
async fn an_idle_provider_switch_refuses_and_no_ops_without_moving_the_route() {
    let mut driver = driver(Provider::mock(Vec::new()));
    let revision = driver.provider_state().active_route().revision;
    let before = driver.cfg().provider_route.public_route();

    let error = driver
        .switch_provider(revision, "nope", None)
        .expect_err("an unknown provider is refused");
    assert!(matches!(
        error,
        ProviderSwitchError::Route(SwitchError::UnknownProvider(_))
    ));
    assert_eq!(driver.cfg().provider_route.public_route(), before);

    let report = driver
        .switch_provider(revision, "mock", Some("mock"))
        .expect("the configured route is a no-op");
    assert!(!report.changed);
    assert_eq!(driver.cfg().provider_route.public_route(), before);
}

/// `/clear` swaps in a session whose history starts empty, and the driver
/// follows it: the old state does not leak into the new session.
#[tokio::test]
async fn a_fresh_session_replaces_the_history_and_the_route_state() {
    let ui: Arc<dyn Ui> = Arc::new(NoUi);
    let mut driver = driver(Provider::mock(Vec::new()));
    let fresh = driver
        .start_fresh_session(&ui)
        .await
        .expect("an ephemeral session can always be replaced");
    assert_eq!(fresh.session_id, "");
    assert!(driver.history().messages().is_empty());
    assert_eq!(driver.cfg().session_id, "");
}
