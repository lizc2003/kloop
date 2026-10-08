# Plan 219 — 后台的 shell 就叫 `Bash`：一个东西不该有三个名字

> 状态：✅ 已完成（提交 SHA 以本条所在提交为准）
>
> 依赖：Plan 38（TUI 工具行/会话头）、Plan 59（后台任务事件）。工具名 `bash` / `bash_output` / `stop_bash` 是照 cc 对齐的**模型契约**，本计划一律不动。

## 起因

用户报「Bash、Shell、BashOutput 很令人困惑」。落下来是同一个轴在同一个界面里有三个名字：

| 你看到的 | 名字 | 出处 |
| --- | --- | --- |
| 前台那条命令的转录行 | **Bash** | `tui/src/toolrow.rs`（`"bash" => "Bash"`） |
| 确认面板那一行 | **Bash command** | `tui/src/choice.rs`、`render.rs` |
| 它被背景化之后的那一条 | **Shell** | `tui/src/render.rs` 的 `background_kind` |
| 读它输出的工具行 | **BashOutput** | `tui/src/toolrow.rs` |
| 停它的工具行 | **StopBash** | `tui/src/toolrow.rs` |

三处具体的别扭：**「Shell」是类别名却站在叶子的位置上**（四个 kind 里 `Agent`/`Program`/`Workflow` 都等于它们的工具名，只有它不对应 `bash`，而不一致的那个恰好最常用）；**`bash_output`/`stop_bash` 是仅有的两个没被译成动词的工具行**（`read_file`→Read、`write_file`→Write、`web_fetch`→Fetch 都译了）；**kind 不带"哪一个 shell"**。

用户拍板：**跟工具名走**。

## 裁决与实现

1. `BackgroundTaskKind::Shell` → **`Bash`**。给 kind 加 `label()`（`core/src/event.rs`），**把两份重复的标签表收成一处**——`core` 的注入注释和 TUI 的 chip 原来各有一份 `Shell/Agent/Program/Workflow`，只改一处会得到"chip 说 Bash、历史说 Shell"，是同一个困惑换个地方复发。
2. wire 上的 `kind` 值 `"shell"` → `"bash"`（`server/src/wire.rs` 的映射 + 夹具）。四个 kind 至此全部等于用户实际调用的那个东西。
3. TUI：chip 标题改成 `Bash(<描述>) <id>`，id 从状态行移上去——那一行讲的是"一个作业"，而同一工具的转录行没有 id，前台/后台的差别由此落在标题上而不是名字上。状态行相应只剩生命周期（`Running`，以及 Program/Workflow 的 `resumable as …`）。
4. `toolrow` 里那两个未翻译的标签译成动词：`bash_output` → **Output**、`stop_bash` → **Stop**（行里已经带 `bg-N` 参数）。

## 一个被代码推翻的设计（记下来免得重走）

原方案是「一 shell 一个变体」：`{Bash, PowerShell, …}` 或给 kind 加 `ShellKind` 载荷，理由是 `Shell` 是 bash 与 powershell 的**类别**（权限层确实是：`Gate::Shell(ShellKind::{Bash, PowerShell})`）。

查代码后不成立：**PowerShell v1 是前台限定的**，`background` 是它明确拒绝的字段（`core/src/tools/powershell.rs` 的输入白名单只有 `command`/`timeout_ms`，报错原文写明 "PowerShell v1 is foreground-only"）。也就是说**后台 shell 任务只可能是 bash**，那个"两个 shell"的组合在后台这一层根本不存在。于是不需要载荷、不需要第二个变体——这是一次**改名**，不是加结构。

## 保留「Shell」的地方（它在那里是对的）

`ShellKind` / `Gate::Shell`（权限层：bash 与 powershell 的类别）、`BackgroundShells` / `ShellRegistry` / `ShellProgram`（机器）、`send_message` 报错里那句 "is a background shell id"（给模型解释 `bg-N` 是什么的散文）、以及 `request_reduction` 的分桶——这些指的是"shell 这一层"，保留。

## 非目标

- 不改工具名 `bash` / `bash_output` / `stop_bash`（模型契约）。
- 不给 powershell 造后台能力（那是另一个决定）。
- 不动 `ProvenanceExecutionKind::Shell`（execution receipt 的类别，与这里正交）。

## 验证

- `make check`（fmt + clippy + test + parity）全绿。
- 行为断言：`event.rs` 的注入注释用例（`Bash(cargo test) · Completed · bg-9 · …`）、`wire.rs` 的 `thread/background_task/updated` 夹具（`"kind": "bash"`）、`tui` 两条后台行用例（标题带 id、状态行只剩生命周期）。