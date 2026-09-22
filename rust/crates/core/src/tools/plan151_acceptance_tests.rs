//! Plan 151, first half: `read_file` tells the model when it just re-read lines
//! it already has.
//!
//! The judgement under test is narrow on purpose. Counting repeated
//! `(tool, arguments)` pairs — the shape this plan originally specified — fires
//! zero times in 5 139 real calls, because a model going in circles varies its
//! offsets. What does hold is a *strict range intersection* against what this
//! session already put in front of the model: those lines are provably still in
//! the conversation. Everything below is either that rule or one of the two
//! resets that keep it honest — compaction (the results left the context) and a
//! changed file (the lines in context no longer describe it).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::testutil::{TestConfig, run_tool, test_ctx_with_cfg};
use crate::config::Config;
use crate::history::History;

static TEMP_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// A file of `lines` numbered lines, each short enough that a 300-line read
/// stays far under `READ_CONTENT_CHARS` — the reread rule reads the range that
/// actually reached the model, so a char-budget truncation would quietly change
/// what these tests are asserting about.
fn temp_file(tag: &str, lines: usize) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "kloop-plan151-{tag}-{}-{sequence}",
        std::process::id()
    ));
    std::fs::write(&path, body(lines, "line")).unwrap();
    path
}

fn body(lines: usize, word: &str) -> String {
    (1..=lines).map(|n| format!("{word} {n}\n")).collect()
}

fn cfg(tag: &str) -> Arc<Config> {
    TestConfig::new(&format!("plan151-{tag}")).build()
}

/// Whether each read in turn came back carrying the advisory. A bool per call is
/// the whole observable: the advisory never replaces or delays a result, so the
/// sequence of "did it fire" is what a judgement change would move.
async fn advised(ctx: &super::ToolCtx, calls: Vec<Value>) -> Vec<bool> {
    let mut fired = Vec::new();
    for input in calls {
        let (out, is_error) = run_tool("read_file", input, ctx).await;
        assert!(!is_error, "{out}");
        fired.push(out.contains("already have in this conversation"));
    }
    fired
}

fn page(path: &Path, offset: usize, limit: usize) -> Value {
    json!({"path": path.to_str().unwrap(), "offset": offset, "limit": limit})
}

/// The advisory fires on the third redundant reread and every third one after,
/// and on nothing else. Ten reads of the same window: the first establishes it,
/// so rereads 1..=9 land on calls 2..=10 and the advisory is due on 4, 7 and 10.
#[tokio::test]
async fn the_advisory_fires_on_every_third_reread_and_no_other_read() {
    let path = temp_file("cadence", 40);
    let ctx = test_ctx_with_cfg(0, cfg("cadence"));

    let calls = vec![page(&path, 1, 20); 10];
    assert_eq!(
        advised(&ctx, calls).await,
        vec![
            false, false, false, true, false, false, true, false, false, true
        ]
    );
}

/// The negative control, and half of why the count means anything: paging
/// forward re-reads nothing, however many pages it takes. A set that merges
/// touching ranges (`1..21` and `21..41` become `1..41`) would call the second
/// page a reread if the test used containment instead of strict intersection.
#[tokio::test]
async fn paging_forward_through_a_file_is_never_a_reread() {
    let path = temp_file("paging", 200);
    let ctx = test_ctx_with_cfg(0, cfg("paging"));

    let calls = (0..8).map(|n| page(&path, 1 + n * 20, 20)).collect();
    assert_eq!(advised(&ctx, calls).await, vec![false; 8]);
}

/// Any strict intersection counts, however little the arguments resemble the
/// earlier call: a window wholly inside an earlier one, one that straddles its
/// edge, and a whole-file read with no `limit` at all.
#[tokio::test]
async fn an_intersection_counts_however_the_arguments_differ() {
    let path = temp_file("shapes", 400);
    let ctx = test_ctx_with_cfg(0, cfg("shapes"));
    let p = path.to_str().unwrap();

    assert_eq!(
        advised(
            &ctx,
            vec![
                // Establishes lines 1..=200.
                json!({"path": p, "offset": 1, "limit": 200}),
                // Wholly inside it.
                json!({"path": p, "offset": 150, "limit": 50}),
                // Straddles its trailing edge.
                json!({"path": p, "offset": 100, "limit": 200}),
                // No limit at all: to the end of the file, so it intersects too.
                json!({"path": p}),
            ],
        )
        .await,
        vec![false, false, false, true]
    );
}

/// A read that lands past the end of the file observes no lines, so it can
/// neither be a reread nor make the next read one.
#[tokio::test]
async fn a_read_past_the_end_of_the_file_observes_nothing() {
    let path = temp_file("past-eof", 4);
    let ctx = test_ctx_with_cfg(0, cfg("past-eof"));
    let p = path.to_str().unwrap();

    let calls = vec![
        json!({"path": p, "offset": 100}),
        json!({"path": p, "offset": 100}),
        json!({"path": p, "offset": 100}),
        json!({"path": p, "offset": 100}),
    ];
    assert_eq!(advised(&ctx, calls).await, vec![false; 4]);
}

/// Compaction folded the earlier results away, so their lines are not in front
/// of the model any more and reading them again is correct behaviour. Without
/// this reset the advisory fires on exactly that — worse than not firing at all.
#[tokio::test]
async fn compaction_clears_what_the_model_is_held_to() {
    let path = temp_file("compacted", 40);
    let provider =
        kloop_provider::Provider::mock(vec![vec![kloop_protocol::AssistantBlock::Text {
            text: "what happened so far".into(),
        }]]);
    let cfg = TestConfig::new("plan151-compacted")
        .provider(provider)
        .context_window(Some(200_000))
        .build();
    let ctx = test_ctx_with_cfg(0, Arc::clone(&cfg));

    // Three reads: two rereads, one short of the advisory.
    assert_eq!(
        advised(&ctx, vec![page(&path, 1, 20); 3]).await,
        vec![false; 3]
    );

    let mut history = History::new(cfg.offload_dir.clone());
    history.record(kloop_protocol::Message::user_text("old request"));
    history.record(kloop_protocol::Message::assistant(vec![
        kloop_protocol::ContentBlock::Text {
            // Fat enough that the keep budget has to fold it, whatever that
            // budget is today.
            text: "old work ".repeat(200_000),
        },
    ]));
    history.record(kloop_protocol::Message::user_text("current request"));
    crate::compact::run_compaction(
        &cfg,
        cfg.provider_route.model(),
        &mut history,
        &CancellationToken::new(),
    )
    .await
    .expect("compaction should succeed");

    // The count starts over, so three more reads stay silent instead of firing
    // on the fourth.
    assert_eq!(
        advised(&ctx, vec![page(&path, 1, 20); 3]).await,
        vec![false; 3]
    );
}

/// The lines in context stop describing the file the moment its bytes change,
/// so the count starts over — and it keys off the file version rather than off
/// which tool wrote, which is the only way an out-of-band write counts. In this
/// repo's own logs `edit_file` never appears and `write_file` appears five
/// times: edits go through `bash`, and a rule that watched tool names would
/// have called the read after one of them a reread.
#[tokio::test]
async fn a_changed_file_clears_the_count_whoever_changed_it() {
    let path = temp_file("rewritten", 40);
    let ctx = test_ctx_with_cfg(0, cfg("rewritten"));

    assert_eq!(
        advised(&ctx, vec![page(&path, 1, 20); 3]).await,
        vec![false; 3]
    );

    // Not a tool call: exactly what a `bash` heredoc or another process does.
    std::fs::write(&path, body(40, "rewritten")).unwrap();

    assert_eq!(
        advised(&ctx, vec![page(&path, 1, 20); 3]).await,
        vec![false; 3]
    );
}

/// The same reset seen from the tool side, which is the case the plan named.
#[tokio::test]
async fn editing_the_file_clears_the_count() {
    let path = temp_file("edited", 40);
    let ctx = test_ctx_with_cfg(0, cfg("edited"));
    let p = path.to_str().unwrap();

    let whole = json!({"path": p});
    assert_eq!(advised(&ctx, vec![whole.clone(); 3]).await, vec![false; 3]);

    let (out, is_error) = run_tool(
        "edit_file",
        json!({"path": p, "old_string": "line 7", "new_string": "edited 7"}),
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");

    assert_eq!(advised(&ctx, vec![whole; 3]).await, vec![false; 3]);
}

/// Counts are per agent. A sub-agent's reads are its own context, so they must
/// not push the parent over the line — nor be pushed over it by the parent.
/// This falls out of `subagent_from` handing the child a fresh `FileState`;
/// the test is here because that is load-bearing for the advisory, not just for
/// write authority.
#[tokio::test]
async fn a_sub_agent_counts_separately_from_its_parent() {
    let path = temp_file("per-agent", 40);
    let parent_cfg = cfg("per-agent");
    let child_cfg = Arc::new(parent_cfg.subagent_from(
        &parent_cfg.effective_workspace(),
        None,
        "agent-1".parse().unwrap(),
    ));
    let parent = test_ctx_with_cfg(0, parent_cfg);
    let child = test_ctx_with_cfg(1, child_cfg);

    // One shared counter would make this sequence read 0,1,2,3,4,5,6 and fire on
    // the child's third read and the parent's fourth. Two counters fire once,
    // on the parent's fourth — so both halves of the isolation are pinned by
    // where the `true` is, not just by whether there is one.
    assert_eq!(
        advised(&child, vec![page(&path, 1, 20); 3]).await,
        vec![false; 3]
    );
    assert_eq!(
        advised(&parent, vec![page(&path, 1, 20); 4]).await,
        vec![false, false, false, true]
    );
}

/// The advisory never blocks, delays or replaces anything: the read that
/// triggers it returns the same bytes it would have returned without it, with
/// the notice appended. Asserted as the whole result, because "contains the
/// page" would also pass if the advisory had eaten part of it.
#[tokio::test]
async fn the_read_that_triggers_it_still_returns_everything() {
    let path = temp_file("intact", 4);
    let ctx = test_ctx_with_cfg(0, cfg("intact"));
    let p = path.to_str().unwrap();

    // The advisory names the path the count is keyed on, which is the canonical
    // one: two spellings of one file share a counter, so they must share a name.
    let canonical = std::fs::canonicalize(&path).unwrap();
    let canonical = canonical.display();
    let (plain, _) = run_tool("read_file", json!({"path": p}), &ctx).await;
    assert_eq!(plain, "1\tline 1\n2\tline 2\n3\tline 3\n4\tline 4\n5\t");

    for _ in 0..2 {
        let (out, _) = run_tool("read_file", json!({"path": p}), &ctx).await;
        assert_eq!(out, plain);
    }
    let (out, is_error) = run_tool("read_file", json!({"path": p}), &ctx).await;
    assert!(!is_error);
    assert_eq!(
        out,
        format!(
            "{plain}\n<system-reminder>That read covered lines of {canonical} you already have in \
             this conversation; 3 reads of this file have now done that. Those lines are still \
             above — look back at the earlier result instead of reading again. If re-reading is \
             not getting you what you need, change approach: widen the range, grep for what you \
             are after, or look somewhere else.</system-reminder>"
        )
    );
}

/// The advisory is model-visible, so it has to survive a rollout the way every
/// other model-visible byte does. Riding on the tool result is what buys that:
/// the block the turn records is the block dispatch returned, advisory included,
/// so a resumed or forked session replays the same history rather than a
/// conversation the model never had.
#[tokio::test]
async fn the_advisory_is_recorded_as_part_of_the_tool_result() {
    let path = temp_file("recorded", 40);
    let p = path.to_str().unwrap().to_string();
    let read = |n: usize| {
        kloop_provider::MockTurn::Blocks(vec![kloop_protocol::AssistantBlock::ToolUse {
            id: format!("call-{n}"),
            name: "read_file".into(),
            input: json!({"path": p, "offset": 1, "limit": 20}),
        }])
    };
    let provider = kloop_provider::Provider::mock_scripted(vec![
        read(1),
        read(2),
        read(3),
        read(4),
        kloop_provider::MockTurn::Blocks(vec![kloop_protocol::AssistantBlock::Text {
            text: "done".into(),
        }]),
    ]);
    let cfg = TestConfig::new("plan151-recorded")
        .provider(provider)
        .max_rounds(Some(6))
        .build();
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(kloop_protocol::Message::user_text("read it a few times"));
    let end = crate::agent::run_turn(
        &cfg,
        &mut history,
        &(Arc::new(super::testutil::SilentUi) as Arc<dyn super::Ui>),
        &CancellationToken::new(),
        0,
    )
    .await;
    assert_eq!(end.reason, crate::agent::EndReason::Completed);

    // Exactly one of the four recorded results carries it: the fourth read, the
    // one whose count reached three.
    let advisories: Vec<String> = history
        .messages()
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            kloop_protocol::ContentBlock::ToolResult { content, .. } => Some(content.as_text()),
            _ => None,
        })
        .filter(|text| text.contains("<system-reminder>"))
        .map(|text| text.into_owned())
        .collect();
    assert_eq!(advisories.len(), 1, "{advisories:?}");
    assert!(
        advisories[0].contains("3 reads of this file have now done that"),
        "{}",
        advisories[0]
    );
}

// ---------------------------------------------------------------------------
// Plan 151, second half: every call runs under a budget its own tool declares.
//
// The hole was narrow and real: `bash` kills its own process tree at
// `timeout_ms` and hooks have their own limit, but an external `ToolSource` —
// an MCP server, a web provider — had no bound at all, so one that stopped
// answering hung the session. The budget is declared by the tool (a `Builtin`
// arm, or `ToolSource::call_timeout`) rather than by a table of names in the
// dispatcher, and the default is no budget: a tool waiting on a person or
// running a whole sub-agent has no deadline anyone could pick for it.

/// A source tool that never returns, and counts how many calls are inside it.
/// It observes no cancellation at all — which is the case that matters, since
/// the seam hands a source no token and a wedged server would not read one.
struct HungSource {
    budget: Duration,
    entered: Arc<AtomicUsize>,
}

impl super::ToolSource for HungSource {
    fn defs(&self) -> Arc<[kloop_protocol::ToolDef]> {
        Arc::from(vec![
            kloop_protocol::ToolDef {
                name: "hung__wait".into(),
                description: "never returns".into(),
                schema: json!({"type": "object"}),
            },
            kloop_protocol::ToolDef {
                name: "hung__answer".into(),
                description: "returns at once".into(),
                schema: json!({"type": "object"}),
            },
        ])
    }

    fn is_readonly(&self, _tool: &str) -> bool {
        true
    }

    fn call_timeout(&self, _tool: &str) -> Option<Duration> {
        Some(self.budget)
    }

    fn call<'a>(
        &'a self,
        tool: &'a str,
        _input: &'a Value,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<super::SourceOutput>> + Send + 'a>,
    > {
        Box::pin(async move {
            if tool == "hung__answer" {
                return Ok(super::SourceOutput::text("answered".into()));
            }
            self.entered.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<()>().await;
            unreachable!("the hung tool never returns")
        })
    }
}

fn hung_ctx(tag: &str, budget: Duration) -> (super::ToolCtx, Arc<AtomicUsize>) {
    let entered = Arc::new(AtomicUsize::new(0));
    let source = Arc::new(HungSource {
        budget,
        entered: Arc::clone(&entered),
    });
    let ctx = super::testutil::test_ctx_with_sources(
        0,
        &format!("plan151-{tag}"),
        vec![source as Arc<dyn super::ToolSource>],
    );
    (ctx, entered)
}

/// A tool that never stops gets its future dropped, and the model is told so in
/// words that do not claim anything was killed — because nothing was. This is
/// the whole point of the second half: before it, this call never returned.
#[tokio::test]
async fn a_tool_that_never_returns_times_out_and_says_it_may_still_be_running() {
    let (ctx, entered) = hung_ctx("hung", Duration::from_millis(40));
    let (out, is_error) = run_tool("hung__wait", json!({}), &ctx).await;

    assert!(is_error, "{out}");
    assert_eq!(entered.load(Ordering::SeqCst), 1);
    assert_eq!(
        out,
        "hung__wait: timed out after 40ms. The call was asked to cancel and this \
         result is what it settled to. Nothing killed it: a tool that does not honour \
         cancellation may still be running in the background, and its side effects \
         may still land."
    );
}

/// The budget cancels before it drops. A tool that watches its token settles
/// under its own power and hands back a real verdict, which is why the future
/// is not simply raced: dropping it at the instant the budget expires would
/// have stranded the cleanup this asserts ran.
#[tokio::test]
async fn the_call_is_cancelled_first_and_allowed_to_settle() {
    struct Settling {
        saw_cancel: Arc<AtomicUsize>,
        cleaned_up: Arc<AtomicUsize>,
    }

    impl super::ToolSource for Settling {
        fn defs(&self) -> Arc<[kloop_protocol::ToolDef]> {
            Arc::from(vec![kloop_protocol::ToolDef {
                name: "settling__work".into(),
                description: "stops when asked".into(),
                schema: json!({"type": "object"}),
            }])
        }

        fn is_readonly(&self, _tool: &str) -> bool {
            true
        }

        fn call_timeout(&self, _tool: &str) -> Option<Duration> {
            Some(Duration::from_millis(40))
        }

        fn call<'a>(
            &'a self,
            _tool: &'a str,
            _input: &'a Value,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<super::SourceOutput>> + Send + 'a>,
        > {
            Box::pin(async move {
                // Stands in for a tool that does watch its token: it notices,
                // unwinds, and reports. The seam passes no token today, so the
                // test models the shape rather than borrowing one.
                tokio::time::sleep(Duration::from_millis(80)).await;
                self.saw_cancel.fetch_add(1, Ordering::SeqCst);
                self.cleaned_up.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("settling__work: stopped on request")
            })
        }
    }

    let saw_cancel = Arc::new(AtomicUsize::new(0));
    let cleaned_up = Arc::new(AtomicUsize::new(0));
    let source = Arc::new(Settling {
        saw_cancel: Arc::clone(&saw_cancel),
        cleaned_up: Arc::clone(&cleaned_up),
    });
    let ctx = super::testutil::test_ctx_with_sources(
        0,
        "plan151-settling",
        vec![source as Arc<dyn super::ToolSource>],
    );

    let (out, is_error) = run_tool("settling__work", json!({}), &ctx).await;
    assert!(is_error, "{out}");
    // The cleanup ran to completion before the result came back — the budget
    // waited for it rather than dropping the future at expiry.
    assert_eq!(
        (
            saw_cancel.load(Ordering::SeqCst),
            cleaned_up.load(Ordering::SeqCst)
        ),
        (1, 1)
    );
    assert!(
        out.starts_with("settling__work: timed out after 40ms."),
        "{out}"
    );
}

/// One timeout does not take its batch down with it. The dispatcher runs
/// consecutive read-only calls concurrently under `MAX_CONCURRENT_TOOL_CALLS`
/// permits; a hung call holds a permit for its budget and no longer, so the
/// others neither fail nor queue behind it.
#[tokio::test]
async fn one_timeout_does_not_fail_the_rest_of_the_batch() {
    let (ctx, entered) = hung_ctx("batch", Duration::from_millis(40));
    let calls = vec![
        ("a".to_string(), "hung__answer".to_string(), json!({})),
        ("b".to_string(), "hung__wait".to_string(), json!({})),
        ("c".to_string(), "hung__answer".to_string(), json!({})),
    ];

    let results: Vec<(String, bool)> = super::dispatch_tools(calls, &ctx)
        .await
        .into_iter()
        .map(|block| match block {
            kloop_protocol::ContentBlock::ToolResult {
                content, is_error, ..
            } => (content.as_text().into_owned(), is_error),
            other => panic!("expected a tool result, got {other:?}"),
        })
        .collect();

    assert_eq!(entered.load(Ordering::SeqCst), 1);
    assert_eq!(results[0], ("answered".to_string(), false));
    assert_eq!(results[2], ("answered".to_string(), false));
    assert!(results[1].1, "{:?}", results[1]);
    assert!(results[1].0.contains("timed out after"), "{:?}", results[1]);
}

/// `bash` already kills its own process tree at `timeout_ms`, and that bound is
/// the model's to choose. The outer one is derived from it rather than
/// replacing it, so it can only ever catch a `bash` that failed to stop itself.
#[test]
fn bash_keeps_the_timeout_the_model_asked_for() {
    let budget = |input| super::Builtin::Bash.timeout(&input);
    let grace = super::BASH_TIMEOUT_GRACE;

    // The model's value wins, whatever it is — high or low.
    assert_eq!(
        budget(json!({"command": "sleep 1", "timeout_ms": 5_000})),
        Some(Duration::from_secs(5) + grace)
    );
    assert_eq!(
        budget(json!({"command": "sleep 1", "timeout_ms": 900_000})),
        Some(Duration::from_secs(900) + grace)
    );
    // No value: bash's own default, not some new number.
    assert_eq!(
        budget(json!({"command": "sleep 1"})),
        Some(Duration::from_secs(60) + grace)
    );
    // Strictly above the inner bound in every case, so the killing one wins.
    for timeout_ms in [1_u64, 1_000, 60_000, 900_000] {
        let outer = budget(json!({"command": "x", "timeout_ms": timeout_ms})).unwrap();
        assert!(outer > Duration::from_millis(timeout_ms), "{timeout_ms}");
    }
}

/// The conservative default, asserted as the whole table rather than by
/// spot-check: a tool that waits on a person or runs a whole sub-agent must not
/// acquire a deadline because someone added a variant and reached for a number.
#[test]
fn only_the_tools_that_can_be_bounded_carry_a_budget() {
    use super::Builtin;

    let bounded: Vec<&str> = super::builtin::ALL
        .iter()
        .filter(|tool| tool.timeout(&json!({"command": "x"})).is_some())
        .map(|tool| tool.name())
        .collect();
    assert_eq!(bounded, vec!["bash", "read_file", "grep", "glob"]);
    assert_eq!(
        Builtin::ReadFile.timeout(&json!({})),
        Some(super::READ_ONLY_TOOL_TIMEOUT)
    );
    // And a source that declares nothing gets the external default — the bucket
    // the mechanism was built for, where "no bound at all" was the old answer.
    struct Quiet;
    impl super::ToolSource for Quiet {
        fn defs(&self) -> Arc<[kloop_protocol::ToolDef]> {
            Arc::from(Vec::<kloop_protocol::ToolDef>::new())
        }
        fn is_readonly(&self, _tool: &str) -> bool {
            false
        }
        fn call<'a>(
            &'a self,
            _tool: &'a str,
            _input: &'a Value,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<super::SourceOutput>> + Send + 'a>,
        > {
            unreachable!("never called")
        }
    }
    assert_eq!(
        super::ToolSource::call_timeout(&Quiet, "anything"),
        Some(super::EXTERNAL_TOOL_TIMEOUT)
    );
}
