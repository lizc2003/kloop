# Plan 22 — 中途注入:steering + 子 agent 回灌

> **机制 + TUI steering 已完成(2026-07-13,提交见文末)。** 子 agent 回灌挂账——
> 它离不开异步派发,回源加固见完成记录。开工前读 HANDOFF + plan 17 的"回源调研结论"节。

## ✅ 完成记录(2026-07-13,机制 + TUI steering)

**回源(教训 11/14,三家真读代码,file:line 落地,沉淀在 `refs/README.md` steering 一节)**:
- **claw-code**:无 steering,纯阻塞 REPL(`conversation.rs:325` 同步 `run_turn`,读一行→整轮→再读);Ctrl+C 只杀 hook 子进程,连 turn 都断不了。反面。
- **cc(TS 真源码)**:单一优先级队列(`messageQueueManager.ts:53`,`now>next>later`),干活时提交=入队不打断(`handlePromptSubmit.ts:346`);**绝不插在途请求**,严格在工具结果收齐后、下一次 `callModel` 前 drain(`query.ts:1864`,注释点破"interleave tool_result 与 user 消息会 API 报错");注入带 framing(`messages.ts:5988` "The user sent a new message while you were working…");interrupt(`user-cancel`)vs steer(`interrupt`)靠 AbortController reason 区分。
- **codex(codex-rs)**:`TurnInput` 队列(`input_queue.rs:12`),`Op::UserInput` 遇活跃 turn→`steer_input` 入队不 abort(`session/mod.rs:3903`,Review/Compact 拒 steer);**在 run_turn 循环顶部 drain**(`turn.rs:229`,`can_drain_pending_input` 初 false 让本轮首请求先跑);子回灌 `forward_child_completion_to_parent`(`mod.rs:1881`)投父 mailbox + delivery-phase 闸门(工具后 CurrentTurn / 终答后 NextTurn)+ autowake,**Interrupted 子 `is_final=false` 不回灌**;成功摘要**原样透传**(仅 error 截 900 token——refs/README 旧记"截 900"已订正)。
- **收敛(两家独立)**:① steering=入队绝不 abort turn,硬断是另一条独立路径(cancellation token);② 绝不插在途请求,只在 step 边界、下一次采样前 drain;③ **子 agent 回灌整套依赖异步派发**(codex spawn→wait→mailbox→autowake / cc `later` 优先级 + Sleep-flush + agentId 分域)。

**scope 决定(开工时与用户定,回源加固)**:kloop 现有 task 是**同步 await**(`task_tool` 直接 `handle.await` 拿 final_text 当 tool_result),parent 阻塞等着——回灌没有独立价值;要它有意义得先建异步派发,那是 plan 17 片 6 明说"别提前抽象、等真实需求"的整套子系统。故本会话只做**机制 + 用户 steering**;子 agent 回灌 + 异步派发留独立 plan(建议 plan 26),机制留好接口(inbox 是中性字符串队列,回灌到时 push 自己 framing 的摘要即可)。

**实现**:
- `Config.inbox: Arc<Mutex<Vec<String>>>`(step 边界注入队列,照 `todos` 那套过程态;子 agent 各自 fresh,`task` 在克隆 Config 上重置——running 子 agent 绝不 drain 父的 steering)。
- `core/src/agent.rs`:`drain_inbox` helper + 两个 drain 点——**round 循环顶部**(交付上一轮工具执行期间打的字,在本轮采样前)+ **收尾兜底**(`tool_uses` 空、准备结束前再 drain,late steer 命中则 `continue` 不结束,turn 继续处理而非丢弃)。注入成 user 消息、带 cc 式 framing(`STEERING_PREFIX`),记进 history/rollout(过压缩、resume 重放);因排在该轮 `tool_results` 之后作独立 user 消息,天然不交错(合规两家收敛)。
- TUI:`Command::Steer(String)`,`App::on_key` 的 Enter 在 running 时返 Steer(推 `Cell::User` 显示、不新起 turn、不重置 todo 块);`ui_loop` 持 `cfg.inbox.clone()`,Steer 命令 push 入队;agent 每 round 边界 drain。plain/server 的 enqueue 侧挂账(plain 阻塞读、server `turn/steer`),但**drain 路径三前端都活**(inbox 在 Config 上,run_turn 天然 drain,空队列 no-op)。

**测试**(321 个,+4):core 三本——边界注入(steer 在 round 0 工具执行期打入 → 进 round 1 请求、不进 round 0 在途请求、成 `tool_results` 后的 framed user 消息)、收尾兜底(final 采样期 late steer → 不结束、turn 续跑、最终答案是处理 steer 后的、队列已清)、子 agent 隔离(running 子 agent 的请求永不含父 steer、父在自己下一边界交付);tui 一本(running 时 Enter 返 `Command::Steer` + User cell、不结束/不重置 todo)+ 更新旧断言(running 时 Enter 从"忽略"改"steer")。

**真 key 验收**(anthropic 轨,sonnet-5;throwaway example 脚本化 mid-turn steer,验完即删):任务是"分三次单独 bash 跑 `sleep 3 && echo STEP-ONE/TWO/THREE`",后台任务在 t=4s(turn 运行中)把一条 steer push 进 `cfg.inbox`。history 精确印证机制:STEP-ONE 的 tool_result 之后**紧跟一条带 `STEERING_PREFIX` 的独立 user 消息**(排在 tool_result 后、下一次采样前,未插进在途请求、未与 tool_result 交错)→ 模型"Noted the steering message"跑 `echo STEERED-MIDTURN` → 继续 STEP-THREE → 最终答复明确确认"收到你干活时发的消息并处理了"。turn 未被打断,5 rounds Completed。B 方案(脚本 push inbox)验的是核心 drain + 模型响应闭环;TUI enqueue 侧(`Command::Steer`→push)由单测锁定。

**提交**:27d0430(fmt/clippy/test 全绿,321 个测试)。

### ✅ 追加(2026-07-14,server 入队侧,plan A 会话·提交号 bf555d5)

steering 的 enqueue 侧原为 TUI-only,现补上 **server**:新 RPC `turn/steer
{threadId, input}` push `InboxItem::Steer` 进该 thread 的 `cfg.inbox`(clone 存进
`ThreadHandle`)。**无条件 push、不检查 running、不启动 turn**——运行中 push 由 worker
下一 round 边界 drain 折进当前 turn;空闲 push 由下一次 `turn/start` 顶部 drain 交付
(client-driven server 无 autowake,同 plan 22 平台事实)。测试 +2(server duplex):
运行中 steer 端到端(approval 挂住 turn→push→放行→drain,断言 history 出现带
`STEERING_PREFIX` 的 framed user 消息)、steer 到不存在 thread 干净报错。**plain REPL
(阻塞读)入队侧仍挂账**(平台事实,drain 三前端都活)。

## 目标

一个"**step 边界注入**"机制,两个消费者:
1. **用户 steering**:agent 干活时用户插一句,下一个 round 边界注入进历史(不硬断
   当前工具,Ctrl+C 才是硬断)。
2. **子 agent 回灌**:子 agent 终态时把摘要投递父 agent 的历史(plan 17 片 6 的
   mailbox 回灌)。

两家收敛(教训 14,已在 plan 17 记录):**只在 step 边界注入,绝不插进在途请求**。
cc task-notification 入队、工具循环 drain 转 attachment;codex mailbox delivery phase
闸门 + `forward_child_completion_to_parent`。形态已定,合成一个机制两个客户最省。

## 关键决定(开工时定)

- **注入点**:round 循环边界(`dispatch_tools` 之后、下一次采样之前)。绝不插进在途
  stream。
- **通道**:一个 per-turn 注入队列挂在 agent 循环可达处。用户 steering:前端输入在
  turn 运行时不打断、进队列;agent 每 round 开头 drain。子 agent 回灌:子 agent 终态
  → 投递同一队列(子 agent 是独立 tokio task,需要终态回调/JoinHandle 完成侦测)。
- **注入形态**:成 user 消息进历史(下一轮采样前)。cc 是 attachment 进本 turn
  toolResults;kloop 倾向简化成 user 消息。子 agent 摘要截断(codex 截 ~900 token +
  "换个任务再派"引导)。
- **打断语义**:steering 软注入,不硬断当前 round 的工具(Ctrl+C 保持硬断那条)。
- **前端接线**:TUI/plain 在 turn 运行时收输入 → 入队而非当作新 turn;server 经
  `turn/steer`(挂账)。

## 现状底座

后台 bash 已有队列/monitor/Ctrl+C 打断;task 子 agent 是独立 tokio task(回灌要它
终态回调);history append-only + 修补语义已能容纳中途插入的 user 消息。

## 不做

硬打断在途工具(Ctrl+C 管);多用户并发 steering;插进在途请求(两家都不做)。

## 测试

round 边界注入(在途请求不受影响)、steering 消息进下一轮历史、子 agent 回灌摘要进
父历史且顺序正确、turn 空闲时注入触发新 turn(若做 autowake)。

## 完成标准

fmt/clippy/test 绿;真 key 一次 steering 或一次子 agent 回灌闭环;README、HANDOFF;
plan 17 片 6 标记为本 plan 承接。
