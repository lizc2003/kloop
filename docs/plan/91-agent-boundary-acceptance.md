# Plan 91 — CodeWhale 借鉴：agent boundary acceptance/conformance

> 状态：规划中
>
> 依赖：Plan 87、Plan 88、Plan 89、Plan 90；复用 Plan 54、Plan 63、Plan 68、Plan 77、Plan 81、Plan 86；参考快照 `refs/codewhale` 当前观察 HEAD `5e3ac84c5cb925b4c90c34dfe582b75f04605cb1`，固定历史审计基线 `b494236312ef3ac36489c83706a0b11ab73935a1`

## Context

Plan 87–90 分别收口 deferred capability binding、child route provenance、provider terminal/reasoning continuity 和 MCP readiness。它们完成后仍需要一个只做跨面验收的计划，证明这些内部契约在 resume、fork、compaction、worktree 切换、child lifecycle、tool refresh 和 provider fallback 后保持一致，并且 CLI、TUI、plain、headless、native server 不各自重新解释内部状态。

本计划不承载新的领域实现，只建立 deterministic conformance 和 public-boundary acceptance；不把未执行的真实 provider、Linux sandbox、Windows、Desktop 或物理终端写成通过。

## 契约与范围

### 本次吸收

1. 验证 discovery/unlock receipt 的 Workspace/worktree/policy/authority/source-generation binding，在 refresh、workspace switch、fork、resume、compaction、child clone 后 stale 时 fail closed。
2. 验证 Agent/Program/Workflow/mailbox/background execution 的 route/provenance identity 与 terminal owner 不混淆；late delivery、duplicate terminal、resume/fork lineage 和 root Task ownership 不回归。
3. 验证 provider typed terminal、incomplete/reasoning continuity、tool pairing、fallback/continuation 和 Plan 81 usage record 边界一致；incomplete 不成为成功 history，usage 不重复记账。
4. 验证 MCP configured/planned/starting/ready/degraded/failed/stale receipt、catalog generation、auth availability、resource read 和 tool call 的 failure semantics 一致；MCP failure 不伪装成 missing tool。
5. CLI/TUI/plain/headless/native server 只消费 bounded stable projection，不暴露 raw receipt、secret、opaque reasoning 或内部 registry；公共 protocol、snapshot、generation/sequence shape 不扩张。

## 关键文件类别

- core integration tests/evaluator：config/workspace/tools/agent/compact/history/rollout/event
- protocol typed projection and negative tests
- provider fixtures/stream/semantic lifecycle tests
- MCP transport/catalog/readiness fixtures
- server events/wire/read/list/resume/fork tests
- CLI/TUI/plain/headless contract tests
- `README.md`、`docs/capability-report.md`、`docs/plan/HANDOFF.md` 与各 Plan 完成记录

## 非目标

- 不新增 capability、route、provider、MCP registry 或 execution runtime。
- 不扩 public protocol 字段，不创建 public durable event journal、snapshot 第二真值或 Desktop DTO。
- 不汇总 Agent/Program/Workflow/MCP 子调用费用，不改变 Plan 81 ledger owner。
- 不把真实 provider、Linux sandbox/CI、Windows、Desktop E2E、物理 terminal 未执行冒充 deterministic local test。
- 不以自然语言自评、单次 mock smoke 或 aggregate count 替代 typed state、bytes、lineage、terminal 和 negative assertion。

## 为什么不能与 Plan 87–90 合并

Plan 91 必须在领域契约稳定后才能收口；如果提前合并，验收会反向固化未完成的内部类型，并掩盖 capability、provenance、provider 和 MCP 各自的 owner 边界。它只能验证和投影，不能偷偷承载领域实现。

## 实施与测试方向

建立跨面矩阵：每个维度至少覆盖 happy path、stale/deny/failure、duplicate/late、resume/fork/compaction、child/worktree scope 和 public projection。用固定 JSON/rollout bytes、typed object equality、generation/sequence/lineage、terminal count、provider request capture 和 secret-negative mutation 断言；测试过滤器必须校验实际命中，避免 `0 passed` 被误判为通过。运行 focused core/server/provider/MCP/CLI tests、fmt、clippy、workspace test、mock/headless smoke 和 diff check，分别记录未执行环境。

## 完成标准

- Plan 87–90 的跨面负契约在 resume/fork/compaction/worktree/child/refresh/fallback 生命周期全覆盖。
- 四类 receipt 的 owner、scope、generation、terminal、failure 和 public projection 不互相冒充。
- CLI/TUI/plain/headless/native server wire shape 保持既有边界；无 secret/raw reasoning/second truth。
- 完成记录准确区分 Darwin 本地、mock、真实 provider、Linux/Windows/Desktop/物理 terminal 未执行项；不新增依赖、不修改 Desktop、不 push。
