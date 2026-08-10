# Plan 69 — 后台三原语生命周期展示

> 状态：📋 待实施（2026-08-07；本文件仅规划，不含生产代码修改）
>
> 依赖：Plan 51、52、53、66、68

## 背景

当前 Claude Code 会把一次后台子 Agent 展示为 `Agent(description)`，立即返回 opaque task ID，并在 UI 中提供后台状态、来源标签和管理入口；旧式 `TaskOutput` 路径也可能让模型连续查询 `still running`。这套体验里，“人能看懂正在运行什么”和“完成后自然收到结果”值得借鉴，但通用 Task ID、模糊 Task 命名和模型轮询不适合直接复制到 kloop。

kloop 已有更严格的运行时基础：模型工具名是 `run_agent`、`run_program`、`workflow`；后台布尔统一为 `background`；Shell / Agent / Program / Workflow 分别使用 `bg-N` / `agent-N` / `program-N` / `workflow-N`；Program 与 Workflow 的 durable `run-*` / `wf_*` 只用于恢复；`wait_for_activity` 是无 ID、non-draining 的 session activity barrier；TUI、plain 和 native server 都会在 Inbox activity 后自动 delivery。

本计划只借鉴 Claude Code 的**展示层、来源辨识和生命周期可见性**，不重开 Plan 66/68 已拍板的模型协议与恢复契约，也不引入 `TaskOutput` 风格轮询。

## 借鉴结论

| Claude Code 表现 | kloop 裁决 |
|---|---|
| UI 显示 `Agent(description)` | 借鉴：人类界面使用 Agent / Program / Workflow 产品名和短 description。 |
| Agent tool 使用 `run_in_background` | 不照搬：kloop 继续统一使用 `background`。 |
| 后台任务返回 opaque task ID | 不照搬：继续使用资源类型可见的 typed execution ID。 |
| `TaskOutput(id)` 查询状态/输出 | 不照搬：继续自动投递；`wait_for_activity` 只等待 activity，不读取结果。 |
| `Message from Explore` | 部分借鉴：消息同时显示产品类型、可选 agent type 和实例 ID，不能只显示类型。 |
| `↓ to manage` / 可折叠状态 | 本计划先落结构化 lifecycle row；可操作资源管理面板需 snapshot/control 协议，单列后续阶段。 |
| `Allowed by auto mode classifier` | 不混入生命周期：权限决策、资源状态和 Agent 消息保持三类独立事件。 |

## 已拍板契约

1. **模型 wire 不改名。** 继续使用 `run_agent`、`run_program`、`workflow`、`wait_for_activity` 和四个 typed stop；不增加 `Agent` PascalCase alias、通用 `task_output`、`task_status` 或 `stop_task`。
2. **后台参数不改名。** Agent、Program、Bash 继续使用 `background`；Workflow 继续始终后台。
3. **typed ID 职责不变。** `agent-N` / `program-N` / `workflow-N` / `bg-N` 用于 session lifecycle；`run-*` / `wf_*` 只用于恢复，不能 stop。
4. **模型工具名与 UI 产品名分层。** JSON tool name 保持 snake_case；TUI/plain/native client 面向人显示 `Agent(...)`、`Program(...)`、`Workflow(...)`，不能把 wire 名直接当产品标题。
5. **Agent 与 Program 增加 optional `description`。** 它只作受限长度的展示元数据，不替代 prompt/source，不进入子 Agent prompt，不改变 Program source manifest、byte-identical resume、journal key 或结果。缺省时沿用现有安全 preview；显式空白、错误类型和控制字符在任何 spawn/persist 前拒绝。
6. **Workflow 只有一个 description 真相。** 继续使用 managed script 的 `meta.description`；不再从顶层 ignored 字段制造第二来源。
7. **完成仍由事件驱动。** 后台终态进入 Inbox，在 step/final/idle delivery boundary 自动投递；`wait_for_activity` 保持 non-draining，不能演化为 output getter，也不鼓励短周期重复调用。
8. **生命周期消息必须可归属。** 固定携带资源产品名和 execution ID；Program/Workflow 额外显示 durable run ID。Agent 有 agent type 时可显示为 `Agent · Explore · agent-3`，但 type 不能代替实例 ID。
9. **权限与状态分层。** Allow/deny、Running/Completed/Failed/Cancelled、progress/completion message 分开呈现，UI 文案不成为协议常量。
10. **结果预算不回退。** Agent/Program 的超大成功文本继续 offload；Workflow 继续返回 bounded summary + `result.json`；展示 description 不得被拼接成无界上下文。

## 目标展示

模型发出的调用仍是：

```json
{
  "name": "run_agent",
  "input": {
    "description": "加固资源生命周期",
    "prompt": "实现 asset lifecycle hardening，并运行相关测试。",
    "background": true
  }
}
```

面向人的启动/状态展示为：

```text
Agent(加固资源生命周期)
  Running · agent-3
```

Program：

```text
Program(批量验证资源清理)
  Running · program-2 · resumable as run-...
```

Workflow：

```text
Workflow(并行生命周期审查)
  Running · workflow-1 · resumable as wf_...
  Phase: Verify
```

Inbox 注入使用稳定来源 framing：

```text
[Agent agent-3]
...

[Program program-2] run run-...
...

[Workflow workflow-1] run wf_...
...
```

这里的标题和状态是 presentation；机器协议继续依赖 typed event 字段，不能解析 UI 文本推导状态。

## 当前缺口

- `run_agent` 与 `run_program` 没有独立 description，展示标题来自 prompt/source preview。
- 后台 Agent 启动文案仍混用 “Sub-agent”，没有统一到产品名 Agent。
- Program 的启动回执已有 `program-N` + `run-*`，但 `BackgroundTask` native projection 尚未稳定携带 Program `run-*`。
- Workflow 已有 `meta.description`，但顶层 ignored description 容易让实现或文档误以为存在第二真相。
- Inbox 的 typed item 已区分三原语，但最终注入 framing 没有统一显示 `Agent/Program/Workflow + execution ID`。
- TUI 把 `BackgroundTaskUpdated` 降级成无关联的 Note；同一资源的 running/phase/terminal 不能按 ID 原位更新。
- plain/headless note 的名称和 description 不统一。
- native server 已有 `thread/backgroundTask/updated` 和 optional `runId`，但没有统一投影 Program durable ID。
- `stop_program {program_id:"run-*"}` 尚需像 `wf_*` 一样给出“恢复 ID 不能 stop”的定向诊断。
- 两套后台 registry 只有运行计数，没有统一只读 snapshot；当前不具备安全实现完整资源管理面板的前提。

## 实施

### 1. 统一 description 输入与展示来源

- 修改 `kloop/crates/core/src/tools/subagent.rs`、`tools/codemode.rs`、`tools/workflow.rs`、`tools/mod.rs`。
- 给 `RunAgentInput`、`RunProgramInput` 和 JSON Schema 增加 optional `description`，执行器仍 `deny_unknown_fields` 并独立验证类型、非空、长度和单行控制字符。
- Agent 缺省回退 `agent_preview(prompt)`；Program 缺省回退 `program_preview(source)`；显式 description 只传给展示和 lifecycle event。
- Workflow 始终从 `PreparedWorkflow.meta.description` 取值；顶层 ignored 字段不能覆盖。
- 启动回执统一输出产品类型、description、execution ID；Program/Workflow 保留 durable run ID 和既有 artifact/script 指针。
- TUI tool row 将 wire 名投影成人类标签：Run Agent、Run Program、Workflow、Stop Agent、Stop Program、Stop Workflow；detail 优先显示 description。

### 2. 补齐 typed lifecycle 投影

- 修改 `kloop/crates/core/src/event.rs`、`inbox.rs`、`tools/background_executions.rs`、`tools/subagent.rs`、`tools/codemode.rs`、`tools/workflow.rs` 与 `kloop/crates/server/src/wire.rs`。
- `BackgroundTask` 明确：Agent/Shell 无 durable run ID；Program 使用 `run-*`；Workflow 使用 `wf_*`。
- Program 的 running/terminal event 都携带同一个 `run-*`；唯一 terminal 和 stop/shutdown 仲裁不变。
- `InboxItem::ProgramResult` 保留 durable run ID；三种结果统一使用“产品类型 + execution ID + optional durable ID” framing。
- 给 Program `run-*`、Workflow `wf_*` 及所有跨资源 ID 补定向 fail-closed stop 提示，不能落入泛化 “not running”。
- native method 继续是 `thread/backgroundTask/updated`；Program 实际使用既有 optional `runId`，不升级协议版本、不新增 UI 文本字段。

### 3. 将 TUI 生命周期从 Note 升级为结构化 row

- 修改 `kloop/crates/tui/src/app.rs`、`render.rs`、必要时新增小型 presentation module；不复用前台 `Cell::Agent`。
- 增加 `BackgroundTask` 专用 cell，以 `task.id` 建立 live-tail 索引。
- 同一 ID 的 Running → phase/detail → Completed/Failed/Cancelled 在仍可变的 live tail 中原位 upsert，显示 kind、description、execution ID、optional durable ID、status、detail、output path。
- 已经进入不可变 scrollback 的 running row不回写历史；晚到 terminal 追加一张明确关联同一 ID 的 terminal row。
- 颜色和图标只表达 typed status，不从 detail 文本猜测成功或失败。
- plain/headless 使用同一个 presentation formatter 输出简洁单行 lifecycle，不复制 TUI 逻辑。

### 4. 收紧等待与自动投递说明

- 更新模型可见 tool description：后台资源会在 activity 后自动 delivery；只有调用者确实需要阻塞等待任意 activity 时才使用一次 `wait_for_activity`，不得把它写成资源轮询循环。
- timeout 结果明确说明“未消费任何结果、仍会自动投递”，避免模型把 timeout 当失败或立即连续重试。
- 不新增 status/output getter；不从 registry 或 Inbox 暴露未完成 Agent 的中间推理文本。
- 保持 TUI、plain 和 native server idle delivery 同一语义，并用测试固定“终态事件一次、delivery 一次”。

### 5. 文档与真实 dogfood

- 同步 `kloop/README.md`、`docs/capability-report.md` 和完成时的 `docs/plan/HANDOFF.md`。
- 文档分别列出 wire tool name、UI product name、execution ID、durable ID 和 stop/resume 用途。
- 用真实 Anthropic 与 OpenAI Chat 各运行一组后台 Agent、后台 Program 和 Workflow：模型可正确生成 `description`/`background`，无需 `TaskOutput`，协议事件与自动 delivery 完成闭环。
- dogfood 只记录脱敏后的资源类型、计数、状态和 ID 形状；不记录 key、endpoint、raw provider response 或 transcript。

## 后续阶段：可操作后台资源面板

本计划只做结构化 lifecycle row，不立即实现 Claude Code 式 `↓ to manage` 面板。完整面板必须在后续独立计划中先补齐：

1. Shell registry 与 Agent/Program/Workflow registry 各自提供只读 snapshot DTO，不为 UI 合并 ownership。
2. native server 提供初始 hydration/list surface，客户端重连后不能只靠增量事件猜当前状态。
3. 客户端按 typed ID upsert，区分 running 与 terminal retention/cleanup。
4. Stop 操作继续走 type-specific、可审计的控制面，不能变成通用 `stop_task`。
5. 展开结果只读取已投递 preview、offload pointer 或 Workflow artifact，不读取 Inbox、不暴露运行中 Agent 的隐藏上下文。
6. 单独设计键位、筛选、历史保留、session shutdown 和 reconnect 语义后再实施。

这一步不作为 Plan 69 完成条件，避免为了复制一行 UI 文案而仓促引入通用 Task registry 或轮询协议。

## 非目标

- 不修改 Plan 67 的 PDF 工作。
- 不实现 Task V2、Agent Teams、peer mailbox、远程 Agent 或 Agent 间任意进度消息。
- 不追求 Claude Code wire compatibility，也不新增 PascalCase tool alias。
- 不引入 `TaskOutput`、通用 task ID、统一 stop/status/output tool。
- 不合并 Shell registry 与 Agent/Program/Workflow registry。
- 不改变 Program journal v2、source identity、Workflow edited-script resume、并发限制或 session-scoped shutdown。
- 不把 ordinary sub-agent 的中间推理、tool calls 或 workspace state流式注入父上下文。
- 不改变权限分类器；权限 note 只改展示归属，不与 lifecycle event 合并。

## 验证矩阵

| 场景 | 必须证明 |
|---|---|
| Agent description | 可选；显式合法值贯穿回执/event/UI；缺省回退 preview；空白、错误类型、控制字符、未知字段在 spawn 前拒绝。 |
| Program description | 只影响展示；不进入 source manifest、byte comparison、journal key；resume hit/miss 与 Plan 68 完全不变。 |
| Workflow description | 回执/event/UI 只取 `meta.description`；顶层 ignored 字段不能覆盖。 |
| typed IDs | 四类 execution ID 正常 stop；全部跨资源组合、Program `run-*`、Workflow `wf_*` 都给出定向错误。 |
| lifecycle event | Agent 无 runId；Program running/terminal 使用同一 `run-*`；Workflow使用同一 `wf_*`；每个资源只有一个 terminal。 |
| Inbox | 三种注入都含产品类型和 execution ID；Program/Workflow 含 durable ID；offload/artifact pointer 不丢失。 |
| TUI live row | 同一 live ID 的 running/phase/terminal 更新一张 row；scrollback 后追加相关联 terminal row；状态颜色不依赖文本。 |
| plain/headless | 与 TUI 共用 typed formatter 语义，显示 kind、ID、description、status 和 optional run ID。 |
| native wire | method 与协议版本不变；Program `runId` 是既有 optional 字段的 additive 使用；事件无伪造 turn owner。 |
| delivery | `wait_for_activity` 不 drain；timeout 不吞结果；TUI/plain/server idle 均自动 delivery；stop 不注入成功结果。 |
| 无轮询回归 | catalog 中不存在 `TaskOutput`/`task_output`/通用 status getter；真实验收不依赖连续 wait/status calls。 |
| 历史回归 | Plan 52/53/59 parity、Plan 66 typed stop、Plan 68 source/journal/offload/Workflow lifecycle 全部通过。 |

全量执行：

```bash
cd kloop
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cargo run -p kloop -- --mock --headless --json "run the mock demo"
python3 -B ../refs/claude-code-2.1.220/verify.py
python3 -B ../refs/claude-code-2.1.220/verify.py --corpus-only
git diff --check
```

真实 dogfood：Anthropic 与 OpenAI Chat 各覆盖一次后台 Agent、Program、Workflow 的启动、状态、唯一终态和自动 delivery；不以最终自然语言自评代替 event/ID/artifact 证据。
