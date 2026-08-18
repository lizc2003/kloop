# Plan 93 — Responses 真实原语验收稳定性

> 状态：✅ 已完成（2026-08-18；提交 SHA 以本文件所在提交为准）
>
> 基线：`b6ea59a`（Plan 92 完成记录）
>
> 依赖：Plan 24、Plan 68、Plan 75、Plan 88、Plan 92

## Context

Plan 92 后真实 provider 复测确认：Anthropic Sonnet 4.6 的完整 Agent/Program/Workflow 合约可通过，OpenAI Responses 的最小 sampling、route、usage、assistant provenance 与 secret-negative 也通过；但同一个 Responses 完整 evaluator 连续暴露三类不稳定失败：模型把 optional `description` 物化为纯空白、一个包含两次 Program 调用和 child sampling 的长 turn 撞上固定 300 秒 watchdog、以及模型在首次预期失败后直接 EndTurn而没有自主发起第二次 journal resume。

确定性 core tests 与 Plan 75 的历史真实验收都证明 `run_program` journal/resume runtime 没有已知断裂。问题是两条边界叠加：模型可见 description schema 允许空白但 runtime 正确拒绝；真实 evaluator 又把“同一长 turn 内自主读取错误并继续恢复”当成 runtime correctness gate。

## 契约

- optional display description 的 schema 与 runtime 必须一致：缺失或 `null` 表示 omission；非空单行字符串是合法显示标签；`""`、纯空白、控制字符、超长和非字符串继续在任何副作用前 fail closed。
- Program resume 保持显式 caller-driven。runtime 不自动重跑 source；journal 只复用完成的 `agent()`，不承诺普通工具或外部副作用 exactly once。
- journal active 的失败稳定显示 `Durable Run ID: run-*`，并保留现有 `resume_from_run_id: "run-*"` 文本兼容入口。
- 真实 Program 验收分为两个 client turn：第一阶段创建失败 run，第二阶段以第一次实际 source 和真实 run id 显式 resume。仍要求两个外层 tool pair、byte-identical source、同一 run id、两次预期 failure sentinel、仅一次 child spawn。
- Responses high-effort evaluator 使用更长但有界的 watchdog；生产 provider timeout不改。timeout 与 child exit必须区分，失败清理 app-server，不输出 stderr、endpoint、key、raw SSE 或完整 transcript。
- 不因真实模型随机性放宽 Responses SSE parser、未知字段、空 identity、伪 run id 或 runtime strict gate。

## 实施

- `kloop/crates/core/src/tools/mod.rs`
  - `run_agent.description` 改为 `string | null`，增加 non-whitespace pattern。
  - `optional_display_description` 将 `null` 解释为 omission，保留所有非法字符串拒绝。
  - schema/parser exact tests覆盖 nullable、pattern、null omission与空白负向。
- `kloop/crates/core/src/tools/codemode.rs`
  - `run_program.description` 同步 nullable + non-whitespace pattern。
  - resume hint 增加稳定 Durable Run ID；工具说明强调下一次显式调用、same source、真实 ID。
- `kloop/crates/core/src/tools/{subagent.rs,codemode/tests.rs}`
  - null description 使用既有 preview fallback并成功执行；空白继续零副作用拒绝。
  - journal failure hint 与显式 resume仍证明只启动一次 child。
- `kloop/crates/provider/tests/responses.rs`
  - 固定 reasoning + run_program call + error function output 的请求重放，逐字保留 failure sentinel、Durable Run ID和resume key。
- `kloop/crates/cli/tests/real_agent_program_workflow.rs`
  - Program首次与resume拆为两个真实 turn，合并后保留全部强断言。
  - optional control只接受 omission/null、语义默认值或合法非空description；空白仍失败。
  - Responses默认900秒watchdog，可由 `KLOOP_REAL_EVALUATOR_TIMEOUT_SECS=60..3600` 覆盖。
  - timeout/closed channel检查child exit status；Drop兜底kill/wait。

## 验证

- focused：core tools 31、Program 37、Agent 34、Responses HTTP 18，全绿。
- `cargo fmt --all -- --check`、workspace all-target/all-feature Clippy `-D warnings`、`cargo test --workspace`、mock smoke与`git diff --check`全绿。
- 真实 Anthropic `claude-sonnet-4-6` 新两阶段 evaluator：通过，114.97秒。
- 真实 OpenAI Responses `gpt-5.5`：连续三次完整合约通过，分别88.26秒、109.54秒、125.91秒。每次均为1次foreground Agent、2次foreground Program、仅1次Program child spawn、1次background Agent、1次background Program、1次Workflow、3个唯一background terminal、3次exactly-once automatic delivery。
- 真实运行未打印或提交 key、Authorization、private endpoint、raw provider response或完整 transcript。
- OpenAI Chat 本轮上游曾返回502→503，未作为本修复完成条件；Chat deterministic tests继续覆盖。

## 完成标准

- schema不再允许纯空白 description越过provider校验，runtime仍不把非法值归一为默认。
- Program失败明确暴露durable id，但resume仍是第二次显式tool call，无内部自动重放。
- Responses完整真实合约连续三次通过，journal resume只生成一次child。
- evaluator超时可诊断、失败进程可清理、敏感数据不进入输出或仓库。
