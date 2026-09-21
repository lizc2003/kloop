# Plan 99 — TUI inline 提交/回滚渲染两个 dogfood bug(scrolling-regions 关 + 尾部留活)

> 状态:✅ 已完成(2026-08-26;提交见本次 git log,plan99)
>
> 依赖:Plan 38 切片 0(inline `insert_before` 进 native scrollback)、Plan 43(每批 resync + repaint)、Plan 76/78(TUI 测试分层 + terminal 依赖)。

## Context(dogfood 暴露的两个渲染 bug)

真实 dogfood kloop 自己的 TUI 时(iTerm2),撞到两个渲染错:

- **Bug #1(转录乱码/串行)**:某个 Read 工具行的 preview 几行被无关内容涂抹叠印,"… full result in session" 出现两次。因为提交进 native scrollback 后已冻进真实回滚缓冲(repaint 修不回来),必须从源头改渲染机制。**这是 kloop 自己 TUI 的渲染**(用户明确纠正过一次误判——不是 Claude Code 的问题)。
- **Bug #2(resume 空白)**:`kloop -c` 恢复会话时,最后一条 assistant 消息(顶部)与 resume marker + composer(底部)之间出现一大片空白。

### 根因

- **Bug #1**:tui crate 开了 ratatui 的 `scrolling-regions` feature。开着时,**全屏高**的 inline `insert_before` 会逐行 `scroll_region_up(0..1, 1)`(DECSTBM 一行滚动区)。iTerm2 把这套逐行 DECSTBM 在 scrollback 里涂抹串行。关掉该 feature 后,`insert_before` 走 ratatui 默认路径:`set_cursor_position(0, height-1)` + `append_lines`(底行 LF 滚动),这是每个终端都稳妥处理的真实滚动。
- **Bug #2**:`commit_count` 的溢出提交循环只保证"提交后 remaining ≤ 某上界",没保证**留下的活尾巴至少一屏高**。resume 时若最后是一条很高的 assistant 消息 + 一行 note,循环会把那条高消息也冻进 scrollback,只剩一行 note 活着 → composer 上方一整屏空白。

## 已拍板设计

用户拍板「同意,都修」——两个 bug 合进本 plan。

### Bug #1:关掉 `scrolling-regions` feature

- `crates/tui/Cargo.toml`:`ratatui` 去掉 `features = ["scrolling-regions"]`,附 6 行注释说明这是**刻意**关闭(DECSTBM 逐行 vs append_lines LF 滚动 / iTerm2 串行 / plan 99)。
- 该 feature 关掉后,`Backend::scroll_region_up/down` 不再是 `insert_before` 的路径 → 删掉 `PinnedBackend` 对这两个方法的转发(留 `append_lines` 转发——真实滚动靠它)。
- 同步删掉 mock `StagedSizeBackend` 的 `scroll_region_up/down`(它们原本记 "scroll" 事件)。它的 `append_lines` 仍转发给 inner 但**不**记事件——因为 `compute_inline_size` 在 init 和每次 autoresize/resize 都会调 `append_lines`(不是提交独占),拿它当"提交发生了"的代理会误报。
- 两处受影响的测试改断言真实可观测量 `app.cells.len()`(提交/未提交的铁证),不再断言 "scroll" 事件:
  - `resize_before_commit_repaints_without_stale_overflow`:断言 `draw` 发生、viewport `(80,24)`、`app.cells.len() == 12`(未提交)。
  - `size_pin_keeps_commit_and_repaint_on_confirmed_geometry`:断言 `draw` 发生、viewport `(80,6)`、`app.cells.len() < 12`(已提交)。
- `insert_scrollback_blocks` 的文档注释改述 append_lines 路径 + 引 plan 99。

### Bug #2:提交时给活尾巴留够一屏

- `crates/core/…` 不涉及;改的是 `crates/tui/src/render.rs` 的 `commit_count`,在循环里加一道守卫(保留原有条件):
  ```rust
  if remaining - heights[committed] < active_h {
      break;
  }
  ```
  即"提交这条会让活尾巴矮于一屏就停手"。循环条件本就排除最后一条 cell。

## 关键文件

- `rust/crates/tui/Cargo.toml` — 去掉 `scrolling-regions` feature + 注释。
- `rust/crates/tui/src/lib.rs` — 删 `PinnedBackend` 与 mock 的 `scroll_region_*` 转发/记录;`insert_scrollback_blocks` 注释改述;两个测试改断 `cells.len()`。
- `rust/crates/tui/src/render.rs` — `commit_count` 加一屏守卫 + 新回归测试 `commit_keeps_a_tall_final_message_live_instead_of_a_blank_pad`。
- `rust/crates/cli/tests/tui_pty.rs` — PTY 测试改名 `two_turn_overflow_commits_without_scroll_regions_then_repaints`,断言**不出现** scroll-region 转义(`\x1b[1;1r`/`\x1b[1S`)+ clear→first-tail→second-tail 顺序。
- `rust/DESIGN.md` — overflow-commit 段补两点:提交只冻结"留活尾巴 ≥ 一屏"的前缀(高尾消息在 `-c` resume 不被空白挤走)+ 刻意关 `scrolling-regions`(append_lines LF 滚动 vs DECSTBM iTerm2 串行)。

## 非目标

- 不改提交/溢出的整体机制(仍是 plan 38/43 那套 insert_before + resync + repaint)。
- 不动 core/protocol/wire;不加依赖/工具。
- 不给单测强行复现 iTerm2 特有串行(TestBackend 两条路径都建模正确、PTY 只能断原始字节里机制已换);Bug #1 的"串行消失"须人工 iTerm2 dogfood 确认。

## 测试 / 验证

- `cargo fmt --check` 干净;`cargo clippy --workspace --all-targets -- -D warnings` 全绿。
- `cargo test --workspace` 全绿:tui crate 173 测试、tui_pty 9 测试(含改名的 PTY 断言)、新回归测试全过,0 失败。
- **边界**:Bug #2 已完整单测(高尾 + 一行 note 的 resume 场景);Bug #1 单测/PTY 只能断"机制换成 append_lines、scroll-region 字节不再出现",iTerm2 特有涂抹须人工 dogfood 复验。

## 完成标准

- 两个 bug 均落地:`scrolling-regions` 关 + `commit_count` 一屏守卫;新回归测试 + PTY 断言到位。
- 不改 core/wire/依赖;不动提交整体机制。
- `cargo fmt` + clippy(`-D warnings`) + `cargo test` 全绿;README 同步;HANDOFF 补教训;一次 commit。

## 完成记录(2026-08-26)

- **Bug #1**:关 `ratatui` 的 `scrolling-regions` feature,全屏 inline `insert_before` 改走 append_lines LF-at-bottom 滚动(iTerm2 不再涂抹 DECSTBM 逐行);删 `PinnedBackend`/mock 的 `scroll_region_*` 转发与 "scroll" 事件代理,两个受影响测试改断 `app.cells.len()`(真实提交观测量);PTY 测试改名并改断 scroll-region 字节**缺席** + clear→tail 顺序。
- **Bug #2**:`commit_count` 加"留活尾巴 ≥ active_h"守卫,高尾 assistant 消息 + 尾 note 的 `-c` resume 不再被整屏空白挤到底;新回归测试 `commit_keeps_a_tall_final_message_live_instead_of_a_blank_pad`。
- 验证:`cargo fmt --check` 干净;`cargo clippy --workspace --all-targets -- -D warnings` 全绿;`cargo test --workspace` 全绿(tui 173、tui_pty 9、新回归测试,0 失败)。README overflow-commit 段已同步两点。
- 如实边界:Bug #1 的 iTerm2 串行消失属终端特有渲染,自动化只证"机制已换"(append_lines、无 scroll-region 字节),须人工 iTerm2 dogfood 复验;Bug #2 已完整单测。secrets/endpoints 未落任何文件。
