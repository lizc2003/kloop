# Plan 60 — CodeWhale 源码审计与 kloop 可借鉴项

> 状态：调研完成；未形成实施计划，未修改产品代码
>
> 日期：2026-07-27
>
> 调研对象：<https://github.com/Hmbown/CodeWhale>
>
> 本地参考库：`refs/codewhale`（独立 Git 克隆，由根 `.gitignore` 排除）
>
> 固定快照：`b494236312ef3ac36489c83706a0b11ab73935a1`（2026-07-26）
>
> 用途：供后续新会话讨论下一步规划。本文是规划输入，不代表任何候选项已经拍板。

## 一、结论

CodeWhale 最值得 kloop 借鉴的不是功能数量，而是四类控制面机制：

1. provider stream 的超时、容量和重试边界；
2. runtime event 的单调序号、持久回放和断档恢复；
3. subagent 生命周期的可观察性，以及 tool preparation/resource claim；
4. instruction、MCP、Skills、Web bootstrap 等外部输入边界治理。

不建议照搬 CodeWhale 的整体架构。kloop 应继续保留自己的：

- canonical protocol；
- 单一而强制的 permission/sandbox 主链；
- append-only rollout、lineage、fork 和 compaction marker；
- provider wire adapter 与 core agent loop 的清晰所有权；
- 小而可验证的核心。

一句话方向：

> 保留 kloop 的机制内核，选择性吸收 CodeWhale 的控制面、事件恢复、subagent 可观察性和输入边界治理。

若下一步只做一项，当前建议优先讨论 **provider stream guard**。

## 二、调研边界与证据纪律

本轮采用只读源码审计：

- 克隆位置：`refs/codewhale`（调研后从临时目录移入，保持独立 Git 元数据）；
- 固定提交：`b494236312ef3ac36489c83706a0b11ab73935a1`；
- 阅读 Rust workspace、npm runtime SDK、Web、VS Code extension、CI 和测试；
- 对照 kloop 当前 core/provider/server/rollout/tools/permissions/hooks/context/subagent/MCP/Skills；
- 没有构建、运行、测试或安装 CodeWhale；
- 没有修改 CodeWhale 源码、其他参考库或 kloop 产品代码。

因此本文区分：

- **源码已接通**：能从入口追到生产执行链；
- **局部实现**：存在类型、配置、测试或辅助模块，但未完整接入生产主路径；
- **stub/scaffold**：接口或产品表面存在，执行能力未完成；
- **建议**：结合 kloop 当前结构作出的取舍，不是 CodeWhale 的事实。

静态测试统计只能说明测试资产规模，不能替代真实 provider、真实 sandbox 或目标设备 E2E。

## 三、CodeWhale 的真实主路径

CodeWhale 是入口很宽的本地 agent 平台。workspace 约 19 个 Rust crate，另有 npm SDK、Web、VS Code 和聊天桥。

真正的 live runtime 仍主要位于 `refs/codewhale/crates/tui`。仓库自己的架构文档也承认 crate 拆分尚未形成唯一真相：

- `refs/codewhale/docs/ARCHITECTURE.md:4-13`
- `refs/codewhale/crates/tui/src/core/engine.rs:552-568`
- `refs/codewhale/crates/tui/src/core/engine/turn_loop.rs:362-2213`

生产主路径可概括为：

```text
TUI Engine
  → RuntimeThreadManager
  → /v1/threads/* HTTP/SSE
  → durable JSONL events
```

对应位置：

- `refs/codewhale/crates/tui/src/runtime_threads.rs:1417-1485`
- `refs/codewhale/crates/tui/src/runtime_api.rs:2181-2598`
- `refs/codewhale/crates/protocol/src/runtime/mod.rs:6-48`

其他入口主要是兼容包装：

- `/v1/stream` 复用同一个 Engine；
- stdio app-server 启动 HTTP runtime 子进程，再翻译 SSE；
- legacy `/v1/chat/completions` 只接受 Chat Completions wire，并拒绝 streaming。

关键位置：

- `refs/codewhale/crates/app-server/src/lib.rs:979-1206`
- `refs/codewhale/crates/app-server/src/chat_completions.rs:302-363`

这套结构证明 CodeWhale 有较完整的 runtime 控制面，也暴露了所有权没有完全收敛的问题：核心 loop、provider client、runtime、persistence、MCP 和大量工具仍聚集在 TUI crate。

## 四、分领域结论

### 4.1 Provider、streaming 与 retry

CodeWhale 的 provider identity 很多，但底层只有三类 wire：

- OpenAI Chat Completions；
- OpenAI Responses；
- Anthropic Messages。

位置：

- `refs/codewhale/crates/config/src/provider_kind.rs:11-232`
- `refs/codewhale/crates/config/src/provider.rs:32-42`
- `refs/codewhale/crates/tui/src/client.rs:2080-2141`

这一点与 kloop 当前三 adapter 结构一致。kloop 已有成熟 translation，不应重写：

- `rust/crates/provider/src/anthropic.rs`
- `rust/crates/provider/src/openai.rs`
- `rust/crates/provider/src/responses.rs`
- `rust/crates/provider/src/sse.rs`

CodeWhale 值得借鉴的是 guard 组合：

- 建连/header timeout；
- chunk idle timeout；
- stream wall-clock timeout；
- response content cap；
- status/retry taxonomy；
- `Retry-After`；
- partial output 后抑制不安全 retry。

位置：

- `refs/codewhale/crates/tui/src/client/stream_entry.rs:17-163`
- `refs/codewhale/crates/tui/src/core/engine/turn_loop.rs:700-1219`
- `refs/codewhale/crates/tui/src/client/chat.rs:707-887`
- `refs/codewhale/crates/tui/src/client/responses.rs:182-411`
- `refs/codewhale/crates/tui/src/client/anthropic.rs:215-277`

但不能直接复制其终态语义：

- outer chunk timeout 可能先发 `Error`，随后仍产生 `Completed`；
- 某些 adapter 对没有协议完成标志的 clean EOF 处理过宽；
- Anthropic 的 stream-open retry 与另两条 rail 不完全一致。

kloop 当前最明显的缺口在：

- `rust/crates/core/src/agent/sampling.rs:59-279`
- `rust/crates/provider/src/lib.rs:181-343`

借鉴时应先固定不变量，而不是复制常量：

1. 每个 sampling attempt 只能有一个 terminal outcome；
2. 没有协议完成标志时，不得把 EOF 自动解释为成功；
3. 已产生用户可见 text/reasoning/tool JSON 后，不得透明重放整个请求；
4. partial tool JSON 不能伪造成完整 tool use；
5. timeout、cancel、protocol error、HTTP retryable error 必须可区分。

### 4.2 Provider catalog 与 route

CodeWhale 明确区分：

- canonical model；
- wire model；
- provider identity；
- wire API；
- route candidate。

位置：

- `refs/codewhale/crates/config/src/route/resolver.rs:42-263`
- `refs/codewhale/crates/config/src/route/candidate.rs:181-325`
- `refs/codewhale/crates/config/src/catalog.rs:603-664`

好的约束是：不从 prompt 或 model prefix 猜 provider。

但其 live catalog 尚未完整接入生产 route runtime；core 与 TUI 也有重复 provider config schema：

- `refs/codewhale/crates/tui/src/route_runtime.rs:194-245`
- `refs/codewhale/crates/config/src/lib.rs:95-138`
- `refs/codewhale/crates/tui/src/config.rs:2728-2940`

kloop 可借鉴 capability metadata，例如：

- 支持的 wire；
- reasoning/tool/image 能力；
- context/output limit；
- stream/retry 限制。

不应追逐“30+ provider 名称矩阵”，也不应复制两套 config/catalog 所有权。

### 4.3 Runtime event、replay 与协议

CodeWhale runtime event envelope 包含：

```text
schema_version
seq
thread_id
turn_id
item_id
timestamp
payload
extra
```

位置：`refs/codewhale/crates/protocol/src/runtime/mod.rs:6-48`。

其 HTTP/SSE runtime 支持：

- durable replay；
- snapshot 到 live SSE 的交接；
- replay/live `seq` 去重；
- broadcast lag 后回 durable log 补洞。

位置：`refs/codewhale/crates/tui/src/runtime_api.rs:2181-2325`。

kloop 当前 canonical JSON-RPC item protocol 已经更适合作为唯一协议所有权，但 event 没有稳定 replay cursor：

- `rust/crates/server/src/wire.rs:55-203`

值得借鉴的是 additive event `seq`，不是把 kloop 改成 CodeWhale HTTP API。

建议顺序：

1. 先定义 thread 内单调、跨 turn 不复位的 event `seq`；
2. 明确 process restart 后如何恢复 next seq；
3. 再设计 cursor replay；
4. 最后补 snapshot→live handoff 和 pending interaction 恢复。

不要一次同时引入 HTTP runtime、SSE compatibility layer 和 legacy protocol。

### 4.4 Tools、preparation、resource claim

CodeWhale 的 tool registry 会校验 payload kind、mutating 属性、per-tool timeout，并在调度前做无副作用 preparation：

- `refs/codewhale/crates/tools/src/lib.rs:224-523`
- `refs/codewhale/crates/tui/src/core/engine/tool_preparation.rs:26-159`

prepared call 可声明资源，再由 conflict-aware scheduler 决定并发：

- `refs/codewhale/crates/tui/src/core/engine/dispatch.rs:528-619`

machine-readable outcome 区分：

- `Succeeded`
- `Failed`
- `Denied`
- `InvalidArguments`
- `Cancelled`
- `TimedOut`

位置：`refs/codewhale/crates/tools/src/outcome.rs:10-110`。

kloop 当前用 `is_concurrency_safe` 做安全布尔分类，结构简单且已形成稳定主链：

- `rust/crates/core/src/tools/mod.rs:101-117,474-710`

可做一个窄原型：

```text
raw input
  → prepare/normalize
  → resource claims
  → hook mutation
  → re-prepare
  → permission gate
  → conflict-aware schedule
  → execute
  → structured outcome
```

重点不在增加抽象层，而在解决动态输入下的精确冲突：

- 同一路径的读/写；
- cwd/worktree/session 级独占资源；
- shell 是否声明未知资源；
- hook 修改参数后旧 permission/claim 失效。

### 4.5 Permission 与 hooks

CodeWhale execpolicy 的可借鉴点：

- typed `Allow/Ask/Deny`；
- matched rule/source/reason；
- workspace、command、path scope；
- Bash command arity matcher。

位置：

- `refs/codewhale/crates/execpolicy/src/lib.rs:75-123,423-745`
- `refs/codewhale/crates/execpolicy/src/bash_arity.rs:293-377`

例如 `git status` 的 flag 可以匹配，但同一宽规则不会误放 `git push`。

CodeWhale hook 可返回：

```text
decision
reason
updatedInput
additionalContext
```

位置：`refs/codewhale/crates/tui/src/hooks/executor.rs:283-365`。

若 hook 修改 input，CodeWhale 会重新 preparation。这个约束值得吸收。

但 kloop 当前 permission 主链更强，已经明确：deny、安全检查、mode、allow/cache/approver 的顺序与 bypass 免疫。不能用 CodeWhale execpolicy 替换：

- `rust/crates/core/src/permissions.rs:108-994`
- `rust/crates/core/src/hooks.rs:24-364`

可借鉴的最小数据形状：

```text
PolicyDecision {
  action: allow | ask | deny,
  source,
  matched_rule,
  reason,
  rememberable,
}
```

若未来支持 structured pre-tool hook，必须固定：

1. hook 不能绕过 deny 和安全检查；
2. `updatedInput` 后重新 parse/preparation/resource claim；
3. 用修改后的 input 重新经过完整 permission gate；
4. 审批 UI 展示最终实际执行参数。

### 4.6 Instruction/context 边界

CodeWhale 的 instruction discovery 会：

- 拒绝 symlink；
- 拒绝非普通文件；
- Unix 使用 `O_NOFOLLOW`；
- 对内容做 SHA-256 缓存。

位置：

- `refs/codewhale/crates/tui/src/project_context.rs:24-91,1269-1419`
- `refs/codewhale/crates/tui/src/project_context_cache.rs:15-124`

kloop 已有 instruction layering、import 和 project boundary：

- `rust/crates/cli/src/context.rs:19-250`

近期可补 leaf-file no-follow，不应改掉已有 canonical/project-boundary 语义。测试至少覆盖：

- instruction leaf symlink；
- import leaf symlink；
- FIFO/device/非普通文件；
- 检查后替换竞态；
- Unix `O_NOFOLLOW`；
- 非 Unix 明确 fail-closed 或平台等价实现。

### 4.7 Persistence、checkpoint 与 durability

CodeWhale session snapshot 使用 temp + fsync + rename，并有 async persistence actor：

- latest-wins；
- coalesce；
- `FlushAndReport`；
- shutdown flush；
- checkpoint；
- offline queued input；
- repair。

位置：

- `refs/codewhale/crates/tui/src/session_manager.rs:250-730`
- `refs/codewhale/crates/tui/src/tui/persistence_actor.rs:14-332`

kloop 的 append-only rollout 更适合作为 source of truth：

- `rust/crates/core/src/rollout.rs:34-478`
- `rust/crates/core/src/history.rs:75-182`

它已经有 lineage、fork、compaction marker 和 torn-tail repair，不应退化为单 JSON snapshot。

近期更合适的增强是 turn-boundary durability：

- `turn_terminal`；
- compaction marker；
- fork/lineage 边界；
- 明确 checkpoint 请求。

这些边界调用 `sync_data`，而不是对 token/delta 逐条 fsync。需要跨进程重启的组合测试，验证 durable boundary 前后的精确恢复语义。

async persistence worker、queued input 和 checkpoint 可后续单独规划，不能与 rollout source-of-truth 混为一谈。

### 4.8 Subagent

CodeWhale 的进程内 subagent 主路径较成熟，但实现集中在约 1.2 万行的单文件：

- `refs/codewhale/crates/tui/src/tools/subagent/mod.rs`

真实执行模型是同进程 Tokio background task，不是独立进程：

- `refs/codewhale/crates/tui/src/tools/subagent/mod.rs:5139-5162`

已有能力包括：

- parent/child tree；
- concurrent/admitted gate；
- status/peek/wait/cancel；
- background completion；
- mailbox；
- depth、step、token、wall-clock 等预算模型；
- terminal completion 的一次性仲裁。

可借鉴到 kloop：

- 稳定 parent task ID；
- 明确 queued/running/completed/failed/cancelled/timed-out；
- status、wait、cancel 分离；
- completion receipt 可回放；
- terminal state 单写者；
- parent cancel 和 detached child 的清晰边界。

kloop 当前位置：

- `rust/crates/core/src/tools/task.rs:39-460`
- `rust/crates/core/src/tools/background_tasks.rs:30-220`

CodeWhale 的成熟度边界必须保留：

1. per-worker token budget 主要在响应后检查；
2. 共享 scope token budget 主要是 spawn-time admission，并非严格预留的硬总账，并行 sibling 仍可能合计超额；
3. stale cleanup 是 status/spawn 等操作触发的机会式清理，没有独立 timer supervisor；
4. checkpoint receipt 已存在，但 Interrupted child 的原地恢复尚未实现。

因此不能把 CodeWhale 描述成已有严格分布式 budget ledger 或可恢复 worker supervisor。

### 4.9 Fleet

CodeWhale Fleet 真正成熟的是：

- Local 启动 `codewhale exec --auto --output-format stream-json` 外进程；
- SSH 启动本地 `ssh` 子进程和远端 CodeWhale；
- process-tree containment；
- durable ledger；
- generation fencing；
- orphan lease 恢复；
- NDJSON 日志与 terminal receipt。

位置：

- `refs/codewhale/crates/tui/src/fleet/executor.rs:178-250`
- `refs/codewhale/crates/tui/src/fleet/host.rs:570-640`

但 Docker backend 未接通：

- `refs/codewhale/crates/tui/src/fleet/executor.rs:565-569`

host/task concurrency、worker capacity、token/tool/time budget、environment requirement、alert delivery 等多个字段只解析、写 manifest 或出现在局部测试调度器，没有完整进入生产 worker 主循环。

结论：Fleet 不应作为 kloop 近期规划对象。等产品明确需要跨进程/跨主机调度时，只回看 ledger、generation fencing 和 process-tree cleanup。

### 4.10 MCP

CodeWhale 实际存在三套并列 MCP 面：

1. TUI 自建异步 MCP client/pool；
2. `refs/codewhale/crates/mcp` 同步 stdio 聚合器；
3. `mcp_server.rs` 将 CodeWhale 暴露为 MCP server。

主路径位置：

- `refs/codewhale/crates/tui/src/mcp.rs:1191-1215`
- `refs/codewhale/crates/tui/src/mcp/oauth.rs`
- `refs/codewhale/crates/mcp/src/lib.rs`
- `refs/codewhale/crates/tui/src/mcp_server.rs`

值得借鉴：

- tools/resources/prompts 分页发现；
- page/item/byte budgets；
- cursor loop 检测；
- stdio、Streamable HTTP、legacy SSE；
- OAuth；
- catalog refresh。

位置：`refs/codewhale/crates/tui/src/mcp.rs:2012-2286`。

两个重要警示：

1. stale-session 无条件重放 `tools/call`，对非幂等工具可能产生重复副作用：
   `refs/codewhale/crates/tui/src/mcp.rs:1474-1490,3555-3581`；
2. `refs/codewhale/crates/mcp` 的 `ToolFilter` 只过滤 `tools/list`，已知工具名仍可直接 `tools/call`，不能当作授权边界：
   `refs/codewhale/crates/mcp/src/lib.rs:322-373,466-474`。

kloop 应坚持：

- 单一 MCP 实现所有权；
- catalog filter 与执行授权分离；
- 非幂等 call 断线后默认不重放；
- retry 需要 request id、幂等声明或明确 caller opt-in；
- budgets 在 untrusted remote catalog 进入 prompt 前执行。

### 4.11 Skills

CodeWhale Skills 的可借鉴点：

- aliases；
- explicit-only invocation；
- locale；
- recursive discovery；
- conflict warning；
- catalog prompt budget；
- plugin trust。

位置：`refs/codewhale/crates/tui/src/skills/mod.rs:58-355,602-973,1091-1244`。

kloop Skills 基础已扎实，不应整体替换。后续可以独立评估：

- alias 的冲突和稳定解析；
- explicit-only 是否需要进入 tool visibility；
- catalog 的 item/byte/token budget；
- plugin 来源是否进入 permission/trust metadata。

### 4.12 Web、IDE 与其他产品表面

CodeWhale loopback Web bootstrap 做得稳健：

- nonce 换 HttpOnly/SameSite cookie；
- CSP；
- CORS/loopback 限制；
- `no-store`；
- `nosniff`。

位置：`refs/codewhale/crates/tui/src/runtime_api/web.rs:23-153`。

这适合未来只读本地 Web UI，但远程 runtime 文档明确没有 TLS 和多用户隔离，只适用于可信 LAN/VPN：

- `refs/codewhale/docs/RUNTIME_API.md:698-716`

VS Code extension 自称 scaffold，当前主要是 runtime attach、health/status 和 terminal launch：

- `refs/codewhale/extensions/vscode/package.json:1-4`
- `refs/codewhale/extensions/vscode/src/runtime.ts:41-150`

其他未完成或不一致表面：

- remote setup `--apply` 未实现：`refs/codewhale/crates/tui/src/remote_setup/mod.rs:1-6`；
- VM/CI lane runtime 为 stub：`refs/codewhale/crates/lane/src/runtime.rs:1039-1080`；
- npm SDK 暴露 Fleet create/events，但 Rust server 没有对应 route：
  `refs/codewhale/npm/runtime-sdk/index.js:31-97`、`refs/codewhale/crates/tui/src/runtime_api.rs:604-635`。

因此不能根据入口名称把 CodeWhale 评价成已经完成的 Web/IDE/remote/Fleet 全平台 agent。

### 4.13 测试与 CI

静态扫描约有：

- 9,894 个 Rust 测试入口；
- 约 317 个 JS/TS/Python cases。

较强覆盖包括：

- 真实 Engine；
- 文件系统；
- TCP/subprocess/PTY；
- Runtime API；
- MCP child process；
- Wiremock provider contract。

边界：

- provider 主要是 Wiremock，不是真 provider；
- Seatbelt/Bubblewrap 多为策略和命令构造测试，缺完整越界阻断 E2E；
- 常规 GitHub PR workspace test 主要跑 macOS/Windows，Linux 依赖其他门禁或 tag release；
- 多个 bridge、runtime SDK、Python tests 未进入统一常规 CI；
- ARM/Android/OHOS 主要是 build/check，不是目标设备执行测试。

CI 位置：

- `refs/codewhale/.github/workflows/ci.yml:311-373`
- `refs/codewhale/.github/workflows/release.yml:110-165`

可借鉴的是 Wiremock、PTY、TCP、subprocess contract test 的组合，不是测试数量本身。

## 五、与 kloop 的核心取舍

| 领域 | kloop 当前优势 | CodeWhale 可借鉴 | 不应复制 |
|---|---|---|---|
| Agent loop | core 单一所有权、三 wire adapter | stream guards、retry taxonomy | 巨型 `turn_loop.rs` |
| 协议 | canonical JSON-RPC item protocol | additive `seq`、replay cursor | 多套 HTTP/SSE/legacy 表面 |
| 历史 | append-only rollout、lineage/fork/compaction | boundary sync、async flush 思路 | 单 JSON snapshot 取代 rollout |
| 权限 | deny-first、安全检查、mode、approver 单主链 | typed decision、arity、workspace-exact scope | 替换现有权限/沙箱主链 |
| 工具 | 动态安全并发、ToolSource 清晰 | preparation、resource claim、structured outcome | 为抽象而拆多层 registry |
| Subagent | 递归复用同一 core，结构较小 | lifecycle/status/wait/cancel/budget | 1.2 万行 manager 或虚假硬预算 |
| MCP | 已有独立 crate 和 CLI 胶合 | catalog budgets、resources/prompts、transport recovery | 三套并列 MCP 栈、非幂等自动重放 |
| 产品面 | 原生 app 协议方向已拍板 | loopback Web bootstrap、runtime attach | 同时铺 Web/IDE/Fleet/remote/mobile |

## 六、候选优先级

### A：近期直接规划候选

#### A1. Provider stream guard

目标候选：

- header/open timeout；
- chunk idle timeout；
- wall-clock timeout；
- response content cap；
- 429/5xx/`Retry-After` taxonomy；
- 仅在没有可见输出时做安全 retry；
- partial output/partial tool JSON 不伪造 Done；
- 三 adapter terminal conformance tests。

这是当前首推项。它横切 provider/core，但边界清楚、用户价值直接，也不要求引入新产品表面。

#### A2. Context no-follow

目标候选：

- instruction、rules、import 拒绝 symlink/非普通文件；
- Unix `O_NOFOLLOW`；
- 保留现有 canonical/project-boundary checks；
- TOCTOU 与平台差异测试。

体量较小，安全收益明确，可独立成计划。

#### A3. Rollout turn-boundary durability

目标候选：

- terminal/compaction/fork 等边界 `sync_data`；
- token/delta 不逐条 fsync；
- crash/restart/torn-tail 组合回归；
- 写失败后的内存退化语义重新确认。

#### A4. Protocol additive `seq`

目标候选：

- thread 内单调 event `seq`；
- 跨 turn 不复位；
- restart 后继续分配；
- JSON-RPC response、notification、rollout runtime event 的所有权；
- replay/pending interaction 先留扩展点，不一次做完。

### B：近期窄原型候选

- tool preparation 与 resource claim；
- machine-readable `PolicyDecision`；
- Bash arity matcher与 workspace-exact grant；
- structured pre-tool hook，`updatedInput` 后重新 preparation 和 permission；
- `SubAgentManager` 的 parent ID、status/wait/cancel；
- subagent step/token/wall-clock budget；
- MCP page/item/byte/cursor-loop hard limits；
- MCP resources/list/read；
- Skills aliases、explicit-only、catalog budget；
- async persistence worker、checkpoint、queued input；
- provider capability metadata；
- durable event replay 和 pending-interaction snapshot。

B 类每项都应先做单一机制切片，不能打包成“平台化重构”。

### C：产品方向确定后再考虑

- stable constitution + volatile goal/memory/handoff；
- TUI tool detail pager/copy；
- workspace relevance file picker；
- loopback read-only Web UI；
- VS Code runtime attach/status；
- LSP read-only tools；
- dynamic MCP；
- goal continuation；
- Workflow JS；
- 大规模 PTY/browser acceptance。

### D：明确不建议

- 不把 rollout 改成单 JSON snapshot；
- 不复制巨型 `turn_loop.rs` 或 subagent 单文件；
- 不追逐 provider 名称数量；
- 不替换 kloop permission/sandbox 主链；
- 不复制完整 TUI view stack；
- 不把 terminal launcher 宣称完整 IDE；
- 不自动重放任意 MCP stale-session call；
- 不同时铺 Web、IDE、移动桥、Fleet 和远程 VM；
- 不引入 CodeWhale 多套 Runtime/legacy/app-server 协议表面。

## 七、下一会话应先讨论的规划问题

本文不替下一会话拍板。建议依次收敛：

1. 是否先独立规划 A1 provider stream guard；
2. timeout 是固定默认值、provider capability，还是用户配置；
3. 哪一个时刻算“已有可见输出”，从而禁止透明 retry；
4. clean EOF 缺完成标志时，三类 wire 各自应返回哪种错误；
5. timeout/cancel/protocol error 如何映射到 core `EndReason`、item terminal 和 server wire；
6. 本计划与 Plan 49–59 的 Claude Code tool parity 路线如何排序，避免并行改动同一主链；
7. A2–A4 是依次独立计划，还是只先记录挂账；
8. 哪些候选必须先回看 codex、Claude Code 当前官方行为和 claw-code，再进入设计。

建议首个实施计划保持窄范围：

```text
provider open/read guards
  + retry safety invariant
  + three-wire conformance tests
  + mock/real API verification
```

不要在同一计划顺带加入 provider catalog、event replay、subagent manager 或 Web runtime。

## 八、后续会话的回源清单

CodeWhale 快照可能更新，开工前应重新确认目标 commit。优先回看：

- streaming：
  `refs/codewhale/crates/tui/src/core/engine/turn_loop.rs`、
  `refs/codewhale/crates/tui/src/client/stream_entry.rs`、
  `refs/codewhale/crates/tui/src/client/chat.rs`、
  `refs/codewhale/crates/tui/src/client/responses.rs`、
  `refs/codewhale/crates/tui/src/client/anthropic.rs`；
- runtime：
  `refs/codewhale/crates/tui/src/runtime_api.rs`、
  `refs/codewhale/crates/tui/src/runtime_threads.rs`、
  `refs/codewhale/crates/protocol/src/runtime/mod.rs`；
- tools/permission/hooks：
  `refs/codewhale/crates/tools/src/lib.rs`、`refs/codewhale/crates/tools/src/outcome.rs`、
  `refs/codewhale/crates/execpolicy/src/lib.rs`、
  `refs/codewhale/crates/execpolicy/src/bash_arity.rs`、
  `refs/codewhale/crates/tui/src/core/engine/tool_preparation.rs`、
  `refs/codewhale/crates/tui/src/core/engine/dispatch.rs`、
  `refs/codewhale/crates/tui/src/hooks/executor.rs`；
- context/persistence：
  `refs/codewhale/crates/tui/src/project_context.rs`、
  `refs/codewhale/crates/tui/src/project_context_cache.rs`、
  `refs/codewhale/crates/tui/src/session_manager.rs`、
  `refs/codewhale/crates/tui/src/tui/persistence_actor.rs`；
- subagent/Fleet：
  `refs/codewhale/crates/tui/src/tools/subagent/mod.rs`、
  `refs/codewhale/crates/tui/src/fleet/executor.rs`、
  `refs/codewhale/crates/tui/src/fleet/host.rs`；
- MCP/Skills：
  `refs/codewhale/crates/tui/src/mcp.rs`、`refs/codewhale/crates/mcp/src/lib.rs`、
  `refs/codewhale/crates/tui/src/mcp_server.rs`、`refs/codewhale/crates/tui/src/skills/mod.rs`；
- product/test：
  `refs/codewhale/crates/tui/src/runtime_api/web.rs`、`refs/codewhale/extensions/vscode`、
  `refs/codewhale/npm/runtime-sdk`、`refs/codewhale/.github/workflows`。

kloop 对照入口：

- `rust/crates/core/src/agent.rs`
- `rust/crates/core/src/agent/sampling.rs`
- `rust/crates/provider/src/lib.rs`
- `rust/crates/server/src/wire.rs`
- `rust/crates/core/src/rollout.rs`
- `rust/crates/core/src/history.rs`
- `rust/crates/cli/src/context.rs`
- `rust/crates/core/src/tools/mod.rs`
- `rust/crates/core/src/permissions.rs`
- `rust/crates/core/src/hooks.rs`
- `rust/crates/core/src/tools/task.rs`
- `rust/crates/core/src/tools/background_tasks.rs`
- `rust/crates/core/src/skills.rs`
- `rust/crates/mcp/src/lib.rs`
- `rust/crates/cli/src/mcp.rs`

回源纪律：

- CodeWhale 只提供设计证据，不自动成为 kloop 契约；
- 当前 CodeWhale commit 以本文固定值为准，若更新必须记录新 commit 和差异；
- Claude Code 当前行为查精确二进制 fixture 或官方文档，不能用过时本地快照推断；
- codex、Claude Code、claw-code、CodeWhale 只读，不在其上开发或推送；
- 先固定 kloop 自己的不变量，再选择实现。

## 九、本轮完成记录 ✅

- 已完成 CodeWhale 固定提交的只读源码审计和 kloop 机制对照。
- 固定源码已从临时目录移入 `refs/codewhale`；该独立 Git 克隆由根 `.gitignore` 排除，
  不随 kloop 提交，也不得在其中开发或推送。
- 已区分生产主路径、兼容层、局部接线、stub/scaffold 和未完成能力。
- 已记录近期候选、窄原型、产品后置项与明确不建议项。
- 未构建或运行 CodeWhale，未修改任何产品行为，未执行真实 API 验收。
- 验证：`git -C refs/codewhale rev-parse HEAD`、remote/clean-worktree/`check-ignore` 校验、
  文档内 `refs/codewhale` 路径存在性检查、`cargo fmt --all --check`、
  `cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`、
  `cargo run -p kloop -- --mock`、`git diff --check` 全绿。
- 初始调研提交：`e2926e8`；参考副本落位与文档更新为本次提交（见 git log）。
- 下一步未拍板；新会话应从第七节开始讨论。
