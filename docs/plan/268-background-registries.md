# Plan 268 — 后台执行 registry 统一

> 2026-10-28 立。来源：全仓只读审查与随后的架构讨论(HANDOFF「〇⁵」节)。
> 状态：待实现。**先做 255 与 257**(它们修了 `stop_bash` 与 Program stop 的具体语义问题，
> 这里统一接口)。行号是 2026-10-28 的读数，开工先自己量。

## 问题

1. **两个 registry 各自实现同一套关闭流程**。`tools/bash/background.rs:494-573` 与
   `tools/background_executions.rs:527-578`:设 closed 标志 → 协作取消 → 用 activity watch 等 idle →
   硬杀或 abort → 再等 1 秒 → Drop 兜底。background.rs 还把"等 idle"内联写了两遍，而
   background_executions 已有 `wait_idle_until`。子 agent 的后台路径(`subagent.rs`)又有自己的一份。
2. **三份"10ms 轮询 + deadline"循环**:`process_tree/unix.rs:168-181`、`process_tree/windows.rs:1426-1440`、
   `windows.rs:735-751`。
3. **后台 supervisor 收尾样板三份**:截断函数 `subagent.rs:1065` `truncate_error`、
   `codemode.rs:543` `truncate_program_error`(两者逐字相同)、`workflow.rs:373` `truncate`;
   `emit_workflow` 与 `emit_background_task` 里 `ExecutionStatus → BackgroundTaskStatus` 的映射逐字
   重复;`execution_status_detail` 两份，Completed 的文案已经不同。
4. **Windows 进程创建重复约 80 行**。`process_tree/windows.rs:375-519` `spawn_impl` 与 `594-732`
   `create_debugged_root`:校验绝对路径与 stdin、`create_job_set`、创建闸门、`prepare_stdio`、
   `AttributeList`、STARTUPINFOEX、`CreateProcessW`、Assign 与 Resume,只差 `DEBUG_PROCESS` 与失败清理。
5. **`wait_for_activity` 把 Finishing 计成 still running**(观察)。这是 `finish_registration` 的刻意设计
   (避免结果送达前读到零个运行中),但文案对模型有误导。

## 开工先问用户

- 统一到什么程度：(a) 只抽共享的关闭流程与 `wait_until(activity, predicate, deadline)` helper,
  三个 registry 各自保留;(b) 一个泛型 `ExecutionRegistry<K>` + trait(cancel / hard_kill / status),
  三类后台执行都放进去。推荐 (a) 起步:(b) 动到 shell/Program/子 agent 三条生命周期，风险与收益
  不成比例。

## 做法

1. 抽 `wait_until` 与共享关闭流程;两个 registry 与子 agent 后台改用它。
2. 进程树轮询收进 `process_tree/mod.rs`,按 `is_alive` 闭包泛化。
3. 截断改用 plan 248 的 `clip_chars`;`emit_background_task` 增加 `output_path` 参数并承担 workflow
   的事件;`execution_status_detail` 只留一份(Completed 文案开工时选一种)。
4. 抽 `create_suspended_root(spec, extra_flags) -> (process, thread, jobs, stdio)`;Windows 只能
   交叉编译验证(教训 183、201:本机 `cargo build` 对 `#[cfg(windows)]` 文件是零信息)。
5. Finishing 状态的文案区分"刚完成、正在交付"。

## 验收

- 行为零变更(第 3 步文案与第 5 步除外，单独提交并说明):现有后台 bash、Program、Workflow、
  子 agent 的测试不变;Windows 部分交叉编译 check/clippy 通过，完成记录照实写"未在 Windows 执行"。
- 外层执行沙箱之外 `make check`;HANDOFF 记进展。
