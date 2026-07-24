# Plan 41 — TUI 品牌主色去粉红

## 背景

用户不喜欢 TUI 会话头、working spinner、mode 徽标使用的 magenta 粉红主调，同意改成更克制的低饱和青蓝。

## 范围与决定

- `render.rs` 的品牌槽 `BRAND` 从 ANSI magenta 改为 `#6c9aa6` (`Rgb(108, 154, 166)`)。
- 只改 kloop chrome：会话头边框/标题、working spinner、footer mode 徽标。
- 保留 ANSI cyan 的用户·状态·选中语义，以及 green/red/dim 的既有语义。
- 代码块 keyword 的 magenta 属于独立语法语义，不随品牌色修改。
- 回归测试锁定 RGB 值，并断言品牌色不退回 magenta、也不与 semantic cyan 合并。

## 完成标准

- README、plan 38 历史决定和 HANDOFF 同步新品牌色。
- `cargo fmt --all --check`、clippy、全 workspace tests、mock smoke 全绿。
- 一次提交。

## 完成记录 ✅（2026-07-24）

- `BRAND` 已改为低饱和青蓝 `#6c9aa6`，会话头、working spinner、mode 徽标统一跟随。
- semantic cyan 与代码块 magenta 保持原语义；新增测试锁定 RGB 并防止两者混槽。
- README、plan 38 与 HANDOFF 已同步。
- 验证：`cargo fmt --all --check`、clippy `-D warnings`、全 workspace tests、mock smoke 全绿。
- 提交：本次（plan 41，见 git log）。
