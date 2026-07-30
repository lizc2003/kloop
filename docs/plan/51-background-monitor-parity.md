# Plan 51 — 后台任务与 Monitor 对齐

> 状态：✅ 已完成（2026-07-30）
>
> 母计划：Plan 48
>
> 依赖：Plan 50
>
> 固定目标：Claude Code 2.1.220；版本身份以 Plan 48 manifest 为准。

## 背景

Plan 48 的 `bash-background` fixture 捕获了 CC 的 task start/update/completion notification，但这不等于 Monitor 已被注册或完整后台状态机已被证明。`monitor@clean-cli` 当时八个维度全部为 `unknown`。

kloop 已有后台 shell 与后台 agent/program 两套生命周期。它们的交付介质不同：shell 正文持续写输出文件，agent/program 产出终态结果回灌 inbox。本计划不为内部整齐强行合并两套 registry，只统一模型和前端可见的外部生命周期。

## 证据裁决

### 显式后台 Bash

精确 2.1.220 的 `bash-background` 与新增 `bash-background-failure` raw/normalized fixture 固定：

```text
task_started
→ 初始 Bash tool result（task id + output file）
→ task_updated(status = completed | failed)
→ task_notification
→ 下一次 sampling origin.kind = task-notification
```

失败路径同时固定 exit code、summary 与 output-file pointer。fixture 未证明的自动后台化阈值、stall 时钟不作推断。

### Monitor

exact bundle 静态链已裁决：

- Monitor 是 model-visible tool，不是后台 Bash 的内部 adapter。
- 注册要求 Bash available，并受 `tengu_amber_sentinel` gate 控制；该 gate 默认 `false`。
- schema 要求 `command` 与 `ws` 恰好二选一，并收 `description`、`timeout_ms`、`persistent`。
- `timeout_ms` 最小 1000ms、默认 300000ms、最大 3600000ms；`persistent` 任务可活到 session 结束。
- command 分支复用 Bash permission/sandbox；WebSocket 分支走 URL 合规、SSRF、userinfo/ASCII/protocol 校验与审批。
- command 的每行 stdout、WebSocket 的每个 text frame 都会产生 live notification；终态由 TaskStop、timeout 或 session teardown 收尾。
- 返回面含 `taskId`、`timeoutMs`、`persistent`，并允许并发调用。

clean CLI 没有暴露 Monitor。本地 harness 无法权威开启 server-controlled `tengu_amber_sentinel` profile，因此 `monitor@clean-cli` 八个维度继续标 `unknown`，不是 `missing`；不伪造一个不可运行的 true-profile fixture。

## 产品决定

1. **不新增同名 Monitor。** one-shot 后台终态通知不能冒充逐 stdout 行/WebSocket frame 的 Monitor。
2. **保留显式后台。** 自动后台化与 stall detection 没有可执行 fixture，前台 Bash 继续维持 Plan 50 的 no-survivor 语义。
3. **两套 registry 不合并。** `BackgroundShells` 保留文件型任务，`BackgroundTasks` 保留 agent/program 回灌型任务；两者只共享外部 `BackgroundTask` DTO/event。
4. **统一终态状态。** 前端可见状态为 `running/completed/failed/cancelled`；内部可有 `Stopping`、`CancelRequested`、`Finishing` 等竞争裁决状态。
5. **shell 只回灌指针。** shell 终态向模型交付 status、summary、output path，不把命令输出正文再次塞进上下文。

## 实现

### 外部生命周期

`kloop-core` 新增 session-scoped：

- `BackgroundTaskKind::{Shell, Agent, Program}`
- `BackgroundTaskStatus::{Running, Completed, Failed, Cancelled}`
- `Event::BackgroundTaskUpdated`

该 event 不属于启动它的 turn。server/headless 投影为 `thread/backgroundTask/updated`，wire 不带 `turnId`；TUI 投影为 note，不再把晚到的 detached completion 伪装成已结束 turn 的 sub-agent item。

### step-boundary 交付

`InboxItem` 新增 `ShellResult`，只携带 id、status、summary 与 output path。inbox、后台 shell registry、后台 task registry 的活动信号均改为 `tokio::sync::watch` generation，避免旧 `Notify` permit 和首次 poll 前未注册 waiter 导致的丢失唤醒。

- TUI：后台 event 唤醒 UI loop；idle 且 inbox 非空时自动起无用户文本的交付 turn。
- plain/server：下一次用户 turn / `turn/start` 在 sampling step 边界 drain。
- session shutdown：不 enqueue 已不可能再消费的 shell terminal item。

### 原子终态裁决

`BackgroundTasks` 使用 `Running → CancelRequested/Finishing → Terminal` 状态机：

- stop 与自然完成在 registry lock 内竞争。
- 只有赢得 `finish` 的路径发布 terminal event/inbox。
- worker/supervisor 分离，panic、cooperative cancel 与 deadline 后 abort 都仍由 supervisor 裁决一次终态。
- session closing 后拒绝新 agent/program。

`BackgroundShells` 使用 `Running → Stopping → Exited/Killed/Failed`：

- spawn 与 registry insert 在同一临界区，shutdown 看不到未登记 PID 的半成品。
- 用户 kill、watchdog、自然 exit 只允许一次 `Running/Stopping → Finishing → terminal`；event/inbox 发布完成前 `Finishing` 仍计 active，shutdown 不会提前越过交付。
- 取消和 shutdown 杀整个 process group 并 reap direct child。
- session closing 后拒绝新 shell。

### shutdown 顺序

三前端统一先 `Config::shutdown_background_work()`：

1. registry 标 closed；
2. cooperative cancel；
3. deadline 后 agent/program abort、shell SIGKILL process group；
4. 等 supervisor/monitor 完成终态裁决；
5. 最后 teardown active worktree。

server thread channel 关闭也遵守同一顺序，避免 sender/worker 已退出而 detached task 仍悬挂。

## Fixture 与 parity 产物

- corpus：82 → **83 captures**。
- static evidence：109 → **116 records**。
- 新 fixture：
  - `fixtures/raw/bash-background-failure.json`
  - `fixtures/normalized/bash-background-failure.json`
- 新 Monitor 静态证据覆盖 gate、schema/concurrency、command executor、WebSocket executor、permission/result。
- 新 kloop 静态证据覆盖 lifecycle code 与 tests。
- matrix：56 rows / 448 cells；状态保持 70 compatible / 90 intentional-diff / 20 missing / 236 unknown / 22 n/a / 10 same。
- `monitor@clean-cli` 保持 unknown；background lifecycle 只提升有 executable evidence 的单元，不把 Monitor contract 外推给 kloop。

## 回归覆盖

- shell：start + success/failure/cancel exactly-once event；terminal inbox pointer；unknown/repeated kill；session shutdown 杀 stubborn child/grandchild；teardown 不留垃圾 inbox；closed registry 拒绝 spawn。
- agent/program：success/stop 精确 `Running → Completed/Cancelled`；stop/completion 竞争；panic/abort supervisor 收尾；session shutdown；closed registry 拒绝注册；dirty worktree 位置不丢。
- inbox：四种 item framing；watch generation；stale permit 不误唤醒。
- wire/TUI：background event 无 turn owner；running/terminal note 投影。
- 既有 foreground Bash、sandbox、permission、background query/kill、agent/program backfeed 行为保持回归。

## 明确保留的兼容边界

- 不实现自动后台化或 stall detection。
- 不实现 model-visible Monitor 的 command/WebSocket 逐事件 watch。
- 不实现交互 stdin、PTY 或 HeadTailBuffer/LRU。
- 不把两套内部 registry 合并。
- 不把 output-file 正文重复回灌模型。

## 验证

```bash
python3 -B refs/claude-code-2.1.220/verify.py
cd kloop
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

## 完成记录 ✅（2026-07-30）

- exact bundle 已固定 Monitor 的默认关闭 gate、注册条件、schema、command/WebSocket executor、权限分流、live notification 与 persistent/session cleanup；因 true feature profile 不可权威运行，matrix 保守保留 unknown。
- exact `bash-background` 成功路径与新增 failure fixture 均证明 terminal notification 后会以 `origin.kind=task-notification` 重新 sampling。
- kloop 已为 shell/agent/program 增加无 turn owner 的统一 lifecycle event、shell terminal pointer backfeed、TUI idle autowake 与三前端显式 shutdown。
- 两套 registry 各自实现一次性终态裁决和 closed-session gate；agent/program deadline abort 与 shell process-group kill 均有真实 worker/process-tree 回归。
- README、HANDOFF、capability report、refs 导读、raw/normalized fixture、manifest、static evidence 与 matrix 已同步。
- 自动后台化、stall 与逐事件 Monitor 明确保留为产品边界，不用无证据实现制造伪 parity。
- 提交：本次（plan 51，见 git log）。
