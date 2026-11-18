# Plan 269 — 会话驱动器：前端只渲染，turn 生命周期归 core

> 2026-10-28 立。来源：全仓只读审查与随后的架构讨论(HANDOFF「〇⁵」节)。
> 状态：✅ 已完成（2026-11-18）。依赖的 250、258、259 均已先完成。
> 行号是 2026-10-28 的读数，保留为当时的依据；实现以函数/顶层项定位。

## 完成记录

- **驱动器**：新增 `core/src/driver.rs` 的 `SessionDriver`，持有 `Config`、`History`、
  `Arc<SessionProviderState>` 与 `--image`；前端只构造 `Input`（`Message` / `Command` /
  `Wake`）并投影 `Report`（`Turn` / `Command`）。staging、skill 展开、`/clear` 的
  `start_fresh_session`、rewind、两个 idle provider 事务、`reannounce_if_owed` 与
  `shutdown_session` 都在这里。`agent::run_turn` / `run_turn_with_input` 改为
  `pub(crate)`，前端不能再“先 record 再跑”。
- **四个前端**：headless、plain、server thread worker、TUI agent worker 均改为
  `dispatch` + 各自的事件投影；server 的 `run_turn_or_command`、`run_provider_command`、
  `parse_effort_level` 删除，`ThreadWorkerMsg` 增加 `Wake` 并去掉 `Turn.delivery_only`；
  TUI 的 rewind 搬到驱动器，`current` 在每次 dispatch 后同步。
- **收尾与装配**（第 1 片）：六处收尾共用 `shutdown_session`（TUI 保留
  “关后台 → abort worker → 关 worktree”的顺序）；`prepare_workspace` 收掉
  `SessionState::open` 与 `serve_config_factory` 的重复；`finalize_session_config`
  收掉 mock `at_revision` / `session_route` / `bind_session` 的三份拷贝。
- **唤醒**：server 的两个 wake helper 由 `reannounce_if_owed` 取代（命令不排空、模型
  turn 只重发期间到达的新活动）；plain 在每次 dispatch 后调用；TUI 的 `on_agent_events`
  本来就在每个事件边界 `autowake`，未再改。
- **第 4 片复核后不成立**：258 的 `PlainStdin` 已是唯一 stdin 读者，审批/提问经
  Config 的 `Approver`/`Questioner` 进入同一队列；驱动器不碰 stdin，因此“plain 审批改
  走驱动器通道”没有遗留工作。
- **附带修复**：`thread/start` 的 config build 在阻塞池完成时，若客户端已关 stdin，
  响应会丢（`mock_hermetic` 可复现）。server 现在在 EOF 后限时排空已接受的 build。
- **review 修正**（2026-11-18）：
  - server 收尾改为先取消运行中的 turn，再限时排空已接受的 build；超时的 build 回 JSON-RPC
    错误而不是沉默；writer 限时 flush 后 abort，卡在 factory 里的 build 不再通过持有
    `out` sender 拖住 shutdown。
  - 中断的 skill turn 退回**原命令行**而不是展开后的 prompt；TUI 的 slash 提交也保存
    `in_flight` 草稿，普通输入和命令都按同一路径恢复。
  - 展开期间已被取消的 token 不再进入 `run_message`（不会在中断后跑 `pre_turn`）；
    plain 在技能采样被 Ctrl+C 时先打印中断原因再退出。
  - `rewind` 与 `start_fresh_session` 先构造可失败状态再替换；rewind 的分支路由在当前
    配置里已不可用时回退到分支自己的 route 并记一条 note，不再留下半切换。
  - server 的 `/provider …` 先发布路由快照与 `thread/provider/changed`，再发成功文本。
- **验证**：`cargo test -p kloop-core --lib`（1459）、`-p kloop-server`（53）、
  `-p kloop-tui --lib`（353）、`-p kloop --bins`（169）、
  `cargo test -p kloop --test session_rollout_parity --test plain_pty --test headless_contract --test mock_hermetic`
  全绿；TUI PTY 25 项在沙箱外全绿（沙箱内 24/25，`a_shell_command_gets_a_row_of_its_own`
  被嵌套 seatbelt 挡住，`sandbox_apply: Operation not permitted`）；
  四个受影响 crate 的 `cargo clippy --all-targets -- -D warnings` 干净。
- **review 修正（第二轮）**：
  - CLI 在 `run` 返回后用 `shutdown_timeout` 结束 runtime：卡在 factory 里的 blocking build
    不能被 abort，默认的 runtime drop 会一直等它。
  - server 在 EOF 时先发 `shutdown` 令牌，worker 不再启动新 turn；审批/提问的等待在令牌触发时
    立即返回 Stop/Unavailable，不再等一个已经关闭的客户端。
  - 超时 build 的请求回 JSON-RPC 错误；`thread/fork` 的删除守卫移到读循环注册处，未注册的
    分支不会留在 `thread/list` 里。
  - rewind 用 `History::branch` 构造独立分支，路由落定后才退役旧会话；路线既不能写、也不能从
    目录恢复时直接拒绝，不再报告“成功但无法采样”。
  - 展开期间取消按 `Aborted` 报告并带 `entered_loop=false`；`--image` 由驱动器重新挂回下一次输入
    （命令没有 composer 文本可载图）。
  - TUI 的命令草稿只含文本与 paste，图片留在输入框：保留的图片恢复时挂回，用户清掉的不复活。
- **review 修正（第三轮）**：只读 RPC 在 EOF 后也有答复期限，fork 删除守卫由读循环持有，
  factory 卡住时也能在回错误前删掉分支。排队的 provider 事务有答复，排队的 turn 有 `aborted`
  终态。rewind 换入分支前准备 provider，停止旧后台后把审批补写到旧文件，分支从第一条请求起沿用
  已冻结的 stub。技能展开被取消时退回原命令，包括 `!cmd` 以错误退出的情况。TUI 保留用户刚贴的
  草稿，以及输入框里剩余的图片编号与在途读取。
- **既有基线**：全仓 parity 的 `Plan 56 parser contract drift` 不通过——`refs` 语料期望
  `enter_name_null`/`enter_path_null`/`exit_discard_null` 判为 invalid，而 plan 267 的
  native report 明确 `null_is_omitted`。本次改动未触 `core/src/tools/`，不是这条 plan 引入的。
- 全量 workspace 测试未在单次 10 分钟内跑完；以上按受影响 crate 分跑，未覆盖的 crate
  本次没有改动。

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
