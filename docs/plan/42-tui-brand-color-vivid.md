# Plan 42 — TUI 品牌色增强醒目度

## 背景

Plan 41 将粉红品牌色替换为低饱和青蓝 `#6c9aa6`。用户看过当前色与更醒目候选色的并排预览后，明确选择右侧的 `#4fb3c8`。

## 范围与决定

- `render.rs` 的品牌槽 `BRAND` 改为 `#4fb3c8` (`Rgb(79, 179, 200)`)。
- 仍只影响 kloop chrome：会话头边框/标题、working spinner、footer mode 徽标。
- ANSI cyan 的用户·状态·选中语义、green/red/dim 及代码块 magenta 均不变。
- 同步 README、plan 38 与 HANDOFF；更新 RGB 回归测试。

## 完成标准

- `cargo fmt --all --check`、clippy、全 workspace tests、mock smoke 全绿。
- 一次提交。

## 完成记录 ✅（2026-07-24）

- 用户在并排预览中选择右侧后，`BRAND` 已从 `#6c9aa6` 提亮为鲜明青蓝 `#4fb3c8`。
- 品牌槽覆盖范围和其余语义色均未改变；README、plan 38 与 HANDOFF 已同步。
- RGB/语义分槽回归测试已更新。
- 验证：`cargo fmt --all --check`、clippy `-D warnings`、全 workspace tests、mock smoke 全绿。
- 提交：本次（plan 42，见 git log）。
