# Plan 70 — Local Agent Mailbox 与 A2A 1.0 分层

> 状态：📋 待实施（本文件只规划，不含生产代码修改）
>
> 依赖：Plan 51、52、53、66、68、69

## 背景

当前 kloop 的 Subagent 通讯只有两段：父 Agent 启动时一次性下发 `prompt`，Subagent 结束时以前台 tool result 或后台 `InboxItem::SubAgentResult` 返回最终结果。每个 Subagent 有独立 Inbox，父会话的用户 steering 不会泄漏给子 Agent；但父与运行中子 Agent、兄弟 Subagent 之间没有消息通道。

当前 Claude Code 的普通后台 Subagent 已可通过 `SendMessage` 与 `main` 双向通讯，也可由运行时路由到其他可达 Agent；完成通知仍是独立 lifecycle event，不依赖 `TaskOutput` 轮询。kloop 值得补齐同类能力。

同时，Linux Foundation A2A 已发布 Agent2Agent Protocol 1.0.0。A2A 面向通过 Agent Card 发现的独立远程 Agent endpoint，定义 `SendMessage` / `SendStreamingMessage`、Message/Part、Task、Artifact、streaming、push notification 和认证。A2A `Message` 没有任意本地 `to` 字段：目标由客户端选择的 Agent endpoint 决定；Task ID 由服务端创建，终态 Task 不能继续执行。

因此不能把 `agent-N` 进程内路由直接宣称为 A2A，也不能为每个临时 Subagent 伪造 HTTP endpoint/Agent Card。本计划采用**两层设计**：

1. 落地 session-scoped Local Agent Mailbox，解决 `main ↔ agent-N` 与同 session `agent-N ↔ agent-N`。
2. 将内部 envelope 与 A2A 1.0 的 Message/Part/Context 概念对齐，并冻结清晰 adapter seam；外部 Agent Card、HTTP/JSON-RPC/gRPC、认证、SSE/push 由后续独立 A2A gateway 计划实现。

## A2A 1.0 裁决

| 维度 | A2A 1.0 | Plan 70 Local Mailbox |
|---|---|---|
| 对端 | Agent Card 发现的远程 Agent service endpoint | 同一 kloop session 内的 `main` 或 live `agent-N` |
| 寻址 | endpoint/interface；Message 内无 recipient | tool envelope 的 `to`，仅接受 `main` / 可达 `agent-N` |
| 发送者 | endpoint 与 Message `role` 表达；无本地 sender 字段 | `from` 由当前 `ToolCtx`/Config 注入，模型不能提交 |
| 内容 | Message + one-or-more Part；text/raw/url/data | 第一版只开放一段 bounded text；内部保存为 Part-like text body |
| 上下文 | `contextId` 关联多次交互 | 绑定 session-local context，不对外暴露 session path/credential |
| 长任务 | server-created Task lifecycle | Agent lifecycle 已由 `agent-N`/BackgroundExecutions 管理，不伪造 A2A Task |
| 结果 | Message 或 Task/Artifact | 中间 mailbox message；最终 Subagent result 继续走现有 completion path |
| 异步 | SSE streaming、task subscription、push webhook | Inbox activity + round-boundary delivery；无网络 transport |
| 认证 | Agent Card 声明 security scheme，endpoint 鉴权 | 同 session capability/ownership 校验；无跨 session 路由 |

### 结论

- 模型工具可以叫 `send_message`，但它是 **A2A-aligned local adapter**，不是 A2A wire operation。
- `to` 属于本地 transport envelope，不塞入 A2A Message metadata 冒充标准字段。
- `agent-N` 是 session-scoped execution address，不是 Agent Card URL，也不进入未来对外 Agent Card。
- 第一版不引入官方 `a2a-rs` 生产依赖；没有外部 transport 时只为内部消息拉入 client/server/gRPC/SSE 依赖会扩大编译面且制造虚假兼容。后续 A2A gateway 开工时再固定 `a2a-lf` / `a2a-client-lf` / `a2a-server-lf` 的确切版本、MSRV、协议 fixtures 和安全面。
- 未来 gateway 必须使用 A2A 1.0 Message/Task/Artifact 原生类型和官方 Rust SDK或经规范验证的等价 binding，不能把本计划的 local JSON schema直接暴露为 A2A。

## 已拍板契约

### 1. 模型工具

新增一个统一工具：

```json
{
  "name": "send_message",
  "input": {
    "to": "agent-4",
    "summary": "请复核取消竞态",
    "message": "我在 lifecycle.rs 的清理分支发现疑似竞态，请独立验证触发条件。"
  }
}
```

Schema：

```json
{
  "type": "object",
  "properties": {
    "to": {
      "type": "string",
      "description": "main or an accessible live agent-N"
    },
    "summary": {
      "type": "string",
      "description": "Optional short UI preview"
    },
    "message": {
      "type": "string",
      "description": "Text delivered at the recipient's next safe round boundary"
    }
  },
  "required": ["to", "message"],
  "additionalProperties": false
}
```

- `summary` 可选；缺省时从 message 第一行生成 bounded preview。它只用于 UI，不改变语义 body。
- 不接受 `from`、`message_id`、`context_id`、`task_id`、`status`；这些均由 runtime 管理。
- 不增加 `send_parent_message`、`send_agent_message`、PascalCase `SendMessage` alias，也不复用 `task_*`。
- 新增 read-only `list_agents {}`，返回当前 sender 可达的 live Agent：`id`、`parent_id`、`agent_type?`、bounded description、state。它是本地 ephemeral roster，不是 A2A Agent Card discovery。

### 2. 地址、身份与作用域

- 引入 typed `LocalAgentId = Main | Agent(agent-N)`；禁止在核心路由逻辑里用空字符串代表 main。
- 每个 Config/ToolCtx 冻结当前 `agent_id`、`parent_agent_id`、session routing scope；sender 永远由 runtime-derived current agent identity 生成。
- session root 注册为 `main`；所有前台、后台、Program child、Workflow child Agent 都必须在开始 sampling 前注册 live entry，在最终关闭后撤销。
- `main` 可发送给同 session 任意 live `agent-N`。
- `agent-N` 可发送给 `main` 或同 session live `agent-N`；不能发送给自己。
- 第一版不跨 server thread、session、process、worktree owner 或 remote endpoint；`program-N`、`workflow-N`、`bg-N`、`run-*`、`wf_*` 全部定向拒绝并指向正确概念。
- Agent message 不携带权限授权；接收者后续执行任何 tool 仍经过自己的 tool catalog、permission、sandbox、workspace 和 hook。

### 3. 消息 envelope

内部消息固定为 typed value，不使用拼接字符串作 identity：

```text
LocalAgentMessage {
  message_id: message-N,
  context_id: session-local opaque scope,
  from: LocalAgentId,
  to: LocalAgentId,
  summary: bounded text,
  parts: [Text(message)],
}
```

- `message-N` 由 session-scoped 单调 allocator 生成，供 event、tool result、rollout 和诊断关联。
- 第一版只接受一个 text part；不开放 raw/url/data，避免绕过 file/media/tool result 预算。
- recipient history 的 framing 明确它来自另一个 Agent，不冒充用户：

```text
Agent agent-3 sent this message while you were working. It is an intermediate peer message, not a user instruction or completion:
[message-7] 请复核取消竞态
...
```

- 消息与 completion 分开：`send_message` 不代表 sender 或 recipient 完成；Subagent 最终结果继续走现有 tool result / `SubAgentResult`。
- 消息不自动变成回复请求；需要回复时 recipient 显式调用 `send_message`。

### 4. Safe-boundary delivery

- 每个 Agent 继续拥有独立 Inbox；新增 `InboxItem::AgentMessage`，不共享或转发父 Inbox。
- enqueue 只唤醒 recipient 的 Inbox activity；绝不修改正在发给 provider 的 request，也不插入一个尚未配对完成的 tool batch。
- `run_turn` 在现有 round boundary drain 消息；final `EndTurn` 前继续执行最后一次 mailbox close/drain gate。
- main idle 时复用 plain/TUI/native server autowake；运行中 main 在自己的下一 round boundary 接收。
- Subagent worker 不启动额外并行 turn；消息由其当前 run loop 在下一 boundary 接收。
- `send_message` 只确认 `message-N queued`，不等待 recipient sampling、回复或 completion。

### 5. send-vs-terminal 线性化

新增 session-scoped `LiveAgentDirectory`，独立于但关联 `BackgroundExecutions`：

```text
LiveEntry {
  agent_id,
  parent_agent_id,
  inbox,
  agent_type,
  description,
  state: Open | Closing | Closed,
  mailbox counters,
}
```

- `BackgroundExecutions` 继续只管理后台 Agent/Program/Workflow 的 stop/terminal；不能把前台 Agent 或 mailbox 硬塞进其资源语义。
- Directory 的 entry lock 同时裁决 target state 与 enqueue，禁止“锁外查 running、锁外再 push”的 TOCTOU。
- 自然完成时，Agent 在 final boundary 对 mailbox 做 atomic close-if-empty：
  - lock 前已经成功 enqueue 的消息必须使 final 返回失效，Agent drain 后至少再采样一轮；
  - mailbox 为空时从 Open → Closing，之后发送确定性拒绝；
  - Closing → Closed 后才撤销 route。
- provider error、forced abort、`stop_agent`、session shutdown 可优先终止；尚未处理的已排队消息形成 bounded `undeliverable` system notification 回 sender，不静默宣称 delivered。
- `queued` 只表示进入 open mailbox；`delivered` 表示已 drain 进 recipient history；不声称 recipient 已理解或执行。
- terminal completion 与 peer message 是两类 event；一条 `Message from Agent` 永远不能被 UI 当终态。

### 6. 前台 Agent 的限制

- 前台和后台 Agent都注册，以便 sibling 能向任一 live Agent发送消息。
- 但 main 在同步 `run_agent` tool 内正等待 JoinHandle，主模型此时没有下一轮可调用 `send_message`；Local Mailbox 不能突破这一因果限制。
- 需要父 Agent中途追加指令时，应使用 `background:true`；tool description 和 README 明确这一点。
- 用户发给 main 的 steering 仍只进入 main Inbox，不自动广播给任何 child；main 必须在恢复后显式 `send_message`。

### 7. 发现与 sibling 通讯

- `list_agents {}` 只列当前 session、当前 sender 可达且尚未 Closing 的 live Agent，不泄漏其他 thread/session。
- parent/agent type/description 是展示与协调元数据，不是授权凭据。
- 并发启动存在真实注册时序：列表是 snapshot，不保证未来 sibling 已出现；发送到尚未注册或已 Closing 的 ID 必须失败，不排队等待“同名未来 Agent”。
- parent 可在收到后台启动回执后把 sibling ID 发给各 Agent；Workflow/Program child 也可通过 `list_agents` 发现当时已注册 peers。
- 第一版不支持按 description、agent type 或自由名称模糊路由，避免多个 Explore/同名任务误投。

### 8. 资源预算与防消息环

硬限制以 UTF-8 bytes 计：

- 单条 `message`：8 KiB。
- 单条 `summary`：200 Unicode scalar values，且编码后不超过 1 KiB。
- 单 mailbox pending：32 条且总 body 不超过 128 KiB。
- 单次 boundary 最多注入 8 条且总 body 不超过 32 KiB，其余保留 FIFO 顺序等待下一 boundary。
- 每个 Agent 最多发送 64 条；每个 session 最多接受 256 条且总 body 不超过 512 KiB。
- 达限 fail closed，返回 typed/bounded 错误；不截断 body、不静默丢弃、不 offload 后继续让 Agent 互相轰炸。
- completion、undeliverable 系统通知不计作模型发送额度，但自身严格 bounded、按 message ID 聚合。
- 总量计数 session-scoped、不因 recipient drain 清零，防止两个 Agent 无限 ping-pong。

## 实施

### Slice 1：身份与 LiveAgentDirectory

修改代表文件：

- `kloop/crates/core/src/config.rs`
- `kloop/crates/core/src/tools/mod.rs`
- `kloop/crates/core/src/tools/subagent.rs`
- `kloop/crates/core/src/tools/background_executions.rs`
- 新增 `kloop/crates/core/src/agent_mailbox.rs`

内容：

1. 增加 `LocalAgentId`、session-scoped message allocator、`LiveAgentDirectory` 和明确的 parent identity。
2. session root 在 bind/start 时注册 `main`；subagent clone 继承同一 directory，但继续创建自己的 Inbox、todo、file state 和 worktree state。
3. 前台/后台 `run_agent`、Program bridge child、Workflow child 在 sampling 前注册，在自然/失败/取消/cleanup 后由 RAII guard 闭合并撤销。
4. Background Agent 的 typed stop 仍由 `BackgroundExecutions` 控制；Directory 只负责 live routing/mailbox，不增加通用 stop。
5. session shutdown 先关闭所有 mailbox、拒绝新 send，再沿用既有 cooperative cancel/abort/registry teardown。

### Slice 2：send_message、list_agents 与 mailbox boundary

修改：

- `kloop/crates/core/src/inbox.rs`
- `kloop/crates/core/src/agent.rs`
- `kloop/crates/core/src/tools/mod.rs`
- `kloop/crates/core/src/tools/codemode.rs`
- `kloop/crates/core/src/permissions.rs`

内容：

1. 新增 strict parser 和 model ToolDef；`send_message` / `list_agents` 在 main 与 child Agent tool catalog 可用。
2. Program JavaScript catalog、Workflow JavaScript runtime、MCP/external source 均不能伪造调用；只有真实 Agent ToolCtx 有 sender capability。Program/Workflow 内部启动的 child Agent 本身可用。
3. 增加 `InboxItem::AgentMessage`、bounded FIFO drain、message/event framing。
4. 将 final boundary 改为 atomic close-if-empty gate，锁定 accepted-before-close 不被自然 completion 静默跳过。
5. 权限分类仅认可同 session local message；不触发 OS permission prompt，但仍经过 tool lifecycle/hook/audit。

### Slice 3：事件、rollout 与三种前端

修改：

- `kloop/crates/core/src/event.rs`
- `kloop/crates/cli/src/ui.rs`
- `kloop/crates/cli/src/main.rs`
- `kloop/crates/tui/src/app.rs`
- `kloop/crates/tui/src/render.rs`
- `kloop/crates/server/src/wire.rs`
- `kloop/crates/server/src/lib.rs`

新增 typed lifecycle：

```text
AgentMessageUpdated {
  id,
  from,
  to,
  summary,
  status: Queued | Delivered | Undeliverable,
}
```

- event 不携带完整 body，避免 UI/event log复制语义内容；body 只进入 recipient history/rollout。
- TUI 显示 `Message from Agent · agent-3`，同时显示 `message-N`；多个同类型 Agent 不混淆。
- plain/headless 输出 bounded 单行来源与状态。
- native server 映射为 thread-scoped `thread/agentMessage/updated`，无伪造 turn owner；包含 `messageId/from/to/summary/status`，不包含 provider history 或正文。
- recipient rollout 持久化带 message ID 的 framed history；sender rollout 已有 send_message tool use/result。session resume 不恢复已终止 Agent 或未投递 mailbox。
- 与 Plan 69 的 BackgroundTask row 保持独立：message status 不更新 Agent terminal card。

### Slice 4：A2A adapter seam 与文档

- 在 `kloop/crates/protocol` 定义 transport-neutral Local Agent envelope，不复用 provider chat `Message`，并为未来 A2A adapter 保留显式转换边界。
- 加入 A2A 1.0 contract fixtures，证明：
  - local `to` 是 transport address，不被序列化进标准 A2A Message；
  - local text body 可无损映射为 A2A text Part；
  - local `message-N` 与 session context 可映射到 creator message ID/context ID，但不映射成 A2A Task ID；
  - `agent-N` 不进入 Agent Card endpoint、skill 或 security metadata。
- 不在本计划注册 `/.well-known/agent-card.json`，不监听 A2A HTTP，不实现 JSON-RPC/gRPC/SSE/push，不加载远程 Agent Card。
- README 明确“Local Agent Mailbox（A2A-aligned，不是 A2A endpoint）”；capability report 不宣称 A2A compatible。
- 在 `docs/plan/HANDOFF.md` 记录：进程内地址、远程 service discovery、消息内容和 Task lifecycle 是四个不同层次。

### Slice 5：真实双 provider 验收

新增默认 ignored 的 native-server evaluator，真实 Anthropic 与 OpenAI Chat 各验证：

1. main 后台启动两个 Agent，拿到不同 `agent-N`。
2. main 向 Agent A 追加指令。
3. Agent A `list_agents` 后向 Agent B 发消息。
4. Agent B 向 `main` 返回一条中间消息，然后继续工作。
5. UI/native event 明确区分 queued/delivered message 与两个 Agent 的唯一 terminal。
6. final result 仍各自通过现有 completion delivery；不使用 `TaskOutput` 或 status polling。
7. evaluator 以 tool lifecycle、message ID、recipient rollout、event 顺序和唯一 terminal/delivery 判定，不以模型自然语言自评。

## 后续独立计划：外部 A2A Gateway

Plan 70 完成后，外部 A2A 应另开计划，至少包括：

1. 固定 A2A 1.0.0 与官方 `a2a-rs` crates 版本、MSRV、license/SBOM、三平台 build 和协议 conformance fixtures。
2. 发布 kloop service-level Agent Card；Agent Card 描述稳定 service/skills，不暴露 ephemeral `agent-N`。
3. A2A `SendMessage` 创建或继续 top-level kloop context；非终态 Task 使用 server-created A2A task ID，不复用 `agent-N`、`program-N`、`workflow-N`。
4. A2A `input-required` 映射为可恢复的外部 task turn，不映射成本地 Subagent mailbox。
5. Artifact 映射到有界、授权可读的结果资源；不能直接暴露本地任意 output path。
6. JSON-RPC/HTTP+JSON 或 gRPC binding、SSE subscribe、push webhook、SSRF、redirect、认证、tenant、rate limit、replay/idempotency 和 task retention 单独威胁建模。
7. 外部 A2A client 若作为模型工具，使用 Agent Card endpoint/skill discovery，不接受 `agent-N` recipient。

只有这一后续 gateway 通过官方 conformance 和真实互操作后，文档才可以宣称 A2A 1.0 support。

## 非目标

- 不实现 Agent Teams、共享 Task V2 图、owner/dependency、team lead 或任意 peer hierarchy。
- 不让 Subagent spawn Subagent；Plan 68 的 depth boundary 不变。
- 不让 Program/Workflow JavaScript runtime 伪装成 Agent recipient。
- 不让用户 steering 自动广播给 child。
- 不支持发送给已完成 Agent并自动 resume；terminal 后必须拒绝。恢复旧 context 另行设计。
- 不提供 unread/read receipt、同步 request-response RPC、消息撤回、优先级或广播。
- 不传输 hidden reasoning、未完成 tool call、权限 token、credential 或任意 workspace capability。
- 不实现外部 A2A transport，也不宣称 Local Mailbox 等同 A2A。
- 不修改 Plan 67 PDF 工作；不把 Plan 69 的结构化后台 UI 扩成通用 Task registry。

## 验证矩阵

| 场景 | 必须证明 |
|---|---|
| strict schema | `to`/`message` 必需，summary 可选；unknown/type/empty/oversized/control-invalid 在任何 enqueue 前拒绝。 |
| sender identity | main/child sender 来自 runtime；输入无法伪造 from/message ID/context/session。 |
| scope | main↔child、sibling 成功；self、未知、future、Closing、跨 session/thread、Program/Workflow/Shell/durable ID 全部定向拒绝。 |
| roster | 只列同 session live Open Agent；快照不把 description/type 当授权，不泄漏其他 server thread。 |
| inbox isolation | Agent message只进入目标 Inbox；父 steering、其他 child message、completion 不串线。 |
| round boundary | provider request/tool batch 中途不插入；下一安全 boundary 按 FIFO/批预算投递。 |
| natural terminal race | enqueue-before-close 必被 drain 并触发后续采样；close-before-enqueue 确定拒绝；无 check/push TOCTOU。 |
| cancel/failure race | forced terminal 不死锁；pending message 变 Undeliverable 并 bounded 通知 sender；不伪报 Delivered。 |
| foreground limit | foreground child 可收 sibling message；main 因同步 tool 阻塞时不会虚构可中途发送，文案建议 background。 |
| budgets | 单条、summary、mailbox、boundary、per-agent、per-session 六层限制精确；drain 不重置 session anti-loop 计数。 |
| event/UI | message queued/delivered/undeliverable 与 Agent running/terminal 分离；来源同时有类型、agent ID、message ID。 |
| native wire | thread-scoped event 无正文/credential/伪 turn ID；并行 thread 不交叉。 |
| rollout/resume | sender tool result与 recipient framed message 可审计；重启不复活 live route、不重复投递旧 mailbox。 |
| Program/Workflow | JS runtime不能直接 send；其 child Agent可注册、list、send，且 journal/source/resume 语义不变。 |
| A2A boundary | fixtures 证明 endpoint addressing、Message/Part、Task ID 与 local IDs 不混用；无虚假 Agent Card/A2A support 声明。 |
| 回归 | Plan 52/53/59、Plan 66 typed stop、Plan 68 topology/source/offload、Plan 69 lifecycle UI 均不回归。 |

全量执行：

```bash
cd kloop
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cargo run -p kloop -- --mock --headless --json "exercise local agent messaging"
python3 -B ../refs/claude-code-2.1.220/verify.py
python3 -B ../refs/claude-code-2.1.220/verify.py --corpus-only
git diff --check
```

真实凭据、endpoint、raw provider response、完整 mailbox body 和 transcript 只在 gitignored 临时环境中使用，不进入输出或提交。
