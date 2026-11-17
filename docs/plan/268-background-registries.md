# Plan 268 — 后台执行 registry 统一

> 2026-10-28 立。来源：全仓只读审查与随后的架构讨论(HANDOFF「〇⁵」节)。
> 状态：✅ 2026-11-17。依赖的 255、257 已完成。

## 问题

1. **两个 registry 各自实现同一套关闭流程**。`tools/bash/background.rs` 与
   `tools/background_executions.rs`:设 closed 标志 → 协作取消 → 用 activity watch 等 idle →
   硬杀或 abort → 再等 1 秒 → Drop 兜底。background.rs 还把"等 idle"内联写了两遍，而
   background_executions 已有 `wait_idle_until`。子 agent 的后台路径(`subagent.rs`)又有自己的一份。
2. **三份"10ms 轮询 + deadline"循环**:`process_tree/unix.rs`、`process_tree/windows.rs` 的
   `wait_tree_empty` 与 `Debugger::finish`。
3. **后台 supervisor 收尾样板三份**:截断函数 `subagent.rs` `truncate_error`、
   `codemode.rs` `truncate_program_error`(两者逐字相同)、`workflow.rs` `truncate`;
   `emit_workflow` 与 `emit_background_task` 里 `ExecutionStatus → BackgroundTaskStatus` 的映射逐字
   重复;`execution_status_detail` 两份，Completed 的文案已经不同。
4. **Windows 进程创建重复约 80 行**。`spawn_impl` 与 `create_debugged_root`:校验绝对路径与
   stdin、`create_job_set`、创建闸门、`prepare_stdio`、`AttributeList`、STARTUPINFOEX、
   `CreateProcessW`、Assign 与 Resume,只差 `DEBUG_PROCESS` 与失败清理。
5. **`wait_for_activity` 把 Finishing 计成 still running**(观察)。这是 `finish_registration` 的刻意设计
   (避免结果送达前读到零个运行中),但文案对模型有误导。

## 开工先问用户

- 统一到什么程度：(a) 只抽共享的关闭流程与 `wait_until(activity, predicate, deadline)` helper，
  三个 registry 各自保留;(b) 一个泛型 `ExecutionRegistry<K>` + trait(cancel / hard_kill / status)。
  推荐 (a) 起步:(b) 动到 shell/Program/子 agent 三条生命周期，风险与收益不成比例。

**2026-11-17 用户定 (a)。**

## 做法

1. 抽 `wait_until` 与共享关闭流程;两个 registry 与子 agent 后台改用它。
2. 进程树轮询收进 `process_tree/mod.rs`,按 `is_alive` 闭包泛化。
3. 截断改用 plan 248 的 `clip_chars`;`emit_background_task` 增加 `output_path` 参数并承担 workflow
   的事件;`execution_status_detail` 只留一份(Completed 文案开工时选一种)。
4. 抽 `create_suspended_root(spec, extra_flags) -> (process, thread, jobs, stdio)`;Windows 只能
   交叉编译验证。
5. Finishing 状态的文案区分"刚完成、正在交付"。

## 完成记录(2026-11-17)

五条读代码复核后全部成立，但第 1 条的第三份已经不存在：子 agent 的注册→spawn→supervisor→
worktree 收尾在 plan 267 已抽成 `run_supervised_child`，剩下的只有 shell 与 execution 两个
registry。

- **共享模块**：新增 `core/src/tools/background_registry.rs`，装三样两边逐字相同的东西：
  `wait_until(activity, deadline, probe, done)`（订阅在读取之前，一次 terminal 变更不会丢 wakeup，
  返回最后一次读数所以调用方不必读两遍表）、`shutdown(activity, timeout, cancel, force, active)`
  （关闭→协作取消→等 idle→硬杀/abort→再等 1 秒→报残留数），以及 `emit`（`ExecutionStatus`
  映射与 `BackgroundTask` 投影只此一份，带 `output_path` 参数，workflow 的 phase 事件也走它）。
  两个 registry 各自保留表与状态机：shell 仍杀进程树，execution 仍取消并 abort worker。
- **子 agent / Program / Workflow 收尾**：三处「截断」里真正逐字相同的是
  `truncate_error` 与 `truncate_program_error`（同一上限 3600、同一后缀），合并为
  `tools::clip_error` 与一个 `MAX_REINJECT_ERROR_CHARS`；workflow 的 `truncate` 上限与后缀都不同，
  没有合并。`emit_workflow` 删除，三处 supervisor 与 workflow 的 phase 事件统一走
  `background_registry::emit`。
- **进程树轮询**：`process_tree/mod.rs` 的 `wait_until_clear(deadline, still_running)` 收掉三份
  10ms 循环（unix/windows 的 `wait_tree_empty` 与 `Debugger::finish`），统一用
  `sleep_until(min(deadline, now + 10ms))`——原来 unix 是固定 sleep 10ms，超时最多晚 10ms。
- **Windows 创建**：`spawn_impl` 与 `create_debugged_root` 合成 `create_suspended_root(spec,
  mode, fault)`，`CreationMode` 只决定 `DEBUG_PROCESS`、`DebugSetProcessKillOnExit` 与测试用的
  silent breakaway；失败清理按「Job 是否已经持有」分，而不是按创建模式复制两遍
  （`kill_unassigned_root` = TerminateProcess + 等句柄，已 Assign 后走
  `terminate_job_and_wait`）。debug 路径因此多了等待与 Job 级终止，都在失败路径上。
- **第 3 条文案定案**：detail 只说状态说不出来的事——Running/Completed/Failed 一律 `None`
  （workflow 的 `completed`、agent 的 `background agent failed` 消失，状态本身已经写着），
  MaxRounds 仍是 `stopped at round limit`（状态显示为 Completed），Aborted 仍是 `stopped`
  （保留的 worktree 位置拼在它后面）。
- **第 5 条文案**：`wait_for_activity` 的报告把「还在跑」与「已跑完、结果在投递」分开
  （`ActiveWork::read` 用两个 registry 新增的 `active_count`），全都在投递时写
  `No background work is still running; 1 item already finished and its result is being
  delivered`；`bash_output` 在阻塞等待超时且状态是 `Finishing` 时写
  `the command finished; its result is still being delivered after Nms`，不再写
  `still finishing after Nms`（此时命令确已结束，只有投递未完成）。活跃数与投递数在**同一次
  持锁**里读出（`ActiveCount`，两个 registry 各实现一份），`total - delivering` 因此不会下溢：
  子 agent 共享父会话的 registry，分两次加锁读取时，中途变 `Finishing` 的条目会让投递数大于
  先读到的活跃数，debug 下直接 panic。

提交分两次：先纯结构、行为零变更，再单独提交文案变更（第 3 条的 detail 与第 5 条的两处措辞），
符合验收要求。

## 验收

- 行为零变更（第 3 步文案与第 5 步除外，见完成记录）。
- 验证：kloop-core lib 全部 1453 项通过、1 ignored（新增 wait 报告措辞用例；`clip_error`
  用例从 subagent 挪到 tools）；kloop-tui lib 353 项、kloop-server lib 27 项通过；kloop-process-spawn 17 项
  通过；`cargo check --workspace`；core 与 process-spawn 的 clippy（all targets，`-D warnings`）；
  fmt；`cargo xwin clippy --target x86_64-pc-windows-msvc -p kloop-process-spawn --all-targets`
  通过。**Windows 部分未在 Windows 执行**，只有交叉编译。
- 未跑：全量 `make check`、parity 语料（本机无可跑语料时按约定跳过）、真实 API。
