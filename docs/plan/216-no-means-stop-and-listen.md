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

1. ~~**轮次因 Stop 结束，要不要单独的结束原因?**~~ 用户「同意」推荐:加 `EndReason::Stopped`(server 的 turn
   状态 `stopped`),TUI 在转录末尾补一行 dim 提示"stopped — tell kloop what to do differently"。
2. ~~**server 的 `decline` 是否也改成停?**~~ 用户「同意」推荐：不改,`decline` = 不执行、继续,`cancel` = 停。
3. (开工时新发现、另问)**同批里排在 No 之后、本来不用审批的调用要不要也跳过?** 3.2 只写了"要审批的直接拒",
   没想到同批里还有自动放行的写(工作目录内的 `edit_file`、沙箱自动放行的 bash、bypass 下几乎一切)——照字面，
   按了 No 之后它们照样改文件。用户「同意」推荐:**已停之后，同批里还没开始的调用一律不跑**,正在跑的不打断。

## 七、收尾

- DESIGN.md:权限/审批一节(拒绝的两种语义与来源表)、Plan mode 一段("reject leaves the session in Plan"
  之外补"并结束本轮")、server 协议里 `decision` 各取值的含义、TUI 面板一段(Esc/No 的效果)。
- HANDOFF:状态行 ✅ + 提交号;有新教训就写。

## 八、✅ 完成

2026-09-29 当次会话做完，一次提交(SHA 以本条所在提交为准)。`make check` 全绿。

与第三节的出入(都是实现时定的，不改语义):

- **`stop` 放在 `TurnOptions` 里**,不单独加 `Turn` 字段、也不给 `run_turn_with_options`/`turn_rounds` 再加一个
  位置参数:options 本来就穿过整个循环。`run_turn_in_execution`/`run_structured_turn_in_execution` 多一个
  `stop` 参数，由调用方决定(前台传 `ctx.stop`,后台传新的)。`run_turn`/`run_turn_with_input` 每次新建。
- **停止检查放在轮次循环头**,不放在 `dispatch_round` 里:效果对自己的批次一样，另外还能接住"自己这批已经
  结束、兄弟子 agent 才按的 No",不会多采样一轮。
- **"没跑"只有一种文案**:"Not run: the user stopped the turn before this call ran."——没开始的、审批被撤回的
  都用它(第三问的结果让 3.3 的"before this call was approved"不再准确)。检查放在 `run_one` 开头(还没发
  `ItemStarted`,不跑 pre-tool hook);审批等待与 `stop` 赛跑的那一处放在 `PreparedCall::authorize`。
  `exit_plan_mode` 与沙箱升级的等待也赛跑(`unless_stopped`),撤回的升级按拒绝处理(保留沙箱内的失败)。
- **后台子 agent 被 No 停下**:状态记 `Aborted`(界面上是 "stopped"),但与 stop_agent 停的不同，**要注回一条**
  "[sub-agent stopped by the user] … wait for their next message rather than re-dispatching it"——模型没停它，
  不注回就不知道它为什么没了。
- **workflow 的子 agent**:workflow 桥不走 `run_one`,停了之后还可能起新的子 agent;它们在第 0 轮前就以
  `Stopped` 结束，所以报错文案不写"在这个子 agent 里拒绝了",只写"用户拒绝了一次审批、停了本轮"。
- **server 断连 / 发不出去**也按 `Stop`(3.1 表里"断连"一行);plain 的选项文案从 `n = deny` 改成
  `n = no, and tell kloop what to do differently`,与 TUI 一致。
- **TUI 的计划 cell** 仍打 `✗ not approved — still planning`(第四节说可改非必须):仍在 Plan 档，这句仍然对。

| 测试 | 锁住什么 |
|---|---|
| `tools::stop_tests::a_no_leaves_the_rest_of_the_batch_unrun` | `[bash(问→Stop), 目录内 write_file(自动放行), bash]`:结果依次是 stop 文案、Not run、Not run(整对象);只问一次;只有 b1 发过 `ItemStarted`;文件没写;`cancel` 没被触发 |
| `tools::stop_tests::a_deny_refuses_one_call_and_the_batch_goes_on` | headless 的形状:`Deny` 只拒这一条，同批下一条照跑,`stop` 不触发 |
| `tools::stop_tests::a_waiting_approval_is_withdrawn_when_the_turn_stops` | 正在等的审批遇到 stop:5 秒内收成 Not run,回复通道被关闭 |
| `agent::stop_tests::a_no_ends_the_turn_without_asking_the_model_again` | `EndReason::Stopped`、状态串 `stopped`、脚本里备好的第 1 轮**没被请求**、历史三条且结果配对 |
| `agent::stop_tests::a_no_inside_a_foreground_sub_agent_stops_the_parent` | 子 agent 里的 No 让父也停:只请求了父第 0 轮与子第 0 轮;`run_agent` 结果写明用户在子 agent 里叫停 |
| `agent::stop_tests::a_no_on_the_plan_ends_the_turn_in_plan_mode` | 计划被 No:轮次停、仍在 Plan 档、结果文案整句 |
| `subagent::tests::a_no_inside_a_background_agent_leaves_the_parent_running` | 后台子 agent 里的 No:注回 "[sub-agent stopped by the user]…",父的 `stop` 不受影响 |
| `subagent::tests::classify_background_maps_outcomes` | `Stopped` → `(Aborted, 注回说明)` |
| `bash::tests::seatbelt::escalation_no_keeps_denial_and_stops_the_turn` | 沙箱升级被 No:保留沙箱失败、以 `ESCALATION_STOPPED` 结尾、`stop` 触发(macOS) |
| `permissions::tests::a_stop_is_its_own_refusal_only_inside_a_turn` / `…escalate_sandbox_maps_decision_and_mode` / `…confirm_exit_plan_reports_a_stop_and_stays_in_plan` | 闸里 `Stop` → `Refusal::Stopped`,`check_call`(注入/调度)读作普通拒绝;升级与计划各自的 `Stopped` |
| `app::tests::withdrawn_confirms_leave_the_queue` 等 | TUI:No/Esc/`n`/最后一行都回 `Stop`;回复已关闭的审批在下一个事件时出队，别处的留下;`Stopped` 的提示行 |
| `server.rs::approval_cancel_or_a_bad_reply_stops_the_turn_but_decline_goes_on` | 契约:`cancel` 与畸形回复 → `turn/completed` 状态 `stopped` 且不再问;`decline` → `completed`(模型接着跑) |
| `ui::tests::approval_prompt_and_answers_follow_advertised_scopes` | plain:`n` 与未提供的范围都是 `Stop` |

反证：五处机制逐一撤回，各自的测试都红——去掉循环头检查(三条 agent 测试)、去掉 `run_one` 的提前检查
(批次测试，靠 `ItemStarted` 断言;只看结果文案时它被 authorize 的赛跑遮住，见教训 204)、审批等待不赛跑
(撤回测试，超时)、前台子 agent 拿新令牌(父停测试)、TUI 不剪队列(出队测试)。

## 九、真实测试与计划措辞(同日追加，第二次提交)

用户问「真实测试了吗」——第一次提交只跑了 `make check`。补测用当时的二进制、临时 HOME(复制配置，0600/0700)、
临时工作区,pty 驱动(脚本留在会话 scratchpad,不进仓库)。

**机制，全部照预期:**

- plain + Responses(sky-bj)与 plain + Messages(sky-claude):审批回 `n` → `[stopped — …]`,终态行 `stopped`,
  下一句"只写到当前目录"被接住并执行——两条 rail 都收"tool_results 后面接一条用户消息"。Chat 兼容 rail
  本机配置里没有，没测。
- 两次模型都把"写工作区外"和"写当前目录 b.txt"放在同一批:No 之后 b.txt 是 "Not run",文件确实没写——
  第六节第 3 问的决定在真实流量里触发了。
- 真 TUI + plan 模式：计划审批按 Esc,两轮都 `stopped`、仍在 Plan、转录末尾有提示行，改完再交。

**计划措辞只部分起效 → 量了四版，换了一版。** 第一次提交只改了 `exit_plan_mode` 描述(条件句"计划必须
已定"),而每次 plan 模式请求都带的 plan-mode 提示一个字没提。真实模型仍会以"请确认采用哪种"收尾。用户同意
先量再改。两道有真分歧的题(可配置问候语、多语言)+ 两道对照题(改 f-string;加类型注解和 docstring——后者
留了 docstring 风格这个诱饵),三个 provider(sky-bj / sky-claude / sky-us 各自的默认模型),每次采到第一个
`ask_user_question`/`exit_plan_mode` 调用为止:

| 版本 | 分歧题：先问 / 已定 / 留尾巴 | 对照题：多问 |
|---|---|---|
| A 第一次提交的措辞 | 9 / 2 / **5**(16) | 0 / 12 |
| B 规则进提示，三条动作;"代码或需求能定的你来定" | 6 / 10 / 0 | 0 / 6(只跑了 f-string) |
| C "用户可能两种都要的就问，哪怕有默认" | 17 / 0 / 0 | **2** / 12(sky-bj 问 docstring 风格) |
| D = C + "风格与惯例归你" | 12 / 3 / 0 | 0 / 9 |
| E = D 调语序(提交的版本，改完重量) | 12 / 3 / 0(15) | 0 / 9 |

B 消灭了尾巴，却让 sky-claude/sky-us 连"支持哪些语言"都自己定;C 反过来咬了诱饵;D/E 两头都守住。
E 的 24 个样本里只有一处"没加模块级 docstring,需要的话我再加"式的范围说明，计划本身是定的。另见
sky-claude 偶尔用日语回中文提问(A 里就有),与本条无关，未跟。

落地：`PLAN_MODE_REMINDER` 常量改成 `plan_mode_reminder(depth, questions)`——顶层三条动作;前端不能答
`ask_user_question` 时改说"在回复里问、不调 exit_plan_mode 就结束本轮";子 agent 只说"报告发现，计划由顶层写"
(旧文本叫子 agent 去调它调不了的 `exit_plan_mode`)。`exit_plan_mode` 描述同步成动作句。测试
`agent::tests::plan_mode_reminder_tells_the_top_level_what_to_do_with_a_users_choice` 整串钉住三种文本。教训 205。

