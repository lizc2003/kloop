# Plan 236 — TUI 先绘制，MCP 启动期间显示进度

> 2026-10-21。用户选择"更便宜的替代方案"：界面先起来、MCP 后台补齐，但首轮
> 工具目录与启动状态快照语义保持不变。
> 状态：已实现；待外层执行沙箱之外的完整门禁与提交。

## 目标与边界

- TUI 的终端、banner 在第一台 MCP server 握手之前就画出来；footer 显示
  `MCP connecting k/N · 服务器名 ready|failed`。
- 启动窗口内除退出键外不接受输入，**第一轮 turn 只能在全部 server settle 之后
  开始**，因此首轮目录仍是完整快照，`mcp_server_status/list`、就绪回执和失败
  分类都不改。
- `--plain`/`--headless`/`--serve`/`--mock` 仍先连后启动；不做后台懒补齐。
- 单台慢 server 仍会让会话晚可用，但可见、可退出。

## 实现

- `crates/cli/src/mcp.rs`：新增 `McpStartup::plan(servers)`（同步：planned +
  starting、lifecycle 注册、OAuth 读取、resource server 与源）与
  `connect(warn, progress)`；`connect_servers` 变成包装。进度按完成顺序回调
  `McpStartupProgress { done, total, name, connected }`，sources/statuses/警告
  仍按配置顺序折叠。resource 工具源改在 plan 阶段就进入 sources，保证提前构建
  的 `Config` 已持有全部源。
- `crates/cli/src/main.rs`：`ProcessState` 增加 `mcp_startup`；TUI 路径只 plan、
  不等待，`tool_merge_warnings` 移到连接完成后。`run_tui` 构造
  `kloop_tui::StartupTask`，进度写 footer 行，警告与失败走 note。
- `crates/tui/src/lib.rs`：`StartupTask`/`StartupReporter`；`run` 在
  `setup_terminal` 之后 spawn 任务，watch 通道驱动 footer 行；任务结束
  （或发送端被丢弃）即清行。`App.startup` 存在时 `on_input` 丢弃除 Ctrl+C/Ctrl+D
  外的输入。
- `crates/tui/src/render.rs`：footer 有启动行时替换 idle hints。
- 退出时 abort 启动任务，再由 lifecycle shutdown 收尾。

## 测试

- `mcp.rs`：`planned_startup_publishes_progress_in_completion_order`——plan 后
  sources 含两台 server 与 resource helpers；150ms/600ms 两台 server 的进度按
  完成顺序（fast→slow、done 1→2），statuses 仍按配置顺序。
- TUI：`footer_shows_startup_line_until_it_clears`；`a_pending_startup_only_passes_the_quit_chords`。
- PTY：`startup_paints_mcp_progress_and_gates_input_until_connected`——延迟 800ms
  的 MCP handshake 下先在屏幕看到 `MCP connecting 1 server(s)`；连接期间输入并
  回车，启动结束后 provider 仍只有一次请求且内容是启动后输入；证明启动窗口
  没有开 turn。
- 外层执行沙箱之外 `make check` 全绿（含本机 parity）。

## 文档

- `rust/DESIGN.md` MCP 启动段补充 TUI 顺序与输入门。
- `docs/plan/HANDOFF.md` 教训 249。

## 完成记录

- 实现：`McpStartup::plan/connect`（resource 源在 plan 阶段进入 sources；进度按
  完成顺序回调，statuses/sources/警告仍按配置顺序折叠）；TUI 路径延迟握手，
  `StartupTask` + watch 行 + 输入门；footer 启动行；警告走 note；退出时 abort
  启动任务。`--plain/--headless/--serve/--mock` 路径不变。
- 验证：`planned_startup_publishes_progress_in_completion_order`（150/600ms 两台
  server，进度 fast→slow、done 1→2，statuses 配置顺序）；TUI footer 与输入门
  单测；PTY `startup_paints_mcp_progress_and_gates_input_until_connected`
  （800ms 握手：先看到 `MCP connecting 1 server(s)`；启动中提交的输入没有到达
  provider；启动后首轮请求带完整目录与 user 文本）。
- 外层执行沙箱之外 `make check` 全绿，含本机 parity：CLI 156、core 1133、
  server 41、TUI 282、TUI PTY 22。首轮全量运行时 core 的 hooks/shell_env 共 11 条
  进程/钩子相关用例在整套并行负载下失败；单独重跑 hooks 17 条全绿、全量复跑
  全绿，与本改动无调用关系。
- 边界：慢 server 仍让会话晚可用（可见、可 Ctrl+C 退出）；`--plain` 仍先连再出
  提示符；未做后台懒补齐。
- 提交号以本条所在提交为准。
