# Plan 43 — TUI inline scrollback 后重同步 viewport

## 背景

用户在 iTerm2、未 resize 的情况下看到旧 composer、上下分隔线和 footer/status
留在 transcript 中，新 chrome 又继续画在底部，造成重叠和错位。Plan 41/42 只改
品牌色，不涉及布局。

根因是满高 `Viewport::Inline` 的 overflow 路径：Ratatui 0.29 的
`insert_before` 特例调用 `scroll_region_up(0..1, 1)`，Crossterm 发
`CSI 1;1r` + `CSI 1S`。DECSTBM 规范要求 top margin 小于 bottom margin，
单行 region 不可移植；iTerm2 下物理屏幕可能整体上滚，但 Ratatui 只恢复顶行，
内部 diff buffer 仍认为底部 chrome 没动，下一帧便留下旧内容残影。

## 范围与决定

- 保留满高 inline viewport、`scrolling-regions` feature 和终端原生 scrollback。
- 不改 cell 渲染、overflow 选择、底部布局、颜色或事件模型。
- 每批 finalized blocks 全部 `insert_before` 成功后，调用一次公开的
  `Terminal::clear()`，再 drain App 尾部；现有下一帧完整重画 viewport。
- 顺序锁定为 `insert_before(all) → clear → drain_committed → draw`：clear 失败时
  不提前丢 App 状态；inline clear 不清 native scrollback。
- 本次不混入 resize 扩高问题：Ratatui viewport 仍受启动高度限制，而
  `commit_overflow` 使用当前物理高度，另记 HANDOFF 后续账。

## 实现

- `crates/tui/src/lib.rs`：抽泛型 `insert_scrollback_blocks`，集中历史插入和批末
  viewport 重同步；`commit_overflow` 继续负责测量、选择和 drain。
- 回归测试使用满高 `Viewport::Inline` + `TestBackend`：先画带 transcript/rules/
  composer/footer 的 frame，插入多块历史后断言 scrollback 保留、visible viewport
  被 clear，再完整 draw 后只恢复一套正确 chrome。删除批末 clear 时测试应失败。
- README、Cargo 注释、plan 38 历史结论和 HANDOFF 当前状态/教训同步。

## 完成标准

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `cargo run -p kloop -- --mock`
- PTY 应答 CPR 后验证 overflow 批末 clear + 完整重画；真 iTerm2 固定窗口验证
  多轮 overflow 不再残留旧 chrome，原生回滚/选择/Cmd+F 仍可用。
- 一次提交。

## 完成记录 ✅（2026-07-24）

- `insert_scrollback_blocks` 现将整批 finalized blocks 写进 native scrollback，批末只
  `Terminal::clear()` 一次；`commit_overflow` 在成功重同步后才 drain App 尾部，
  现有下一帧负责完整重画。
- 新增满高 `Viewport::Inline` terminal-level 回归测试，锁定 committed history
  留在 scrollback、当前 viewport 被 clear、下一帧恢复唯一一套底部 chrome。
- README、Cargo 注释、plan 38 更正和 HANDOFF 当前状态/教训均已同步；resize 扩高
  的独立高度账本问题留后续。
- 验证：fmt、clippy `-D warnings`、全 workspace **706 tests**、mock smoke 全绿。
  固定 `120×30` PTY 跑真实二进制恢复 109-message 会话，捕获到 507 次满高
  scroll-region 序列，最后一次后有唯一 viewport clear；pyte 可见屏幕为 1 个 footer、
  2 条 composer rules，无重复 chrome；另一次真实 TUI 驱动 `/exit` 正常以 0 退出。
  物理 iTerm2 的最终手感/原生回滚仍需用户在修复后二次确认（PTY 不模拟 iTerm2 的
  无效 DECSTBM 细节，也不用于断言 native scrollback）。
- 提交：本次（plan 43，见 git log）。

参考：[DECSTBM — VT510 Programmer Information](https://vt100.net/docs/vt510-rm/DECSTBM.html)。
