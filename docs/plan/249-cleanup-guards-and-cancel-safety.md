# Plan 249 — 资源守卫与"工具 future 可被丢弃"的契约

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：待实现。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
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

## 验收

- 单测/集成(旧代码先红):前台 worktree 子 agent 在运行中被取消，worktree 被清理或按
  finish 规则保留并有 Note,agent end 事件发出;registry 已关闭时后台 bash 不留文件;
  rewind 在 recover/replace 两个出口失败都不留分支文件;server fork 在 resume_options
  失败时不留文件。
- 外层执行沙箱之外 `make check`;DESIGN 补契约;HANDOFF 记进展与教训。
