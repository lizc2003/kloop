# Plan 101 — TUI 提交进 scrollback 的 CJK 每字多一个空格(宽字符续格未跳过)

> 状态:✅ 已完成(提交见「完成记录」)
>
> 依赖:Plan 38 切片 0(inline `insert_before` 进 native scrollback)、Plan 99(scrolling-regions 关 + 留活尾巴)、Plan 100(分批合并提交)。

## Context(dogfood 暴露的渲染 bug)

用户报告 + 截图:kloop 里中文渲染「中间有空格,有的没有」。截图整屏是**滚回的历史**(native scrollback),每个 CJK 后面都多一个可见空格:`关 键 逻 辑 是`;ASCII 单词(`UpstreamOp`/`data;`/`provider`)不受影响。「有的没有」= 实时视口那一小截(最后一屏,走 `Terminal::draw`)是紧的 `关键逻辑`,一旦提交进 scrollback 就变成 `关 键 逻 辑`。

### 根因(已实测 + 读 ratatui 源码确认)

记录型 backend 抓 `Backend::draw` 收到的 cell 流,对比两条路径(宽 16、文本 `你好AB`):

- **`Terminal::draw`(实时视口)**:`0:"你" 2:"好" 4:"A" 5:"B"` —— diff 已按符号宽度**跳过**宽字符续格(x=1、3 不发)。终端里 `你` 占 0–1、`好` 占 2–3,紧挨,正确。
- **`insert_before`(提交进 scrollback)**:`0:"你" 1:" " 2:"好" 3:" " …` —— 把续格的 `" "` 占位也发给终端。

ratatui-core 0.1.2 `terminal/inline.rs`:kloop 关了 scrolling-regions,走 `insert_before_no_scrolling_regions → draw_lines`,它把临时 buffer 的**每个 cell** 原样 `backend.draw`(不 diff、不跳续格);而 scrolling-regions 那条 `draw_lines_over_cleared` 用 `old.diff_iter(&new)`(会跳)。plan 99 为躲 iTerm2 的 DECSTBM smear 关掉了 scrolling-regions,所以踩中这条。

为什么表现为「每字后一个空格」而非错位:crossterm `draw` 按**宽度 1** 记游标位,`你`(双宽)打完终端游标其实在第 2 列,但 crossterm 以为在第 1 列;续格 `" "` 于是落在第 2 列,`好` 落在第 3 列……每个宽字后恰好空出一列。

## 已拍板设计

定点绕过 ratatui 的这条 bug,不改 scrolling-regions 决策(仍关)、不升级 ratatui(0.30.2/core 0.1.2 无已发布修复)。

### 修法:`PinnedBackend::draw` 复刻 diff 的宽字符 skip

所有 draw(实时 + insert_before)都过生产 backend 包装 `PinnedBackend`。在它的 `draw` 里按 ratatui `Buffer::diff` 同款逻辑丢弃续格:

- 维护 `to_skip`(上一个发出 cell 的符号显示宽度 − 1)与 `next_x`(期望的下一个连续坐标 `(x+1, y)`)。
- 当前 cell 若与 `next_x` 连续且 `to_skip > 0`:丢弃并 `to_skip -= 1`。
- 否则发出,并按本 cell 的 `UnicodeWidthStr::width(symbol) - 1` 重置 `to_skip`。

**对实时路径是 no-op**:续格本就不在 diff 流里;流里的 changed cell 彼此非连续,`next_x` 不匹配 → 不会误删任何真 cell。只有 `insert_before` 那条把续格塞进来的流会被清掉续格。宽字符位于行尾、零宽符号、真空格(前一个是窄字符 `to_skip==0`)都不受影响。

## 关键文件

- `kloop/crates/tui/src/lib.rs` — `PinnedBackend::draw` 加宽字符续格 skip(唯一改动;用 `unicode_width::UnicodeWidthStr`,该 crate 已是 tui 依赖)。
- `kloop/crates/tui/src/lib.rs`(tests)— 新增 `RecordingBackend`(记录 `draw` 收到的 `(x,y,symbol)`);集成测试 `scrollback_commit_drops_wide_char_continuation_cells`:走生产 `PinnedBackend<RecordingBackend>` + `insert_scrollback_blocks`,断言 `关键Ab` 提交后发给终端的行是 `[(0,关),(2,键),(4,A),(5,b),(6,space),(7,space)]` —— 续格 x=1/3 被丢、宽字符间隔 2、行尾是真 padding。
- `kloop/README.md` — inline/scrollback 段补一句:默认路径会把宽字符续格 `" "` 原样 blit,`Terminal::draw` 的 diff 会跳而 `insert_before` 不跳,故 backend 包装统一复刻该 skip。

## 非目标

- 不重开 scrolling-regions(plan 99 决策不动)。
- 不升级/patch ratatui;不加依赖。
- 不改提交/溢出判定(`commit_count`、`coalesce_scrollback_batches`、reserve 都不动)。
- 不动 core/protocol/wire。

## 测试 / 验证

- `cargo fmt` 干净;`cargo clippy --all-targets`(tui)全绿;`cargo test -p kloop-tui` 178 全绿(含新测试)。
- `cargo test --workspace` / `cargo clippy --workspace --all-targets` 全绿。
- 「实时视口不受影响、scrollback 不再多空格」的视觉效果须人工 iTerm2 dogfood 复验(自动化只证发给终端的 cell 流已无续格空格)。

## 完成标准

- `PinnedBackend::draw` 丢弃宽字符续格;新集成测试证发给终端的 CJK 行无续格空格。
- 不改 scrolling-regions/依赖/提交判定;不动 core/wire。
- `cargo fmt` + clippy + `cargo test` 全绿;README 同步;HANDOFF 补教训;一次 commit。

## 完成记录

- 实现:`kloop/crates/tui/src/lib.rs` `PinnedBackend::draw` —— 用 `to_skip`/`next_x` 复刻 ratatui diff 的宽字符 skip,过滤掉 `insert_before` 那条路径多塞的续格 `" "`;对已跳过的 `Terminal::draw` 流是 no-op(非连续不误删)。
- 测试:`RecordingBackend` + `scrollback_commit_drops_wide_char_continuation_cells`(见「关键文件」)。
- README:inline/scrollback 段补宽字符续格 skip 说明。
- 验证:`cargo test -p kloop-tui` 178 全绿;`cargo fmt`/clippy 干净;`cargo test --workspace` + `cargo clippy --workspace --all-targets` 全绿。
- 教训(HANDOFF):ratatui `insert_before` 无 scrolling-regions 路径 `draw_lines` 不做宽字符续格 skip,只有走 diff 的路径才跳;kloop 关了 scrolling-regions,须在 backend 层自己补 skip。
- 提交:见本 plan 所在提交(`git log` 顶部 `fix(plan101)`)。
