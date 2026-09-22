# Plan 191 — 一个没有发生过的回合

> 来源:2026-09-21 一次对话。用户问「kloop 用 esc 打断的请求,会放到历史里吗」,
> 查完答"会",接着问「如果打断的时候 llm 还没有返回,是不是就可以直接取消掉这个请求」,
> 再接着是判断:「实践中往往会有用户看自己写错了,马上 esc 打断,所以这种情况不应该记历史吧」
> 「这种情况,应该不记吧,否则污染了历史」。**✅ 同一次会话做完,见文末。**

## 一、原来是什么样

前端在 `run_turn` **之前** 就把用户消息 `history.record()` 了(TUI `lib.rs`、server
`lib.rs`、headless、plain 四处同形),而 `record` 是写透 rollout 的。于是 esc 打断之后:

- 用户那句话留在 `items` 和会话文件里;
- 已经流出来的部分 assistant 内容也留着(未签名 thinking、未派发的 tool_use 被
  `replayable_partial` 筛掉);
- 收尾再写一行 `turn_terminal{status:"aborted"}`。

"写错了马上 esc"于是留下一条没人回答的用户消息,下一轮照发给模型。

## 二、裁决

**用户的输入只有在这一轮真的产生了东西之后,才进对话。** 一个在模型开口之前被打断的
回合是**没有发生过的回合**:`items` 不动、会话文件一行不写(连 terminal 都不写)、
输入退回前端。

两个边界(用户 2026-09-21 拍板):

1. **只有 `Aborted` 作废,`Error` 照旧入册。** provider 报错(401、网络断、余额不足)
   同样是"模型零输出",但报错值得留痕——转录里要看得出"这次问失败了",否则用户只看到
   输入框里自己那句话原样躺着,不知道发生过什么。变更面也更小。
2. **判据是"流上有没有来过东西",不是"历史里留下了什么"。** `replayable_partial`
   会筛掉未签名 thinking,所以"partial 为空"同时covers"一个字没来"和"思考了两分钟但
   没有一个字可回放"两种情况。后者不是"什么都没发生"。

## 三、形状:History 的暂存位

核心是一条不变量,落在 `History` 一个类型里:

> 本轮输入 `stage()` 进暂存位——**进请求视图,不进 `items`、不落 rollout**。
> 任何"要排在它后面"的写入,入口第一行先把它提交。

于是漏不掉:`record` / `record_provider_assistant` / `record_turn_terminal` 都自动提交,
`drain_inbox` 用的就是 `record`,所以 steer 消息永远排在输入之后。两个刻意的例外:

- **`replace_all`(压缩)不提交。** 替换集是按 `messages()`(不含暂存)算出来的,先提交
  就等于把输入交给一个从未包含它的重写,即丢掉它。暂存留着,落在摘要之后——它本来就是
  对话里最新的东西,不是被折叠的那部分。
- **`record_provider_usage` 不提交。** 它是账本条目,不占转录位置;而压缩正是在"算出
  替换集"和"装上替换集"之间记一条 usage(plan 81 定的 `usage→compacted` 顺序),提交
  就会撞上上一条。回合被接受时由 `absorb_round` 显式提交一次,所以 usage 行仍然排在
  付钱的那条消息之后,rollout 行序与改动前逐字节一致。

`provider_request_view` 含暂存(模型必须看见要回答的问题),`estimated_tokens` 含暂存
(一大段粘贴正是能在入册前撑爆窗口的输入),`messages()` 不含(压缩和 rollout 的视图是
"会话已经承认的东西")。

## 四、改了什么

| 位置 | 改动 |
|---|---|
| `core/src/history.rs` | `staged: Vec<Message>` 字段 + `stage`/`has_staged`/`take_staged`/`commit_staged`;`record` 拆出 `record_committed` 避免递归;三个写入口提交、两个刻意不提交;请求视图与 token 估算含暂存 |
| `core/src/agent/sampling.rs` | `Sampled::Cancelled`/`SampleError::Cancelled` 带 `produced_nothing`;取消分支按 `blocks`/`text_accum`/`think_accum` 三个全空判定(在 `blocks` 被消费前读);backoff 期间取消恒为 true(有语义输出的尝试走 `AfterOutput`,从不重试) |
| `core/src/agent.rs` | 新 `run_turn_with_input(...) -> (TurnOutcome, Option<Message>)`;pre_turn hook 的 `Allow{context}` 改 `stage`(跟着这一轮一起作废);`absorb_round` 显式提交;`settle_sample` 在 `!produced_nothing` 时提交;收尾 `Aborted && has_staged && inbox 空` 则直接返回,不写 terminal、不记 stop hook 的 context |
| `provider/src/lib.rs` | 新 `MockTurn::DeltasThenGate`——流完 delta 再挂住,测"看得见的思考"那一档(现有 `Gate` 是"一个字都没说"那一档) |
| `tui/src/lib.rs`/`events.rs`/`app.rs` | worker 改用新入口;新 `AgentEvent::InputReturned{text, images}`;`App::return_input` 撤回 echo(`drop_input_echo`)并回填 composer |
| `server/src/lib.rs` | 改用新入口;作废时 `projection.discard_turn(turn.id)`,快照不留这一轮的 input |
| `cli/src/headless.rs`、`cli/src/main.rs` | 改用新入口;plain 的 `Aborted` 分支按 `returned` 分成两句文案 |

### TUI 侧的三条判断

- **回填前看 composer 空不空。** esc 能走到 turn 就说明 composer 是空的(有草稿时
  第一下只 arm 清除,plan 160),但从打断到 worker 把输入交回之间用户可能已经开始打字。
  新草稿赢:transcript 和 composer 都不动,而不是把旧文本插进人家正在写的东西里。
- **撤 cell 靠比对,不靠计数。** 从尾部往前:N 个 `[image: …]` 占位 + 一条等于
  `text.trim()` 的 `Cell::User`,对不上就整个不动。留一条过期的 echo 是观感问题,
  删掉别人的 cell 不是。
- **已冻进 scrollback 的不撤。** `cut == 0 && head_frozen.is_some()` 时放弃——
  native scrollback 擦不掉,撤了会剩一段没有 cell 的前缀。

### 已知的小退化(接受)

- 退回的图片附件用 media type 当 label(`[image: image/png]`),因为原文件名不在
  `Message` 里。与 resume 重放的显示方式一致。
- 长文本退回 composer 会走 `paste` 的大粘贴占位(`[Pasted #N: … chars]`),提交时展开
  成全文,内容不丢。
- plain/headless 没有可回填的地方,输入就是没了;plain 的 `Aborted` 文案说明了这点。
- slash 展开成 prompt 的那条路(TUI `lib.rs`、server `lib.rs`)仍走 `record`:它的
  输入不是用户打的那行字,退回无意义,echo cell 也是 `/name` 对不上。

## 五、验证

`make check` 全绿(fmt + clippy `-D warnings` + 全工作区 test,含文件体积棘轮)。新增测试:

| 测试 | 锁住什么 |
|---|---|
| `agent::…::an_interrupt_before_the_first_token_leaves_no_trace` | `messages()` 为空、会话文件只有 `provider_route_initial` 一行、输入原样交回;**且请求确实发出过**(是对话没动,不是线上没发) |
| `…::an_interrupt_after_the_model_speaks_keeps_the_turn` | 一个词就够:user + partial assistant + terminal 三行都在 |
| `…::an_interrupt_after_visible_reasoning_keeps_the_input` | 关键判据:未签名 thinking 记不下任何东西,但输入留下 |
| `…::a_provider_failure_keeps_the_input_it_failed_on` | 只有打断能作废 |
| `…::an_interrupt_with_steering_queued_keeps_the_input` | steer 要答的那条消息不能被抽走 |
| `history::…::a_staged_input_rides_the_request_without_joining_the_conversation` | 请求视图含、`messages()` 不含、估算含、取回后一切复原 |
| `history::…::compaction_does_not_swallow_the_staged_input` | `replace_all` 不提交,暂存落在摘要之后 |
| `app::…::a_returned_input_undoes_its_echo_and_refills_the_composer` 等 3 条 | 撤 echo + 回填;有新草稿时不动;尾部不是 echo 时不动 |

改掉的既有测试:`cli/tests/plain_pty.rs` 的 `running_ctrl_c_patches_history_then_exits`
→ `running_ctrl_c_before_any_output_discards_the_turn`。它本来就是这个场景(请求发出、
30 秒后才回、Ctrl+C),旧断言锁的是旧文案 `[interrupted — history patched; exiting]`。

**没做真机验证**(需要真 key 和代理);mock 轨与 PTY 轨覆盖了全部判定分支。

## ✅ 完成

2026-09-21 排定并完成,一次提交(SHA 以本条所在提交为准)。开工前问过用户一个待定点
(作废范围要不要罩住 provider 报错),答"同意"按窄的来。

实现里比设计更清楚的两件事:

1. **第一版设计想让"所有 persist 型写入口都提交暂存",做下去撞上压缩**——
   `compact.rs` 在 `replace_all` 之前记 usage,一提交就把输入交给了一个不含它的替换集。
   改成 `record_provider_usage` 不提交 + `absorb_round` 显式提交一次,既躲开压缩,又让
   rollout 行序与改动前完全一致(另一条路是把 `compact.rs` 那两行对调,但 plan 81 明确
   定了 `usage→compacted` 的顺序,不该为别的目的动它)。
2. **"round 0 的 drain_inbox 要不要提前提交"这个顾虑是多余的**——`drain_inbox` 用的
   就是 `history.record`,而 `record` 第一行就提交暂存,顺序天然正确。把不变量放进
   `History` 的写入口而不是调用方,省掉的正是这一类"记得在这里也调一次"的检查。
