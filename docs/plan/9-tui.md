# Plan 9 — TUI ✅(24842ac;真 key 手工验收挂账)

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

## 完成记录

**实现**(新 crate `crates/tui`,依赖 core/protocol;cli 依赖 tui 分发):

- 渲染形态:**alternate screen 全屏 + 自维护 cell 缓冲 + 滚动偏移**,不是 codex 的 inline viewport。后者的移植成本不在它的依赖 fork(核实过:ratatui fork 仅 +`expose set_viewport_area` 一个提交,crossterm fork 仅 +`query_fg/bg_color`;scrolling-regions 在上游 ratatui 0.29 本来就有),而在它 vendor 的 ~35KB CustomTerminal(ratatui Terminal 魔改副本)+ insert_history 自写 SetScrollRegion/ResetScrollRegion 命令 + resize reflow 那套机制——对最小可用太重。代价是历史不进终端原生 scrollback。
- 结构(借鉴 codex 的分层,调研结论已入 `refs/README.md` 风格的对比不再重复):
  - `events.rs`:`AgentEvent` 枚举 + `ChannelUi` 同时实现 `Ui` 和 `Approver`,全部经 unbounded mpsc 进 UI 循环;审批决定走 oneshot 回传,**sender 被丢 = Deny**。
  - `app.rs`:纯状态机。cell 流(User/Assistant/Tool/Note),delta 聚进最后一个开放 Assistant cell,工具行/note 会关闭它保序;confirm 用 VecDeque 排队(并发批可能连发);按键 → `Command`(Submit/Interrupt/Quit)交给循环执行副作用。
  - `render.rs`:纯函数 cell→行(CJK 宽度感知的 wrap/truncate、工具行折叠单行 + …/✓/✗ 状态)、光标窗口化的单行输入、居中 y/a/p/n 弹层。
  - `lib.rs`:agent 独立 tokio task 持有 History;`select!` 合并 crossterm EventStream 与 agent 事件;delta 攒批重绘(drain try_recv);panic hook 恢复终端。
- core 的最小配套改动:`Ui` trait 加 `tool_start`/`tool_end` 默认方法(默认退化为现有 note 行为,plain REPL 零改动),`run_one` 改调用它们——TUI 工具行状态的唯一数据源。
- cli:`--plain` 保留裸 REPL;`--mock` 仍走 plain(文档化的无交互验证命令不能变成阻塞 TUI);`build_permissions`/`config_from_env` 参数化 approver + notify,plain 传 CliApprover/eprintln,TUI 传弹层/transcript note。

**验证**(fmt/clippy -D warnings/test 全绿,115 个测试,tui 新增 16):

- 单测按计划守两条线:Ui→channel 事件序列契约;cell→行纯函数渲染。另覆盖 App 状态折叠、confirm 排队与键盘捕获、中断/退出命令。
- 无 key 自动化冒烟(`script` PTY + 假 Anthropic SSE 服务):进出 alt screen 干净、状态行渲染、流式 delta 分帧渲染、工具行 ✓、turn 收尾回 idle、**运行中 Ctrl+C → [interrupted] 回 idle**、Ctrl+D 干净退出(exit 0)。

**挂账 — 真 key 手工验收清单**(需要真终端 + 真 key,自动化无法替代):

- [ ] 流式输出连贯不闪烁
- [ ] 工具状态行:运行中 … → ✓/✗
- [ ] 权限弹层 y/a/p/n 各按一遍;p 落 config.toml 且 transcript 出现保存 note
- [ ] Ctrl+C 中断运行中的 turn,历史合法可续
- [ ] resize 不花屏,窄终端换行正常(中文宽度)
- [ ] `--resume` 进 TUI 继续旧会话
- [ ] `--plain` 行为与之前一致
