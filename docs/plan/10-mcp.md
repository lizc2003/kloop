# Plan 10 — MCP 客户端

> 一个会话完成。开工前先读根目录 CLAUDE.md(会话启动自动加载)。参考:两参考库收敛的 `server__tool` 命名;codex 的 mcp crate 形态。

## 目标

kloop 作为 MCP 客户端接外部工具服务器:配置里声明 server,启动时握手拿工具清单,合入 tool_defs,调用走 MCP。

## 设计要点

- **新 crate `crates/mcp`**:MCP 协议(JSON-RPC over stdio 起步,SSE/HTTP 后续)client 实现。依赖 protocol;core 通过 trait 用它,避免 core 依赖 mcp(工具注册处留 `ToolSource` 缝:内建 or MCP)。
- **命名**:`{server}__{tool}`(双下划线,两参考库的必然解);冲突时后注册者报警跳过。
- **工具形态**:MCP inputSchema 直接透传为 ToolDef.schema;ToolDef 的 name/description 目前是 &'static str——需要改成 String(小重构,protocol 里动,连带 tools.rs)。
- **并发安全分类**:MCP 工具一律不安全(串行),除非配置显式标注只读。
- **超长清单**:工具多了挤上下文——这是 deferred 工具 + tool_search 的入口,但本 plan 不做,清单超阈值(比如 30 个)先打警告。
- **配置**:`.kloop/config.toml` 的 `[mcp.servers.<name>] command = [...]`;若 Plan 8 已做,MCP 工具默认走询问。
- **生命周期**:server 进程随会话启停;握手失败降级为警告(不阻塞启动)。

## 测试

进程内 mock MCP server(stdio 管道喂 JSON-RPC)契约测试:握手、清单、调用往返、错误映射;命名冲突;schema 透传整对象断言。

## 完成标准

fmt/clippy/test 全绿;用一个真实 MCP server(如官方 filesystem server)手工验收一次调用闭环;README 更新。
