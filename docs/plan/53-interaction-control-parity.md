# Plan 53 — 交互与控制工具对齐

> 状态：未开工
>
> 母计划：Plan 48
>
> 依赖：Plan 48
>
> 固定目标：Claude Code 2.1.220；版本身份以 Plan 48 manifest 为准。

## 背景

交互与控制能力受 TTY、permission mode、plan state 和 surface_kind 影响。Plan 48 只对 ExitPlanMode 建立了 outside/headless 与 PTY approve/reject/cancel 证据；AskUserQuestion、EnterPlanMode、StructuredOutput 和 Workflow 没有完整执行链。

本计划先区分 model-visible tool、internal adapter、workflow primitive 和前端审批 seam，再决定 kloop 是否需要独立工具或兼容层。

## 当前证据与差距

对应 matrix 行：

- `ask-user-question@plan-pty`
- `enter-plan-mode@plan-pty`
- `exit-plan-mode@plan-pty`
- `structured-output@clean-cli`
- `workflow@clean-cli`

当前结论：

- AskUserQuestion、EnterPlanMode 的 registration/schema 为 `missing`。
- ExitPlanMode 的 registration/schema/parser/executor/output/lifecycle 为 `compatible`，permission 为 `intentional-diff`，concurrency 为 `n/a`。
- StructuredOutput 当前标为 `internal-adapter` 且全维 `unknown`；ReportFindings 可见不构成其注册证据。
- Workflow 在 CC clean fixture 可见；kloop 只有 `run_program` 内部的 parallel/pipeline 编排，registration/schema 为 `missing`。
- 当前 matrix 将 Workflow executor 标为 `intentional-diff`，但 CC 侧引用仍是 registration capture，不是 standalone 执行证据；开工时必须先重采调用 fixture，再重新裁决 executor。

优先复用：

- `kloop/crates/core/src/tools/plan_mode.rs`
- `kloop/crates/core/src/permissions.rs`
- `kloop/crates/core/src/tools/codemode.rs`
- core `Approver`、TUI/plain/headless/server 的交互 seam
- Plan 48 的固定 PTY driver 和 ExitPlanMode fixtures

## 目标

1. 固定五项能力的 surface_kind、注册条件和 schema。
2. 固定 AskUserQuestion 的问题、选项、答案、自由输入、取消和无交互行为。
3. 固定 Enter/ExitPlanMode 的状态转换、工具集变化、批准/拒绝/取消和恢复。
4. 固定 StructuredOutput 是模型工具、结果约束还是内部 adapter。
5. 固定 Workflow 的执行、权限、并发、输出和生命周期；不把 `run_program` 改名冒充。
6. 让 TUI/plain/headless/server 的差异有明确兼容边界，不逐字节复制 UI。

## 开工证据闸门

- 从 `cc-exit-plan-entry` 和统一工具 adapter 追 AskUserQuestion、EnterPlanMode、StructuredOutput、Workflow 的独立静态链。
- PTY case 与 `--print` headless case 分开；headless 结果不得为真实用户批准作证。
- 固定 PTY 尺寸、CPR、按键脚本、process group cleanup 和 raw transcript。
- 为每个交互结果保存 raw/normalized fixture；不能归一化选项顺序、状态或错误文案。
- 对 StructuredOutput 先证明 surface_kind；不能从名称或 ReportFindings 反推。
- 对 Workflow 先取得 standalone tool 的调用 fixture，再评估与 code mode 的 adapter。

## 实施切片

### 0. surface 与注册条件

按 clean、plan、interactive、headless profile 固定工具数组、schema 和 gate。

### 1. AskUserQuestion

- 单选、多选、preview/annotation、自由输入、取消、EOF 和坏 schema。
- 答案如何回到 tool_result，以及 turn 是否继续。
- 评估是否复用 Approver transport，但不混淆审批和一般问题。

### 2. Plan mode

- Enter 前后工具集和权限 mode。
- Exit outside mode、headless、PTY approve/reject/cancel。
- 嵌套、重复进入/退出、断线和 session restore。

### 3. StructuredOutput

- 确认注册面与输入/输出 schema。
- 若只是内部 adapter，保持 `unknown`/`n/a` 的准确 surface；不创建假工具。
- 若为真实可调用能力，再补成功、schema mismatch、取消和结果生命周期。

### 4. Workflow

- 固定 script/meta/args、执行、权限、并发、失败、取消、resume 和结果。
- 比较 `run_program` 的内部 primitives，仅在证据支持时提供适配层。
- `run_program` 继续作为 kloop-only 行单列。

### 5. 产品与回归

只实施已裁决能力；前端新增交互必须覆盖 TUI/plain/headless/server 的 fail-closed 行为。

## 非目标与有意保留

- 不把现有 Approver UI 自动等同 AskUserQuestion。
- 不把 ReportFindings 当 StructuredOutput。
- 不把 `run_program` 改名或计作 Workflow parity。
- 不逐字节复刻 Claude Code 的 Ink UI、按键提示或 prompt。
- 不用真实用户会话、配置或终端 transcript 作 fixture。

## Fixture 与测试

至少覆盖：

- AskUserQuestion 的合法答案、自由输入、取消、无 TTY 和坏输入；
- Enter/ExitPlanMode 的 outside、headless、approve、reject、cancel、重复转换；
- PTY 超时和整个进程组清理；
- StructuredOutput/Workflow 只在取得真实 surface 证据后加入执行 case；
- 四个前端的可用、降级和拒绝路径。

验证：

```bash
python3 -B refs/claude-code-2.1.220/verify.py
cd kloop
cargo test -p kloop-core tools::plan_mode::tests
cargo test -p kloop-core tools::codemode::tests
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

## 文档同步

完成时同步本 plan、HANDOFF、refs/README、kloop README、capability report 与 parity 产物。明确哪些是 model-visible tool，哪些只保留为内部能力。

## 完成标准

- 五个 surface 的类型和当前平台可执行链有明确证据。
- PTY 三分支与无交互路径可重放且无残留进程。
- 所有 missing/unknown/intentional-diff 被裁决或有明确保留理由。
- 所有门禁全绿，一次提交，提交信息带 `plan53`。

## 开工时定 / 问用户

- AskUserQuestion 是否成为 kloop model-visible tool，以及四前端交互契约。
- EnterPlanMode 是否独立暴露，还是保留现有控制路径。
- StructuredOutput/Workflow 真实 surface 取得证据后的产品适用性。
