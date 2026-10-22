# Plan 237 — 启动门约束自动唤醒

> 2026-10-21。Plan 236 的审查发现（P2）：启动门只拦键盘，autowake 能在 MCP
> 握手窗口内提前启动首轮。
> 状态：已实现；待外层执行沙箱之外的完整门禁与提交。

## 问题

恢复一个带持久定时任务的会话时，`cfg.bind_session()` 会在 MCP 握手之前启动
scheduler worker。任务在连接窗口内到期时，`Inbox` 的 activity 分支和
`on_agent_events()` 都会调用 `autowake()`；而 `dispatch_autowake` 只看
`running` 与 inbox 是否非空，不看 `app.startup`，于是直接发 `WorkerMsg::Wake`，
在 sources 仍是 `Starting`、`defs()` 为空时发出首轮请求。

## 实现

- `dispatch_autowake` 的 readiness 谓词加入 `startup_pending`；`autowake_ready`
  变成 `!running && !startup_pending && !inbox.is_empty()`，两条调用路径
  （inbox activity、`on_agent_events`）自动共用。
- 启动行清空（任务发送 `None` 或发送端被丢弃）时立刻再调用一次 `autowake()`，
  把等待期间已进入 inbox 的通知送出去；否则 activity 边沿已被消费，任务会一直
  滞留到下一次无关活动。
- 不改变 `WorkerMsg::Wake`、inbox 投递或 scheduler 的任何语义。

## 测试

- TUI 单测 `scheduled_wake_waits_for_a_pending_startup`：inbox 非空 + `startup`
  存在时 `dispatch_autowake` 返回 None 且不发消息；清掉 startup 后同一 inbox
  能立刻发出 `Wake`。
- PTY 回归 `restored_scheduled_wake_waits_for_the_mcp_startup`：预置带
  `provider_route_initial` 的 session 录音与 `scheduled_tasks.json`（due、
  durable、owner=session id、`kind: "cron"`），用 `--resume` 启动并配置 800ms
  延迟的 MCP server（广告 `echo`）。断言：
  - 屏幕出现 `MCP connecting 1 server(s)`；
  - 握手窗口内（500ms）provider 收到 0 个请求；
  - 放开后定时任务触发一次，首请求带 `timer work` 与 `slow__echo`。
- 为支持该回归，PTY 夹具增加 `PtyOptions.prepare`（在子进程启动前拿到
  home/workspace 写私有状态）、`McpFixture::start_with_tools`，并在
  `RecordedRequest` 里记录 `tools[].function.name`。
- 外层执行沙箱之外 `make check` 全绿（含本机 parity）。

## 文档

- `rust/DESIGN.md` TUI 启动段补充 autowake 同门与开闸重查。
- `docs/plan/HANDOFF.md` 教训 250。

## 完成记录

- `autowake_ready` 现在要求 `!running && !startup_pending && !inbox.is_empty()`；
  `dispatch_autowake` 读 `app.startup`，inbox activity 与 `on_agent_events` 两条
  路径自动共享。启动行清空时再调用一次 `autowake()`。
- 单测 `scheduled_wake_waits_for_a_pending_startup` 绿。
- PTY `restored_scheduled_wake_waits_for_the_mcp_startup` 先红后绿：去掉修复时，
  握手窗口内就出现一次 provider 请求，且 `tool_names` 只有内置工具、没有
  `slow__echo`；恢复修复后窗口内 0 请求，放开后唯一请求同时带 `timer work` 与
  `slow__echo`。
- 外层执行沙箱之外 `make check` 全绿，含本机 parity：CLI 156、core 1133、
  server 41、TUI 283、TUI PTY 23。
- 提交号以本条所在提交为准。
