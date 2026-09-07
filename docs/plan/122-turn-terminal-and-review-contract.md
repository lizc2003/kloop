# Plan 122 — turn 为什么结束要落盘；审查要检验自己的结论

> 来源：2026-09-07 三方对照。同一个 prompt「审查：09b9c94e^..09f21d9e」分别交给
> claude、codex、kloop 跑同一个 gateway 仓库，读三份 rollout 做的对比。
> 三家都只报了同一个 P2（图片链路零金额时响应缺 `usage.cost`），定级一致。
> 差距不在"有没有找到"，在找到之后做了什么。

## 一、效率那条线可以先放下

| | 采样轮 | 工具调用 | 未缓存 input | 命中率 |
|---|---|---|---|---|
| kloop（首轮审查） | 17 | 82 | 538k | 73% |
| codex | 37 | 36 | 2.26M | 64% |
| claude | 34 | 33 | 0.2M | 96%（Anthropic 直连，不可比） |

codex 与 kloop 这次是**同一个 provider（`gw_router`）、同一个模型
（`gpt-5.6-sol`）、同一个 API family（Responses）、同一天的同一个任务**，对照
干净（codex 侧 `model_reasoning_effort = "xhigh"`，kloop 侧 rollout 不记 effort，
这是唯一未对齐的变量）。**kloop 轮数只有 codex 的一半，总 prompt 是它的三分之一，
命中率反超 9 个点**——Plan 120 记的「codex 70% / kloop inline 版 51%」那个差距
已经追平。这次不动缓存。

## 二、kloop 的两处实质差距（已在 gateway 源码上核实）

**1. 修复建议是错的，而推翻它的证据就在 kloop 自己的报告里。**

kloop 建议「在 credits 存在时也显式写入 `usage["cost"] = cost`」。但
`upstream/<module>/image.go`（09f21d9e）里 `resp.Cost` 只在 `a.catalog != nil` 时
设，`resp.Credits` 却在 catalog 判断之外无条件设；catalog 为 nil 是真实部署形态
（`upstream/<pkg>/<module>.go:46` 注释写明 "catalog may be nil"）。照这条建议
改，会把「没有定价」渲染成「结算为 0」——比原缺陷更糟。

刺眼的地方：kloop 的"已排除的候选"里**自己写了**「无 catalog 视频任务的
`cost: 0`」。它读到了这个事实，却没有拿它去检验自己的修复建议。

**2. 触发路径只找了一条，漏掉的那条更常见。**

kloop 只给了 `discount=0`（要人为配一个零折扣价格行）。而 `formatCost` 在
`v <= 0` 时返回 `"0"`、`nonNegativeNumber` 接受 0，**<module> 直接报 `credits: 0`
就能触发**，不需要任何异常配置——`docs/api/images.md:84` 刚好承诺了「明确返回 0
时仍保留」，缺 cost 在这条路径上最打脸。

两条是同一个根因：**报告写完了，但没有回头拿手上已有的事实交叉检验结论**。内置
`code-review` skill 对"缺陷"要求了失败场景，对"修复建议"和"同一判断点还接受哪些
输入"没有任何要求。

## 三、要做的三条

### 1. turn 的终止状态在每个入口都落盘

会话第 9 轮之后 turn 停了：工具结果正常返回（`go test` 全绿），没有再采样，用户
手打「继续」才恢复，11 分钟空转 + 126k token 重发。**为什么停，rollout 里查不到。**

`record_turn_terminal` 只在 `crates/server/src/lib.rs:1756` 调用；TUI 走
`crates/tui/src/lib.rs:367` 直接调 `run_turn`，把 `outcome.reason` 只发给 UI
事件，从不落盘；CLI 五个调用点同样。本机全部 TUI 会话的 rollout 里没有一条
`turn_terminal`。

不是孤例：`20260903-091113` 那次是压缩之后 turn 结束，用户连打两次「继续」，
41 分钟；本机 3 个会话出现过这个形状。而 README:284 早就写着这条链"also carries
display `turn_terminal` records"——**实现与文档不符，这是修 bug 不是加功能。**

改法：记录下沉到 `run_turn_with_options`（core），三个出口都覆盖——循环的
"one exit"，以及循环前的两处早退（provider route 初始化失败、pre_turn hook
Block）。这两处正是"turn 突然结束"最难查的场景。同时删掉 server 的显式调用，
否则 server 会写两条。

**已知的行为变化（接受）**：server 上纯 slash command 路径不再写 turn_terminal。
它本来就不是模型 turn（不记 user message、不采样），server 那处是把命令分支和
turn 分支一起兜了。

子 agent 不受影响：`subagent.rs:926/1232` 用 `History::new`，没有 rollout，
`persist` 直接 no-op。

### 2. `code-review` skill：结论要像缺陷一样被检验

补两条，都进 "Verifying" 一节：

- **修复建议要验证。** 建议一个改法之前，在代码里确认它在所有已知分支下成立——
  尤其是报告自己在"已排除"里写下的那些分支。一个会引入新缺陷的建议比不给建议差。
- **一条触发路径不等于全部。** 找到一个能触发缺陷的输入之后，回头看同一个判断点
  还接受哪些输入：边界值（0、空、缺省）常常比要人为构造的配置更常见，也更可能
  已经被文档承诺过。

### 3. task 状态更新不单独占一轮

`#41`、`#53` 两轮是纯 `task_update` 轮，一次完整采样（含 3233 / 731 output
token）只为改任务状态，不带任何实质工作。`BASE_SYSTEM`（`context.rs:120`）现在
写的是「track the work with the task tools and keep their state current」——
"keep current" 鼓励随时更新，没说和实质工作同轮发。补一句。

## 四、非目标

- 不动缓存（第一节：这次 kloop 反超 codex；Plan 120 已定案超支 98% 在网关侧）。
- 不改 `turn_terminal` 的 schema、public wire 或 protocol 2.0 projection。
- 不给 skill 加"必须给修复建议"的要求——不给建议一直是合法的，给了就要验。

## 验证

- `cargo fmt` + `clippy -D warnings` + `cargo test --workspace` 全绿。
- 新增回归：TUI/CLI 入口跑完一个 turn 后 rollout 末尾有 turn_terminal；
  循环前早退（hook Block）同样落一条；server 不再写重复的两条。
- README:284 那段与实现对齐后复读一遍。

## ✅ 已完成（2026-09-07；提交 SHA 以本条所在提交为准）

三条都做了。

**1. turn 终止状态下沉到 core。** `EndReason::terminal()` 新增在
`crates/core/src/agent.rs`，`run_turn_with_options` 的三个出口各记一条：循环那个
"one exit"，以及循环前的两处早退（`ensure_initial_provider_route` 失败、
pre_turn/subagent_start hook Block）。`crates/server/src/lib.rs` 的调用点和它本地
的 `turn_terminal()` 一并删除——契约写在 `run_turn_with_options` 的 doc 上：前端
不许自己记，否则同一个 turn 会写两条。

落点选在 post_turn/subagent_stop 注入的 `stop_context` **之后**：terminal 因此是
rollout 里真正的分隔符，该 turn 产出的每条消息都在它前面。

CLI 与 TUI 共用 `crates/cli/src/args.rs:449` 的 `open_history` 挂 rollout，所以
两个前端都真的会写；子 agent 用 `History::new`（无 rollout），`persist` 直接
no-op，不会把子 agent 的 terminal 混进父会话。

**2. `code-review` skill 的 Verifying 一节补了两条**（原文见 SKILL.md）：
「A fix you suggest gets the same scrutiny as the defect」与「One trigger is not
the trigger set」。

**3. `BASE_SYSTEM`**（`crates/core/src/context.rs:120`）的 task 那句补了
「send those updates in the same round as the work they describe, never as a
round of their own」。

### 行为变化（接受）

server 上纯 slash command 路径不再写 turn_terminal。它本来就不采样、不记 user
message，server 那处是把命令分支和 turn 分支一起兜了；
`slash_commands_surface_as_system_notifications` 加断言锁死这个新语义。

### 测试

- 新增 `every_exit_records_why_the_turn_stopped`：provider `Refused` 的 turn 落
  一条 error terminal 且 `typedError.kind` 保留 typed 原因（不只是渲染文本）；
  hook Block 的早退同样落一条并逐字断言 hook 理由；两处各断言
  "exactly one terminal per turn"。
- `validated_terminal_usage_is_recorded_before_assistant_message` 的行序期望补
  `turn_terminal`——它在所有消息之后，不会挤进 Plan 81 钉住的
  usage→assistant 之间。
- server 侧 `slash_commands_surface_as_system_notifications` 断言 terminals 为空。
- README 那段同步成「每个 turn 恰好写一条、来自 agent loop 自己的出口而非前端、
  命令行不写」。
