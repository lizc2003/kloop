# Plan 90 — CodeWhale 借鉴：MCP readiness/lifecycle receipt

> 状态：规划中
>
> 依赖：Plan 16、Plan 27、Plan 34、Plan 34b、Plan 54、Plan 87、Plan 88；参考快照 `refs/codewhale` 当前观察 HEAD `5e3ac84c5cb925b4c90c34dfe582b75f04605cb1`，固定历史审计基线 `b494236312ef3ac36489c83706a0b11ab73935a1`

## Context

kloop 已有 MCP startup handshake、stdio/HTTP transport、ToolSource catalog/generation、dynamic refresh、resources list/read/directory 和 OAuth 生命周期，但 server readiness、catalog health、refresh failure、auth availability 与 tool/resource availability 仍分散在 CLI、MCP crate 和 ToolSource。CodeWhale 的可吸收原则是：先区分 capability planning 与实际启动，失败/disabled server 不能伪装成 tool missing，健康和可用性要有 bounded、可消费的 receipt。

本计划只收口 MCP 控制面的 readiness/lifecycle，不重做 MCP wire、不把 resources 与 tools 合并，也不把 receipt 变成权限 grant 或第二套 registry。

## 契约与范围

### 本次吸收

1. MCP server 从 configured/planned/starting/ready/degraded/failed/stale/closed 形成 typed readiness/availability receipt，至少绑定 server identity、endpoint binding、auth availability（不含 secret）、capability/catalog generation、refresh 状态、最近健康结果和 bounded failure reason。
2. startup handshake 保持 `initialize` → `notifications/initialized` → `tools/list`；stdio EOF、pending request failure、notification channel close 进入统一 health transition；shutdown/cancel 有明确 owner。
3. refresh 失败保留旧 catalog、标记 degraded、执行有界重试；旧 catalog 不自动等同 ready。HTTP transport 继续明确报告 startup-fixed catalog，不虚报动态 refresh 能力。
4. Tool discovery、resource read 和 call 都消费明确 availability/generation receipt；未启动、未认证、不健康或 stale 不能被对外伪装成“没有工具”。普通 permission、sandbox、hooks、concurrency gate 仍在 receipt 之上生效。
5. 若需要进入 native server projection，只增加 bounded typed status/event 设计；不改变既有 immutable `mcpServerStatus/list` 语义，receipt 不包含 raw command、URL secret、header 或 credential。

## 关键文件

- `kloop/crates/mcp/src/{lib.rs,http.rs,sse.rs,oauth.rs}`
- `kloop/crates/mcp/tests/`
- `kloop/crates/core/src/{config.rs,tools/mod.rs,tools/tool_search.rs,permissions.rs}`
- `kloop/crates/cli/src/{startup.rs,mcp.rs}`
- `kloop/crates/server/src/{lib.rs,events.rs,wire.rs}`
- MCP/core/server lifecycle and availability tests

## 非目标

- 不重做 MCP JSON-RPC/stdio/HTTP/SSE wire，不新增远程发现、WebSocket、旧 SSE 或新的 provider。
- 不把 receipt 变成 permission grant，不绕过 ToolSource generation、sandbox、hooks 或 concurrency。
- 不把 MCP resources 折算成 tool catalog，不创建全局 doctor、第二套 MCP registry 或 snapshot 真值。
- 不在本计划引入新的 OAuth/keyring/CIMD/XAA/step-up 流程；不暴露 token、header、command、URL secret。
- 不做跨 session A2A、child billing 或 provider terminal receipt。

## 为什么不能与其他计划合并

MCP readiness 是外部连接控制面的生命周期；Plan 89 是模型 provider 数据面 terminal，Plan 87 是工具授权 binding，Plan 88 是 child route provenance。它们需要不同 owner、刷新条件和失败语义。

## 实施与测试方向

覆盖 startup success/failure、stdio EOF、pending failure、notification close、refresh success/failure/lag、旧 catalog 保留、HTTP fixed catalog、auth unavailable、permission deny/ask、stale generation、resource list/read/directory、call cancellation 和 clean shutdown。验证失败 receipt 有界且不含 secret，工具调用不能把 degraded/failed 伪装成 missing，native projection 不泄漏内部 transport 细节。

## 完成标准

MCP readiness、health、auth availability、catalog generation、refresh 状态可消费且 fail-closed；旧 catalog 与 degraded 明确区分；HTTP/stdio 能力不被夸大；permission/tool generation/resources 边界不回归；不新增依赖、second registry、public protocol 双栈或未实现的 OAuth/远程能力。
