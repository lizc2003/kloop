# Plan 185 ✅ — 二十二个方法排成一张表,这张表在一个 `impl` 里

> 本批判据与统一纪律见 `HANDOFF.md` 第〇节。**本批最大的三条之一。**

## 一、现状（立项读数，开工复核见第五节）

`rust/crates/server/src/lib.rs`,**1910 code 行 / 2194 总行**。
crate 里已经有 `events.rs` 与 `wire.rs`,但 lib.rs 还装着五样:

| 行 | code 行 | 是什么 |
|---|---|---|
| 1–84 | 63 | mod 声明与 use |
| 85–271 | 147 | **wire DTO**:`ThreadStartOptions` / `ConfigSnapshot` / `SandboxConfigInfo` / `SkillScope` / `SkillContext` / `SkillInfo` / `SkillsSnapshot` / `McpTransportKind` / `McpServerState` / `McpToolInfo` / `McpServerStatus` / `ServerPaths` / `ServerConfig` + 四个 `type` 回调别名 |
| 272–1367 | 982 | `serve` / `write_loop` / 线程与待决交互的类型 / **`impl Server` 的 22 个方法** |
| 1368–1644 | 242 | **参数解析与错误映射**:`MethodResult` / `SwitchRequestError` / `object_params` / `ensure_known_params` / `ensure_empty_params` / `resolve_cwd` / `parse_thread_start_options` / `str_param` / `parse_input` 等 |
| 1645–1964 | 295 | **线程工作者**:`thread_worker` / `provider_command_args` / `run_provider_command` / `run_turn_or_command` |
| 1965–2165 | 181 | **`ThreadUi`**:`Ui` / `Approver` / `Questioner` 三个 trait impl + `approval_decision` / `approval_scope_name` |

`impl Server` 那 22 个方法本身是一张**按主题分得很整齐的表**:

- 线程生命周期:`thread_start` / `thread_resume` / `thread_fork` / `thread_list` / `thread_read` / `thread_events_sync` / `spawn_thread` / `locate_thread`
- 查询与 provider 切换入队:`provider_catalog_read` / `config_read` / `skills_list` / `mcp_server_status_list` / `read_scope` / `queue_thread_provider_switch`
- turn:`turn_start` / `turn_steer` / `turn_interrupt`
- 分发与握手:`handle_line` / `handle_request` / `initialize` / `handle_interaction_response` / `send`

## 二、切法

`Server` 这个 struct 留在 lib.rs,**`impl Server` 分块写进不同文件**——
Rust 允许同一 crate 里多个 `impl` 块,这是最不伤内聚的拆法:

| 新文件 | 内容 | 预估 |
|---|---|---|
| `dto.rs` | 85–271 整段 | ≈150 |
| `params.rs` | 1368–1644 整段 | ≈242 |
| `worker.rs` | 1645–1964 整段 | ≈295 |
| `thread_ui.rs` | 1965–2165 整段 | ≈181 |
| `methods/threads.rs` | `impl Server` 的线程生命周期那八个 | ≈350 |
| `methods/query.rs` | 查询与 provider 切换入队那六个 | ≈300 |
| `methods/turn.rs` | turn 三个 | ≈150 |
| `methods.rs` | 只有三行 `mod` 声明——`methods/` 需要一个门面,它不放逻辑 | ≈5 |
| `lib.rs`(留) | `serve` / `write_loop` / `Server` 定义 / 分发与握手 / `pub use` | ≈400 |

## 三、坑

- **`Server` 的字段全是私有的**。它定义在 crate 根模块，`methods/*.rs` 是根的
  后代模块，可以直接读这些字段；无需放宽字段。反方向仍须按调用者提方法可见性：
  根分发表调用的入口用 `pub(crate)`，相邻 query 调用的 `locate_thread` 用
  `pub(super)`，同文件内的 `spawn_thread` 和 `read_scope` 保持私有。
- `MethodResult = Result<Value, (i64, String)>` 那个元组错误类型贯穿所有方法,
  它跟着 `params.rs` 走,其余文件 `use` 它。
- 分发表(`handle_request` 里的 `match method`)**留在 lib.rs**:它是这个 crate 的目录,
  分散出去就没人能一眼看全服务端支持哪些方法了——这正是 plan 143 的教训。
- plan 63 的 `accept_for_project`、plan 92 的 provider 切换、plan 39 的版本协商都在这个文件。
  **重构不改任何线上行为**;`crates/server/tests/` 的双工协议测试是安全网,逐条必须过。
- 开工复核：2271 总行中底部审批单测模块占 35 行，另有注释与空行；
  **安全网主要在 `crates/server/tests/`**，迁移前后均须全绿。

## 四、验收

- `make check` 全绿;`crates/server/tests/` 逐条通过。
- `lib.rs` 降到 ≈400。
- 服务端的方法表(`handle_request` 的 match)仍然在一个地方能看全。

## 五、✅ 完成（2026-10-16）

实现提交：本节所在提交（`refactor(plan185): split native server responsibilities`）。
提交号可用 `git log -1 --format=%h --grep='^refactor(plan185):'` 查询。

开工复核为 **1958 生产 code 行 / 2271 总行**，第一节的 1910 / 2194 为立项读数。
沿用 plan 181–184 的物理行口径：取底部 `#[cfg(test)]` 测试模块之前的非空、
非 `//` 注释行，不展开条件编译。原 `impl Server` 仍为 22 个方法。

| 文件 | 职责 | 生产 code 行 / 总行 |
|---|---|---|
| `lib.rs` | serve/write loop、私有状态、握手、分发表、交互响应与公开门面 | 376 / 454 |
| `dto.rs` | 公开配置/发现 DTO、四个回调别名与默认 ServerConfig 构造 | 152 / 198 |
| `params.rs` | 参数解析、运行时映射、steering/provider 错误分类 | 278 / 319 |
| `worker.rs` | 每线程 History、操作串行化、turn/命令与 effort 处理 | 330 / 367 |
| `thread_ui.rs` | 事件投影、审批/提问反向请求与原审批单测 | 208 / 276 |
| `methods/threads.rs` | 线程创建/恢复/fork、列表/读取、事件同步与工作者启动 | 471 / 511 |
| `methods/query.rs` | 只读查询、作用域与 provider 切换入队 | 201 / 217 |
| `methods/turn.rs` | turn 启动、条件 steering 与中断 | 98 / 122 |
| `methods.rs` | 仅三个子模块声明 | 3 / 3 |

- `Server`、线程状态、待决交互与 Drop guard 留在根模块，字段可见性均保留。
  分发表及 steering/provider 切换的特殊错误响应分支仍集中在 `handle_request`。
  14 个供根分发调用的迁移方法为 `pub(crate)`，`locate_thread` 仅为 `pub(super)`，
  `spawn_thread`、`read_scope` 私有；其余跨模块帮助项仅提至 `pub(super)`。
- 原有 24 个公开符号保持原路径与可见性：17 个 DTO/回调别名从根重导出，
  原有 5 个 wire 重导出和 `serve` / `serve_stdio` 保留；新增模块均私有。
- 词法核对 58 个函数的签名与正文、30 个类型/常量/别名，除可见性和 rustfmt
  格式变化外均一致。DTO 完整片段另核对，包含 serde 属性；审批单测连属性与
  正文一起迁入 `thread_ui.rs`。`wire.rs`、`events.rs` 与集成测试逐字节未改。
- 迁移前服务端协议基线为 40 通过、2 忽略；忽略项需要真实 API 凭据，本次未运行。
- DESIGN 原位补齐职责边界与目录表，并纠正本节开头残留的 `2.0` 协议版本描述；
  本轮重构前后 `PROTOCOL_VERSION` 均为 `1.0`，未修改协议或运行时文案。

Windows 在独立临时 workspace/target 中钉住 `1.98.1`，对真实 server 及依赖执行
`--target x86_64-pc-windows-msvc --all-targets --all-features` 类型检查。
四个职责模块、三个方法模块和迁移单测共八处故意错误均报预期 E0425；恢复全部
真实 crate 输入并逐字节核对、清理 server 产物后复验无告警通过。按教训 201，
只为 Windows triple 的 C 编译/归档生成空产物，不对 Rust 接口做 stub；
本检查不代表 C 代码、链接或 Windows 执行通过。

首轮完整门禁在 core 的
`shell_env::tests::replay_preparation_failure_warns_with_its_cause_and_preserves_the_capture`
失败：预期成功捕获后回放准备失败，实际提前落入捕获失败分支。该文件未改动，
原代码定向复测通过，完整门禁重跑时 core 1124 条也全部通过；首次捕获失败的
具体原因未确定，未放宽断言或超时，首轮日志留在仓库外。


最终源码在外层执行沙箱之外的完整 `make check` 全绿：fmt、clippy、workspace
debug 全量测试与本机 parity 均通过。server 26 条单测与 40 条协议集成测试通过，
2 条真实 API 测试按原设置忽略；CLI 154 条单测、plain PTY 7 项、TUI PTY 20 项、
core 1124 条全部通过。经单独批准，仅修订本机语料中一条 kloop 证据的 server
源码位置，其断言、捕获及其余字段未改，语料不进仓库。完整差异、
`git diff --check` 与 `git diff --cached --check` 通过；教训见 HANDOFF 239。
