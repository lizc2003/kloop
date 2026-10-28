# Plan 257 — 子 agent、Program、Workflow 与调度器

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：待实现。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
> 先读代码确认，不成立就在本文件划掉并写明原因。中断时的 worktree 泄漏在 plan 249;
> 两个 `call_agent` bridge 的合并在 plan 267。

## 问题

1. **Program/Workflow 的 `agent()` 传 `max_rounds` 必定失败**(已复核)。
   `core/src/tools/codemode.rs:702` `for k in ["agent_type","max_rounds","model"]`、
   `workflow.rs:486` `copy_option(.., "max_rounds", ..)` 都把它转发给 `run_agent_admitted`,而
   `RunAgentInput`(`subagent.rs:58-72`)是 `deny_unknown_fields`,plan 108 已去掉这个字段 →
   `run_agent: invalid input: unknown field max_rounds`。`codemode.rs:789` 的
   `type AgentOptions = { …; max_rounds?: number; … }` 还在对模型宣传它;`codemode/tests.rs:1192`
   断言了这份过期声明。
2. **后台 Program 卡在永不 settle 的 Promise 上时 stop 无效**(待复核)。
   `codemode/src/lib.rs:409-456`:取消只靠 interrupt handler(`:471-487`),只在 JS 实际执行时被调;
   `await new Promise(() => {})` 时没有东西唤醒它，execution 停在 CancelRequested,running_count
   不归零，只有 session shutdown 的 abort 能结束。前台不受影响。
3. **stop 之后 `agent()` 仍有约一半概率起新的子 agent**(待复核)。`workflow.rs:477-484`、
   `codemode.rs:693-700`:`select!` 没有 `biased;`,入口也不先查 `cancel.is_cancelled()`。
4. **structured agent 与 fork skill 不受轮次上限**(疑似是否有意)。`subagent.rs:238-247`、`271`、
   `688`:`build_sub_config(.., max_rounds=None, ..)`、`clone_for_subagent(.., None, ..)`;同一个
   workflow 里不带 schema 的 agent 却被限 200 轮。DESIGN.md 约 3060 行
   "Sampling rounds are unbounded by default" 已与 plan 108 冲突。
5. **journal 每次整文件重写并 fsync,超 16MB 静默停止持久化**(待复核)。
   `tools/codemode/journal.rs:150-200`:`record` → `persist` 在持 std Mutex 时排序、序列化全部条目，
   再 `write_atomic`(两次 fsync),O(n²);`run_store.rs:15` 的 16MB 上限超出后 `let _ =` 吞掉错误，
   `is_active()` 仍为 true,resume 时上次成功写入之后的调用全部重跑。
6. **workflow 脚本解析递归无深度上限**(疑似)。`codemode/src/lib.rs:320-346`、`215-259`:
   `reject_nondeterminism` 按树深递归，约 5 万层嵌套可在 2MB worker 栈上溢出;每个
   member_expression 还 `split_whitespace().collect::<String>()` 一次，长链 O(n²)。
7. **CPU burst 检测可能误杀 await 密集的程序**(疑似)。用 50ms 内 tick 间隔判断是否在 await;
   每轮 JS 运算较重、工具调用很快时相邻 tick 都落在 50ms 内，跑满 5 秒被判 "exceeded CPU time
   limit"。没有测试覆盖。
8. **墙钟 deadline 换算成单调时钟 sleep,系统休眠后 session-only 任务延迟**(待复核)。
   `core/src/scheduler.rs:53-56`、`646-654`:macOS 上 Instant 在休眠期间不走;08:00 合盖、10:00
   打开，09:00 的任务约 11:00 才触发。有 durable 任务的会话被 15s 的 STORE_POLL 兜住。
9. **cron 的 DoM `1-31` 被当作 `*`**(观察)。判断依据是 `len()==31`,Vixie 看字面是否有星号，
   于是 `0 0 1-31 * 1` 只在周一触发。

## 开工先问用户

- 第 1 条：(a) 不再转发、从 `AgentOptions` 删掉;(b) 显式报错说明轮次上限固定。推荐 (a)。
- 第 4 条：structured agent 与 fork skill 默认统一用 `SUBAGENT_MAX_ROUNDS`,显式值只能往下收紧——
  要不要?推荐要。
- 第 9 条要不要对齐 Vixie(按字面星号)?推荐对齐，并在 DESIGN 写明。

## 做法

1. 按问答处理 `max_rounds`;改 `codemode/tests.rs` 那条断言。
2. `run_js` 用 `tokio::select!` 同时等 `ctx.async_with(...)` 与 `cancel.cancelled()`,取消时返回
   "program interrupted"。
3. 入口先查 cancel,select 加 `biased;` 并把 cancel 分支放前面。
4. 按问答统一轮次上限，改写 DESIGN 约 3060 行。
5. journal 改 append-only JSONL(`from_storage` 本就按行 collect 进 HashMap,后写覆盖前写),
   写失败至少发一条 Note 并让 `is_active()` 如实反映。
6. 改用 TreeCursor 迭代遍历并设深度上限;Math.random 判断改为比较 object/property 两个字段文本。
7. 补一条"JS 重计算 + 快速宿主调用"的测试;误判成立就改为在宿主调用返回时重置 burst 起点。
8. 单次 sleep 设上限(如 60s),每次醒来按墙钟重算。
9. 按问答处理 cron。

## 验收

- 单测(旧代码先红):Program 与 Workflow 里 `agent()` 不再因选项失败(或按 (b) 得到明确错误);
  永不 settle 的 Promise 被 stop 后 running_count 归零;stop 后的 `agent()` 必定被拒;
  structured agent 默认 200 轮;journal 1000 条记录线性写入且超限有 Note;深嵌套脚本被拒而
  不崩溃;await 密集程序不被误杀;scheduler 用可控时钟模拟"单调时钟落后墙钟"后按时触发;
  `1-31` 的 DoM。
- DESIGN 对应段落先读再改;外层执行沙箱之外 `make check`;HANDOFF 记进展与教训。
