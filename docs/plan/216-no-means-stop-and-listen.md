# Plan 216 — 选 No,就停下来听

> 来源:2026-09-29,plan 214/215 同一次会话。用户截图(计划审批面板，计划末尾写着"请选 A/B/C"):
> 「我选No,这轮turn就继续询问，结束turn是否更合理。」核对后用户同意推荐:**所有审批**(不只计划)
> 选 No 都结束本轮。「同意」。

## 一、现状：文案承诺了一件没做的事

- 每个审批面板的拒绝行都写着 **"No, and tell kloop what to do differently"**(`tui/src/app.rs` 的
  `confirm_choices`),可选了之后没有地方可以说。
- 普通工具被拒，模型拿到 `user_denial`(`core/src/permissions.rs`):"Do not retry the same call; take a
  different approach, or ask the user how to proceed." 计划被拒(`tools/plan_mode.rs`):"Stay in plan mode …
  refine the plan, and call exit_plan_mode again." **两种都接着跑。**
- 真实会话里的后果：计划最后问"请选 A/B/C",用户想答一句"选 C",只能等模型猜、或 Esc 打断再打字;
  一次选 No 之后模型又自己出了一道选择题(引出 plan 214 删掉的那个 `Notes (optional)`)。

**codex 同一句文案的语义就是停。** `refs/codex/codex-rs/protocol/src/protocol.rs` 的 `ReviewDecision` 分两种拒绝:
`Denied`("should not execute it, but it should continue the session and try something else",界面上是
"No, continue without running it")与 `Abort`("should not do anything until the user's next command",
界面上正是 "No, and tell Codex what to do differently",`tui/src/bottom_pane/approval_overlay.rs`)。
kloop 借了后者的文案，做的是前者的语义。

## 二、裁决

- **人按的 No(含 Esc)= 这次不执行，并且本轮在这一批工具做完后结束，把话筒交还给用户。**
  用户想让模型继续、只是别跑这一条时，下一句说"跳过它，继续"即可。
- **没有人在背后的拒绝仍是"不执行、继续"**:headless 的 `DenyApprover`(`cli/src/headless.rs`)——没人可以
  听，停下来只会让任务白白失败。

## 三、形状

### 3.1 决定分两种

`Decision`(`permissions.rs` 的 `pub enum Decision { Allow(ApprovalScope), Deny }`)加一种:

```rust
pub enum Decision {
    Allow(ApprovalScope),
    /// Not this call; the model carries on without it. For an approver with
    /// no one behind it (headless), and for a client that asked for exactly that.
    Deny,
    /// Not this call, and stop the turn here: the user will say what to do instead.
    Stop,
}
```

谁产出什么:

| 来源 | 今天 | 之后 |
|---|---|---|
| TUI 的 No / Esc | `Deny` | `Stop` |
| plain REPL 的 `n`、其它非法输入、EOF(`cli/src/ui.rs`) | `Deny` | `Stop` |
| server `decision: "cancel"` | `Deny` | `Stop` |
| server `decision: "decline"` | `Deny` | `Deny`(照 codex:客户端明说"继续") |
| server 未知/缺失/畸形/断连(fail closed) | `Deny` | `Stop`(审批通道坏了，不该让模型接着动手) |
| headless `DenyApprover` | `Deny` | `Deny` |
| TUI 回复 sender 被丢(轮次已死) | 读作 `Deny` | 不变(轮次本来就结束了) |

### 3.2 停止信号：跟着"轮次树"走的一个令牌

- `Turn`/`ToolCtx` 加 `stop: CancellationToken`,与 `cancel` 并列但语义不同:`cancel` 是立即中断，`stop` 是
  "这一批做完就收"。顶层轮次开始时新建;**前台子 agent 沿用父的同一个**(照 `subagent.rs` 里前台沿用
  `ctx.cancel.clone()` 的做法),于是子 agent 里的 No 也会让父轮次停;**后台子 agent 新建自己的**(照
  `spawn_background` 的 `own_cancel`),它的 No 只停它自己。
- 三个在轮次里问人的地方，拿到 `Stop` 就 `ctx.stop.cancel()`:
  - `tools/mod.rs` 的主闸(`check_call_with_resolved_path` 那次调用);闸目前返回 `Err(String)`,需要让它能
    说出"这是 Stop"——建议给 `check_call_with_resolved_path` 一个带种类的拒绝(`check`/`check_call` 这两个
    测试常用入口可以继续返回字符串，少动几十处测试);
  - `tools/plan_mode.rs`(`PlanExitOutcome` 加 `Stopped`);
  - `tools/bash.rs` 的沙箱升级(`EscalationOutcome` 加 `Stopped`)。
- `tools/inject.rs`(slash 命令里的 `` !`cmd` ``)与 `tools/scheduler.rs` 不在轮次里:`Stop` 当 `Deny` 处理。
- **已经排队的其它审批**:问人之前先看 `ctx.stop`,已停就直接拒、不问;正在等的 `confirm` 要与
  `stop.cancelled()` 赛跑，停了就撤回——TUI 那边，队列前进时跳过 `reply.is_closed()` 的项(tokio
  `oneshot::Sender::is_closed`),否则用户会被接着问一串已经没人等的问题。
- `dispatch_round`(`agent.rs`,记完 tool_results、查完 `cancel` 之后):`stop` 已触发 → 结束本轮，不再采样。

### 3.3 模型看到什么

- 被拒的那条:"The user declined this {name} call and stopped the turn to tell you what to do instead. Their
  next message says what they want; do not retry this call unless they ask for it."
- 同批里因此没问、没跑的:"Not run: the user stopped the turn before this call was approved."
- 计划被拒:"The user did not approve the plan and stopped the turn to say what to change. You are still in
  plan mode; revise the plan from their next message."
- **`exit_plan_mode` 的工具描述重写**(`tools/plan_mode.rs` 的 `exit_plan_mode_def`,用户 2026-09-29 定:
  「要明确显示plan了，yes就是做，no就是停」;另要求有待选的分歧先用 `ask_user_question` 问清，计划不要以
  "请选 A/B/C"结尾——截图里那份正是这样)。**与本条的行为同一次提交**,否则"No 就是停"是一句假话。
  措辞(model-facing,英文):

  > Present your finished implementation plan to the user for a go/no-go decision. Use this ONLY when the
  > session is in plan mode, you have finished exploring (read-only), and you have a concrete, step-by-step
  > plan. The user sees the full plan and answers Yes or No. **Yes means implement exactly this plan**: plan
  > mode turns off and you proceed. **No means stop**: the turn ends there, you stay in plan mode, and the
  > user's next message says what to change. Yes and No are the only answers, so the plan must already be
  > decided: settle every open choice with ask_user_question before calling this, and never end the plan by
  > asking the user to pick (for example "choose A, B or C"). The plan goes in the `plan` argument and nowhere
  > else: it is shown in full, so do not also repeat it in your reply. Do not use it to ask general questions
  > or when not in plan mode.

  `enter_plan_mode` 的描述与返回文案里 "for approval" 的说法与此一致，不用改;测试里若有对描述全文的
  整串断言，一并更新。

### 3.4 轮次怎么结束

- 历史保持合法:tool_use 都有 tool_result;下一条用户消息接在 tool_results 之后(与 Esc 打断工具之后
  用户再说话是同一种形状——实施时确认三条 rail 都收)。
- `EndReason`:见第六节第 1 问。

## 四、坑

- **headless 不能被连带改掉。** `DenyApprover` 保持 `Deny`;加一条测试钉住"headless 下被拒后模型仍继续"。
- **`Decision` 的所有 match**:`confirm_exit_plan` 的 `Decision::Allow(WorkspaceSession | Project) |
  Decision::Deny => Declined`、主闸与升级的 `let Decision::Allow(scope) = decision else`、server/plain 的映射、
  十几个测试 approver——编译器会列全;`let … else` 那两处不会报错，要逐个看。
- **子 agent 的结果**:前台子 agent 因 Stop 结束时，它交回父的 `run_agent` 结果要说明"用户在子 agent 里
  叫停";后台子 agent 的终态注回主 agent 时同理(`classify_background`)。
- **`run_program`/workflow**:程序里的工具调用走同一个闸;停了之后程序后续调用都被拒，程序跑完这一轮即结束。
- **计划模式不退出**:计划被 Stop 时仍留在 Plan 档(与今天的拒绝相同)。
- **TUI 的计划 cell** 仍打 `✗ not approved — still planning`;可考虑改成更贴切的字样，非必须。

## 五、测试与验收

- `make check` 全绿。
- core:scripted approver 对第 0 轮的 bash 回 `Stop` → 轮次在第 0 轮后结束(scripted provider 准备了第 1 轮
  回复，断言**没有被请求**),历史里 tool_use/tool_result 配对、结果是 3.3 的文案;同批两个待批调用，第一个
  `Stop` → 第二个**没被问**(approver 计数 1)且结果是"Not run";前台子 agent 里 `Stop` → 父轮次也结束;后台
  子 agent 里 `Stop` → 主轮次不受影响;`exit_plan_mode` 被 `Stop` → 轮次结束且仍在 Plan 档;沙箱升级被
  `Stop` → 同上;headless `Deny` → 照旧继续(现有测试保持绿)。
- TUI:No 与 Esc 回 `Stop`;队列里回复已关闭的审批被跳过、不显示。
- server:`cancel` → 结束本轮;`decline` → 继续;畸形回复 → 结束本轮(契约测试)。
- plain:`n` → `Stop`。

## 六、开工时定(问用户，一次一个)

1. **轮次因 Stop 结束，要不要单独的结束原因?** 推荐加 `EndReason::Stopped`(server 的 turn 状态 `stopped`),
   TUI 在转录末尾补一行 dim 提示"stopped — tell kloop what to do differently",让人一眼知道"轮到我说了"。
   另一选择是复用 `Completed`、什么都不显示。
2. **server 的 `decline` 是否也改成停?** 推荐不改：照 codex 保留"不执行、继续"与"停下来"两种，给客户端选。

## 七、收尾

- DESIGN.md:权限/审批一节(拒绝的两种语义与来源表)、Plan mode 一段("reject leaves the session in Plan"
  之外补"并结束本轮")、server 协议里 `decision` 各取值的含义、TUI 面板一段(Esc/No 的效果)。
- HANDOFF:状态行 ✅ + 提交号;有新教训就写。
