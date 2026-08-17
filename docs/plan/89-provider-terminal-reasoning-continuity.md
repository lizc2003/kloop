# Plan 89 — CodeWhale 借鉴：provider terminal/reasoning continuity

> 状态：✅ 已完成（2026-08-17；提交 SHA 以本文件所在提交为准）
>
> 依赖：Plan 64、Plan 65、Plan 75、Plan 81、Plan 86；参考快照 `refs/codewhale` 当前观察 HEAD `5e3ac84c5cb925b4c90c34dfe582b75f04605cb1`，固定历史审计基线 `b494236312ef3ac36489c83706a0b11ab73935a1`

## Context

Plan 64/65/75 已在 provider rail 建立单终态、typed outcome、reasoning signature/encrypted content、tool identity 和 incomplete 规则；Plan 81 已确定 accepted terminal usage 的 ledger 边界。当前仍需把 provider typed terminal 语义稳定地传到 core turn lifecycle，避免 Refused、Filtered、Incomplete、semantic partial、transport/protocol failure 和 cancellation 全部退化为普通字符串错误，也避免 compaction、continuation、fallback 或跨 rail replay 错误处理 opaque reasoning。

CodeWhale 的可吸收原则是：incomplete 不是成功答案，已报告 usage 仍可保留；opaque reasoning 只能在 provider/API family/final wire model 兼容时连续回放；不能用 model 名称猜能力，也不能为了压缩而暴露 raw CoT。

## 契约与范围

### 本次吸收

1. core 保留 completed、provider incomplete、transport/protocol failure、cancellation、semantic partial 和 core execution failure 的 typed distinction。
2. Refused、Filtered、Incomplete 不自动转成成功，也不无条件 retry/fallback；已经发出 semantic text/reasoning 后，断流不得生成第二份回答。
3. reasoning/thinking 的 text、signature、encrypted/opaque payload 在合法同 rail replay 时原样保留；跨 provider/API family/wire model 不猜测回放，Chat rail 剥离 reasoning 的既有 intentional boundary 保持不变。
4. ToolUse/ToolResult pairing、continuation、compaction 和 fork/resume 的边界不得丢 reasoning identity 或破坏 provider message ordering。
5. terminal usage 继续完全复用 Plan 81：只在 validated terminal `Some(Usage)` 记一次；`Usage::total()` 仍只用于 context estimate，不是 billing total。

## 关键文件

- `kloop/crates/protocol/src/lib.rs`
- `kloop/crates/provider/src/{lib.rs,failure.rs,stream.rs,anthropic.rs,openai.rs,responses.rs}`
- `kloop/crates/core/src/{agent.rs,agent/sampling.rs,compact.rs,history.rs,rollout.rs,usage.rs}`
- `kloop/crates/server/src/events.rs`
- provider/core lifecycle and reasoning/tool-pair tests

## 非目标

- 不新增 provider、不重做三条 rail parser、不改变 pause_turn 映射或 Plan 64/65/75 已闭合 wire 规则。
- 不把 incomplete 自动改成 provider continuation，不做跨 provider opaque replay，不暴露 raw chain-of-thought。
- 不扩展 usage ledger schema，不做价格、金额、quota、budget、child billing 或 public snapshot usage。
- 不把 typed terminal 状态通过错误字符串反解析，不建第二套 provider receipt/parser。

## 为什么不能与其他计划合并

这是 provider wire 到 core 状态机的数据面语义边界。Plan 88 的 provenance 是执行血缘，Plan 90 的 readiness 是 MCP 控制面；混合会让 provider terminal、child route 和外部连接健康共用错误 owner。

## 实施与测试方向

覆盖三条 rail 的 EndTurn/ToolUse/OutputLimit/Refused/Filtered/Incomplete、transport/protocol/partial/cancel、reasoning signature round-trip、same-rail resume/fork/compaction、cross-rail fail-closed、fallback actual model、accepted usage once、no-usage absence 和 semantic-output retry seal。断言 public protocol 不暴露 raw reasoning，ledger 不重复记账，incomplete 不成为成功 history。

## 完成标准

core、provider、history、compaction、resume/fork 和 native projection 保持 typed terminal/reasoning 边界；失败/incomplete 不伪装成功；合法 opaque continuity 原样保留；Plan 81 usage 事实不变；不新增 provider、依赖、public wire 或 child billing。

## 实施结果（2026-08-17）

- `Message` 新增 private `ProviderResponseProvenance`，绑定无凭据的 provider endpoint identity、typed API family 与 exact final wire model。每个 provider-produced assistant message（含 semantic partial/cancel replay fragment）在写入 History/Rollout 时固定该 provenance；旧 rollout 缺 provenance 的 reasoning 按 incompatible 处理，不猜迁移。
- `Provider::stream` 在任何 adapter/网络 I/O 前统一验证 reasoning replay。Anthropic Messages、OpenAI Responses 只有 exact provider/family/model 可原样回放 thinking/signature/encrypted/redacted payload；provider、family 或 model 任一不符均返回 non-retryable typed protocol failure。OpenAI Chat 保留既有 intentional boundary：出站剥离 reasoning，不按模型名、URL heuristic 或当前 rail 猜来源。
- sampling 不再把 `ProviderFailure` 提前 `to_string()`；retry exhausted、non-retryable 与 semantic partial 全程携带同一 typed failure，`after_semantic_output` 继续封住 retry/fallback。Core `TurnError` 区分 core execution、typed `AssistantOutcome` 与 typed `ProviderFailure`；Refused、Filtered、Incomplete 和两种 OutputLimit 保留原 variant，用户 cancellation 仍是独立 `Aborted`。
- rollout 的既有 `turn_terminal` additive 增加 internal `typedError`，而 status/error 继续作为 protocol 1.0/display projection；provider failure 使用 rollout-local serde mirror 持久化完整 kind/message/retry metadata/semantic watermark，不新增 provider receipt/parser 或 dependency。resume/fork 物理保留 message provenance 与 typed terminal，SnapshotTerminal 继续只暴露旧显示形状。
- compaction 的 recent tail 逐 Message 原样保留 provenance；folded prefix 仍通过当前 provider 的统一 replay validator，incompatible reasoning 在 summary I/O 前失败，accepted usage→compacted 顺序与 Plan 81/86 不变。context token estimate 明确排除不会进入 provider prompt 的 provenance bytes。
- `thread/read`、initial event snapshot 与 refreshed snapshot 全部走 display-safe projection：移除 provider provenance、thinking signature/encrypted payload 与 redacted blob，保留 live reasoning item 已展示的 summary text；public protocol schema、usage snapshot 和 child billing 未扩展。
- CodeWhale 当前可读树 HEAD 由 refs 文件确认；feature commit 归因来自公开 GitHub API/raw spot-check，受 isolated-worktree guard 限制，未在本地 nested repo 运行 `git show` 二次复核。吸收 exact provider/API/final-model opaque replay、incomplete-before-side-effect 与 append-only ordering；明确不吸收其 post-semantic headless/interactive retry、字符串 error taxonomy 和 unknown-status→end_turn fail-open。

## 验证记录（2026-08-17）

- `cargo test -p kloop-protocol`：15 passed；`cargo test -p kloop-provider`：33 unit + Anthropic 18 + Chat 14 + Responses 17 passed；
- `cargo test -p kloop-core`：712 passed；`cargo test -p kloop-server`：25 unit + 33 integration passed；
- `cargo test --workspace --all-targets --all-features`：全绿（2 项真实 credential tests ignored）；
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`、`cargo fmt --all -- --check`、`git diff --check`：通过；
- `cargo run --locked -p kloop -- --mock`、`--mock --headless`、`--mock --headless --json`：通过；
- `.kloop/env.local` 的真实 OpenAI Responses rail（effort=high）：fresh headless 与两组 `--continue --headless` resume 均通过，分别得到 `OK → STILL_OK` 与 arithmetic answer `83810205 → CONTINUED`；真实 transcript 的 opaque-block 统计读取被 permission guard 拒绝，未绕过，因此 encrypted payload 的 exact capture/replay 仍以 wiremock + persisted provenance/fork/resume 回归为权威证据；
- 真实 Anthropic 初次验收中，ignored primitive contract 两次在首个 `run_agent` 断言看到 0 tool starts，随后 direct `claude-sonnet-4-6` headless smoke 明确返回三次 HTTP 429；配额恢复后重测，direct smoke 返回 `OK.`，`real_agent_program_workflow_contract` 通过（311.52s），覆盖真实多轮 Agent/Program/Workflow continuity；
- 未执行 Linux sandbox/CI、Windows、物理终端与 Desktop E2E。
