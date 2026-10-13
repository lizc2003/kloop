# Plan 230 — 回退原生 PR 审查阶段 ✅

> 2026-10-13。用户判定：宁可完全回退，也不能让 review 阶段侵入共享调度。
> plan 226–229 的原生阶段整体回退；只保留两处零架构成分。
> 实现提交 SHA 以本文件所在提交为准。

## 决定与范围

改本轮用具表、原生带问题结束 turn 这两件事，只有共享循环能给。为了这两点把
review 概念放进 `ToolCtx`、`dispatch_tools` 和 agent 循环，是核心循环的永久概念
债务——每次有人动 `dispatch_round` 都要理解一个 skill 的相位状态。用户的选择是：
宁可失去这条能力，也不接受这笔债务。

未采用的替代（记录取舍，避免重复讨论）：

- **fork 缝重做**：用现成的 `context: fork` + `allowed-tools` 限制子代理工具集、
  用子代理隔离承载 packet、用既有 `ask_user_question` 承载缺目标提问，可以做到
  共享路径零改动。但目标分支只能来自模型给出的参数，**丢掉 plan 229 的"只认用户
  原话"**，交还提问回到提示词层，固定版本补读失去结构化保证，同轮排队调用不再
  被拦。一个基于错误基线却看起来权威的报告，被认为比没有这条能力更糟。
- **最小侵入重构**：把三根通道收成一根、退出判定收进 `Flow`、用位置拦截取代
  dispatcher 分支，能力全部保留。代价是循环永久多一个 review 概念、`Turn` 多一个
  阶段字段，且 226–229 的真实样本验收是对着旧形态做的，部分不再适用。

## 回退内容

- `rust/crates/core/src/agent.rs`、`history.rs`、`tools/mod.rs`、`tools/skill.rs`、
  `tools/builtin.rs`、`rust/crates/core/Cargo.toml`、`rust/Cargo.lock` 恢复到
  `5907128^`；`NOT_RUN_STOPPED` 恢复原语义与文案。
- 删除 `tools/review.rs`、`tools/review/flow.rs`、`tools/review/flow/tests.rs`、
  `tools/review/tests.rs`。
- 审计不变量：`git diff 5907128^ -- rust/crates/core rust/Cargo.lock` 只余下表列的
  保留项（`skills.rs`、`SKILL.md`）。

## 保留（零架构成分）

- `rust/crates/web/src/fetch.rs` 的非成功 HTTP 指引（401 / 403 / 404 / 410 / 429 /
  5xx）：既有工具的错误文案，自带集成测试，用户明确要求过；DESIGN 对应段落保留，
  其中"原生 review 复用认证恢复"那句随原生阶段一并删去。
- `rust/crates/core/src/skills/code-review/SKILL.md` 的 PR 手工契约：description 的
  PR 字样、"Resolve the target once"、匿名 404 / DNS 段、补读与截断条款、frozen
  baseline 改写。删除描述原生运行时的 "Native PR phases" 一节——留着就是假话。
  `skills.rs` 的 12 条契约断言逐条落在保留段落内，不改。

## 后果

- PR 审查回到"提示词 + 自由 bash"的手工路径：plan 226 实测的定位浪费、版本混读、
  32 / 16 轮交不出报告的状态回归。
- 随之消失：`review_read`、固定 packet、补查预算与最后一轮预留、原生交还提问、
  同轮排队拦截，以及 review 的跨轮状态机（含复核时发现的两处排序问题）。
- 未被证实的部分（finding 质量、覆盖、漏报率）原本就没有证据，不受本次影响。

## 验证

- 审计不变量与残留清单逐项核对；`NOT_RUN_STOPPED`、`ToolCtx` 字段、`dispatch_tools`
  分支、`history` 转发方法、`builtin.rs` 并发分类、`Cargo` 依赖均已复原。
- 外层执行沙箱之外 `make check`：fmt、clippy、workspace debug 全量测试、本机 parity。
- `git diff --check` 与暂存后 `git diff --cached --check`。
- 没有真实模型样本；不声称审查能力有所改善或保持。
