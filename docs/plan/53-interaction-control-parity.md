# Plan 53 — 交互与控制工具对齐

> 状态：✅ 已完成（2026-07-31；提交：本次 plan 53，见 git log）
>
> 母计划：Plan 48
>
> 依赖：Plan 48
>
> 固定目标：Claude Code 2.1.220；版本身份以 Plan 48 manifest 为准。

## 背景

交互与控制能力受 TTY、permission mode、plan state 和 surface kind 影响。Plan 48 只对 ExitPlanMode 建立了 outside/headless 与 PTY approve/reject/cancel 证据；Plan 53 进一步固定 AskUserQuestion、EnterPlanMode、StructuredOutput 和 Workflow 的真实注册、执行和生命周期，并在此基础上实现 kloop 原生能力。

本计划不把相似能力改名冒充 parity：

- 权限审批 `Approver` 不等于一般用户问答。
- `ReportFindings` 不等于 StructuredOutput。
- `run_program` 不等于 Workflow。
- 不逐字节复刻 Claude Code 的 Ink UI；只对齐可观察 contract，并明确 kloop 的 intentional differences。

## 证据闸门（✅ 2026-07-30）

已完成并通过：

```bash
PYTHONDONTWRITEBYTECODE=1 python3 refs/claude-code-2.1.220/verify.py --corpus-only
PYTHONDONTWRITEBYTECODE=1 python3 refs/claude-code-2.1.220/verify.py
```

结果分别为 corpus 与 full exact-binary 全绿。当前未提交证据改动必须保留，包含 collector、PTY driver、raw/normalized fixtures、manifest、static evidence、matrix generator、generated matrix 和 verifier。

### 已固定结论

#### AskUserQuestion

- interactive model-visible tool，不是 permission adapter。
- 基础 registry 有 descriptor；interactive/profile gate 决定实际是否提供。
- schema 固定 1–4 个问题、每题 2–4 个选项、单选/多选、Other、preview、annotations/notes、metadata。
- descriptor 声明 read-only/concurrency-safe，但 `checkPermissions()` 固定进入专用问答 dialog。
- 已采 single、multi、Other、two questions、preview、preview+notes、cancel、invalid、headless。
- headless 不提供；PTY cancel 不产生普通成功 tool_result。

#### EnterPlanMode / ExitPlanMode

- EnterPlanMode 是 interactive model-visible session mode primitive；不弹审批，切换 session permission mode 到 Plan，agent context 禁用。
- 重复 Enter 幂等成功，不重复完整 reminder，也不覆盖进入前 mode。
- ExitPlanMode 拥有 approve/reject/cancel 和恢复生命周期。
- Enter→Exit approve fixture 证明批准后恢复非 Plan 状态；headless 不提供 Enter。
- CC Exit 输入是空 object；kloop 现有 `exit_plan_mode` 需要 inline `plan`，这是保留的产品差异，不伪称 exact schema same。

#### Workflow

- model-visible、feature/policy-gated、始终后台的独立 workflow primitive，不是同步 `run_program`。
- observed schema 包含 inline/named/file script resolution、args 和 resume run ID；strict object 禁止未知字段。
- valid fixture：调用立即返回 `async_launched`、task ID、run ID、script path；随后经历 local_workflow started → phase progress → completed → task notification。
- invalid script 在后台 task 注册前同步拒绝。
- VM 禁止 dynamic import 和非确定性 Date/random；resume 使用 journal/checkpoint。

#### StructuredOutput

- tool-shaped internal synthetic adapter，不是普通主循环工具，也不只限某一个调用点。
- 主工具池显式排除 `StructuredOutput`。
- `agent({schema})` 动态替换成 schema-specialized ToolDef；AJV 校验，成功产生 structured attachment 并结束 child turn。
- 未调用时 query loop 注入强制 nudge；失败有有界重试。
- Stop/SubagentStop hook 也可注入派生 adapter，因此不能把它描述成 standalone model tool。

## 产品裁决（✅ 用户确认）

1. kloop 新增独立 model-visible `ask_user_question`；与权限审批在类型、transport、UI 和结果语义上分离。
2. kloop 新增独立 model-visible `enter_plan_mode`；只切 session mode、重复幂等、depth > 0 禁用。`exit_plan_mode` 继续审批并恢复。
3. `structured_output` 只实现为 internal protocol；首期服务 Workflow `agent({schema})`，不注册到主工具列表。
4. kloop 新增独立 model-visible `workflow`；`run_program` 继续作为 kloop-only CodeAct 工具。
5. 两个脚本 surface 共享 QuickJS/runtime 基础，但抽象边界不同：
   - `run_program` 可直接 `tools.<name>()`，默认前台、可选后台。
   - `workflow` 只能编排 agent，始终后台，有 meta/args/phase/persistence/resume；脚本中完全不存在 `tools`/`__call_tool`。
6. TUI/plain/server 支持一般问答；headless、EOF、断线及不支持 capability 的客户端 fail closed，绝不自动选择答案。
7. kloop-owned 工具（包括内部 synthetic tool）统一使用 snake_case；精确 CC 证据中的目标名仍为 `StructuredOutput`。

## 实施计划

### Slice 1 — Shared interaction contract 与工具注册能力

新增 `rust/crates/core/src/interaction.rs`：

- `QuestionRequest`、`Question`、`QuestionOption`。
- `QuestionAnswer`：按输入 index/顺序保存 selected labels、Other、preview、notes。
- `QuestionOutcome::{Answered, Cancelled, Unavailable}`。
- object-safe async `Questioner` trait，独立于 `Approver`。
- bounded validator：问题/选项数量、multiSelect、preview、文本字段和总 payload 上限；未知字段拒绝。

`Config` 新增 optional Questioner 和明确的 frontend surface capability；更新 `all_tool_defs`、agent allowlist、collision/defer 测试：

- `ask_user_question`、`enter_plan_mode`、`exit_plan_mode`、`workflow` 均 depth-0 only。
- session capability 固定；mode toggle 不改变 tool array。
- 这些 control/orchestration tools 在 `run_program` TypeScript API 生成后追加，绝不进入 `tools`。
- TUI/plain 启用 questions + plan + workflow；server 按握手启用 questions 并支持 plan/workflow；one-shot headless 不提供依赖活跃交互或 detached 生命周期的 surface。
- 子 agent 继承 permission mode，但不获得 session-control tool。

关键文件：

- `rust/crates/core/src/config.rs`
- `rust/crates/core/src/lib.rs`
- `rust/crates/core/src/tools/mod.rs`
- `rust/crates/core/src/agent.rs`
- `rust/crates/core/src/agent_type.rs`

### Slice 2 — AskUserQuestion core tool

新增 `rust/crates/core/src/tools/question.rs`：

- ToolDef 使用冻结 fixture 的 question/options/multiSelect/preview/metadata contract。
- depth-0 guard、严格 parser、Questioner 调用和结果序列化。
- Answered 覆盖单选、多选、Other、多问题、selected preview、notes。
- Cancelled 生成成对的非错误结果，明确用户不回答，避免 dangling history。
- Unavailable 生成 `is_error`，要求模型不要重复等待不存在的交互端。
- 即使兼容 schema 允许 `answers`/`annotations`，executor 只能采用 Questioner 返回值，不能信任模型预填内容。
- 结果只作为对应 tool_result 进入 History，不额外伪造 user message。
- tool 归类 orchestration-only/read-only；deny/显式 ask rule 仍遵循 permission gate 的既有优先级。

Core 测试：完整 schema object、边界和坏类型、Answered/Cancelled/Unavailable、depth-1、连续问题串行、模型 Ask→tool_result→下一轮。

### Slice 3 — Enter/Exit Plan 状态机

扩展 `rust/crates/core/src/tools/plan_mode.rs` 与 `permissions.rs`：

- 把分离的 `mode`/`pre_plan` 原子操作收口为共享 `ModeState` 临界区，提供 `enter_plan()` 和批准后的 `exit_plan()`，避免 TUI shift+Tab、Enter、Exit 的恢复态竞态。
- `enter_plan_mode` 使用空 object schema，depth-0 only。
- 非 Plan → Plan：保存前态、切 mode、发一次 `Event::ModeChanged(Plan)`。
- 重复 Enter：幂等成功，不覆盖 pre-plan，不重复状态事件。
- 下一 sampling round 复用 `agent.rs` 现有动态 plan reminder；不把完整 reminder 写死进工具结果。
- `exit_plan_mode` 保留 inline plan preview、Approver、approve 恢复、reject 留在 Plan、NoApprover fail closed；Exit 不复用 Questioner。
- Enter/Exit 均为 session-control/read-only；deny、sensitive-read、plan hard gate 顺序不变。

测试：Manual/AcceptEdits/Bypass→Plan→Exit 精确恢复、重复 Enter、outside/depth-1、Enter 后 read 允许而 write/side-effect bash 拒绝、approve/reject/no approver、mode 事件与 tool array 稳定。

### Slice 4 — 四前端 Question transport

#### Plain

在 `rust/crates/cli/src/ui.rs` 将 approval/question 组装到共享 terminal interaction object；同时实现 `Approver` 与 `Questioner`，共用一个 async mutex，避免并发 prompt 交错。支持编号单选、逗号多选、Other、preview、notes、cancel 和 EOF；坏输入在本问题内重试，EOF 返回 Unavailable。

#### TUI

在 `rust/crates/tui/src/events.rs`、`app.rs`、`render.rs`（必要时新增 `question.rs`）把 confirm-only queue 提升为单一 typed modal queue：

- `AgentEvent::Question` + oneshot `QuestionOutcome`。
- 纯 App 状态机管理 question index、cursor、multi toggle、Other editor、preview/notes。
- approval 与 question 共享 modal owner，并发到达按队列顺序处理。
- Esc 返回 Cancelled；event/reply channel drop 返回 Unavailable，不挂起 turn。

#### Server

在 `rust/crates/server/src/lib.rs`、`wire.rs`、`tests/server.rs` 增加独立 `question/request` reverse RPC：

- initialize 报告 questions capability并读取 client capability；不支持时 thread 不安装 Questioner。
- request 带 threadId、turnId、question index 和完整 options；response 明确 answered/cancelled。
- pending map 提升为 typed pending interaction；response 依 id/type 解析。
- Drop guard 保证 turn interrupt、disconnect、shutdown 时清理 pending；同时修复旧 approval future 被取消后的 stale entry。
- malformed/unknown/dropped response fail closed，不跨 thread；late background event 继续没有伪造 turnId。

#### Headless

`rust/crates/cli/src/headless.rs` 保留 `DenyApprover`，不安装 Questioner，也不注册 Ask/Enter/detached Workflow；不得读 stdin 等答案或输出假答案。server/headless 共享 event projector 后续支持 Workflow background kind。

前端测试覆盖 answered/cancelled/unavailable、plain parsing/EOF、TUI modal keyboard/queue、server exact JSON/malformed/unknown/disconnect/thread isolation、headless 不阻塞。

### Slice 5 — 安全 RunStore 与 code-mode runtime profile

新增共享 `RunId`/`RunStore`（代表路径 `rust/crates/core/src/tools/run_store.rs`）：

- run id 只允许受限 ASCII component；拒绝 absolute、`..`、separator 和 symlink escape。
- 受控 `.kloop/{program-runs,workflow-runs}/` 下 create-new/atomic replace；manifest/script/args/journal/result/error 有 format version。
- 迁移 `run_program` 的 `resume_from_run_id`，修复当前直接 `.join(run_id)` 可把只读 orchestration tool 变成任意路径写入的风险。
- journal result 从 `String` 升级为 `serde_json::Value`，旧 string entry 继续读；key 纳入 schema、isolation、agent_type/max_rounds 等影响结果的 canonical opts。

在 `rust/crates/codemode/src/lib.rs` 抽共享 runtime profile：

- `run_program` 保持现有 `tools`/`__call_tool`、agent/log/parallel/pipeline、前台/可选后台 contract。
- `run_workflow` 只安装 args、agent、log、phase、parallel、pipeline；引擎层不注册 `__call_tool`，`tools` 和动态隐藏访问均为 undefined。
- 两个 profile 共用 QuickJS memory/stack/CPU/cancel、item/agent caps 和 envelope。
- `HostBridge::call_agent` 从 `Result<String>` 提升为 `Result<Value>`；普通 program agent 包装为 JSON string，现有 run_program 行为保持。
- `phase` callback 更新 Workflow progress；`log` 继续发 Note，不进入最终 return。

Workflow meta 用 `tree-sitter-javascript` 定位并验证首条 `export const meta = <pure literal>`，再在无 host capability 的 QuickJS 中转成 DTO；禁止正则截取。限制 meta name/description/phases 的大小、唯一性和纯数据形状。

关键文件：

- `rust/Cargo.toml`
- `rust/crates/codemode/src/lib.rs`
- `rust/crates/codemode/src/tests.rs`
- `rust/crates/core/src/tools/codemode.rs`
- `rust/crates/core/src/tools/codemode/journal.rs`

### Slice 6 — 独立、始终后台的 Workflow

新增 `rust/crates/core/src/tools/workflow.rs`：

- schema 支持 inline `script`、任意 JSON `args`、受控 `script_path`、`resume_from_run_id`；新运行要求 script，resume 可读取/编辑持久化 script。未知字段与非法组合 fail closed。
- 首版不实现 named registry、嵌套 workflow、budget；这些明确记为 intentional differences。
- 注册后台 task 前完成 schema/meta/syntax/determinism/path 校验；坏脚本同步报错，不创建 task。
- launch 立即返回 task id、`wf_*` run id、script path 和 resume 提示；无 foreground flag。
- Workflow bridge 复用 subagent helper、agent cap、journal、UI；child agent 的真实工具调用继续走 `run_one` 的 allowlist→hooks→permission→sandbox→executor。
- 扩展 `BackgroundTaskKind::Workflow`、`InboxItem::WorkflowResult` 与 wire projection；完整结果/错误落 `result.json`/`error.txt`，step-boundary inbox 注入带 task/run id、bounded result 或 output pointer。
- 复用 `BackgroundTasks::register/attach_abort/finish`，保证 complete/fail/cancel/shutdown exactly-once；`wait`/`stop_agent` 用 task id，resume 用 run id。
- phase/log 实时可见，不污染最终 return，不作为 journal side effect。

测试：depth-0 注册与 headless/child 缺席；tools/__call_tool/fs/net/process/import 不可达；args/meta；parallel barrier、pipeline no-barrier、caps；child tool permission/hook/plan gate；立即 launch；phase；complete/fail/stop/shutdown；inbox boundary；persistence；resume hit/miss；task/run/store/event identity。

### Slice 7 — Internal `structured_output`（CC：`StructuredOutput`）

在 core 新增 internal structured turn contract（代表路径 `agent/structured_output.rs`），使用成熟 JSON Schema validator，并限制 schema/result 大小、深度和 `$ref`：

- `run_turn` 增加内部 options/入口；普通主 turn、task、run_program 默认路径不变。
- 仅 Workflow `agent({schema})` 在该 child request 临时追加 `structured_output` ToolDef，input schema 就是调用方 schema。
- synthetic tool 不进入 `all_tool_defs`、defer count、run_program API 或 frontend capability。
- agent loop 截获调用并 host-side validate；成功记录成对 tool_result、保存 `TurnOutcome.structured_output`，立即结束 child turn。
- mismatch 记录 `is_error` tool_result（bounded JSON path/error）并继续；无 tool call直接结束时追加强制 nudge；到硬上限后 reject Promise，绝不降级成未验证 final text。
- 同一 response 的普通 tool uses 仍通过 dispatch并保持配对；仅合法 structured value 写 journal。
- 从 `task.rs` 抽内部 `SubAgentRequest`/runner，让 task、run_program agent、Workflow structured agent 共用 lineage、worktree isolation、hooks、permissions 和 cleanup。
- 不修改 provider wire、不引入 provider-native output format；动态 ToolDef 已能跨 provider rail传 schema，本地 validation 是最终裁决。

测试：只在 structured child request 出现；object/array/scalar；required/type/enum/nested mismatch；invalid→valid；missing-call nudge；retry exhaustion；普通 tool use 后 terminal；cancel/provider error；无效中间文本不流 parent；JS `result.field`；Value journal round-trip；无 schema agent仍返回 string。

### Slice 8 — Parity、文档、dogfood 与提交

新增/更新 kloop 精确证据（建议 `plan53_parity_tests.rs` 输出 registry/schema/transition/question/workflow/structured lifecycle report），重新裁决 matrix：只有 executable pair 证明的 cell 标 same，其余使用 compatible/intentional-diff。

同步：

- 本文件：完成记录、测试和提交号。
- `docs/plan/HANDOFF.md`：Questioner/Approver 分离、ModeState、Workflow profile、`structured_output`、RunStore 安全教训。
- `rust/DESIGN.md`：四前端 Ask、Plan control、Workflow 与 run_program 区别、resume、internal `structured_output`、headless 降级。
- `refs/README.md`、`docs/capability-report.md`、server native protocol 文档。
- `refs/claude-code-2.1.220/{static-evidence.jsonl,tool-matrix.json,verify.py}` 和生成产物；保留本会话新增 fixtures。

## 完成记录（✅ 2026-07-31）

- 新增独立 `Questioner` typed seam 与 `ask_user_question`；问题 identity 使用稳定 index，Answered/Cancelled/Unavailable 三态不折叠，模型预填 answers 无权替代前端回复。
- `Permissions` 的 mode/pre-plan 收口为同一 `ModeState` 临界区；`enter_plan_mode` 幂等保存前态，`exit_plan_mode` 继续单独走 Approver 审批并精确恢复。
- plain/TUI/server 均接通一般问答：terminal interaction 串行化审批与问题，TUI 使用单一 FIFO modal owner，server 以 `capabilities.questions` 协商独立 `question/request` reverse RPC；headless 与断线路径 fail closed。
- `SurfaceCapabilities` 显式固定 questions/plan/workflow/worktree；四个 session-control surface 均 depth-0、子 agent 不注册，mode toggle 不改变 provider tool array，也不进入 `run_program` TypeScript API。
- `kloop-codemode` 新增独立 Workflow runtime profile与 AST 级 pure-literal meta parser；脚本只得到 args/meta/agent/log/phase/parallel/pipeline，tools/`__call_tool`/fs/net/process/import/Date/random 均不可达。
- 新增始终后台的 `workflow`：同步校验后立即返回 task/run/script identity，phase 与 terminal event 保持同一 task/run/description，结果/错误 step-boundary 回灌；stop、shutdown、失败、resume hit/miss 与 edited managed script 均有专门测试。
- 新增 Unix descriptor-bound `RunStore`：run ID/component 校验、namespace/run directory FD 绑定、artifact openat/no-follow、descriptor-relative atomic rename、advisory run lease 与 Program/Workflow journal 均不再依赖可被 symlink swap 重定向的裸路径；manifest version 统辖同目录 artifact，journal entry 自带 version 且兼容旧字符串 result。非 Unix fallback 只承诺路径复核，不宣称 reparse-point race hardening，留待 Windows backend。
- 新增内部 `structured_output`（CC 精确目标名 `StructuredOutput`）：仅 schema Workflow child 临时注入，schema/result 大小与深度有界、拒 `$ref`、host-side `jsonschema` 复验；invalid/missing 有界重试，合法 object/array/scalar 才终止 child并以原生 JSON Value 入 journal/JS。
- Structured response 中的普通 tool uses 重新合并为一次真实 `dispatch_tools` batch，再按原 response slot 复位结果；回归测试以 barrier 证明 concurrency-safe 普通工具仍真并发且 `structured_output` 插槽前后顺序不变。
- 保留并明确 intentional differences：kloop-owned 工具名用 snake_case；Exit 继续 inline plan preview；Workflow 首版没有 named registry、嵌套、token budget、remote execution或任意 workspace script path；per-agent `effort` 尚无 session-local provider seam，`label`/per-call phase 只保留为未来更富进度树输入，不宣称同形。

### 关键验证

- exact 2.1.220 corpus/binary verifier：通过。
- Ask/Plan、四前端、Workflow/RunStore/`structured_output`、codemode、server wire 的定向与 workspace 测试：通过。
- `cargo fmt --all --check`、workspace `clippy -D warnings`、workspace tests、mock 与真实 API dogfood 的最终结果见本文件“验证”节及提交说明。

## 关键不变量

### Question

- 一般问答与审批是独立 typed seam。
- 只允许 top-level；headless/断线不伪造答案。
- Answered、Cancelled、Unavailable 不互相折叠。
- 多问题/多选保持顺序；模型预填 answers 无权替代人类。
- 每个 tool_use 始终有合法 terminal 结果。

### Plan

- deny/sensitive-read 优先级不变。
- Enter 后下一次 dispatch 立即受 Plan hard gate。
- 重复 Enter 不覆盖 pre-plan。
- Exit approve 精确恢复；reject/cancel 保持 Plan。
- 子 agent 继承 mode 但不能 Enter/Exit。
- mode 前后 tool definitions 稳定。

### Workflow

- 独立 tool，不是 run_program alias；始终后台。
- Workflow script 中不存在 tools/__call_tool。
- child agent 工具仍逐调用过正常安全 gate。
- parallel 是 barrier；pipeline 每 item 独立跨 stage、无 stage barrier。
- background terminal、inbox 和 frontend event exactly-once。
- stop/shutdown 后不得晚到成功结果。
- resume cache 中 object 仍是 object。

### `structured_output`（CC：`StructuredOutput`）

- 不进入主注册面。
- 只在显式 schema Workflow child 中注入。
- host validation 是最终裁决。
- mismatch/缺调用不得成为成功文本；retry 有硬上限。
- 只有 validated value 写 journal并结束 child turn。

## 非目标与有意保留

- 不把 Approver UI 当 AskUserQuestion。
- 不把 ReportFindings 当 StructuredOutput。
- 不把 run_program 改名或计作 Workflow parity。
- 不复制 Ink UI/按键提示/文案字节。
- 不实现 Claude plans 目录/slug；Exit 继续使用 inline plan preview。
- 首版不做 named Workflow、嵌套 Workflow、token budget、remote CCR。
- 不扩 tool-input strictness，不加 provider-native response format。
- 不宣称跨进程恢复 permission mode；resume 仍以现有 session/runtime contract 为准。

## 验证

由窄到宽：

```bash
PYTHONDONTWRITEBYTECODE=1 python3 refs/claude-code-2.1.220/verify.py
cd kloop
cargo test -p kloop-core tools::plan_mode
cargo test -p kloop-core tools::question
cargo test -p kloop-core tools::workflow
cargo test -p kloop-codemode
cargo test -p kloop-tui
cargo test -p kloop-server
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

再从仓库根 `.kloop/env.local` 读取真实配置，dogfood：

- Ask → 回答 → 下一轮继续。
- Enter → Plan hard gate → Exit approve/reject。
- background Workflow phase/completion/cancel。
- Workflow `agent({schema})` invalid→valid 与结构化对象访问。
- script edit + local resume，不重复未变化的 agent calls。


### 最终验收结果（2026-07-31）

以下全部通过：

```bash
PYTHONDONTWRITEBYTECODE=1 python3 refs/claude-code-2.1.220/verify.py
cd kloop
cargo fmt --all --check
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo test --workspace --offline
cargo run -p kloop --offline -- --mock
cd ..
git diff --check
```

真实 API dogfood 从仓库根 `.kloop/env.local` 只注入进程环境，未打印/落盘凭据：

- plain `ask_user_question`：模型发单选 Color，terminal 回 `2`，下一轮精确得到 `ANSWER:Blue`。
- plain Plan：模型 `enter_plan_mode` 后按要求尝试 side-effect Bash，hard gate 拒绝；`exit_plan_mode` 经 y 审批恢复 manual，最终 `PLAN-DONE`；临时 marker 确认不存在。
- plain Workflow + `structured_output` + resume：首个 `wf_*` run 的 schema child 成功产生 `RESUME-53` 后脚本按计划失败；第二次以同一 inline script、`args.fail=false`、原 run ID resume，完成返回；终端观测两次 Workflow launch 但只有一次 child start，证明 JSON Value journal hit 跳过第二次真实采样。
- 默认轨一次真实 Workflow 尝试遇到上游 stream read error；按既有 fallback 纪律改用 `claude-sonnet-4-6` 完成验收，没有把服务端瞬时错误误判为实现失败。

真实会话 transcript、key 与代理地址均只留在被 gitignore 的本地 `.kloop/`，不进入提交。全部证据、实现、测试与文档合为一次 `plan53` commit（本次，见 git log）。

## 完成标准

- 五个 surface 的类型、注册条件、schema、executor 和 lifecycle 有静态/动态/实现三层证据。
- Ask/Plan/Workflow/CC `StructuredOutput` ↔ kloop `structured_output` 的上述不变量均由测试锁定。
- TUI/plain/headless/server 的可用、降级、取消和断线路径明确且 fail closed。
- run_program contract 不回归，resume 路径安全收口。
- matrix 的 missing/unknown/intentional-diff 重新裁决并诚实保留。
- exact verifier、fmt、clippy、workspace tests、mock、真实 dogfood 全绿。
- README、capability report、refs、HANDOFF 与本 plan 同步。
- 一次 `plan53` 提交，提交号回填。
