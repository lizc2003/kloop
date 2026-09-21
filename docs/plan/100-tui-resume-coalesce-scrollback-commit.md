# Plan 100 — TUI resume 加载历史慢:合并首帧 scrollback 提交(每 cell 一次 → 分批一次)

> 状态:✅ 已完成(提交见「完成记录」)
>
> 依赖:Plan 38 切片 0(inline `insert_before` 进 native scrollback)、Plan 43(每批 resync + repaint)、Plan 99(scrolling-regions 关 + 留活尾巴)。

## Context(dogfood 暴露的性能 bug)

用户报告:`kloop -c` 恢复长会话时「加载历史很慢,需要滚动很久」;Claude Code resume 很快。第一版误判成「启动器每次 `cargo build`」——用户纠正:不是编译,是**启动后**历史一格一格往上滚、滚很久。

### 根因(已实测 + 读 ratatui 源码确认)

1. 启动时 `app.cells` 一次性装入整段历史(`lib.rs` run:171)。
2. 首帧 `commit_overflow` 把「除最后一屏外」的全部历史 cell 提交进 native scrollback。
3. 提交走 `insert_scrollback_blocks`,它**对每个 cell 单独调一次 `terminal.insert_before`**(`lib.rs:816-825` 的 `for lines in blocks`)。
4. ratatui-core 0.1.2 的 `insert_before`(scrolling-regions 关 → `insert_before_no_scrolling_regions`)每次收尾都 `self.clear()`,inline viewport 走 `clear_viewport → backend.clear_region(AfterCursor)`(`buffers.rs:164`);中途 `scroll_up`(`append_lines`)+ `draw_lines`(带 `backend.flush()`)。
5. kloop 的 inline viewport 是**满屏高**,所以每个 cell 都触发一次满屏滚动 + 清屏 + flush。

历史越长 cell 越多 → 首帧就是 **N 次满屏清屏重绘 + N 次 flush**(N = 溢出 cell 数),这就是「滚很久」。每个 `insert_before` 恰好一次 `backend.clear_region`,可作 O(N) 的铁证。

## 已拍板设计

用户拍板「同意」——按「合并成一次写入」修。

### 修法:`insert_scrollback_blocks` 分批合并

- 新增纯函数 `coalesce_scrollback_batches(blocks, max_rows) -> Vec<Vec<Line>>`:把逐 cell 的 blocks 展平重组成「行数不超过 `max_rows`」的批次,保持顺序与内容不变;单个高于上限的 cell 自成一批(不在 cell 内部切,`insert_before` 内部本就按屏高分块滚)。
- `insert_scrollback_blocks` 改成对**批次**(而非每个 cell)各调一次 `insert_before`,收尾仍一次 `terminal.clear()`。
- 常量 `SCROLLBACK_BATCH_ROWS`(取 512):把单次 `insert_before` 的 buffer 内存(width×height 个 Cell)钳在几 MB 内,同时把清屏次数从「每 cell」降到「每 ~512 行」。典型会话整段历史往往一两批搞定,清屏从上百次降到个位数。

### 为什么合并是等价的

`cell_lines` 已按 `width` 把每个 cell 渲染成固定行;拼接后用一个 `Paragraph::new(lines)` 渲染,视觉与逐 cell 渲染完全一致(行顺序不变、不再 wrap)。`insert_before` 支持 height > 屏高(内部 while 分块),所以一批很高也没问题。

## 关键文件

- `rust/crates/tui/src/lib.rs` — 新增 `SCROLLBACK_BATCH_ROWS` + `coalesce_scrollback_batches`;`insert_scrollback_blocks` 改按批 `insert_before`。
- `rust/crates/tui/src/lib.rs`(tests)— 纯函数单测(N 个短 cell → 批数 = ceil(总行/上限),展平内容与顺序不变;超高单 cell 自成一批);计数 backend 集成测试:满屏 inline viewport + 大量短 cell,断言 `clear_region` 次数 = 批数(O(批)),不随 cell 数线性增长;沿用 `scrollback_insert_clears_viewport_without_clearing_history` 证内容正确。
- `rust/DESIGN.md` — overflow/resume 段补一句:首帧提交按批合并(每 cell 一次 `insert_before` → 分批一次),避免长历史 resume 逐格清屏重绘。

## 非目标

- 不改提交/溢出的整体判定(`commit_count` 一屏守卫、`overflow_commit_count` 的 reserve 计算都不动)。
- 不改 scrolling-regions 决策(仍关,plan 99)。
- 不动 core/protocol/wire;不加依赖/工具。
- 不做「进 viewport 前先 plain-stdout 吐历史」那套更大的重构(留作后续可能性;本 plan 只做定点合并)。

## 测试 / 验证

- `cargo fmt --check` 干净;`cargo clippy --workspace --all-targets -- -D warnings` 全绿。
- `cargo test --workspace` 全绿;新单测 + 计数 backend 集成测试到位。
- 边界:真实 iTerm2 的「秒开」须人工 dogfood 复验(自动化只证「清屏/insert_before 次数从 O(cell) 降到 O(批)」)。

## 完成标准

- `insert_scrollback_blocks` 按批合并;新测试证 clear_region 次数不随 cell 线性增长。
- 不改 core/wire/依赖;不动提交整体判定。
- `cargo fmt` + clippy(`-D warnings`) + `cargo test` 全绿;README 同步;HANDOFF 补教训;一次 commit。

## 完成记录

- 实现:`rust/crates/tui/src/lib.rs`
  - 新增常量 `SCROLLBACK_BATCH_ROWS = 512` 与纯函数 `coalesce_scrollback_batches(blocks, max_rows)`:丢空块、按行数上限把逐 cell 的 blocks 合并成批次(单块永不跨批切;单块超上限自成一批),顺序与内容不变。
  - `insert_scrollback_blocks` 由 `for lines in blocks` 改为 `for lines in coalesce_scrollback_batches(blocks, SCROLLBACK_BATCH_ROWS)`;每批仍一次 `insert_before`,收尾一次 `terminal.clear()`。提交/溢出判定(`commit_count`、reserve)未动。
- 测试(`lib.rs` tests):
  - `coalesce_batches_collapses_many_cells_and_preserves_content`(100 短 cell、cap 10 → 10 批×10 行,首尾内容/顺序不变)。
  - `coalesce_batches_never_splits_a_cell_and_lets_a_tall_cell_stand_alone`(cap 3、形状 [2,3,4,1],超上限单块自成一批)。
  - `coalesce_batches_drops_empty_blocks`。
  - `resume_backlog_commits_in_batches_not_one_clear_per_cell`:新增 `ClearCountingBackend`(计 `clear`/`clear_region`),满屏高 `Inline(10)` viewport + 300 个一行 cell,断言清屏次数 `< cells/10` 且 `<= 3`(O(cell) → O(批)的铁证)。
- README:overflow/resume 段补一句「每 `insert_before` 满屏固定一次滚动+清屏,与块高无关;故 `-c` 长历史按 cap 行分批合并提交,而非每 cell 一次」。
- 验证:`cargo fmt --check` 干净;`cargo clippy --workspace --all-targets -- -D warnings` 全绿;`cargo test -p kloop-tui`(177)全绿;`cargo test -p kloop --test tui_pty -- --test-threads=1` 9/9(含 `two_turn_overflow_commits_without_scroll_regions_then_repaints`,两遍复跑);`cargo test -p kloop-server --test server -- --test-threads=1` 35 通过、1 ignored(真 provider);`cargo test --workspace -- --test-threads=1` 全绿(单线程慢,分批完成,零 failed)。
  - 说明:`tui_pty` 各测试驱动真实二进制、`wait_for` 有 8s 窗口,并发或整仓高负载下偶发超时(非断言失败);单独跑该 binary 单线程稳定 9/9。此性能修复的「iTerm2 秒开」效果需人工 dogfood 复验(自动化只证清屏/`insert_before` 次数从 O(cell) 降到 O(批))。
- 提交:见本 plan 所在提交(`git log` 顶部 `feat(plan100)`)。
