# Plan 265 — 同步/异步边界的约定

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：待实现。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
> 先读代码确认，不成立就在本文件划掉并写明原因。

## 背景

阻塞 IO 散在 async 路径各处。各领域 plan 已经各自认领了一部分：worktree(252)、
`@file` 注入(246)、hook 子进程(255)、TUI 事件循环(263)、server 读循环(259)、CLI 启动的
git 次数(247)。这一条做两件事：**定一条全仓约定**,并修剩下不归任何领域的几处。

## 约定(写进 DESIGN)

- core 的文件系统与 git 函数一律写成同步函数;在 async 边界经 `spawn_blocking`(或一个薄
  封装)调用。不在 async fn 里直接调 `std::fs::*`、`Command::output()`、flock。
- 持有 std `Mutex` 时不做 IO、不 spawn 进程。
- 新代码评审时用 `rg` 过一遍 async fn 体内的 `std::fs::`/`std::process::`(开工时把这条检查
  写成 Makefile 里可手动跑的脚本，**不进 `make check` 门禁**,除非用户要)。

## 剩下的几处

1. **回合边界的已读文件检查**(待复核)。`core/src/file_state.rs:337-456`,调用链
   `agent.rs:1103` `remind_changed_reads` ← async `turn_rounds`:最多对 1024 个已读路径各 stat 一次，
   metadata 变了的整份 SHA-256,没有大小上限;读过的日志涨到 GB 级、或 `cargo fmt` 改了上百个读过的
   文件时，这次回合边界长时间占住 runtime worker。已读路径被换成 FIFO 时 `File::open` 永久阻塞，
   整个 turn 卡死。
2. **后台 bash spawn 持锁做阻塞调用**(待复核)。`tools/bash/background.rs:250-296`:同步
   `create_dir_all`、`create_file`、`process_tree::spawn`,spawn 时持有 registry 的 std Mutex;
   Windows 调试路径的 `spawn_debugged` 还在 `startup_rx.recv()` 上阻塞。期间 `running_count`、
   `snapshot`、`wait_reminder` 都被挡住。
3. **serve 的会话配置工厂在 async 方法里同步执行**(待复核)。`cli/src/main.rs:409-439`
   `serve_config_factory` 内的 context gather、`session_store.ensure`、`build_sandbox`、skills 加载与
   git 探测，由 `server/src/methods/threads.rs:417` 的 async 方法直接调用，阻塞 tokio worker。

## 做法

1. `changed_since_read` 放进 `spawn_blocking`;open 时 `O_NONBLOCK` 并检查 `is_file`;超过某个大小
   (开工定，推荐 16MB)不再 hash,直接按 Rewritten 处理。
2. 锁外先完成 spawn,再加锁检查 closed 并插入;已 closed 就立即 terminate 刚起的子进程。或整段挪进
   `spawn_blocking`。开工比较后选一种。
3. 工厂调用包进 `spawn_blocking`。
4. 写 DESIGN 约定段;加手动检查脚本。

## 验收

- 单测(旧代码先红):已读路径换成 FIFO 后回合边界不阻塞;超大文件不 hash 且提醒为 Rewritten;
  后台 spawn 期间 `running_count` 不被挡(测试钩子让 spawn 变慢);serve 新建线程期间其他线程的
  `turn/interrupt` 正常响应。
- 外层执行沙箱之外 `make check`;HANDOFF 记进展与教训。
