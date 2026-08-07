# Plan 66 — 后台工具资源化命名

> 状态：✅ 已完成（2026-08-07；plan66 提交，SHA 以本文件所在提交为准）
>
> 依赖：Plan 26、Plan 51、Plan 52、Plan 59

## 背景

现有 `task` 实际运行子 agent，却会与后续 Task V2 结构化任务图争用名称；`stop_agent` 实际还能停止 `program-N` 与 `workflow-N`；`wait` 不接收 ID、也不读取结果。工具名、资源 ID 与实际生命周期没有一一对应。

## 已确认契约

| 资源 | 启动/运行 | 查询/等待 | 停止 | ID |
|---|---|---|---|---|
| Shell | `bash {background:true}` | `bash_output` | `stop_bash` | `bg-N` |
| Agent | `run_agent {background?:bool}` | `wait_for_activity` | `stop_agent` | `agent-N` |
| Program | `run_program {background?:bool}` | `wait_for_activity` | `stop_program` | `program-N` |
| Workflow | `workflow`（始终后台） | `wait_for_activity` | `stop_workflow` | `workflow-N`；durable run ID 仍为 `wf_*` |

- `bash_output` 保留；`bash_background` 会误导成第二个启动入口。
- `task` 改为 `run_agent`，把 `task_*` 留给 Task V2。
- `wait_for_activity` 是 session 级 activity barrier：无 ID、不 drain、不充当 output getter，并覆盖 shell/agent/program/workflow。
- `kill_bash` 改为 `stop_bash`；三个回灌型资源各有专属 stop，交叉 ID fail closed 并指向正确工具。
- Bash `run_in_background` 改为 `background`，与 Agent/Program 统一；默认均为 `false`。旧字段、未知字段与错误类型明确报错，不能静默以前台执行；`wait_for_activity` 同样在执行器层严格拒绝资源 ID 与非整数 timeout。
- 不提供旧名 alias 或双分发；旧名只返回定向迁移错误。

## 实施

1. 公开 ToolDef、dispatcher、权限/并发分类和 Skill 映射迁到新名；旧名在 external source 前保留拒绝提示。
2. 回灌型 registry 保存 Agent/Program/Workflow kind，复用既有一次终态与取消仲裁；拆出三个 typed stop。
3. `wait_for_activity` 复用 inbox activity generation，并把 `BackgroundShells::running_count` 纳入 idle 判定。
4. 子 agent、Program、Workflow、Bash 的返回文案、Code Mode 暴露面、TUI 标签和测试同步迁移。
5. 更新当前 README/HANDOFF/refs/capability/parity 产物；历史 Plan 与 Claude Code raw corpus不改。

## 非目标

- 不实现 PDF 或 Task V2。
- 不合并 shell 输出文件 registry 与 agent/program/workflow 回灌 registry。
- 不改 `thread/backgroundTask/updated`、协议版本、同步默认值、并发上限、worktree、journal 或 autowake。

## 验证

- 定向覆盖 catalog、旧名/旧字段拒绝、strict `background`/wait parser、四类 ID 的全部 12 个交叉停止、`workflow-N`/`wf_*` 边界、纯 shell wait、non-drain、stop race、Skill/权限/Code Mode/TUI/parity。
- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `python3 -B refs/claude-code-2.1.220/verify.py`
- `python3 -B refs/claude-code-2.1.220/verify.py --corpus-only`
- `cargo run -p kloop -- --mock`
- 真实 key dogfood 后一次 `plan66` commit。

## 完成记录

- 公开 catalog、dispatcher、权限/并发分类、Skill 映射、TUI 标签与文档均已迁到资源化命名；旧名只在 external source 之前返回定向迁移错误。
- `BackgroundExecutions` 保存 Agent/Program/Workflow kind，Shell registry 继续独立；四个 stop 的 12 个跨资源负组合、durable `wf_*` 边界与各资源正确停止均有回归覆盖。
- `run_agent`、`run_program`、Bash 与 `wait_for_activity` 在任何 spawn/持久化前严格解析输入；错误布尔、未知字段或给全局 wait 传资源 ID 不再静默降级。
- `cargo fmt --all --check`、workspace all-target Clippy `-D warnings`、`cargo test --workspace`、full/corpus-only exact-binary verifier、mock 六轮与 `git diff --check` 全绿。
- 真实 Anthropic dogfood 在隔离 HOME 中先后台启动 `agent-1` 与 `program-1`、继续更新 todo、调用 `wait_for_activity`，再验证两个错误 stop 都指向正确工具，最后分别由 `stop_agent`/`stop_program` 成功停止；provider open timeout 经既有重试后闭环，未输出或落盘凭据。
- 提交：本次 `plan66` 收口提交（SHA 以本文件所在提交为准）。
