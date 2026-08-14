# Plan 89 — CodeWhale 借鉴：provider terminal/reasoning continuity

> 状态：规划中
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
