# Plan 9 — TUI

> 一个会话完成。开工前先读 docs/plan/HANDOFF.md。参考:codex `codex-rs/tui`(ratatui)的整体形态,但只做最小可用。

## 目标

ratatui 终端界面:滚动的会话流 + 输入框 + 工具调用状态,替代裸 REPL 成为默认入口(REPL 保留为 `--plain`)。

## 设计要点

- **新 crate `crates/tui`**,依赖 core/protocol/provider;cli 变薄:解析参数后分发到 tui 或 plain REPL。
- **Ui trait 是现成的缝**:TUI 实现 Ui(text_delta 流进会话流、note 进状态行);若 Plan 8 已做,Approver 对应确认弹层。
- **事件循环**:crossterm 事件 + run_turn 并发——run_turn 在 tokio task 里跑,Ui 实现经 mpsc 把渲染事件发给 UI 循环(Ui 方法是 &self 同步的,发 channel 正合适)。Ctrl+C 触发 CancellationToken(沿用现有中断语义),Ctrl+D 退出。
- **最小界面**:上部会话流(用户/助手/工具行,工具行折叠只显示命令与状态)、底部单行输入。别做主题/鼠标/多窗格。
- 布局跟随终端 resize;长输出截断显示(完整内容本来就在 offload/历史里)。

## 测试

TUI 难 e2e,守住两条:Ui→channel 事件序列的单测(mock 渲染端收到的事件流断言);核心渲染函数(把 Message 列表变成行列表)的纯函数测试。手工验收清单写进 plan 完成记录。

## 完成标准

fmt/clippy/test 全绿;真 key 手工验收:流式输出、工具状态、Ctrl+C 中断、resize 不花屏;README 更新。
