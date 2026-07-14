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

---

## ✅ 完成记录(2026-07-14,提交 d625160)

fmt/clippy/test 全绿(373 测试,+5)。落地按"关键决定"逐条:

**滚动状态**:`App.confirm_scroll: usize`。`on_confirm_key` 里 ↑/↓、j/k、PageUp/PageDown
调偏移(向下自由 +1/+10,向上 saturating);其余键仍是 y/a/p/n 决策。**弹层活跃时
`on_key` 早返回进 `on_confirm_key`**,滚动键天然不漏给输入行/主转录(焦点无争)。归零点两处:
答完一个弹层 `pop_front` 后置 0(下一个排队弹层从头看)、`TurnEnded` 清队列时置 0。
**偏移不在 App 里夹**——照 `scroll_up` 那套:`draw_confirm` 按内容算出合法上界后**写回**
`app.confirm_scroll`(over-scroll 后自动纠正,下一次 ↑ 立即响应)。

**选项 pin(plan 未点名、开工定)**:y/a/p/n 是弹层的**全部意义**,绝不能随 diff 滚出视野。
故弹层内分**可滚动 body**(description + 空行 + diff)与**固定 footer**(空行分隔 + 黄色选项)。
`confirm_body_lines`/`confirm_option_lines` 两个纯函数拆出;`window_lines(lines, scroll, height)`
按偏移取窗口、夹偏移、返 `(clamped, visible, more_above, more_below)`;渲染 = `visible ++ 空行 ++ options`。
因 `window_lines` 保证 visible ≤ content_h(需滚时恰好 = content_h),footer 天然钉在弹层底部。
高度预算:`overhead = 2(边框) + 1(分隔) + options.len()`,`popup_h = (body.len()+overhead).min(area.height)`,
`content_h = popup_h - overhead`(数学自洽:内容行数恒等于 `popup_h-2` 的边框内高度)。

**滚动指示**:`scroll_hint(above, below)` 生成 `↑ more`/`↓ more`/`↑↓ more`,经 ratatui 0.29
`Block::title_bottom(Line::right_aligned)` 放**底边框**——不吃内容行,布局数学不受扰。

**core 截断放宽**:`diff.rs` `MAX_PREVIEW_LINES` 40 → **500**(宽松上界:普通编辑几十行永不触顶,
只挡 minified 整文件覆盖失控成巨串);`MAX_LINE_LEN=200` **保留**(单行过长仍难读,且前端会 wrap)。
`big_diffs_are_capped` 测试改为:100 行不截 + 600 行才触发 remainder marker。

**server/plain 零改动**:server `preview` 是整串由客户端决定显示;plain 直打(终端自滚)。core 上界
放宽后两者只是拿到更多行,符合预期。

**验证**:
- 纯函数:`window_lines`(取窗口/夹偏移/自纠 over-scroll/上下溢出 bool)、`scroll_hint` 三态、
  `confirm_body_lines`/`confirm_option_lines` 拆分(body 含 diff、options 黄色)。
- App 折叠:滚动键调偏移不漏输入行、向上不越零、答完换弹层归零。
- **端到端 headless 集成**(`draw_confirm_windows_a_tall_diff_and_pins_the_options`):用 ratatui
  `TestBackend`(无 TTY)把 60 行 diff 渲染进 40×16 真实 Frame,断言:顶部窗口显示 `+1` 不显示 `+60` +
  选项 pin + `↓ more`;`confirm_scroll=999` 后 over-scroll 自纠、显示 `+60` 不显示 `+1` + 选项仍 pin +
  hint 翻成 `↑ more`。这是对交互滚动最强的可自动化验证(交互 TUI 无法在无 TTY 环境手工驱动)。
- **真键手感已过**(用户在真 TUI,anthropic 轨):prompt 让模型 write_file 一个 120 行新文件,
  审批弹层带超高 `(new file)` diff——用户确认滚动顺、黄色选项行全程钉在底部、`↓/↑ more`
  hint 方向正确。至此 plan 25 无挂账。

**教训**:一个"不可容纳"的过渡妥协(plan 21 的行数硬截断)在容器能滚后要**退到最松**,但别退到"不设限"——
可滚动 ≠ 无边界,minified 整文件覆盖仍能产出十万行巨串,拖慢 wrap/滚动。放宽是把"必然会截到正常内容的紧上界"
换成"只兜病态输入的松上界",不是删掉上界。同理弹层滚动引出的**新约束**(选项是动作条、不能随内容滚走)
plan 没点名,靠"这个 UI 的意义是什么"反推 pin footer——滚动容器里"钉住动作区"和"内容可滚"是一对必然孪生。
