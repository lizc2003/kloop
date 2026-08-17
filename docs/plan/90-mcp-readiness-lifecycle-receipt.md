# Plan 90 — CodeWhale 借鉴：MCP readiness/lifecycle receipt

> 状态：✅ 已完成（2026-08-17；提交 SHA 以本条所在提交为准）
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

## 完成记录（2026-08-17）

- CLI MCP adapter 现在为每个 configured server 建立唯一的 source-owned `McpReadinessReceipt`，typed 保存 server identity、secret-free endpoint digest、auth availability、capability summary、catalog generation、monotonic readiness revision、planned/starting/ready/stale/degraded/failed/closed lifecycle、refresh state、last health 与固定 failure class。HTTP digest 仅绑定 scheme/host/port，stdio 仅绑定 executable/env names；URL userinfo/path/query、argv、header/env/token 值均不进入 digest。receipt 只在内存中由同一 lifecycle owner 更新，不进入 permission、rollout、provider prompt 或 native wire，也没有新增 name→state registry。
- 启动失败 server 不再从 core route 消失：保留空 catalog 的 MCP source claim；精确 qualified tool、`tool_search select:`/qualified exact query 和 resource list/read/directory 都返回 bounded unavailable，而非 unknown/no deferred/Server not found。source resolver 会继续寻找后续 ready owner，配置层同时拒绝相同 sanitized server prefix。ready source 的 generation+readiness revision 同时进入 Plan 87 private binding；Program manifest 另在 source-wide version 两侧取样，阻断 unavailable→ready→unavailable ABA。dispatch 在 hook/permission 前、pre-hook 后与 wire gate 三处复核，resource helper 通过 input-aware preflight 在 hook/approval 前校目标 server；receipt 从不替代普通 permission/sandbox/hooks/concurrency。
- stdio transport 新增 bounded health watch、drop-safe pending request guard 和显式 shutdown/child reap。EOF/read/oversize/write/timeout、pending drain、notification close 与 owner shutdown 统一进入 lifecycle；refresh/health tasks 不再靠 source→client→notification sender 自保活。CLI 在 server/headless/plain/TUI 每条正常退出路径 await 同一 `McpLifecycleOwner::shutdown`，Drop 仅作异常路径 abort fallback。
- catalog refresh 仍与 in-flight call 线性化；进入 refresh 先标 stale，完整 list 成功才 publish generation+1。有界重试耗尽后保留 last-known-good catalog 但标 degraded/RetryExhausted，并从 discovery/call 隐藏；后续成功 refresh 恢复 ready 且旧 discovery receipt 因 readiness revision 变化失效。HTTP 无 notification stream 时继续标 startup-fixed；session 404 reinitialize 不透明重放原 operation，而是以 session gate + revalidation barrier 只允许 `tools/list` 先行，独立 session/auth revision 防 watch 合并丢边沿，更新 capabilities 并主动 re-list；startup `tools/list` 的 404 同样完成受控恢复。`notifications/initialized` 失败保持 barrier 且清掉未完成 session，OAuth 401 refresh/relogin 也更新 auth/readiness，而不虚报动态 subscription。
- 既有 native `mcpServerStatus/list` 继续是 immutable startup snapshot，protocol 1.0、DTO 与 Desktop 安全面均未改；runtime receipt 先只由 tool/resource 控制面消费，没有新增 process-global event/cursor 或 public protocol 双栈。
- 新增回归覆盖 stdio EOF health、取消后 pending 清理、HTTP session/OAuth lifecycle event、receipt 单调性与 secret-free endpoint binding、startup failed route、resource 三入口 availability、refresh failure/old catalog/recovery、generation+readiness discovery invalidation、Program manifest race、permission 前失败、explicit shutdown；保留原 refresh-vs-call、HTTP fixed catalog、OAuth、resource URI 与 immutable native status 契约。
- 验收已执行：`cargo fmt --all`；workspace `cargo clippy --workspace --all-targets --all-features -- -D warnings`；workspace `cargo test --workspace --all-targets --all-features`（CLI 118、core 718、MCP 31+14、server 33+18 等全部通过，2 项真实 provider credential tests 保持 ignored）；`cargo run -p kloop -- --mock` 7 rounds Completed。未新增依赖；未执行真实第三方 MCP/OAuth server、Linux/Windows native 或 Desktop E2E。
