# Plan 235 — MCP 启动并发连接

> 2026-10-21。用户反馈"配置了很多 MCP 后，启动慢"。
> 状态：已实现；已通过外层执行沙箱之外的完整门禁（提交号以本条所在提交为准）。

## 根因

`connect_servers` 在 `crates/cli/src/mcp.rs` 里对每台 server 串行 `await`：
spawn → `initialize`（30s 上限）→ `tools/list`（30s 上限）。这一步位于
`ProcessState::load`，在任何前端启动之前完成，所以 N 台配置的启动耗时是各自
握手时间的**累加**；一台挂起的 server 还会把后面的全部推迟最多 30s。
失败不致命（DESIGN 的 "MCP startup failure never blocks kloop" 说的是不中止，
不是不等待），但慢是实打实的。

## 实现

- 按配置顺序做同步规划：`McpToolSource::planned` + `starting()`、生命周期注册、
  resource server、OAuth 凭据读取。
- 用 `futures::future::join_all` 同时驱动所有 server 的 spawn + 握手 +
  `tools/list`。
- join 后仍按配置顺序回填 `sources`、`statuses` 与警告——工具名撞名优先级和
  警告流都依赖这个顺序。
- 失败分类、降级路由、健康监控/刷新 worker 的启动时机与字段语义不变。
- `crates/cli/Cargo.toml` 增加 workspace 已有的 `futures` 依赖。

## 测试

- 两个 wiremock HTTP server，`initialize` 与 `tools/list` 各延迟固定值（静态
  bearer，避开真实 OAuth store）。500ms 标定时串行实测 2.03s（红）、并发约
  1.0s（绿）；提交版本用 750ms、阈值 2.25s，给整套并行测试的负载留余量。
  断言 `statuses` 与警告都保持 `alpha` → `beta` 的配置顺序。
- 先临时改回串行确认该测试红，再恢复并发让它绿；既有单 server 失败可见性与
  脱敏测试保持绿。

## 文档

- `rust/DESIGN.md` MCP 一节同步"启动并发、结果保序、总时延=最慢一台"。
- `docs/plan/HANDOFF.md` 教训 248。

## 验证

- 外层执行沙箱之外 `make check`（fmt + clippy + workspace debug 全量测试 +
  parity）。
- `git diff --check` 与暂存后 `git diff --cached --check`。

## 边界

- 不改变"启动等所有 server 连完"的语义：单台慢 server（如 `npx` 冷启动）仍占
  它自己的握手时间，TUI 仍要等最慢那台。
- 不做后台懒连接：那会改变首轮工具目录与 `mcp_server_status/list` 的启动快照
  语义，是另一件事。
- 本机未读取用户真实的 `~/.kloop/config.toml`（读取被拒），未测实际 N 台与耗时。

## 完成记录

- `connect_servers` 改为 `join_all` 并发连接；规划、注册与 `starting()` 仍按配置
  顺序完成，join 后按同一顺序回填 sources/statuses/警告。`futures` 只加进 CLI
  crate（workspace 已有，`Cargo.lock` 只多一条依赖边）。
- 回归 `servers_connect_concurrently_and_report_in_config_order`：500ms 延迟标定
  时串行 2.03s 红、并发 1.02s 绿；提交版本 750ms 延迟、阈值 2.25s，实测 1.51s
  通过，并断言 `alpha` → `beta` 的状态与警告顺序。既有 MCP 失败可见性/脱敏测试
  保持绿。
- 外层执行沙箱之外 `make check` 全绿：fmt、workspace all-target/all-feature
  clippy `-D warnings`、workspace debug 全量测试（CLI 155、core 1133、server 41、
  TUI 280 等）与本机 parity 语料。首轮全量运行时 core 的
  `parallel_worktree_subagents_write_their_own_trees` 因整套并行负载在 10s 门槛
  超时一次；单跑 0.65s 通过，复跑全量绿，与本改动无调用关系。
- 边界未变：启动仍等所有 server 连完，单台慢 server 仍占自己的握手时间；
  未读取用户真实配置，未量实际机器上的 N 台耗时。
- 提交号以本条所在提交为准。
