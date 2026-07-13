# Plan 26 — 异步派发 + 子 agent 回灌(备忘)

> **备忘,未开工**。开工前读 HANDOFF + plan 22 完成记录 + plan 17 片 6(候选切片 6
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
