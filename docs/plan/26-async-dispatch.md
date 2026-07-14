# Plan 26 — 异步派发 + 子 agent 回灌(✅ 完成,见文末完成记录)

> **✅ 切片 1+2+3 + 轻量 interrupt 完成**(注册表大泛化与切片 5 落盘挂账,有依据)。以下为原备忘,
> 开工回源结论已回填到文末"完成记录"。开工前读 HANDOFF + plan 22 完成记录 + plan 17 片 6(候选切片 6
> "统一任务注册表")+ `refs/README.md` 的 **steering / 中途注入对比** 一节(plan 22
> 回源已把交付侧收敛点落到 file:line)。**注入侧机制已由 plan 22 建好**(`Config.inbox`
> step 边界注入队列),本 plan 只补**派发侧**:让 task 从"同步 await"变成"fire-and-forget
> + 完成侦测",子终态把摘要 push 进父的 inbox(plan 22 的 drain 侧零改动)。

## 是什么

现在 `task` 是**同步**的:`task_tool` 里 `handle.await` 阻塞等子 agent 跑完,把 final_text
当 tool_result 直接回(cc 并行批形态,plan 17 片 1)。本 plan 让它可**异步**:

1. **spawn 立即返回** 一个 agent id,父 turn 不阻塞、继续自己的 round;
2. 子 agent(本就是独立 tokio task)**终态时把摘要投进父的 `Config.inbox`**(plan 22 机制),
   父在下一个 round 边界 drain 成 user 消息 —— 这就是"子 agent 回灌",plan 22 备忘里
   deferred 的第二个消费者;
3. 可选 **autowake**:父 turn 已空闲时,子完成能起一个新 turn(免用户轮询)。

这是 plan 17 片 6"统一任务注册表"方向的落地:把 `BackgroundShells`(plan 14 后台 bash 的
id/状态/输出/查询/杀/monitor 那套)泛化成一个 `Tasks` 注册表,后台 shell(`bg-N`)与异步
子 agent(`agent-N`)共用。也是 plan 24 code-mode 里 `agent()` 能异步编排的地基。

## 为什么值得(收益 / 为什么现在够格单开 plan)

- plan 22 已把**交付侧**(step 边界注入)建好并真 key 验过,回灌只差**派发侧**——第二个
  真实消费者(异步子 agent)出现了,plan 17 片 6 说的"不要提前抽象、等真实需求驱动"的
  条件到了(教训 16)。
- 长跑子 agent 不再堵父 turn:父可以派几个搜索/构建子 agent 后继续干别的,完成了回灌。
- `wait` 工具 + steering 天然协同:plan 22 的 inbox 已有"新用户输入"信号,可用来**提前打断
  一个 `wait`**(codex 正是这么做的)。

## 回源已知(plan 22 本会话真读,沉淀在 refs/README steering 一节)

两家在**交付侧**已收敛(plan 22 已抄);**派发侧**的这些事实也在本次回源里拿到,但**开工时要
再真读一遍确认**(教训 11:steering 回源聚焦交付/注入,spawn/wait/registry 生命周期只顺带扫过):

- **codex(codex-rs)**:`spawn` 非阻塞返回 canonical task_name → `wait`(订阅 activity watch,
  mailbox/steer 都能提前唤醒,**自己不 drain、只返回**,内容留给下一 step 边界)→ `send_message`。
  子终态 `forward_child_completion_to_parent`(`session/mod.rs:1881`)投父会话级 mailbox +
  `Op::InterAgentCommunication` + **autowake**(`maybe_start_turn_for_pending_work`,仅空闲且
  有 `trigger_turn` mail 时)。**delivery-phase 闸门**(`state/turn.rs:47` `CurrentTurn|NextTurn`):
  工具调用后折进后续请求 / 终答后推迟下一 turn。**Interrupted 子 `is_final=false`→不回灌**。
  摘要截断:completion 上限 1000 token,**仅 error 分支截 900**,成功摘要原样透传。并发默认 3
  (`max_concurrent_threads` 4 − root)。V2 无深度门禁。
- **cc**:后台 agent / task 完成 → 入优先级队列(`later`),`<task-notification>` XML(task-id /
  status / summary / result / output-file),**按 agentId 分域**(每个 agent 只 drain 给自己的),
  turn 末 drain 或 `Sleep` 后 mid-turn drain。cc 现把后台 shell/异步 agent/远程会话收敛成一套
  Task 框架(统一 id/status/output 文件/TaskOutput/TaskStop)。

**收敛的"必然解"**:① 只在 step 边界回灌(plan 22 已做);② 子终态投父队列 + 可选 autowake;
③ Interrupted 不算终态、不回灌;④ 一个统一任务注册表(id 前缀区分种类)。

## 关键决定(开工时定 / 回源后定)

1. **spawn 形态**:`task` 加 `background: true` 参数(省略 = 现有同步并行批,保持不变),还是
   独立的 `spawn_agent` 工具?倾向**参数**(最小面,现有 task 描述/路由/agent_type 复用)。
   spawn 立即返回 `agent-N` id + 一句"已派发,用 wait/查询"引导。
2. **完成侦测**:子 agent 本就是独立 tokio task,持它的 `JoinHandle` 进注册表;完成时(或一个
   monitor task 侦测,对齐后台 bash 的 monitor)取 `TurnOutcome` → 格式化摘要 → push 进**父的
   `Config.inbox`**(plan 22 的 Arc,注意:异步子 agent 的父 inbox 不能像同步 task 那样被"重置为
   fresh"——需要子持有父 inbox 的 clone,和现在 task_tool 重置子 inbox 的逻辑要分清)。
3. **回灌形态**:push 一条 framing 过的摘要(区别于 steering 的 `STEERING_PREFIX`,用
   `[sub-agent agent-N result]` 之类;plan 22 说过 inbox 现在是**中性字符串队列**,各消费者自带
   framing)。**摘要截断**抄 codex(成功透传 / 失败截 ~900 token + "换个任务再派"引导)。
   **Interrupted / Aborted 子不回灌**(抄 codex is_final)。
4. **autowake(最重的架构决定,plan 17 片 6 已点名)**:父 turn 已结束(EndReason 已回前端)时,
   子完成要不要**自动起一个新 turn** 把回灌喂给模型?这动**主循环采样时机**,三前端各要接线
   (TUI 空闲时被唤醒起 turn、server 发通知、plain 阻塞读没法被动唤醒)。倾向**先不做 autowake**:
   父必须用 `wait`(切片 2)或下一次用户 turn 才收回灌,把 autowake 留到有痛感/单独定。若做,
   抄 codex"仅空闲 + 有 trigger mail 才唤醒"的守卫。
5. **`wait` 工具**:阻塞到有 mailbox/steer 活动或超时(codex:min 10s/default 30s/max 1h),
   **可被新用户输入(plan 22 的 inbox steer 信号)提前打断**,自己不 drain 只返回摘要/超时。
   要定:kloop 用什么信号量(一个 `tokio::sync::watch` 或 Notify 挂在注册表上,spawn 完成 /
   inbox push 时 notify)。这是"免轮询"的关键;没有它父只能靠下一轮自己查。
6. **注册表泛化**:`BackgroundShells`(`tools/bash.rs`)→ `Tasks`,id 前缀 `bg-`/`agent-`;
   `bash_output`/`kill_bash` 是泛化(`task_output`/`task_stop`)还是加别名?查询/杀异步子 agent
   要不要?倾向**最小**:先只加 spawn + 回灌 + wait,查询/杀等有需求再补(cc 有 TaskOutput/
   TaskStop,但 kloop 现在没痛点)。**不预抽象**——第二个消费者(异步 agent)出现才泛化,不是
   为了整齐(plan 17 片 6 2026-07-10 拍板)。
7. **并发上限**:同步并行批现在无上限(cc 10、codex 3)。异步派发下 spawn 多了会不会失控?
   倾向设个宽松上限(如 8)+ 超限拒绝或排队,开工定。
8. **历史持久化(plan 17 片 3 挂账)**:异步长跑子 agent 要不要落 `.kloop/sessions/`(信封
   parent 指回父)?审计/断点续查有价值,但增复杂度。倾向**本 plan 可选切片**,不强绑。
9. **深度限**:异步子 agent 仍深度 1、不能再 spawn(现有 `depth>=1` 门保持)。

## 候选切片(开工时和用户定选哪几片、什么顺序)

1. **异步 spawn + 完成侦测 + 回灌**(核心闭环):`task {background:true}` 立即返回 id;注册表持
   JoinHandle/monitor;子终态 push framing 摘要进父 inbox;父下一 round 边界 drain(plan 22 零改)。
   **一片就能真 key 验**:父派一个后台子 agent、继续自己干活、下一轮看到回灌。
2. **`wait` 工具**:阻塞到活动/超时,steer 可打断;免轮询。
3. **autowake**(动主循环采样时机,独立架构决定,单独和用户定):父空闲时子完成起新 turn,三前端接线。
4. **注册表查询/杀 + 泛化 `Tasks`**:`task_output`/`task_stop`,后台 shell 与异步 agent 统一。
5. **异步子 agent 历史持久化**(plan 17 片 3):子会话落盘、parent 链、`--list-sessions` 标从属。

## 不做 / 待定

- 不抄 codex 多 agent 全家桶(role 分层、complexity 分级、CSV fan-out、send_message 双向对话)。
- 不建 cc 那样的全局 Task 框架(远程会话、TaskOutput 弃用引导 Read 那套);只做够两个消费者
  (后台 bash + 异步 agent)共用的最小注册表。
- Interrupted 子不回灌(抄 codex);深度 >1 不做。
- 不预抽象注册表:等异步 agent 这第二个消费者真接进来再泛化 `BackgroundShells`。

## 回源待办(开工必做,教训 11 + 14)

plan 22 回源聚焦**交付/注入侧**,已落 file:line;本 plan 要补**派发/生命周期侧**的真读:
- **codex**:`spawn`/`wait`/`send_message` 工具面(`core/src/tools/handlers/multi_agents_v2/`,
  尤其 `wait.rs` 的 activity watch + deadline)、注册表/agent_graph、autowake 守卫
  (`tasks/mod.rs` `maybe_start_turn_for_pending_work`)、并发上限、canonical task_name vs agent_id。
- **cc**:后台 agent 的 spawn→enqueue notification 生命周期(`LocalAgentTask.tsx` 一族)、
  Task 框架统一 id/status/output、`<task-notification>` 全字段、agentId 分域 drain。
- 找收敛(教训 14):两家在"spawn 非阻塞 + JoinHandle/子 session 终态回调 + 投父队列 + 可选
  autowake + Interrupted 不通知"上是否独立一致(plan 22 初判一致,回源坐实)。

## 测试(方向)

异步 spawn 立即返回不阻塞父(父在子跑完前已进下一 round,时间断言);子终态回灌摘要以 framing
user 消息进父 inbox 并在下一 round 边界 drain(录 MockRequest:回灌进的是父后续请求、顺序正确);
Interrupted 子不回灌;摘要截断契约;`wait` 被 steer 提前打断 / 超时两路;并发上限;注册表 id
前缀区分 bg-/agent-;(若做)autowake 空闲父起新 turn、非空闲不重入。

## 完成标准

按所选切片;fmt/clippy/test 全绿;真 key 至少一次"父异步派子 agent、继续干活、子完成回灌进父
下一轮闭环";README、HANDOFF(标 plan 17 片 6 / plan 22 子 agent 回灌由本 plan 承接)、未选切片挂账。

---

## ✅ 完成记录(切片 1+2+3 + 轻量 interrupt;提交 4b9fad4)

开工时用户定"不要考虑复杂度,要最合理的方案",故不为省事砍——按最合理的完整异步派发面做,
只砍有证据/本质依据的。**做了切片 1(异步 spawn + 完成侦测 + 回灌)+ 2(`wait`)+ 3(autowake,
TUI)+ 轻量 interrupt(`stop_agent`)**;**不做**注册表大泛化(`BackgroundShells→Tasks`)与切片 5
(异步子 agent 落盘)——都是有依据的取舍,见下。

### 回源回填(派发/生命周期侧,steering 那次只覆盖交付侧;本会话真读 codex + cc,file:line)

**收敛(两家独立一致,照抄)**:
- **spawn 非阻塞**:codex `spawn` 立即返 canonical task_name(`spawn.rs:193-294`),不把结果返给模型;
  cc `void runAsyncAgentLifecycle`、立即返 `async_launched`+output-file 指针(`AgentTool.tsx:873,902`)。
- **回灌投父队列**:codex → `Op::InterAgentCommunication` → 父会话级 mailbox VecDeque(`session/mod.rs:1889`
  `forward_child_completion_to_parent`,过 Op 通道在父事件循环串行处理);cc → 全局单例队列
  `enqueuePendingNotification(mode:'task-notification')`、优先级 `later`(`messageQueueManager.ts:142`)。
- **摘要截断**:两家都**成功原样透传、失败才截**。codex 常量坐实 `session_prefix.rs:10-13,33-36`:
  `COMPLETION_MESSAGE_MAX_TOKENS=1000`、`ENVELOPE_RESERVE=100`、error 截 `900`,成功分支不截;cc `<result>`
  内联最终答案全文、`<summary>` 短状态、`<output-file>` 指针三层(`LocalAgentTask.tsx:302-315`)。
- **autowake 守卫**:codex `maybe_start_turn_for_pending_work`(`tasks/mod.rs:474`)——仅"有 trigger_turn 邮件
  且空闲(抢 active_turn 锁失败即 return)"才起合成 turn 喂空 input(turn 内 drain 邮箱);`SubagentAutowake`
  feature 默认开。cc 靠 turn 末 drain / `Sleep`(proactive)后 mid-turn drain `later`(`query.ts:1860-1865`)。

**两个分歧(plan 备忘估错,已按回源纠偏)**:
1. **Interrupted 是否回灌**——plan 备忘写"两家都不回灌(抄 codex is_final)",**回源发现只有 codex 这样**
   (`status.rs:26` is_final 视 Interrupted 非终态 + `session_prefix.rs:41` 返 None 双重过滤);**cc 反而回灌**
   `killed`+`extractPartialResult`(`agentToolUtils.ts:652-680`,注释"must fire unconditionally",只靠 `notified`
   幂等去重)。→ kloop **随 codex:不回灌**(教训 14:参考库里"存在"不等于"收敛";一家的选择别当收敛照抄)。
2. **注册表是否统一**——plan 想"泛化 `BackgroundShells`→`Tasks`",但 **codex 后台/用户 shell 走
   `UserShellCommandTask`、不进 `AgentRegistry`**(两套独立机制);cc `Task.spawn/render` 已在 #22546 删、
   只剩 `kill` 多态,**只统一状态模型不统一 spawn**。→ kloop **新开平行的 `AsyncAgents` 注册表、不强行合并**
   (教训 16/17:不预抽象、别为整齐照搬;第三个消费者出现再议)。

**方向导数**:codex V2 已**离开 detached completion watcher**(`control.rs:460` `maybe_start_completion_watcher`
被 `!= V2` 门控关掉)、转向**终态事件内联 forward**。kloop 无事件总线,取最自然的形态——给子 agent 的
JoinHandle 挂完成逻辑(在 spawned task 尾部),完成时 push 摘要进父 inbox(近 cc 的 lifecycle-driver-then-enqueue)。

### 落地(文件)

- **`core/src/inbox.rs`(新)**:`Config.inbox` 从 `Arc<Mutex<Vec<String>>>` 升级成 `Arc<Inbox>`——带
  `tokio::Notify` 的信号队列 + 类型化 `InboxItem { Steer(String), SubAgentResult{label,summary} }`,`into_message`
  按类型 framing(steering 的 `STEERING_PREFIX` 从 agent.rs 移来 + 新 `SUBAGENT_PREFIX`)。**修正 plan 备忘的
  "中性字符串队列各自 framing"**——实际旧 `drain_inbox` 把所有项硬套 `STEERING_PREFIX`,类型化后才真正各自 framing
  (教训 16 具体落点:plan 对现码的描述也是二手)。`push` 用 `notify_one`(留 permit 防 race),`notify_activity`
  唤醒不入队(中断子唤醒 `wait`),`drain`/`is_empty`/`notified`。
- **`core/src/tools/async_agents.rs`(新)**:`AsyncAgents` 注册表(id→{task,status,own-cancel},并发上限 8、
  Drop 补刀 cancel)+ `AgentStatus` + `wait_tool`(clamp 10s/30s/1h;有 pending 立即返不 drain;无运行且空立即返;
  否则 select notified/timeout/turn-cancel;**信号不搬运**)+ `stop_agent_tool`。
- **`core/src/tools/task.rs`**:`task` 加 `background` 参数。`background:true` 走 `spawn_background`——注册进
  `AsyncAgents`(超并发拒)、用**独立 cancel**(非父 turn cancel,父结束不杀它)detached spawn、立即返"已派发"引导;
  子终态 `classify_background`(Completed/MaxRounds 透传、Error 截 `MAX_REINJECT_ERROR_CHARS=3600`、Aborted→None)
  push 进**父 inbox**(reinject)或 `notify_activity`(中断)。抽出 `build_sub_config`(fresh todos+fresh inbox 共用)。
- **`core/src/agent.rs`**:`drain_inbox(&Inbox, ...)` 按 `InboxItem::into_message` 各自 framing;删除本地
  `STEERING_PREFIX`(移进 inbox.rs)。
- **`core/src/tools/mod.rs`**:depth-0 加 `wait`/`stop_agent` def + 分发;`wait` 非并发安全(阻塞,单飞)、
  `stop_agent` 并发安全(仅信号);`task` 描述补 background 说明。**`codemode.rs`**:program 面排除
  `wait`/`stop_agent`(fire-and-forget 是模型循环概念、program 内同步 `agent()` 无此义)。
- **`core/src/permissions.rs`**:`wait`/`stop_agent` 进只读自判(自动放行,同 `task`/`kill_bash` 理由)。
- **TUI(`crates/tui/src/lib.rs`)**:`WorkerMsg::Wake`(无用户文本、只 drain+采样,inbox 空则 no-op)+
  `autowake_ready(running, &inbox)` 守卫(空闲 且 inbox 非空)+ ui_loop 每批事件后检查触发。**关键简化**:
  autowake 不需要新的 Ui 控制通道——异步子完成的 `agent_end` 本就是个 AgentEvent、会唤醒 ui_loop 的 select,
  于是只要"每批事件后 空闲+inbox 非空 → 派 Wake"就够,race(子在父末次 drain 后完成)天然覆盖(教训 21)。
- Config 各构造点 + `/clear`(清 inbox 改 `drain()`)+ 各 steering 测试(inbox 类型/push)随改。

### 前端矩阵

| | 异步 spawn + 回灌 | wait / stop_agent | autowake |
|---|---|---|---|
| TUI | ✓ | ✓ | ✓(空闲起 Wake turn) |
| plain | ✓(下一 round 边界 / 下一 user turn drain) | ✓ | ✗(阻塞读无事件循环,下一 user turn 交付) |
| server | ✓(子终态 `agent/completed` 已有;reinject 落 thread inbox,下一 `turn/start` drain) | ✓ | ✗(client-driven turn,协议契约不变) |

plain/server 无 autowake 是**平台事实**(无事件循环 / 客户端驱动),同 plan 22 plain 的 steering 入队挂账同因,
文档标注即可,不是砍功能。

### 测试(核心 238,+6;TUI 38,+1)

- `inbox.rs`:steer/subagent 各自 framing、push/drain 往返、`notified` 唤醒、race 下 permit 存活。
- `async_agents.rs`:并发上限、完成腾槽、stop 取消+拒非运行/未知、Drop 补刀;`wait` 无运行立即返 / 有 pending
  立即返且**不 drain**(经 `run_tool` 全分发路径,含权限门只读放行)。
- `task.rs`:**背景 spawn 立即返"已派发"(非结果)且 detached 子完成后 reinject 成 `SubAgentResult` 进父 inbox、
  腾槽**;**`stop_agent` 的子 Aborted 不回灌**(端到端:派长 bash 子→stop→轮询 running_count→断言 inbox 空);
  `classify_background` 四态映射;`truncate_error` 只截长失败。
- `tui/lib.rs`:`autowake_ready` 空闲+pending 才触发、运行中不重入。
- 既有工具计数/名单契约(`all_tool_defs`、`tool_merge_warnings` 52→54)随 +2 内置更新。

### 真 key 验收(anthropic 轨 sonnet-5,plain --yolo)

`task {background:true}` 派子 agent(回"OCEANWAVE42")→ 立即返"agent-1 started" → 父自己跑 `echo PARENT_ECHO_OK`
→ `wait {}` 阻塞 → "agent-1 finished" → 回灌下一轮边界交付 → 终答一行 **"Sub-agent word received: OCEANWAVE42;
my echo output: PARENT_ECHO_OK."**。**即完成标准的"父异步派子 agent、继续干活、子完成回灌进父下一轮闭环"**。
(autowake 的 TUI 空闲自动起 turn 是真键手感项,plain 覆盖不到,建议用户在真 TUI 快速过一次,同 plan 25。)

### 挂账(有依据,非因难)

- **注册表大泛化 `BackgroundShells→Tasks`**:回源坐实两家不强合并,`AsyncAgents` 平行即可(见分歧 2)。
- **切片 5 异步子 agent 落盘**(plan 17 片 3):持久化子系统(rollout 父链、`--list-sessions` 标记、resume 语义),
  另一根设计轴,不做进本 plan 更正确。
- plain/server 的 autowake(平台事实)、cc 式 output-file 指针(kloop 子 agent 不落盘,无指针可给)、
  `list_agents` 独立工具(wait 返回已带 running 计数,够了)。
