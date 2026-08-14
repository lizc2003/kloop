# Plan 88 — CodeWhale 借鉴：child route identity/provenance

> 状态：✅ 已完成（2026-08-15；提交 SHA 以本文件所在提交为准）
>
> 依赖：Plan 17、Plan 18、Plan 68、Plan 70、Plan 72、Plan 73、Plan 77、Plan 87；参考快照 `refs/codewhale` 当前观察 HEAD `5e3ac84c5cb925b4c90c34dfe582b75f04605cb1`，固定历史审计基线 `b494236312ef3ac36489c83706a0b11ab73935a1`

## Context

kloop 已有 Agent、Program、Workflow、local mailbox、Task graph、rollout lineage 和 background execution，但这些 seam 各自拥有不同的 identity。CodeWhale 的 child route receipt 借鉴价值在于：admission 前固定 requested/effective route 与执行来源，并让 completion、status、resume 和结果交付只透传这份事实。kloop 不应把 `agent-N`、`program-N`、`workflow-N`、`run-*`、`wf_*`、message id、LocalContextId 和 rollout lineage 强行压成一个 ID；需要的是显式、typed、可审计的关联。

## 契约与范围

### 本次吸收

1. 明确区分 session/thread、LocalAgent mailbox identity、execution resource identity、durable run/workflow identity、rollout lineage、worktree identity 和 root Task identity。
2. 为 child admission/后台执行建立 bounded route/provenance receipt，至少表达 parent route、child route、execution kind、authority/workspace/worktree、transient resource ID、durable run ID（如有）和 terminal owner。
3. `run_agent`、structured child、skill fork、Program、Workflow、background execution 的 register/lease/close 语义可审计；同一 execution 只有一个 terminal commit 和最终交付。
4. Program/Workflow 若不是 mailbox peer，显式表示无 LocalAgent mailbox identity，不伪造 `agent-N`；mailbox delivery、native display event 和 durable journal 是独立生命周期。
5. status/completion/resume/fork 只透传已固定 provenance，不从 display text、tool name 或 run id 后缀反推 parent。

## 关键文件

- `kloop/crates/protocol/src/lib.rs`
- `kloop/crates/core/src/config.rs`
- `kloop/crates/core/src/agent_mailbox.rs`
- `kloop/crates/core/src/tools/subagent.rs`
- `kloop/crates/core/src/tools/codemode.rs`
- `kloop/crates/core/src/tools/workflow.rs`
- `kloop/crates/core/src/tools/background_executions.rs`
- `kloop/crates/core/src/inbox.rs`
- `kloop/crates/core/src/rollout.rs`
- `kloop/crates/core/src/event.rs`
- `kloop/crates/server/src/events.rs`
- `kloop/crates/server/src/wire.rs`
- CLI/TUI/headless projection tests

## 非目标

- 不合并现有 ID 命名，不合并 BackgroundExecutions 与 BackgroundShells，不新增第二套 registry。
- 不开放 child 修改 root-owned Task graph，不实现跨 session/跨进程 route、远程 A2A、Team/claim/assignment。
- 不让 Program/Workflow 自动获得 mailbox peer 能力，不做 child usage/cost 汇总，不改变 public protocol shape。
- 不把 provenance receipt 当 permission grant、provider receipt 或 execution checkpoint。

## 为什么不能与其他计划合并

provenance 回答“是谁、从哪里、属于哪个执行树”；Plan 87 回答“当前是否获准调用工具”；Plan 89 回答“provider 是否完整终止”；Plan 90 回答“MCP 是否可用”。四者不能共享一个含糊的 route/status 类型。

## 实施与测试方向

覆盖 parent/child agent、Program、Workflow、background shell、mailbox result、late delivery、stop/cancel、resume、fork、worktree switch、duplicate terminal 和 malformed/missing provenance。断言 journal、rollout、inbox、native projection 使用各自 canonical identity，且不互相冒充；receipt 有界、无 secret、不会进入 provider prompt。

## 完成标准

所有 child/background execution 都能在内部 receipt 中追溯 parent/child route 与 execution kind；mailbox、terminal、journal、native event 生命周期保持分离；resume/fork/worktree/Task ownership 不回归；不新增远程协议、second registry、public wire 字段或 child billing。

## 实施结果（2026-08-15）

- 新增 core-private `ExecutionProvenanceReceipt` v1 与唯一 mint/持久化校验 seam。session/thread 使用不同 domain 的 opaque SHA-256 ref；Agent/Program/Workflow/Shell transient ID 与 Program/Workflow durable ID 分型；parent 是 flat `ExecutionRef`；rollout、workspace disposition、admission authority、terminal/delivery owner 显式保存。非法 kind/mailbox/durable/origin/terminal 组合 fail closed，单份序列化 receipt 硬限 2 KiB。
- `ToolCtx.enclosing_execution` 将真实 execution parent 传播进 admitted child；Agent 的 mailbox parent 仍是实际 local caller。普通/structured/Skill fork、foreground/background Agent 共用 admission seam，Program/Workflow bridge 分别覆盖为 container receipt ref；隔离 worktree 在 receipt mint 前冻结最终 workspace identity。
- `BackgroundExecutions` 保留 Agent/Program/Workflow 现有 registry，并让 Entry 与 typed registration handle 持有同一 receipt；attach/finish 以 receipt identity fence，stop 命中后不再从字符串重建 provenance。`BackgroundShells` 继续独立，保存 Shell receipt/handle 并沿用 file-backed output 与 terminal arbitration。
- 每次 Program（foreground/background/resume）和 Workflow（launch/resume）attempt 都分配 fresh transient ID，并与 durable `run-*`/`wf_*` 关联；持有 run lease 后从有效 sidecar 跳过已用 sequence，因此跨进程 resume 不复用该 run 的旧 attempt ID。RunDir 的受限读取支撑 private `provenance.json` v1：最多 32 attempts、单 receipt 2 KiB、整文件 128 KiB；valid history append-preserve，missing 从当前 attempt 开始，malformed/unknown/oversized/mismatched/cap/IO failure 不覆盖原文件也不阻断执行。background attempt 只在 registry admission 成功后记录，拒绝 launch 不制造审计历史。
- Program/Workflow journal 新写格式直接升级为 v3。成功 live `agent()` 同时保存 validated child receipt；只有 v3 参与 replay，v1/v2/future safe miss且无迁移/双写。topology + complete input 仍是唯一 cache key；v3 receipt missing/malformed/oversized、非 Agent、或 parent durable run 不匹配时只使 historical evidence unavailable，完整 result 仍可 hit，坏 evidence 不再重写，cache hit 不伪装成 fresh admission。
- receipt 不含 Task ID，不进入 `LiveAgentDirectory`、mailbox message、provider prompt/history、`Event`、Inbox framing、rollout schema、server wire、CLI/TUI/headless projection、Plan 87 capability receipt或 billing/usage ledger；Program/Workflow/Shell 明确不能声明 local mailbox peer，execution/run/rollout/workspace/worktree/Task identity 不互相 parse 或替代。

## 验证记录（2026-08-15）

- `cargo test -p kloop-core`：706 passed；journal v3 focused 8、Code mode 34、Workflow 13 tests 通过；
- `cargo test -p kloop-server`：24 unit + 32 integration tests 通过；`cargo test -p kloop` 通过；
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：通过；
- `cargo test --workspace --all-targets --all-features`：通过（2 项真实 provider credential tests ignored）；
- `cargo fmt --all -- --check`、`git diff --check`：通过；
- `cargo run --locked -p kloop -- --mock`、`--mock --headless`、`--mock --headless --json`：通过；
- `.kloop/env.local` 的真实 Anthropic `claude-sonnet-4-6`：`real_agent_program_workflow_contract` 54.81s 通过，覆盖 foreground Agent、Program resume journal hit、background Agent/Program、Workflow、唯一 terminal/delivery；未记录或提交 key、endpoint、raw response 或 transcript；
- 未执行 Linux sandbox/CI、Windows、物理终端与 Desktop E2E。
