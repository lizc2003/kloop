# Plan 121 — 比屏幕还高的那一条：既不在屏幕上，也不在 scrollback 里

> 状态：✅ 已完成（2026-09-04；提交 SHA 以本条所在提交为准）
>
> 来源：2026-09-04 dogfood。用户 `kloop -r` 恢复 gateway 的审查会话
> （`20260904-063311`，32 message(s)），「看不到最后的完整结论」。
>
> 依赖 / 前情：Plan 38 切片 0（inline `insert_before` 进 native scrollback）、
> Plan 99（Bug #2：提交后必须给活尾巴留满一屏）、Plan 116（重放显示对话而非管道）。

## 事实

那条会话最后一条 assistant 消息是一份 4401 字符（168 行原文）的审查结论。重放成 cells 之后
（`cells_from_history`），尾部是两个 cell：

```text
Cell::Assistant(结论)   → 80 列下 166 行
Cell::Note("resumed session — 32 message(s)") → 1 行
```

而 `commit_count`（plan 99）的最后一道闸是：**提交某个 cell 会让剩下的活尾巴
不足一屏时，就停手**。167 行的尾巴减去 166 行只剩 1 行，于是它停在那条结论前面，
一行都不冻。

接着 `draw` 把活尾巴**贴底**渲染：`start = lines.len() - height`，多出来的从**顶部**
切掉。于是 40 行高的终端上，用户看到的是结论的最后 40 行；前面 126 行既没进
scrollback（没提交过），也不在屏幕上（被切了）。**滚轮往上翻不到，它不存在于任何地方。**

不是重放特有的：实时输出一条比屏幕高的回答，同样会切顶；只是实时那条后面迟早
会堆够一屏内容，`commit_count` 补冻进去，内容还能找回来。重放里它是倒数第二条，
后面永远只有那一行 note，于是永久丢失。

## 根因

提交的粒度是**整个 cell**，而屏幕的粒度是**行**。一个 cell 高过视口时，这两个粒度
之间没有出口：

- 整条提交 → plan 99 的空白屏（只剩一行 note 贴在 composer 上，上面一整屏空白）；
- 整条留活 → 这次的切顶丢失。

plan 99 只在这两者里选了后者，没看到第三条路。

## 做法

补上第三条路：**冻掉这一条 cell 里溢出的那些行**，cell 本身留在活尾巴里。

- `render::head_freeze_lines(cells, width, active_h, frozen, head_live) -> usize`
  返回头部 cell 应当累计冻掉的行数：整 cell 提交（`commit_count`）之后仍然溢出多少，
  就再冻多少，上限是这条 cell 自己的最后一行（再往后是整 cell 提交的活）。
  头部 cell 还会变（`display_cell_live`：流式 assistant / reasoning；`is_committable`：
  Running 的工具行、排队中的 agent 消息）就不动它——半条钉进 scrollback 之后
  再重排就修不回来了；最后一条 cell 同理不碰，理由和 `commit_count` 不碰它一样。
- `App.head_frozen: Option<(width, lines)>` 记住冻了多少行、按哪个宽度折的行。
  `commit_overflow` 先按整 cell 提交（此时头部那条已冻的前缀要跳过，否则会写第二遍），
  `drain_committed` 把这份状态清掉（那条 cell 已经整体离开），然后算这一帧的行级冻结，
  和整 cell 的块一起走同一次 `insert_scrollback_blocks`。
- `visible_transcript` 渲染时把头部 cell 的前 `head_skip(width)` 行丢掉：
  **scrollback 的最后一行和视口的第一行严丝合缝**，往上滚就是完整的结论。
- 宽度变了（终端 resize）时 `head_skip` 返回 0：按旧宽度折出来的行数对新宽度没有意义。
  那条 cell 整个回到活尾巴，下一帧按新宽度重新冻一次——scrollback 里重复一段前缀，
  好过在活尾巴里瞎切一刀。

`commit_count` 本身没改：它拿的是整 cell 高度（含已冻部分），只会把 `total`/`remaining`
算大；而保护活尾巴的那道闸 `remaining - heights[0] < active_h` 减掉的是整条高度，
剩下的是 `heights[1..]` 的精确和，所以闸门仍然精确。

## 验证

`crates/tui/src/render.rs`
- `head_freeze_takes_exactly_the_tall_head_overflow`：plan 99 的形状（20 行消息 + 1 行
  note，视口 5 行）——`commit_count` 仍然返回 0，`head_freeze_lines` 返回 16，
  并逐行断言接缝（scrollback 末行 `row 15`、视口首行 `row 16`），活尾巴正好一屏。
- `head_freeze_adds_only_the_new_overflow`：只补新溢出的行，不重算。
- `head_freeze_leaves_a_settled_or_fitting_head_alone`：尾巴够短 / 头部就是最后一条 /
  头部在流式 / 头部是 Running 工具行——四种情况整表断言都不冻。
- `visible_transcript_resumes_the_head_below_the_frozen_seam`：同宽度接着冻结点往下画；
  换个宽度整条回来。

`crates/tui/src/lib.rs`
- `a_tall_resumed_answer_is_frozen_instead_of_clipped_away`：40 行回答 + 1 行 note 在
  12 行终端上真的走一遍 `draw_frame`，断言 **冻掉的行数 + 屏幕上的行数 = 41**
  ——「每一行要么在 scrollback 里，要么在屏幕上」，并断言视口首行正好接在接缝之后。

`cargo fmt` / `cargo clippy --workspace --all-targets -D warnings` 全绿。测试是
**逐个测试二进制**跑的(21 个,`kloop-tui` 207、`kloop-core` 786 等全 ok,只跳过
`#[ignore]` 的真实凭据用例):本机把测试二进制的 stdout 接到管道或文件上会挂住,
`--logfile` + `>/dev/null` 才跑得完——这是本机环境的坑,不是被测代码的问题。
