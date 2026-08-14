# Plan 86 — CodeWhale 借鉴：cache-stable agent compaction seam

> 状态：实施完成，验证记录见文末
>
> 基线：`3cca84a`（Plan 84）
>
> 依赖：Plan 68、Plan 77、Plan 81、Plan 85；参考快照 `refs/codewhale` 当前观察 HEAD `5e3ac84c5cb925b4c90c34dfe582b75f04605cb1`，固定历史审计基线 `b494236312ef3ac36489c83706a0b11ab73935a1`

## Context

CodeWhale 最近的 compaction 变化表明，长期 agent 的关键不只是“达到窗口后调用摘要模型”，而是把 context pressure、summary replacement、工具配对、失败语义和账务事实收敛到一个可验证的 lifecycle seam。相关借鉴包括稳定的 summary request、replacement checkpoint、bounded recent tail、旧 summary replacement、volatile state 不重复注入，以及 pressure 与 billing 分离。

kloop 已有 `crates/core/src/compact.rs` 的 predictive/reactive 压缩、`History::replace_all` 的唯一 sanctioned rewrite、Plan 68 的大结果 offload、Plan 77 的 public projection/replay 分层，以及 Plan 81 的 durable provider usage ledger；但当前 predictive、reactive、manual 三个入口只共享 `run_compaction` 的执行函数，summary replacement、NoOp、stable prompt prefix 和统一 lifecycle 语义仍未明确。

本计划把现有能力收敛成一个内部 compaction seam，不复制 provider server-side compaction，也不改变 kloop 的 provider message wire、public protocol 或 usage ledger 所有权。Plan 85 继续只处理 session snapshot/recovery；本计划不把 recovery marker 与 compaction marker 合并。

## 契约与范围

### 本次吸收

1. **统一执行 seam**：manual `/compact`、predictive compaction 和 reactive overflow recovery 都调用同一个 core 内部 `compact_once`（名称按现有风格确定）。入口只选择触发来源、实际 model 和错误策略，不重复采样、构造 replacement 或记录 usage。
2. **纯规划与 canonical replacement**：从现有 `keep_from_index` 拆出纯 helper，输入当前 `Message` slice，输出安全的 summary boundary/NoOp 原因；另以纯 helper 根据 provider summary 构造 canonical replacement。继续保持 recent tail 有界，并且绝不把 assistant `ToolUse` 与对应 user `ToolResult` 拆开。大 ToolResult 继续复用 History 的 offload preview/pointer。
3. **replacement 而非 summary stacking**：accepted replacement 固定为一条 `[SUMMARY_PREFIX + canonical_summary]` user message 加完整 recent tail。summary 两侧 trim；若模型重复返回 `SUMMARY_PREFIX`，先剥掉已有前缀，再写入恰好一层。已经存在 summary 但之后出现了新的可折叠消息时允许再次压缩；只有待摘要区域没有新的可折叠内容，或 replacement 与当前 effective messages 完全相同，才返回 NoOp，不调用 provider、不记 usage、不写 compacted marker。
4. **明确 outcome/receipt**：内部结果至少区分 `Applied`、`NoOp` 和 error。可增加 `CompactionTrigger { Manual, Predictive, Reactive }` 与 bounded `CompactionReceipt { summarized, kept, model, trigger }`，但均为 `pub(crate)`/core 内部事实，不写 rollout、不进入 native protocol、public event、snapshot、provider message 或 billing receipt。NoOp 原因也保持内部，manual 可以转换为现有 CLI 文案。
5. **稳定摘要请求**：`COMPACT_SYSTEM` 与 `COMPACT_INSTRUCTION` 保持稳定字节，不加入日期、cwd、session id、trigger、token estimate、task graph、ledger 或 provider-specific metadata。摘要请求继续使用固定 system、无 tools，history 与 handoff instruction 属于 volatile transcript suffix；不实现 Anthropic/Responses 的 server-side compaction block 或 provider-specific replay。
6. **压力与账务分离**：predictive admission 继续只使用 `History::estimated_tokens()`、当前 injected context estimate、`max_turn_growth` 与 configured context window；reactive 只响应 provider overflow。`Usage::total()` 仍只用于 context anchor，durable ledger 不参与 threshold/admission，child usage 不汇总到 parent。accepted terminal `Some(Usage)` 才经 `History::record_provider_usage` 记录一次，`None` 保持 absence；compaction/clear 重置 context anchor 但不清 ledger。
7. **失败不改历史**：empty/invalid summary、非 `EndTurn`、cancel、transport/protocol error、provider incomplete 和 replacement 无效均不修改 messages、ledger 或 rollout。accepted compaction 维持现有写入顺序：provider usage（若有）先于 `History::replace_all` 的 compacted marker；不新增第二种 rewrite、sidecar 或 dedupe registry。
8. **入口错误语义**：predictive compaction 失败或 NoOp 后继续原 sampling；reactive compaction 成功后只 retry 一次，NoOp/失败按当前 turn error 语义结束且不循环；manual 返回 applied/no-op/error 的 bounded 文案。fallback 已切换时摘要必须使用传入的实际 active model，而非重新读取 `cfg.model`。

### 所有权与生命周期

- `History::replace_all` 继续是唯一 sanctioned history rewrite；`rollout.rs` 的 `compacted` replay 语义、append-only 文件和现有 provider usage 顺序不变。
- `History::estimated_tokens` 的 anchor reset 与 Plan 81 的 provider usage ledger 保持两套语义；不要把 chars/4 estimate、provider `Usage::total()` 和账单/价格 total 混合。
- compaction 不恢复或持久化运行中的 tool、approval/question、process/kernel、scheduler、mailbox 或 live execution state；Plan 77 public generation/sequence/snapshot 不增加 compaction 字段。
- compaction summary 只能描述当前 effective history 可见的 bounded preview/pointer；不读取或复制 offload 全文，不暴露 raw provider response、opaque reasoning 或 chain-of-thought。
- parent/child rollout 各自拥有 history 与 usage；本计划不做跨 child ledger 汇总、不改变 fork/resume/clear 的 legal-cut 和 lineage 语义。

### 非目标

- 不复制 CodeWhale 的 server-side compaction、Compaction block、SessionManager、Fleet、role/profile/worktree 或 app-server/ACP wire。
- 不修改 `crates/provider/src/{anthropic,openai,responses,sse}.rs`、provider adapter、provider message wire、reasoning replay 或新增 provider。
- 不新增 public receipt、durable public-event journal、snapshot 第二真值、native JSON-RPC 字段、Desktop DTO 或 protocol version。
- 不改变 `History::replace_all`、`rollout` compacted marker 的持久化边界、`chars/4` estimate、hard-coded growth/keep 常量、provider usage ledger schema、价格/billing/quota/budget 语义。
- 不实现 deferred tool activation cache、child route provenance、opaque reasoning continuity、逐 model-call stream receipt、MCP capability planning 或 execution checkpoint；这些另立计划。
- 不将 summary 文本视为结构化、deterministic 或可机器解析真值；不凭 summary 内容猜测 tool pair、进程状态或恢复状态。

## 实施步骤

### 1. 收敛纯 compaction policy/helper

- 复核并保留 `crates/core/src/compact.rs` 的 `max_turn_growth`、`predicted_overflow`、`keep_from_index`、`starts_with_tool_result` 与 `sample_summary` 现有边界。
- 增加内部 `CompactionPlan`/`NoOpReason`（名称可按代码风格确定），让纯 helper 只负责历史长度、可折叠前缀、recent-tail boundary 和 ToolUse/ToolResult pair-safe 判断，不访问 provider、History、ledger 或 cancellation。
- 增加 summary canonicalization/replacement helper：trim、去重复 `SUMMARY_PREFIX`、保留完整 tail、拒绝 replacement 无变化；确保已经存在 summary 时只有新增可折叠上下文才允许再次 replacement。
- 不把 trigger 放进 provider request；保持 `COMPACT_SYSTEM`、无 tools 和现有 summary instruction 的稳定 prompt shape。

### 2. 建立统一 `compact_once` 执行 seam

- 将现有 `run_compaction` 窄化或兼容委托到一个统一内部 API，接收实际 model、trigger、History、cancel，返回 `Applied`/`NoOp` 或错误。
- 在同一 seam 内完成：纯 plan → 一次 summary sampling → canonicalize → accepted usage record → `History::replace_all`；失败路径在任何 mutation 前返回。
- 保持 accepted usage 先于 compacted marker 的现有 rollout 顺序；不因 receipt、NoOp 或重复调用增加 ledger、sequence、lineage、runtime/terminal metadata。
- 保持 `sample_summary` 只接受非空 text + `AssistantOutcome::EndTurn`；`TextDelta`/thinking 的既有收集和 incomplete/non-success fail-closed 规则继续有效。

### 3. 迁移三个入口

- `crates/core/src/agent.rs`：predictive path 只负责压力判定和 note；reactive overflow 只负责一次 retry；二者传入实际 `active_model` 并消费结构化 outcome，不重复写 history/usage。
- `crates/core/src/commands/compact.rs`：manual path 使用统一 seam，区分 applied/no-op/error 文案，不改变 `/compact` 的 public command 名称或 history owner。
- `crates/core/src/history.rs`、`crates/core/src/usage.rs`：只补必要注释/窄 helper 或测试，确认 replace/anchor/ledger 既有语义不被统一 seam 改写；不新增 ledger 语义。

### 4. 补齐测试矩阵

#### 纯 policy/replacement

- 空、单消息、短 history、无可折叠前缀；整对象断言 NoOp reason。
- 只有已有 summary + recent tail；已有 summary 后新增可折叠消息；replacement 与当前完全相同。
- summary 已含前缀、前后空白、重复前缀；最终只保留一层 canonical marker。
- oversized recent message、offload pointer、单/多 ToolUse→ToolResult pair；boundary 不落在 ToolResult 或其 assistant ToolUse 之前。

#### accepted/rejected execution

- `EndTurn + non-empty + Some(Usage)`：实际 model 正确，ledger 只有一条 compaction record，rollout 顺序为 usage → compacted，replacement 正确。
- `EndTurn + usage: None`：成功但 ledger 不新增；provider absence 不伪造零。
- 空 summary、non-EndTurn、provider error、cancel、incomplete、invalid/no-change replacement：messages、ledger、rollout bytes/lines 均不变。
- 已 canonical 文件重复调用：第二次不调用 provider、不追加 usage、不追加 compacted marker，返回 NoOp/zero mutation。

#### agent lifecycle

- predictive 在 sampling 前触发；失败/NoOp 后原 request 继续。
- reactive overflow 后只 compact 一次并 retry；第二次 overflow 不循环。
- manual applied/no-op/error 文案；fallback 后 compaction 使用 actual active model。
- compaction 与 `/clear` 保留累计 ledger，reset context anchor；resume/fork 保留既有 usage/lineage/cut 语义。
- stable compaction system/request capture：trigger、session metadata、pressure estimate 不进入 stable system prefix；summary request 不携带工具/provider-specific wire。
- public event/snapshot/native protocol shape 不出现 receipt、ledger 或 compaction internals。

### 5. 同步文档与证据边界

- 实施完成时回填本文件状态、实际日期、commit SHA、focused/workspace 验证结果和未执行环境；不在规划阶段宣称已实现能力。
- 更新 `kloop/README.md` 的 Compaction 段，说明 predictive/reactive/manual 共用 seam、replacement/no-op、ToolUse/ToolResult boundary、pressure 不等于 billing、失败不改 history。
- 在 `kloop/docs/plan/HANDOFF.md` 顶部补完成事实与教训，明确 Plan 81 ledger 与 context estimate 的分层；只在 `docs/capability-report.md` 已有 compaction 条目时做最小销账，不宣称 server-side compaction。
- 如需更新 `refs/README.md`，只补 CodeWhale 当前观察 HEAD/固定证据边界，不把候选设计写成 CodeWhale 或 kloop 已实现的更大能力。

## 必须复用的现有 seam

- `crates/core/src/compact.rs`：`max_turn_growth`、`predicted_overflow`、`keep_from_index`、`sample_summary`、`SUMMARY_PREFIX`、`COMPACT_SYSTEM`、`run_compaction`。
- `crates/core/src/history.rs`：`messages`、`estimated_tokens`、`record_provider_usage`、`replace_all`、usage anchor reset 和 offload/pointer。
- `crates/core/src/agent.rs`：predictive pressure、reactive `Sampled::Overflow`、actual active model/fallback 和一次 retry guard。
- `crates/core/src/commands/compact.rs`：manual `/compact` output seam。
- `crates/core/src/rollout.rs`：`append_provider_usage`、`append_compacted`、single replay 和现有 append-only durability。
- `crates/core/src/usage.rs`：Plan 81 `UsageLedger`/`ProviderUsageRecord`，不改变其 owner 或字段语义。
- Plan 68 offload/pointer、Plan 77 public projection/generation boundary、Plan 81 durable usage ledger 与 existing compact tests。

## 关键文件

预计修改：

- `kloop/crates/core/src/compact.rs`
- `kloop/crates/core/src/agent.rs`
- `kloop/crates/core/src/commands/compact.rs`
- `kloop/crates/core/src/agent/tests.rs`（若当前测试布局需要补 lifecycle coverage）
- `kloop/README.md`
- `kloop/docs/plan/86-cache-stable-agent-compaction.md`
- `kloop/docs/plan/HANDOFF.md`
- 必要时 `kloop/docs/capability-report.md`、`refs/README.md`

仅在生产调用点要求时窄修改：`kloop/crates/core/src/history.rs`、`kloop/crates/core/src/usage.rs`、`kloop/crates/core/src/rollout.rs`。不修改 provider adapters、server wire/events、Cargo manifests、Cargo.lock 或 Desktop 仓库；若实际触及，完成记录必须解释原因且不得突破 public protocol/无新增依赖边界。

## 验证

从 `<repo>/kloop` 执行：

### 定向

```bash
cargo test --locked -p kloop-core compact
cargo test --locked -p kloop-core agent
cargo test --locked -p kloop-core history
```

应覆盖：纯 planning/no-op、summary replacement、stable prompt、tool pair、accepted usage 顺序、failure no-write、predictive/reactive/manual、actual fallback model、clear/compaction anchor/ledger、resume/fork compatibility。

### 总质量门

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace
cargo run --locked -p kloop -- --mock
cargo run --locked -p kloop -- --mock --headless
cargo run --locked -p kloop -- --mock --headless --json
cd .. && git diff --check
```

验证记录必须写明 Darwin/macOS 实际结果、focused passed 数量、workspace ignored 项，以及真实 provider、Linux sandbox/CI、Windows、物理终端、Desktop E2E 未执行的部分。Mock 只证明本地 lifecycle，不冒充真实 provider、server-side compaction、crash-safe external side-effect recovery 或 Desktop E2E。

## 完成标准

- predictive、reactive、manual 三入口共享一个 core compaction seam；不重复采样、replacement 或 usage 记账。
- summary replacement canonical、不会无限 stacking；NoOp/duplicate 可审计且无 provider/ledger/rollout mutation。
- ToolUse/ToolResult pair、offload pointer、failure no-write、actual fallback model 保持正确。
- context pressure、usage anchor、durable ledger、billing/public protocol/generation 语义保持分离。
- README、HANDOFF、Plan 86 与必要 capability/reference 记录同步，未宣称未实现的 server-side/provider-specific 能力。
- fmt、clippy、workspace tests、mock smoke、diff check 全绿；不新增依赖、不修改 Desktop、不 push。
- 实施完成后以一次范围明确的 kloop commit 收口，回填实际日期、commit SHA、focused/workspace 验证结果与未执行环境。

## 实施记录（2026-08-14）

- 已实现：`compact_once` 统一 predictive/reactive/manual；纯 `CompactionPlan` 与 `NoOpReason`；canonical summary replacement；ToolUse/ToolResult pair-safe recent tail；实际 active model；accepted usage→compacted 顺序；失败/NoOp 零 history/ledger/rollout mutation。
- 已补测试：Plan 86 focused filters 为 compact 30、agent 138、history 18；core crate 675 tests 全绿（含新增 predictive/reactive/manual/no-provider/fallback/canonical/duplicate coverage）；manual applied/no-op/error 文案已锁定。
- 已通过：`cargo fmt --all -- --check`、workspace clippy `-D warnings`、`cargo test --locked -p kloop-core`（675 passed）、`cargo test --locked --workspace`（全绿，2 项真实 provider credential tests ignored）、三个 `--mock` smoke（plain/headless/headless JSON）与 `git diff --check`。
- 真实 provider 验收：通过 `.kloop/env.local` 的 `anthropic` rail、实际模型 `claude-sonnet-4-6` 运行两个 ignored 合约，`real_agent_program_workflow_contract`（64.15s）与 `real_local_agent_mailbox_contract`（49.24s）均通过。真实 `--plain` 首轮 `/compact` 返回 `1 summarized, 1 kept verbatim`，第二轮返回 NoOp；rollout 只有一条 compaction usage（input 161/output 362）和一条 compacted marker，顺序为 usage→compacted，replacement 为 canonical 单层 prefix + 非空正文 + 完整 tail。另以 `KLOOP_CONTEXT_WINDOW=23193` 驱动真实 predictive path，第二 turn 在 sampling 前完成一次 compaction（input 161/output 12）后继续采样，实际 model 保持一致。凭据未打印、未写入仓库。
- 未执行：真实 provider 的 reactive overflow（需构造实际 provider 超窗）、server-side/provider-specific compaction、Linux sandbox/CI、Windows、物理终端、Desktop E2E。
- 最终 commit：以本条所在提交为准。
