# Plan 257 — 子 agent、Program、Workflow 与调度器

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：✅ 完成(2026-11-13)。行号是 2026-10-28 的读数;开工时 9 条全部复核成立。
> 第 1、4、9 条开工时问过用户：1 按 (a)(删掉转发与声明)，4 要(structured agent 与
> fork skill 默认 200 轮、显式只能收紧)，9 对齐 Vixie(按字面星号)。
> 中断时的 worktree 泄漏在 plan 249;两个 `call_agent` bridge 的合并在 plan 267。

## 完成记录(2026-11-13)

- 1 Program 的 `AgentOptions` 去掉 `max_rounds`，`call_agent` 也不再转发；`run_program`
  的 TS 声明只剩 `agent_type`/`model`。workflow 不带 `schema` 的 `agent()` 收到
  `max_rounds` 直接报 "only accepted together with a schema"，不再退成
  `run_agent: invalid input: unknown field max_rounds`。
- 2 `run_js` 用 `tokio::select!`(biased) 同时等 `ctx.async_with` 与取消令牌；停在
  `await new Promise(() => {})` 上的 program 现在能被 stop 结束，后台 program 结束后
  `running_count` 归零(新增 core 回归)。旧实现下该测试 2 秒超时(实测)。
- 3 两个 bridge 的 `call_agent` 入口先查取消，槽位 `select!` 加 `biased;` 且取消分支在前，
  拿到许可后再查一次；`stop_program` 之后 `agent()` 必定被拒(直接对 `CoreBridge` 断言)。
- 4 structured agent 与 fork skill 都默认 `SUBAGENT_MAX_ROUNDS`；显式 `max_rounds` 经
  `child_round_cap` 只能往下收紧。
- 5 journal 改 append-only JSONL：每条一行 + `fdatasync`，受 run-store 16MB 上限约束；
  第一次写失败发一条 Note 并停止宣称可重放，`is_active()` 只认真正落盘的条目。
  "按拓扑顺序重写"的测试改成 append 语义(已有字节是前缀、后写覆盖前写)。
- 6 改迭代遍历并设深度上限(脚本 1024 层、meta 字面量 64 层)；`Math.random` 按
  object/property 字段比较，不再为每个节点拼一份文本。
- 7 误判成立：快速宿主调用之间 tick 间距始终小于 50ms，累计超过 burst 即被杀；改为宿主
  调用返回时置标记并重置 burst 起点。测试在旧判定下先红(实测 `exceeded CPU time limit`)。
- 8 `SystemClock::sleep_until` 改成分步 sleep(60s 上限)，每次醒来重读墙钟。
- 9 dom/dow 的"无限制"改看字段首字符是否 `*`：`0 0 1-31 * 1` 从此每天触发，`*/2` 仍在
  AND 侧。DESIGN 已写明。
- 验证：`cargo test -p kloop-codemode` 28 项、`cargo test -p kloop-core --lib` 1421 项通过
  (1 ignored，真实 seatbelt 用例在 kloop 沙箱内自动跳过)；`cargo clippy -p kloop-core
  -p kloop-codemode --all-targets -- -D warnings` 通过；未跑全量 `make check`。
- 复审修正（同日）：
  - 追加可能留下半行；恢复时先按最后一个换行物理截断，再逐行解码解析。否则下一条成功
    记录会拼进坏行，坏尾截在 UTF-8 中间时整文件解码失败、此前的记录全部变 Miss。
  - 原地追加会跟随硬链接写到外部文件(`NOFOLLOW` 挡不住硬链接)：写前要求普通文件且
    nlink == 1。非 Unix 没有稳定的链接计数 API(Windows 只有 nightly)，保持原样的
    reparse-point 缺口。
  - 打开补 `O_NONBLOCK`：没有读端的 FIFO 会让 `openat(WRONLY)` 永久阻塞，且拦截不了。
  - 首次创建 journal 后同步一次目录，`sync_data` 只覆盖字节、不保证目录项。
  - 动态 `import(...)` 的 callee 节点类型是 `import` 而不是 `identifier`，改成按节点类型
    拒绝(回归在旧检查下先红)。
- 复审第二轮（同日）：
  - 非 Unix 的链接计数不再恒为 1：改用 `tools::fs` 已有、按打开的句柄查询的
    `has_multiple_hard_links`（Windows 用 `FileStandardInfo.NumberOfLinks`，非 nightly），
    否则 Windows 上截尾会把外部硬链接文件截成 0 字节。
  - 截尾失败不再被吞掉：journal 记成 Stopped，后续 `record` 拒绝追加并在第一次调用时
    返回警告，不会把记录拼进坏行却算作已落盘。追加也改成持有同一把锁，两个并发的
    `agent()` 完成不会把两条记录写进同一行。
  - 首次追加创建文件后失败（超限或写入错误）会删掉该文件，下一次追加重新走创建路径并
    同步新目录项，不再出现"文件留下、此后每次 created 都是 false、目录项从未同步"。
- 复审验证：`cargo test -p kloop-codemode` 28 项、`cargo test -p kloop-core --lib` 1427 项
  通过(1 ignored)；`clippy --all-targets -D warnings` 通过；Windows 未编译。

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
