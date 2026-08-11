# Plan 74 — Task graph 的 TUI 实时投影

> 状态：📋 待实施（2026-08-11）
>
> 基线：`d9d843b`（Plan 73）
>
> 依赖：Plan 20、Plan 38、Plan 39、Plan 71、Plan 72、Plan 73

## Context

当前 root-owned session Task graph 已有稳定 ID、单向状态机、`blocked_by/blocks` DAG 和原子更新，但只走普通 ToolCall；TUI 没有持续可见的任务清单。用户给出的 Claude Code 截图证明，结构化 Task 可以直接表现为当前项、pending/completed glyph、`blocked by #N` 和 completed 折叠。目标是在不恢复 `todo_write`、不解析工具输出、也不建立第二套可写状态的前提下，把 `TaskRegistry` 的完整 snapshot 实时投影到 TUI。

回源确认，用户看到的 Claude Code 行为不是“新 user turn 自动 reset”：当前 2.8.4 在所有可见 Task completed 后等待 5 秒，随后 `resetTaskList()` 删除任务文件但保留 high-water；之后新的 `TaskCreate` 才展开一张新表。30 秒 TTL 只影响 completed 截断优先级。该实现的 check→reset 不是同一原子区间，新 create 恰好落在两者之间时可能被误删；而 pinned 2.1.220 也不能证明这套当前源码行为。

kloop 采纳“完成一轮后开启新 Task epoch”的产品语义，但不复制墙钟自动删除和竞态：旧图保持可查，下一次创建**无依赖的独立 Task**时，若当前图已全部 completed，则在同一 registry 写锁内原子清旧图并创建新 Task；high-water 继续递增，UI只看到新 snapshot。用户中途放弃未完成图时，则由显式 root-only `task_clear` 开启新 epoch。

旧 kloop Todo 的“commit 后发布 full snapshot”可复用，但 transcript-bound `Cell::Todo`、历史 replay 和专属 wire 不可复活；当前 Claude Code 的 `TaskListV2/useTasksV2` 提供 glyph、blocked hint 和截断优先级的视觉参考，CodeWhale 也采用 canonical snapshot → UI projection。以上只作交互参考，不外推 parity。

本计划只使用 **Task graph/Task tools** 作为当前产品名；代码不引入 `TaskV2` 类型或版本分发。Plan 71–73 等历史标题及固定取证仍保留当时的 “Task V2” 措辞。

## 产品契约

- `TaskRegistry` 仍是唯一真值；TUI 只保存带 revision 的不可写 full snapshot，不解析普通 ToolCall output，也不应用 task-local patch。
- Task tools 扩为 `task_create/get/update/list/clear`。`task_clear {}` 只清 Task graph、保 ID high-water；不清 conversation，也不停止 Agent/Program/Workflow/Bash。它只供 depth-0 root 在用户明确放弃或替换当前计划时调用。
- 当现有 graph 非空且全部 completed，新 `task_create` 的 `blocked_by` 为空时，create 在同一写锁内自动 rollover：先清旧 records，再创建新 pending Task，只发布包含新 Task 的一个 snapshot。带旧 Task dependency 的 create 视为同一 graph 延续，不 rollover。
- 自动 rollover 或显式 `task_clear` 都必须先完成 strict parse/validation；失败不得清旧图、消耗 ID或推进 revision。
- 成功且改变面板投影的 `task_create/task_update/task_clear` 发布 `TaskGraphUpdated`；失败、`task_get/list`、同值 patch 和仅 description 更新不发布。
- snapshot 在 registry 写锁内与 commit 一起生成，包含 numeric-ID 稳定投影 `id/subject/status/blocked_by/blocks`，不含长 description、owner 或 execution identity。
- `/clear` 清图但保 ID 高水位，无条件推进 revision，并把精确的 revisioned empty snapshot 送给 TUI，作为压过旧事件的 reset fence。
- TUI graph 跨 turn/steer 保留，graph 非空时默认可见且**固定排在输入区上方**，不进入右侧栏或 transcript 历史；`Ctrl+T` 只切换显隐，重新显示时仍来自最新 snapshot。现有 in-process TUI fork 仍共享同一个 `Config.tasks`，所以 fork 后继续显示同一 graph；解决 fork runtime rebinding 不在本计划。新进程、resume、server thread/fork 仍从空 registry 开始。
- 首片只做 TUI。plain 不打印 checklist，server/headless 不新增 Task notification/native item；四工具继续保留 ordinary `toolCall` 生命周期和 root-only/result-only child 边界。

## 实施步骤

### 1. 原子 full snapshot 与 revision

修改 `kloop/crates/core/src/tools/task.rs`：

- 在 `TaskRegistryState` 增加单调 display revision；初始空图为 revision 0。
- 提炼 typed `TaskGraphSnapshot { revision, tasks }` 与紧凑 `TaskGraphTask`，复用现有 `task_summary`/BTreeMap 顺序和 reverse `blocks` 计算；`task_list` 也复用同一投影 helper，避免两套图算法。
- create 在现有写锁内完成验证、可选 epoch rollover、插入、ID 前进、revision 前进和 full snapshot。rollover 只在旧图非空、全部 completed 且新任务无 `blocked_by` 时发生；错误 create 保留旧图。
- update 保持 candidate-then-commit：
  - subject/status/blocked_by 实际变化时 commit、推进 revision 并返回 snapshot；
  - 仅 description 变化仍正常 commit，但不刷新面板；
  - 同值 patch 继续幂等成功，不推进 revision；
  - 任意验证失败不 commit、不推进 revision、不产生 snapshot。
- `clear()` 在同一写锁内清空 tasks、保留 `next_id`、即使原本为空也推进 revision，并返回精确 empty snapshot；供 `task_clear` 与 `/clear` 复用。
- `task_clear` 使用 strict empty-object parser；成功返回清空数量和 empty graph（稳定 JSON），再发布同一 snapshot。它是 graph epoch reset，不具备 execution cancellation 语义。
- 不把 Ui/channel 塞进 registry；registry 保持纯状态服务，tool wrapper 拿 mutation 返回的 snapshot 后再走现有 `Ui::emit`。

必须发送 full graph 而非单 Task：create dependent 会改变 blocker 的 `blocks`，替换 `blocked_by` 也会同时改变旧/new blocker 的反向投影。

### 2. Typed core event，非公开 wire

修改 `kloop/crates/core/src/event.rs`、`tools/task.rs` 与 `tools/mod.rs`：

- 注册 strict `task_clear {}`，纳入现有 root-only catalog/runtime gate、pure session-memory permission、create/update 同一串行 mutation 分类和 Program/Workflow/child 排除；不做旧名 alias。
- 新增 `Event::TaskGraphUpdated(TaskGraphSnapshot)`。
- 仅成功且产生 snapshot 的 create/update/clear 调用 `ctx.ui.emit`；事件顺序自然是：
  `ItemStarted → TaskGraphUpdated → ItemCompleted`。
- `Event::as_note()` 对它返回 `None`，避免 plain 每次 mutation 倾倒整张图。
- `kloop/crates/server/src/wire.rs` 显式把该 event 投影为 `None`，锁定 server/headless 无新 public wire；Task tool rows 仍是普通 started/completed pair。

### 3. `/clear` 与启动顺序

修改 `kloop/crates/core/src/commands/{mod.rs,clear.rs}` 与 `kloop/crates/tui/src/lib.rs`：

- 给内部 `SlashResult` 增加可选 Task graph snapshot；只有 `/clear` 携带 registry 返回的 empty snapshot，其他命令为 None。该字段不序列化为 server 协议。
- TUI worker 对 `/clear` 严格发送：
  1. `ClearTranscript`
  2. `Core(TaskGraphUpdated(empty_snapshot))`
  3. 既有 system 文本
- `ClearTranscript` 只清 transcript/streaming/index state，不私自猜 graph revision；empty event 才清 panel。
- TUI 启动在 Config 构造后、worker 接管前，用 `cfg.tasks.snapshot()` 经同一 event seam seed 首帧。正常 fresh/resume 是 revision 0 empty，但不另开 direct-read 路径。
- `Forked` 重建 transcript 时不清 `App.task_graph`，与仍存活的同一 registry 保持一致。

### 4. 独立 live panel，而非 transcript Cell

修改 `kloop/crates/tui/src/app.rs`：

- `App` 增加当前 `TaskGraphSnapshot`（初始可为 None）和纯显示开关 `show_task_graph`（默认 true）；`apply_core` 只接受严格更高 revision 并全量替换，重复/较旧事件 no-op。
- `Ctrl+T` 在 graph 非空时切换 panel 显隐；它不修改 snapshot、revision 或 registry，跨 turn/fork 保留。footer 按状态显示 `ctrl+t to hide tasks` / `ctrl+t to show tasks`，并复用现有宽度预算避免挤掉 mode/context。
- Task graph 不新增 `Cell::TaskGraph`：普通 Cell 会被 `commit_overflow` 写入 native scrollback 并冻结，无法继续安全更新。
- graph event 不创建 transcript row，不修改 activity、assistant/thinking stream 或 `last_note`；`TurnEnded` 也不清它。

修改 `kloop/crates/tui/src/render.rs`：

- Task lines 作为**非 Cell 的 live chrome**追加到当前 `transcript_area` 的 bottom-anchored tail：顺序固定为历史 transcript → 现有 activity line → Task checklist → input 上边界 → composer。这样只要 panel 可见，它就始终紧贴输入区上方，与截图一致；不做右侧栏，也不会随历史滚入 native scrollback。
- `draw` 与 native-scrollback `commit_overflow` 共用同一个 live-chrome 高度 helper，activity + Task lines 都从可提交高度中扣除，避免屏幕保留高度和冻结预算分叉。
- confirmation/question、completion menu、fork picker 打开时可临时隐藏 checklist；极小终端优先保护 composer、footer 和至少一行 transcript。overlay 关闭后 panel 仍在输入区上方恢复。
- 首片无 panel 焦点、scroll 或编辑交互；唯一按键是全局 `Ctrl+T` 显隐。使用确定性硬上限（建议 8 terminal rows）。

视觉与截断规则：

```text
✻ 迁移同步 usage 消费方… (25m 46s · ↓ 38.2k tokens)
  ⎿ ◼ 迁移同步 usage 消费方
     ◻ 完成 Stage 3 验证与文档 › blocked by #5
     ✔ 收紧 Gemini 流终态
     ✔ 删除 Proxy 重复观察器
      … +2 completed
────────────────────────────────────────────────────
> 
```

- 不额外画 `Tasks N/M` 标题，也不在正常行前重复 `#id`；用现有 activity 作为组头，checklist 首行加 `⎿`。稳定 ID 只在依赖提示中按 canonical `#N` 显示，完整 ID/description 仍由 `task_get/list` 提供。
- `◼` cyan/brand：in_progress；`◻` dim：pending；`✔` green：completed，completed subject 同时 dim + strikethrough，贴近截图。
- pending 只显示仍未 completed 的直接 blocker，并在同一行尾追加 `› blocked by #1, #7`；subject 和 hint 共同走现有 display-width-aware 截断，CJK/窄宽不越界。
- 选择顺序：in_progress → unblocked pending → blocked pending，同组按 numeric ID；completed 在其后按 numeric ID 稳定显示。
- completed 最多显示 3 条，其余汇总为 `… +N completed`；若 unfinished 也超高，保留优先项并显示 `… +N unfinished`。首行 `⎿`、每个 Task 和汇总各占一行，整个 live chrome 有硬上限。
- 首片不复制 Claude Code 的 30 秒 recent-completed/5 秒墙钟 reset：当前 Task 无 completion timestamp，定时删除还会引入 check→reset 竞态。completed 使用确定性折叠；全 completed graph 在下一次无依赖 create 时原子 rollover，或由显式 `task_clear` 立即重置。
- 不新增 activeForm；in_progress 直接显示 canonical subject。

### 5. 保持旧边界并清理当前术语

- 不恢复 `todo_write`、`todo_cell`、`Item::Todo`、plain checklist、native `todo` item 或历史 replay；旧实现只作为 full-snapshot 和 palette 测试模板。
- 不隐藏或改写普通 Task ToolCall row；panel 是 session graph 的额外只读投影，不是另一种工具结果。
- 不引入 Team、owner、assignee、claim、Task↔Agent 绑定、持久化、单 Task 删除、筛选或分页；`task_clear` 只重置整张 graph，也不会隐式 interrupt/stop 任何执行资源。
- Plan 52 native report/verifier同步第五个 root-only `task_clear`、child forged rejection、ordinary ToolCall、high-water/epoch rollover与无 public wire；matrix把它记录为 kloop intentional graph-reset extension，不改 pinned Claude Code raw/normalized fixture。
- 同步 README、HANDOFF、capability report 和 refs 当前说明；定向检查 matrix/static evidence 中“当前 kloop 无 Task UI/只有四工具”的活跃 claim并同步 generated artifact。
- 当前 README/HANDOFF 和源码注释改称 Task graph/Task tools；Plan 71–73 历史、pinned Claude Code 取证和当时事实不回写。

## 关键文件

- `kloop/crates/core/src/tools/task.rs`：revision、原子 mutation snapshot、clear fence。
- `kloop/crates/core/src/event.rs`：typed internal projection event。
- `kloop/crates/core/src/commands/{mod.rs,clear.rs}`：精确 empty snapshot 交接。
- `kloop/crates/tui/src/{app.rs,render.rs,lib.rs}`：snapshot state、panel、布局与启动/clear/fork 顺序。
- `kloop/crates/server/src/wire.rs`、`kloop/crates/server/tests/server.rs`：锁定无新公开 wire 和普通 ToolCall。
- `docs/plan/74-task-graph-tui.md`、README/HANDOFF/capability/refs 当前说明。

## 验证

1. Core registry/event：
   - 初始 revision 0；create、可见 update和clear各推进一次；description-only/no-op不推进；所有失败零 event。
   - all-completed + 无依赖 create原子替换成新 epoch且 ID继续前进；有 dependency create保留旧图；未完成图不会被普通 create隐式清空。
   - `task_clear` string/unknown-field等非法输入不清图；合法 clear返回稳定 JSON/empty snapshot，但不改变 BackgroundExecutions/Shell registry。
   - clear 及重复 clear 均返回更高 revision empty graph，下一 create 不复用旧 ID。
   - RecordingUi 锁 `ItemStarted → TaskGraphUpdated → ItemCompleted`；get/list/失败无 graph event；`as_note()==None`。
2. TUI App：
   - full replacement、stale/repeated revision 拒绝、跨 TurnEnded/steer/fork 保留、clear 只接受精确 empty event。
   - graph event 不产生 Cell，不进入 native scrollback 或历史 replay；`Ctrl+T` 只改显隐且跨 update/turn/fork 保持。
3. Render golden/TestBackend：
   - 三状态 glyph/color、completed strikethrough、numeric order、inline `› blocked by #N`、completed/unfinished 准确折叠。
   - 截图同构场景：activity 在上、checklist 固定紧贴输入区上方，含 1 active、blocked pending、多个 completed 和折叠行。
   - `Ctrl+T` 的 hide/show footer hint、CJK、窄宽、极小高度、confirm/question/menu/fork picker、scrollback commit 同时覆盖且不遮挡输入。
4. 非 TUI 回归：
   - server/headless 明确没有 TaskGraph notification；现有 `task_create` ordinary ToolCall 和 thread isolation 测试不变。
   - root-only、child result-only、DAG/owner strict reject和server thread isolation保持；Plan 52/matrix显式更新为五个 Task tools，mock/headless仍只输出普通 ToolCall。
5. 全质量门：fmt、all-target/all-feature clippy、workspace tests、matrix check、full/corpus verifier、`git diff --check`。
6. 端到端：
   - `--mock` TUI 走 create/dependency/update/list，人工与 TestBackend 确认 panel 实时替换、折叠和 `/clear`；再覆盖 all-completed 后新 create 自动换 epoch，以及未完成 graph 通过 `task_clear` 重置。
   - 使用现有 gitignored provider env，OpenAI Chat 与 Anthropic 各跑一次真实 Task lifecycle：完成一轮后创建独立新 Task 得到干净 panel；另一路模拟用户放弃未完成任务，模型显式 `task_clear` 后重新规划。只记录过滤后的 event/screen 字段，不保留 key、endpoint 或 raw response。
