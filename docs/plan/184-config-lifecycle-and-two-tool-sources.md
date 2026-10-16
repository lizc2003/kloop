# Plan 184 ✅ — MCP 那一侧:配置、生命周期、两个 ToolSource,一个文件

> 本批判据与统一纪律见 `HANDOFF.md` 第〇节。**本批最大的三条之一。**
> 建议在 plan 177(mcp crate 那半边)之后做,那时"MCP 的形状"还在脑子里。

## 一、现状（立项读数，开工复核见第五节）

`rust/crates/cli/src/mcp.rs`,**1909 code 行 / 3320 总行**。
模块 doc 说自己是"MCP glue",实际住着五样东西:

| 行 | code 行 | 是什么 |
|---|---|---|
| 1–444 | 369 | 常量 + `McpServerConfig` / `McpTransport`(含脱敏 `Debug`)+ **生命周期与健康**:`McpLifecycleState` / `McpAuthAvailability` / `McpCapabilitySummary` / `McpRefreshState` / `McpHealthResult` / `McpFailureKind` / `McpReadinessReceipt` / `McpLifecycleOwner` / `McpConnections` |
| 445–670 | 197 | **配置解析**:`load_mcp_servers` / `parse_server` / `http_headers_for` / `qualified_name*` + 两张键白名单 |
| 671–1436 | 674 | **`McpToolSource`**:目录、运行期状态、`ToolSource` impl、`build_catalog` / `build_source` / `spawn_tool_refresh` / `spawn_health_monitor` |
| 1437–1922 | 461 | **`McpResourceSource`**:`list_mcp_resources` / `read_mcp_resource` / `read_mcp_resource_dir` 三个工具 + 它自己的 `ToolSource` impl + 七个辅助函数 |
| 1923–2146 | 208 | **启动**:`initial_tool_catalog` / `classify_startup_failure` / `connect_servers` |

第四块是关键:**`McpResourceSource` 是一个完整的、独立的 `ToolSource`**。
开工核对：它通过 `McpResourceServer` 持有 `McpToolSource`，共用就绪状态与
调用/刷新锁；`McpConnections` 是启动后交给前端的集合，不是两者的运行期接口。

## 二、切法

**`mcp.rs` 留在原地当门面,新文件进 `mcp/` 子目录**(仓库里 `agent.rs + agent/`、
`fs.rs + fs/` 都是这个形状)。不要改成 `mcp/mod.rs`——那是一次路径改名,
基线里那一行会变成"文件没了",白白多一道手续:

| 新文件 | 内容 | 预估 |
|---|---|---|
| `mcp/config.rs` | 445–670 + `McpServerConfig` / `McpTransport` / 脱敏 `Debug` | ≈300 |
| `mcp/lifecycle.rs` | 生命周期、健康、就绪回执、`McpLifecycleOwner` / `McpConnections` + `spawn_health_monitor` | ≈330 |
| `mcp/tools.rs` | `McpToolSource` 全套 + `build_catalog` / `build_source` / `spawn_tool_refresh` | ≈620 |
| `mcp/resources.rs` | 1437–1922 整段 | ≈461 |
| `mcp.rs`(留,原地) | `connect_servers` 启动编排 + `classify_startup_failure` + `mod` 声明 + `pub use` | ≈250 |

五块没有一块超过 800,而且每一块都能用一句话说清自己是什么。

## 三、坑

- **`McpConnections` 是启动后交给前端的集合**，它住 `lifecycle.rs`；
  `config.rs` 不应该依赖它(配置解析不需要知道连接)。如果搬完发现有反向依赖,
  说明切线画错了,回头调而不是加 `pub`。
- `supported_image_mime` 在 `crates/mcp/src/render.rs` 有**同名同义的一份**。
  plan 177 已经记了这一笔;这次**仍然不合并**,但两条 plan 都做完之后值得单独立一条去重。
- `qualified_name` 是 `pub`(core 侧按 `{server}__{tool}` 解名),
  `load_mcp_servers` / `connect_servers` / `McpLifecycleOwner` 也是对 `main.rs`/`startup.rs` 的面。
  **开工第一步:`rg -n 'mcp::' rust/crates/cli/src/` 列全对外符号**，在保留的
  `mcp.rs` 里逐个 `pub use`，不创建 `mod.rs`。
- plan 90 的"就绪回执"和 plan 143 的"目录与门不漂移"都落在这个文件里。
  `McpReadinessReceipt` 的字段与 `McpFailureKind` 的分类**一个都不能改**。
- 开工时 3319 总行里，底部测试模块占 1174 行；总行减生产 code 行还包含注释与空行。

## 四、验收

- `make check` 全绿;`mcp::` 的对外符号表一个不少不多。
- `mcp.rs` 原地降到 ≈250。

## 五、✅ 完成（2026-10-16）

实现提交：本节所在提交（`refactor(plan184): split CLI MCP responsibilities`）。
提交号可用 `git log -1 --format=%h --grep='^refactor(plan184):'` 查询。

开工复核为 **1950 生产 code 行 / 3319 总行，24 条单测**，第一节的 1909 / 3320
是立项时的旧读数。沿用 plan 181–183
的物理行口径：取 `#[cfg(test)] mod tests` 之前的非空、非 `//` 注释行；其他
单独的 `#[cfg(test)]` 项照计，不展开条件编译。

| 文件 | 职责 | 生产 code 行 / 总行 | 原有单测 |
|---|---|---|---|
| `mcp.rs` | 启动编排、初始目录恢复、启动故障分类与公开门面 | 250 / 445 | 3 |
| `mcp/config.rs` | 配置解析、传输类型与脱敏、HTTP 请求头和工具命名 | 266 / 558 | 7 |
| `mcp/lifecycle.rs` | 就绪回执、故障类型、健康监控、生命周期所有权与连接集合 | 322 / 511 | 3 |
| `mcp/tools.rs` | 每服务的工具目录、运行期状态、调用门与目录刷新 | 709 / 1186 | 7 |
| `mcp/resources.rs` | 三个资源工具及其路由、聚合、读取和呈现 | 481 / 684 | 4 |
| `mcp/testutil.rs` | 共用双工服务端夹具 | 81 / 91（测试专用） | — |

- `mcp.rs` 原地保留，`main.rs` 的模块声明与调用方均未修改。7 个原有公开符号
  `McpServerConfig`、`McpTransport`、`McpLifecycleOwner`、`McpConnections`、
  `load_mcp_servers`、`qualified_name`、`connect_servers` 和 crate 内的
  `load_defer_threshold` 全部保持原路径及可见性；四个子模块私有。
- 内部访问只提至 `pub(super)`。配置不依赖生命周期；生命周期所有者仍管理真实
  工具源，资源工具仍经同一个源的就绪状态与读锁访问客户端。没有增加第二套
  回执、注册表或锁，也没有更改锁持有区间。
- 词法核对 116 个函数的签名与正文、31 个数据类型/常量；除可见性及 rustfmt
  的格式变化外均保持一致。24 条测试的名称、属性和正文保留；HTTP 启动恢复
  测试仍为 Unix 条件编译，共用夹具只在测试中编译。
- 原有 DESIGN 的行为说明仍成立，原位补齐职责边界与目录表。两份
  `supported_image_mime` 分别位于 CLI 的 `mcp/resources.rs` 与 wire crate 的
  `render.rs`，本次均保留原实现；跨 crate 去重仍另算。

Windows 在独立临时 workspace 与 target 中，钉住 `1.98.1`，检查真实 CLI
及其依赖的 `--target x86_64-pc-windows-msvc --all-targets --all-features`。
四个职责模块与共用测试夹具各注入一处未定义名字，五处预期 E0425 全部出现；
恢复副本并逐字节核对全部 crate 文件、清理 CLI 产物后复验通过，无告警。探针阶段也发现
父模块 `json` import 只剩 Unix 测试使用，已按调用者条件收窄。
按教训 201，仅为 Windows triple 的 C 编译与归档生成空产物，不对 Rust 接口
做 stub；本检查只覆盖 Rust 类型与测试目标，不代表 C 代码、链接或 Windows
执行通过。

最终源码在外层执行沙箱之外的完整 `make check` 全绿：fmt、clippy、workspace
debug 全量测试与本机 parity 均通过；CLI 154 条单测（含原有 24 条 MCP 测试）、
plain PTY 7 项、TUI PTY 20 项、core 1124 条测试全部通过。本机语料未修改。
完整差异、`git diff --check` 与 `git diff --cached --check` 通过；教训见 HANDOFF 238。
