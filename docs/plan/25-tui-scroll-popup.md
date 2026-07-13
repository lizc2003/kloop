# Plan 25 — TUI 审批弹层可滚动(备忘)

> 备忘,未开工。开工前读 HANDOFF。缘起:plan 21 的 diff 预览为不可滚动弹层做了
> 硬截断(单行 >200 裁剪、总行 >40 截 + `… (N more)`),这是过渡妥协——真正的解法
> 是弹层可滚动。参考:cc/codex 的审批 diff 都靠可滚动 overlay 容纳完整改动,不截行数。

## 目标

TUI 审批弹层(`draw_confirm`,`crates/tui/src/render.rs`)对超出高度的内容(主要是
plan 21 的 diff 预览,也含长 description)可上下滚动,不再腰斩。落地后**放宽/去掉
plan 21 里 core 侧的行数截断**(单行裁剪可保留或也放宽)。

## 现状(gap)

- `draw_confirm` 是固定居中弹层:`popup_h = (lines.len()+2).min(area.height)`,内容超过
  可用高度直接被 ratatui 截掉,用户看不到下半截,也没有滚动。
- diff 预览因此在 core(`diff.rs`)提前砍:`MAX_PREVIEW_LINES=40`、`MAX_LINE_LEN=200`。
  砍在 core 是因为前端没有容纳机制;弹层能滚了,这层截断就该退回前端或去掉。
- 转录区(`Cell` 列表)本来就有 scroll offset(plan 9),弹层是唯一没滚动的部分。

## 关键决定(开工时定)

- **滚动状态放哪**:`App` 上给当前 confirm 加一个滚动偏移(`confirm_scroll: usize`),
  按键(↑/↓ 或 j/k、PageUp/PageDown)调整;弹层换下一个(VecDeque)时归零。渲染纯函数
  按偏移取窗口。注意别和主转录滚动、输入行按键抢焦点——弹层活跃时按键应优先给弹层。
- **core 截断怎么退**:滚动能容纳后,`diff.rs` 的 `MAX_PREVIEW_LINES` 应放宽(比如
  抬到几百行或去掉);`MAX_LINE_LEN` 单行裁剪可保留(单行过长还是难读,且前端也会 wrap)。
  取舍:core 完全不截 = 预览可能极大(minified 整文件覆盖),弹层滚起来也累——倾向
  **保留一个宽松上限**(如 200 行)+ 单行裁剪,既有滚动又不至于失控。开工定具体数。
- **server/plain 不受影响**:server 的 `preview` 字段是整串,由客户端自己决定怎么显示;
  plain REPL 直接打印(终端自己滚)。滚动只是 TUI 弹层的事。
- **滚动指示**:超出时弹层底部标 `↓ more`(或滚动条),让用户知道有更多内容。

## 不做

弹层内搜索;鼠标滚轮(除非 crossterm 已捕获,顺手);把弹层做成独立可 resize 面板
(cc/codex 有 pager overlay,体量大,最小可用不需要)。

## 测试

App 滚动状态折叠(↑/↓ 改偏移、边界不越界、换弹层归零、弹层活跃时按键不漏给输入行)、
渲染纯函数按偏移取窗口 + `↓ more` 指示;plan 21 的 core 截断放宽后同步改 diff.rs 契约。

## 完成标准

fmt/clippy/test 绿;真 key 一次大 diff 编辑审批能滚动看全;plan 21 的 core 截断相应放宽;
README、HANDOFF。
