# Plan 185 — 二十二个方法排成一张表,这张表在一个 `impl` 里

> 本批判据与统一纪律见 `HANDOFF.md` 第〇节。**本批最大的三条之一。**

## 一、现状

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
- 只读查询:`provider_catalog_read` / `config_read` / `skills_list` / `mcp_server_status_list` / `read_scope` / `queue_thread_provider_switch`
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
| `methods/query.rs` | 只读查询那六个 | ≈300 |
| `methods/turn.rs` | turn 三个 | ≈150 |
| `methods.rs` | 只有三行 `mod` 声明——`methods/` 需要一个门面,它不放逻辑 | ≈5 |
| `lib.rs`(留) | `serve` / `write_loop` / `Server` 定义 / 分发与握手 / `pub use` | ≈400 |

## 三、坑

- **`Server` 的字段全是私有的**,分块 `impl` 在同一 crate 内没问题;
  但 `methods/*.rs` 是 `Server` 的子模块**还是兄弟模块**要想清楚——兄弟模块需要
  `pub(crate)` 字段,子模块不需要。**选子模块**(`mod methods;` 在 lib.rs 里),
  这样一个字段的可见性都不用改。这是本条最容易做错的一步。
- `MethodResult = Result<Value, (i64, String)>` 那个元组错误类型贯穿所有方法,
  它跟着 `params.rs` 走,其余文件 `use` 它。
- 分发表(`handle_request` 里的 `match method`)**留在 lib.rs**:它是这个 crate 的目录,
  分散出去就没人能一眼看全服务端支持哪些方法了——这正是 plan 143 的教训。
- plan 63 的 `acceptForProject`、plan 92 的 provider 切换、plan 39 的版本协商都在这个文件。
  **重构不改任何线上行为**;`crates/server/tests/` 的双工协议测试是安全网,逐条必须过。
- 2194 总行里只有 284 行测试,**安全网主要在 `crates/server/tests/`**,开工前确认它全绿。

## 四、验收

- `make check` 全绿;`crates/server/tests/` 逐条通过。
- 七个新文件都 ≤800;`lib.rs` 降到 ≈400 后跑 `make arch-baseline`,它会从基线里被摘掉。
- 服务端的方法表(`handle_request` 的 match)仍然在一个地方能看全。
