//! Plan 216: a person's No on an approval ends the turn, not just the call —
//! and which turn it ends follows the foreground turn tree.

use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use kloop_protocol::AssistantBlock;
use kloop_provider::MockRequest;
use kloop_provider::MockTurn;
use kloop_provider::Provider;
use serde_json::json;

use super::*;
use crate::permissions::Approver;
use crate::permissions::ConfirmRequest;
use crate::permissions::Decision;
use crate::permissions::Mode;
use crate::permissions::PermissionRules;
use crate::permissions::Permissions;

struct NullUi;
impl Ui for NullUi {
    fn emit(&self, _: &Event) {}
}

/// Answers every approval No and counts the asks.
struct AnswersNo(AtomicUsize);

impl Approver for AnswersNo {
    fn confirm(&self, _: ConfirmRequest) -> Pin<Box<dyn Future<Output = Decision> + Send + '_>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Decision::Stop })
    }
}

fn tool_use(id: &str, name: &str, input: Value) -> MockTurn {
    MockTurn::Blocks(vec![AssistantBlock::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }])
}

fn text(t: &str) -> MockTurn {
    MockTurn::Blocks(vec![AssistantBlock::Text { text: t.into() }])
}

/// A session in `mode` whose every approval is answered No.
fn answering_no(
    tag: &str,
    mode: Mode,
    turns: Vec<MockTurn>,
) -> (
    Arc<Config>,
    Arc<AnswersNo>,
    Arc<std::sync::Mutex<Vec<MockRequest>>>,
) {
    let (provider, seen) = Provider::mock_recording(turns);
    let approver = Arc::new(AnswersNo(AtomicUsize::new(0)));
    let mut cfg = crate::tools::testutil::TestConfig::new(&format!("plan216-{tag}"))
        .provider(provider)
        .max_rounds(Some(10))
        .build()
        .test_clone();
    cfg.surface.plan_control = true;
    cfg.permissions = Arc::new(
        Permissions::new(
            mode,
            &PermissionRules::default(),
            cfg.cwd.clone(),
            Some(approver.clone()),
        )
        .unwrap(),
    );
    (Arc::new(cfg), approver, seen)
}

fn only_result(message: &Message) -> (String, bool) {
    let [
        ContentBlock::ToolResult {
            content, is_error, ..
        },
    ] = message.content.as_slice()
    else {
        panic!("expected one tool result, got {message:?}");
    };
    (content.as_text().into_owned(), *is_error)
}

/// The turn ends after the batch that asked: the round the script has ready
/// next is never requested, and history still pairs every call with a result.
#[tokio::test]
async fn a_no_ends_the_turn_without_asking_the_model_again() {
    let (cfg, approver, seen) = answering_no(
        "turn",
        Mode::Manual,
        vec![
            tool_use("b1", "bash", json!({"command": "printf x > one.txt"})),
            text("never requested"),
        ],
    );
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("write a file"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Stopped);
    assert_eq!(outcome.reason.terminal_status(), "stopped");
    assert_eq!(outcome.rounds, 1);
    assert_eq!(seen.lock().unwrap().len(), 1, "no second sampling request");
    assert_eq!(approver.0.load(Ordering::SeqCst), 1);
    assert_eq!(history.messages().len(), 3);
    assert_eq!(
        only_result(&history.messages()[2]),
        (crate::permissions::user_stop("bash"), true)
    );
}

/// A foreground child shares its parent's stop: the No inside it ends the
/// child, and the parent ends too instead of sampling on the child's result.
#[tokio::test]
async fn a_no_inside_a_foreground_sub_agent_stops_the_parent() {
    let (cfg, approver, seen) = answering_no(
        "foreground",
        Mode::Manual,
        vec![
            tool_use("t1", "run_agent", json!({"prompt": "sub work"})),
            tool_use("s1", "bash", json!({"command": "printf x > f.txt"})),
            text("child never asked again"),
            text("parent never asked again"),
        ],
    );
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("delegate"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Stopped);
    assert_eq!(
        seen.lock().unwrap().len(),
        2,
        "parent round 0, child round 0"
    );
    assert_eq!(approver.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        only_result(&history.messages()[2]),
        (
            "[the user declined an approval in this sub-agent and stopped the turn to tell you what to do instead]\n"
                .to_string(),
            false
        )
    );
}

/// A No on the plan ends the turn and leaves the session planning.
#[tokio::test]
async fn a_no_on_the_plan_ends_the_turn_in_plan_mode() {
    let (cfg, approver, seen) = answering_no(
        "plan",
        Mode::Plan,
        vec![
            tool_use("p1", "exit_plan_mode", json!({"plan": "1. do it"})),
            text("never requested"),
        ],
    );
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("plan it"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Stopped);
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(approver.0.load(Ordering::SeqCst), 1);
    assert_eq!(cfg.permissions.mode(), Mode::Plan);
    assert_eq!(
        only_result(&history.messages()[2]),
        (
            "The user did not approve the plan and stopped the turn to say what to change. You are still in plan mode; revise the plan from their next message."
                .to_string(),
            false
        )
    );
}
