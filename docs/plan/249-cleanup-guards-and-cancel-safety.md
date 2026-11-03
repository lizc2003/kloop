# Plan 249 — 资源守卫与"工具 future 可被丢弃"的契约

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：✅ 已实现(2026-11-03)。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
> 先读代码确认，不成立就在本文件划掉并写明原因。

## 问题

同一个形状出了五次："先建资源，再做会失败的步骤，最后显式 finish",错误或取消的出口
漏了清理。根因之一：`core/src/tools/mod.rs:1619` 在 `ctx.cancel` 触发时(排除 deadline、
已起前台 shell、已提交本地 send 三种情况)直接丢弃工具 future,而"工具 future 可在任意
await 处被丢弃"没有写成契约。

1. **前台 `run_agent {isolation:"worktree"}` 被中断时 worktree 泄漏**(已复核)。
   `core/src/tools/subagent.rs:599-667`:future 停在 `handle.await` 时被丢弃，它持有的
   `Option<Worktree>` 跟着 drop;`Worktree` 没有 Drop(`worktree.rs:147`),`worktree::finish`
   不执行，`emit_agent_end` 也不发。`.kloop/worktrees/` 留下无人知晓的 worktree 与
   `kloop-worktree-agent-*` 分支，可能带着子 agent 的改动;TUI 的 agent 行可能一直显示运行中。
2. **workflow 的 `structured_agent_admitted` 在 shutdown abort 时同上**(待复核)。
   `tools/subagent.rs:327-391`。
3. **后台 bash spawn 失败留下空的 `bg-*.out`**(待复核)。`tools/bash/background.rs:252-296`:
   `create_file` 先建了文件，之后 `bail!`/`?` 提前返回不删。
4. **rewind 中途出错留下孤儿分支文件**(待复核)。`core/src/rewind.rs:62`、`97`、`139-141`:
   `fork_session` 建好分支后，`recover_for_writing()` 或 `replace_session` 失败;分支 Rollout
   `preexisting=true`,Drop 不删。它 mtime 最新,`--continue` 会选中这条半成品。函数文档
   说"An error leaves History and the session untouched",只有 Cancelled 分支手动删了文件。
5. **server `thread/fork` 在 `resume_options` 失败时留下孤儿文件**(待复核)。
   `server/src/methods/threads.rs:201`:`:196-198` 与 `:212` 两处失败会 `remove_file`,
   `:201` 的 `?` 不会;每重试一次多一个，出现在 `thread/list` 里。

## 开工先问用户

- worktree 这类**异步**清理在 Drop 里交给谁：(a) `tokio::spawn` 出去、发一条 Note,进程
  退出时可能来不及;(b) 登记进会话的后台工作，让 `shutdown_background_work` 等它完成。
  推荐 (b)。

## 做法

1. core 提供一个守卫类型(名字开工时定，如 `CleanupGuard`):创建即生效，成功路径显式
   `disarm()`/`commit()`;Drop 时同步资源当场清理(删文件),异步资源按问答移交。
2. 第 1、2 条：`Worktree` 由守卫持有;取消时守卫把 `finish` 移交出去并补发
   `emit_agent_end(false)`。也可以照后台路径的做法，把 worktree 交给 spawn 出去的
   supervisor 任务负责 finish——开工比较两种写法，选改动小的。
3. 第 3–5 条：用守卫包住新建的文件，成功后解除。
4. DESIGN 的工具分发一节写明：工具 future 可能在任意 await 处被丢弃，跨 await 持有
   需要清理的资源必须由守卫或 supervisor 持有。

## 实现记录(2026-11-03)

- 五条均成立并修复。异步清理按问答选 (b)：`Config` 新增
  `cleanup_tasks: TaskTracker`(workspace 打开 `tokio-util = ["rt"]`)，
  `shutdown_background_work` 先 `close()`，再以现有的 2s 上限等待；超时按 `len()`
  计入 missed。
- 前台 `run_agent` 与 workflow 的 `agent` 都改成两层任务：worker 只跑子 agent，
  supervisor 持有 worktree、在 worker 结束后 `finish`、发 agent-end，并登记进
  `cleanup_tasks`。工具 future 在 `run_one` 的取消出口被丢弃时 supervisor 仍在，
  所以 worktree 不会跟着被 drop；session 关闭也会等它。
- `core::cleanup::RemoveFileOnDrop` 在创建后立即生效、成功路径 `disarm()`，用于
  后台 bash 的 `bg-*.out`、rewind 的分支文件与 server `thread/fork` 的 fork 文件。
  rewind 的 `open_fork` 把「fork 成功到 replace 提交」整段纳入守卫，取消分支不再
  手写删除；server fork 的三处失败出口也只剩一个 return。
- 回归：前台 worktree 子 agent 在 provider 中取消并 abort 工具 future，断言树与分支
  删除、agent-end 以 false 关闭；registry 关闭后的后台 bash 不新增 `bg-*.out`；
  `open_fork` 遇到不可读文件删除它；rewind 在 `replace_session`(scheduler store
  不可读)失败时不新增 `.jsonl`；server fork 在缺 runtime 时不留文件。
- `make check` 在执行器沙箱外全绿(fmt、clippy、workspace 全量测试，core 1303 项、
  TUI 314 项，本机 parity baseline verified)；期间 parity 的
  `explicit fixture restore` 自检偶发失败一次，单独重跑通过(同 plan 242 记录)。DESIGN
  新增「Dropped tool futures (Plan 249)」契约。Linux 与 Windows 未实测。

### 审查补丁：创建阶段与取消通知(2026-11-03)

- `worktree::create` 现在接收会话 tracker，并在 `git worktree add` 之前武装
  `CreationGuard`；只有正常提交或已同步回滚才 `disarm`。取消落在 add 之后、
  `verify_provenance` 期间时，Drop 把 `worktree remove --force` + `branch -D` 作为
  tracker 任务执行；`git worktree add` 自身设 `kill_on_drop`，回滚开始后不会还有
  git 在继续写。回归用 `#[cfg(test)] creation_probe` 在 add 之后停住创建 future，
  abort 后断言树和分支都被删除。
- `finish` 保留脏树或清理失败时，前台 supervisor 用 `Event::Note` 发布位置/错误；
  取消后工具结果已不存在，不能只靠返回值。wf 的清理失败同样补 Note。新增脏树取消
  回归，断言 note 含 `kloop-worktree-agent-` 且分支保留。
- 三条相关回归(创建取消、脏树取消、原 provider 取消)全绿，
  `cargo clippy -p kloop-core --all-targets --all-features -D warnings` 全绿；提交前
  完整 `make check` 也全绿(core 1305 项、TUI 314 项，parity baseline verified)。

### 第二轮审查(2026-11-03)

- **add 失败不再回滚**：进程内 mutex 不跨进程。另一个 kloop 可能在 pre-check 与
  `git worktree add` 之间创建同名树/分支；add 失败不能证明资源属于本次调用，强制删除
  会毁掉对方的树和未提交工作。只有 add 返回成功后才允许回滚。
- 回滚的 `worktree remove --force` / `branch -D` 改为最多 5 次短退避重试；被
  `kill_on_drop` 结束的 add 留下的 initializing 锁，不再让第一次失败就被当成清理完成。
- 保留分支的 Note 现在**无条件**发布。子 agent 已 Completed、supervisor 正在 finish 时
  调用方取消，结果已经没人读，Note 仍能到 UI；成功路径继续把位置附在工具结果里。
- 已知缺口(按“允许有遗漏”收口)：取消恰好落在 `git worktree add` 运行中时，直接子进程
  会被 kill，但 Git 派出的 checkout 子进程/initializing 锁仍可能让重试预算内的回滚失败；
  失败后不再进一步上报。add 失败留下的本次 partial 资源也不清理，因为无法与对方的资源
  区分。新增回归覆盖“add 失败不删对方的树/分支”和“成功保留也发 Note”；第二条在只把
  Note 追加到结果时先红。

## 验收

- 单测/集成(旧代码先红):前台 worktree 子 agent 在运行中被取消，worktree 被清理或按
  finish 规则保留并有 Note,agent end 事件发出;registry 已关闭时后台 bash 不留文件;
  rewind 在 recover/replace 两个出口失败都不留分支文件;server fork 在 resume_options
  失败时不留文件。
- 外层执行沙箱之外 `make check`;DESIGN 补契约;HANDOFF 记进展与教训。
