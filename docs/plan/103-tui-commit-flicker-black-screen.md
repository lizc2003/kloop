# Plan 103 — TUI 黑屏闪烁:提交时的 clear 独占一次 write,后面还跟一次 CPR 空等

> 状态:✅ 已完成(2026-08-28;提交见本次 git log,plan103)
>
> 依赖:Plan 38 切片 0(inline `insert_before` 进 native scrollback)、Plan 99(关 `scrolling-regions`)、Plan 100(提交批量合并)、Plan 101(宽字符续格)。

## Context(dogfood 报告)

用户实机 dogfood:「TUI 界面时不时会变黑闪屏;大量更新内容的时候也闪屏」。

## 根因(PTY 探针实测,不是推断)

给 `tui_pty_support` 临时加了探针:reader 线程每读完一个 chunk 就用 vt100 快照判一次
"整屏是否全空",并给每个 `ESC[6n` 打时间戳。跑三回合(每回合 60 行长回复,必然溢出提交)的
**修复前**读数:

```
CPR (ESC[6n) during turns: 4
blank-screen windows: 2   blank total: 541 ms   worst: 335 ms
  blank at 1498289 us for 205929 us
  blank at 1777098 us for 335179 us
cpr marks (us): [1291114, 1326642, 1498297, 1777043, 1905814]
```

第一个空屏窗口起于 1_498_289 µs,一次 CPR 落在 1_498_297 µs——**相差 8 µs**。链条:

1. `scrolling-regions` 关掉后(plan 99),`insert_before` 走默认路径,末尾调
   `Terminal::clear()` 把整个 inline 视口清掉;ratatui-crossterm 的 `clear_region` 是
   `execute!` 写的,**自带 flush** → 满屏视口当场变黑,而重画要等下一次 `draw` 才发出去。
2. kloop 自己在 `insert_scrollback_blocks` 末尾**又清了一次**(冗余:`insert_before` 已经
   清过、也已经 reset 过 diff buffer)。而 `Terminal::clear()` 的第一步是
   `get_cursor_position()` → crossterm `ESC[6n` 同步等应答 → 要抢 crossterm 内部 event
   reader 锁,而输入线程正握着它做 `poll(200ms)`(plan 38 切片 0 的刻意设计)。于是这次
   CPR **在屏幕已经全黑之后**空等一个 poll 周期 → 黑屏 205–335 ms。
3. 每帧还被 `execute!` 拆成十几次 write(光标移动、清屏、show/hide cursor 各一次 flush),
   内容多时终端会显示画到一半的帧——就是「大量更新时也闪」。

## 已拍板设计(三条,都在 tui 的 terminal 层)

1. **删掉冗余的第二次 clear**:`insert_scrollback_blocks` 不再自己 `terminal.clear()`。
   `insert_before` 末尾那次 clear 已经同时做了两件事(清视口 + reset diff buffer),第二次
   只是白白多一次全屏 ED + 一次卡在黑屏里的 CPR。
2. **提交期不查光标**:`PinnedBackend` 记住自己最后一次 `set_cursor_position` 的位置,
   `insert_scrollback_blocks` 进出时 `begin_commit`/`end_commit`;窗口内的
   `get_cursor_position` 直接用缓存值,不发 `ESC[6n`。安全性:ratatui 拿这个值只是清屏后把
   光标放回去,而紧接着的重画本来就会重设光标。窗口外(视口初始化、resize 锚点)仍走真 CPR
   ——所以窗口开在 `insert_scrollback_blocks` 内部,而不是 `draw_frame` 的 pin 窗口
   (后者含 `autoresize()`,resize 必须问到真位置)。
3. **一帧一次 write**:新 `FrameWriter` 夹在 `CrosstermBackend` 和 stdout 之间,`flush` 默认
   只缓冲;`PinnedBackend::commit_frame`(由 `draw_and_hand_over` 在每次 `terminal.draw`
   之后调用)放行一次真写,并用同步输出(DEC private mode 2026)把整帧包起来——不支持的终端
   直接忽略该私有模式。于是「清屏 + 重画」是同一次 write、同一次呈现。

## 关键文件

- `rust/crates/tui/src/terminal/frame_writer.rs` — 新增 `FrameWriter`(+ 单测:中途 flush 只缓冲、release 后一次同步写、空帧不写)。
- `rust/crates/tui/src/terminal/pinned_backend.rs` — `with_frames`/`begin_commit`/`end_commit`/`commit_frame`;`set_cursor_position` 记位置,`get_cursor_position` 提交期走缓存、窗口外先交帧再查。
- `rust/crates/tui/src/terminal/scrollback.rs` — 删末尾 `terminal.clear()`;签名收紧成 `Terminal<PinnedBackend<B>>` 并在函数内开合提交窗口(调用方想漏都漏不掉);`ClearCountingBackend` 加数光标查询,断言提交 = 1 次 clear + **0 次** CPR。
- `rust/crates/tui/src/lib.rs` — `FrameWriter` 接进 `setup_terminal`/`Terminal` 别名;新 `draw_and_hand_over`(draw + 交帧)取代三处裸 `terminal.draw`;`TerminalSession::restore` 先交帧再还原终端。
- `rust/crates/cli/tests/tui_pty.rs` — 溢出提交测试补断言:回合中**不出现** `ESC[6n`;clear 落在一对同步输出之内,且重画在同一对之内。
- `rust/README.md` — inline/scrollback 段补这三条。

## 非目标

- 不重开 `scrolling-regions`(plan 99 的 iTerm2 涂抹结论不变)。
- 不动提交/溢出判定(`commit_count`、一屏守卫)、不动批量合并、不动宽字符续格 skip。
- 不动 core/protocol/wire;不加依赖;不升级/patch ratatui。
- 不改「提交后整屏重画」这件事本身(diff buffer 被 reset 是 ratatui 的语义);本 plan 只保证它作为**一帧**呈现。

## 测试 / 验证

- `cargo fmt --check` 干净;`cargo clippy --workspace --all-targets -- -D warnings` 全绿。
- `cargo test --workspace` 全绿(tui 179、tui_pty 9)。
- 同一探针**修复后**读数:`CPR during turns: 0`、`blank-screen windows: 0`、`blank total: 0 us`;
  三回合墙钟从 ~700 ms 降到 ~125 ms(省掉的正是那 4 次 CPR 空等)。探针是临时的,已删除;
  留在仓库里的确定性锁是 tui_pty 的两条断言(无 `ESC[6n`、clear 与重画同帧)+ scrollback 单测
  (1 次 clear、0 次光标查询)。
- **如实边界**:「iTerm2 里肉眼不再闪」须人工 dogfood 复验;自动化只锁机制(不发 CPR、
  一帧一次同步写)。同步输出对不支持 2026 的终端是 no-op,那些终端靠"一帧一次 write"收敛。

## 完成标准

- 三条设计全部落地;新测试到位;`cargo fmt` + clippy(`-D warnings`) + `cargo test` 全绿;
  README 同步;HANDOFF 补教训;一次 commit。
