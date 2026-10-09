# Plan 221 — 后台 bash：标签归标签，命令归命令

> 状态：✅ 已完成（提交 SHA 以本条所在提交为准）
>
> 依赖：Plan 219（后台行以 id 开头）、Plan 220（harness 等待提醒）、Plan 156（bash 的 display-only `description`）。

## 起因

用户报「异步 Bash 的命令不单起一行，看不清」：固定的后台任务行把命令塞进标题尾部
（`Bash(bg-N) <命令>`），长命令一到就被宽度截断。根因不在渲染——`BackgroundTask.description`
对 bash 存的就是命令本身（`bash.rs` 的启动与终态两处），把「模型的一句标签」和「命令」
挤进了同一个字段。

## 裁决与实现

1. DTO 拆开：`description` 在所有 kind 里都只表示模型写的一句标签（可为空），命令进
   `command: Option<String>`（只有 `Bash` 有）。新增 `BackgroundTask::label()`：描述非空用它，
   否则回退命令——「一句话认这个作业」的唯一来源。
2. TUI 生命周期行：标题 `Kind(id) 描述` 不变，其后为 `Bash` 增一整行 `  $ 命令`（复用转录行
   的 `toolrow::command_line`，改为 `pub(crate)`），命令不再挤标题、也不会被截。
3. 一句话投影统一走标签：note（`event.rs` 的 `background_task_note`）与终态 inbox 消息
   （`bash.rs` 的 `summary`）。终态消息不再抄整条命令、不再重复状态——`[{id}] {status}` 已带上。
4. wire 增 `command`（非 bash 为 `null`）；不做旧语义的兼容分支。
5. 模型侧不动：启动回执与 plan 220 的 harness 提醒都只认 `bg-N`，且是实测文案，改它们要重跑
   plan 220 的采样。本次只改显示层与去掉冗余的历史文本。

## 一个被代码推翻的说法（订正 plan 219）

Plan 219 把 `Kind(描述) · 状态 · id` 那条 note 记成「注入进历史、模型面向、被实测过的措辞」。
照代码它是**前端降级**：`background_task_note` 只经 `Event::as_note` 喂 TUI 的 `last_note`、
plain/headless 的打印与 server 的 note 事件，core 从不把它写进 `History`；真正进模型历史的
后台提醒是 plan 220 的 harness `<system-reminder>`。所以改这条 note 是显示层，不需要模型实测。
教训已记 HANDOFF。

## 验证

- `make check`（fmt + clippy + workspace test + release 测试 + parity）全绿。
- 断言：`background_shell_emits_one_start_and_one_terminal_update`（`description`/`command`
  两字段、`summary` = `标签 · 详情`）、`background_shell_without_a_description_falls_back_to_the_command`、
  `background_task_note_formats_workflow_phase_and_shell_output`（带标签与回退两条）、
  `a_background_shell_command_gets_its_own_row`（命令单起一行、长命令在 80/40/24 列下截不断
  `Bash(bg-N)`）、wire 夹具带 `command`。
- 不跑真实模型：显示层 + 去冗余文本，不断言行为变化；历史重放靠 `Injected::Shell` 的类型标记。