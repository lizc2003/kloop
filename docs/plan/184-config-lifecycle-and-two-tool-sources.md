# Plan 184 — MCP 那一侧:配置、生命周期、两个 ToolSource,一个文件

> 本批判据与统一纪律见 `HANDOFF.md` 第〇节。**本批最大的三条之一。**
> 建议在 plan 177(mcp crate 那半边)之后做,那时"MCP 的形状"还在脑子里。

## 一、现状

`rust/crates/cli/src/mcp.rs`,**1909 code 行 / 3320 总行**。
模块 doc 说自己是"MCP glue",实际住着五样东西:

| 行 | code 行 | 是什么 |
|---|---|---|
| 1–444 | 369 | 常量 + `McpServerConfig` / `McpTransport`(含脱敏 `Debug`)+ **生命周期与健康**:`McpLifecycleState` / `McpAuthAvailability` / `McpCapabilitySummary` / `McpRefreshState` / `McpHealthResult` / `McpFailureKind` / `McpReadinessReceipt` / `McpLifecycleOwner` / `McpConnections` |
| 445–670 | 197 | **配置解析**:`load_mcp_servers` / `parse_server` / `http_headers_for` / `qualified_name*` + 两张键白名单 |
| 671–1436 | 674 | **`McpToolSource`**:目录、运行期状态、`ToolSource` impl、`build_catalog` / `build_source` / `spawn_tool_refresh` / `spawn_health_monitor` |
| 1437–1922 | 461 | **`McpResourceSource`**:`list_mcp_resources` / `read_mcp_resource` / `read_mcp_resource_dir` 三个工具 + 它自己的 `ToolSource` impl + 七个辅助函数 |
| 1923–2146 | 208 | **启动**:`initial_tool_catalog` / `classify_startup_failure` / `connect_servers` |

第四块是关键:**`McpResourceSource` 是一个完整的、独立的 `ToolSource`**,
和 `McpToolSource` 之间只通过 `McpConnections` 相认。它自己就是一个 feature。

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

- **`McpConnections` 是四块共用的中枢**,它该住 `lifecycle.rs`;
  `config.rs` 不应该依赖它(配置解析不需要知道连接)。如果搬完发现有反向依赖,
  说明切线画错了,回头调而不是加 `pub`。
- `supported_image_mime`(1903)在 `crates/mcp/src/lib.rs:1011` 有**同名同义的一份**。
  plan 177 已经记了这一笔;这次**仍然不合并**,但两条 plan 都做完之后值得单独立一条去重。
- `qualified_name` 是 `pub`(core 侧按 `{server}__{tool}` 解名),
  `load_mcp_servers` / `connect_servers` / `McpLifecycleOwner` 也是对 `main.rs`/`startup.rs` 的面。
  **开工第一步:`grep -rn 'mcp::' crates/cli/src/` 列全对外符号**,`mod.rs` 里逐个 `pub use`。
- plan 90 的"就绪回执"和 plan 143 的"目录与门不漂移"都落在这个文件里。
  `McpReadinessReceipt` 的字段与 `McpFailureKind` 的分类**一个都不能改**。
- 3320 总行里 1411 行是测试。

## 四、验收

- `make check` 全绿;`mcp::` 的对外符号表一个不少不多。
- `mcp.rs` 原地降到 ≈250。
