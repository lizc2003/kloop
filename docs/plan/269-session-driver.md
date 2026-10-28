# Plan 269 — 会话驱动器：前端只渲染，turn 生命周期归 core

> 2026-10-28 立。来源：全仓只读审查与随后的架构讨论(HANDOFF「〇⁵」节)。
> 状态：待实现。**改动面最大的一条，开工先出设计稿给用户看**。先做 250、258、259(它们
> 修掉这个结构造成的具体 bug,这里消除成因)。行号是 2026-10-28 的读数，开工先自己量。

## 问题

四个前端各自驱动 turn 生命周期：TUI worker(`tui/src/lib.rs`)、server worker
(`server/src/worker.rs`)、plain REPL(`cli/src/plain.rs`)、headless(`cli/src/headless.rs`)。
每一处都自己写：

- 取输入，决定 `stage` 还是 `record`,调 `run_turn` 或 `run_turn_with_input`;
- 发 Usage 与 TurnEnded;
- 订阅 inbox 动静、起唤醒 turn(`subscribe_activity()` 在 tui、server、plain 各一处);
- 审批/提问的输入通道;
- 收尾：`shutdown_background_work` 在 `tui/src/lib.rs:263`、`:329`,`server/src/worker.rs:182`,
  `cli/src/main.rs:567`,`cli/src/plain.rs:257`,`core/src/commands/clear.rs:70` 共六处，
  后面跟着 `finish_active` 与警告。

这个结构直接造成了三个 P1:

- `run_turn` 与 `run_turn_with_input` 的 staging 语义不同 → hook 上下文残留(plan 250 第 1 条);
- plain 的两个 stdin 读者(plan 258 第 2 条);
- server 的唤醒信号被吞(plan 259 第 1 条)。

CLI 里另有两处装配重复：`main.rs:196-224` 的 `SessionState::open` 与 `main.rs:409-439` 的
`serve_config_factory`(context gather + 警告、`session_store.ensure`、`build_sandbox`、skills 加载，
只差警告的输出口);`main.rs:532-539` 与 `plain.rs:233-240` 的"mock 用 `at_revision`,否则用
session_route,再 `bind_session`"。

## 开工先问用户

- 驱动器的边界(要先给用户看类型定义与一张调用链图):草案是 core 提供一个持有 history、inbox、
  cancel 的驱动对象，接收一个输入枚举(用户消息、skill 展开、slash 命令、provider 切换、唤醒),
  统一决定 staging、空闲时重查 inbox、发 Usage/TurnEnded、收尾;审批与提问也经它的通道交给前端。
  前端只负责把输入送进去、把事件画出来。
- 分几片提交。推荐顺序：(1) 收尾与装配的共享 helper(纯搬运);(2) 输入枚举 + 统一 staging
  语义(删掉 `run_turn` 这条先 record 再跑的入口);(3) 唤醒循环;(4) plain 的审批/提问改走
  驱动器通道。每片一个会话也可以。

## 做法

按问答的分片推进。几个要守住的点：

- server 的 JSON-RPC 协议形状、TUI 与 plain 的 PTY 快照、headless 的 JSON 事件流不变。
- `/clear` 开新会话(plan 205)的"从现有 Config 派生新会话"缝要接进来，不另起一套。
- 中断撤回语义(plan 242 的草稿原位恢复)以驱动器为准，前端不再自己 `take_staged`。

## 验收

- 每片：tui、cli、server 现有测试与全部 PTY 用例不变;
- 第 2 片补"四个前端对同一输入序列产生相同 rollout"的对照测试(用 mock provider);
- 第 3 片补"非模型操作之后 inbox 仍被处理"的测试，覆盖四个前端;
- DESIGN 的前端/turn 生命周期一节改写;外层执行沙箱之外 `make check`;HANDOFF 记进展与教训。
