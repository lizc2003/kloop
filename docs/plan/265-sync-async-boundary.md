# Plan 265 — 同步/异步边界的约定

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：✅ 已完成(2026-11-17)。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
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

1. **回合边界的已读文件检查**(已复核，成立)。`core/src/file_state.rs:337-456`,调用链
   `agent.rs:1103` `remind_changed_reads` ← async `turn_rounds`:最多对 1024 个已读路径各 stat 一次，
   metadata 变了的整份 SHA-256,没有大小上限;读过的日志涨到 GB 级、或 `cargo fmt` 改了上百个读过的
   文件时，这次回合边界长时间占住 runtime worker。已读路径被换成 FIFO 时 `File::open` 永久阻塞，
   整个 turn 卡死。
2. **后台 bash spawn 持锁做阻塞调用**(已复核，成立但措辞要修)。`tools/bash/background.rs:250-296`:
   `create_dir_all`、`create_file` 本来就在锁外；真正持锁的是 `process_tree::spawn`。
   Windows 调试路径的 `spawn_debugged` 还在 `startup_rx.recv()` 上阻塞。期间 `running_count`、
   `snapshot`、`wait_reminder` 都被挡住。
3. **serve 的会话配置工厂阻塞读循环**(已复核，成立但前提不同)。`cli/src/main.rs` 的
   `serve_config_factory` 内的 context gather、`session_store.ensure`、`build_sandbox`、skills 加载与
   git 探测，由 `server/src/methods/threads.rs` 直接调用。**这些方法现在是同步方法、由单线程读循环
   顺序调用，不是 async 方法**；因此只在工厂调用外包一层 `spawn_blocking` 再原地等待，读循环仍然
   不会去读下一行，`turn/interrupt` 照样被挡住。

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

## 完成记录(2026-11-17)

- **约定**:`rust/DESIGN.md` 新增「Blocking IO and the async runtime (plan 265)」，
  并把 Transport 一节里 blocking pool 的清单扩到线程创建。检查脚本
  `scripts/check-async-io.py`(跳过 `spawn_blocking` 闭包、测试项和 `async-io-ok` 注释行)，
  入口 `make async-io`，不进 `make check`。
- **已读文件检查**:`changed_since_read` 的调用点改成
  `tools::changed_reads_reminder_async`，整段 stat/hash 在 `spawn_blocking` 里；
  `check_disk` 先拒绝非普通文件，open 用 `O_NONBLOCK`(unix)并在句柄上复查 `is_file`，
  超过 `CHANGED_READS_MAX_HASH_BYTES`(16 MiB)不读内容直接 `Rewritten`。
  回归：FIFO 路径 2 秒内带超时返回 `Rewritten`;稀疏 16 MiB+1 文件不读取即 `Rewritten`。
- **后台 spawn**:选「锁外 spawn、加锁复查 closed」——整段挪进 `spawn_blocking` 仍会把
  注册表序列化，达不到并发。closed 分支先放开锁、terminate 新树、再按原样拒绝；
  `ProcessTreeChild` 的 Drop 兜底。回归用测试钩子把 spawn 停在锁外，2 秒超时内
  `running_count()` 返回 0；原有 closed-registry 测试覆盖输出文件清理。
  顺手把 monitor 每 5 秒的 `std::fs::metadata` 改成 `tokio::fs::metadata`。
- **serve 线程创建**:`thread/start`、`thread/resume`、`thread/fork` 的工厂与后续
  Config 组装整体移到 `spawn_blocking`；完成的 `ThreadHandle` 经 channel 回到读循环，
  由 `finish_thread_build` 注册并发响应。读循环用 `select!` 在下一帧与 build 完成之间
  竞争，`FrameReader` 把半帧保存在 reader 上以保持取消安全；`pending_threads` 挡住
  并发的重复 resume。回归：400ms 的慢工厂下，`thread/start` 与 `turn/interrupt`
  背靠背发出，interrupt 的响应先到。
- **验证**:`cargo test -p kloop-core --lib` 1443 通过、1 ignored(含新增回归)；
  `cargo test -p kloop-server --test server` 51 通过、2 ignored；
  `cargo clippy -p kloop-core -p kloop-server --all-targets -- -D warnings`、`cargo fmt --all`、
  `make async-io` 通过。未跑全量 `make check`、parity、真实 API(按 2026-11-06 约定)。
  遗留：`thread_resume`/`thread_fork` 读整份 transcript 仍在读循环上(本 plan 只认领配置工厂)。
