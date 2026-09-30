# Plan 215 — 每一行，要么在屏上，要么在 scrollback 里

> 来源:2026-09-29,plan 214 同一次会话。修完"待答计划被剪"之后，用户又报「在 turn 结束出结论的时候，也是
> 被截断，但最后吐完了内容，再往上翻，是可以看完整的」,接着问「老是被截断，有彻底的解决方案吗」。对照了
> codex 与 grok-build 的做法后，用户定了 A(逐行冻结，保留原生 scrollback),不要 C(`/transcript` 兜底):
> 「用A,C的兜底没多大意义吧」。**本文件只是 plan,用户要在新会话实现。**

## 一、根因：四道"不许冻"的闸

TUI 的屏幕只画"还没冻进 scrollback 的尾部",贴底对齐(`render::draw` 里 `start = lines.len() - height`)。
尾部比屏高、超出的部分又不许冻时，顶部就被剪掉：不在屏上，也不在 scrollback 里。现在不许冻的:

| # | 闸 | 位置 | 典型现象 |
|---|---|---|---|
| 1 | 还在接收文本的回答整格不冻 | `render.rs:703` `commit_count` 碰到 `display_cell_live` 即停;`head_freeze_lines` 的 `head_live` | 本条的报告：结论边输出边被剪，吐完才能往上翻 |
| 2 | 最后一个 cell 不冻 | `commit_count` 循环条件、`head_freeze_lines`(plan 214 只给计划开了口) | 一份比屏高的末位 cell |
| 3 | Running 的行挡住它后面所有东西，直到 4 屏硬顶 | `render.rs:681` `is_committable` + `hard_cap` | 后台任务行运行时，主 agent 输出的长回答被剪 |
| 4 | 有面板/picker 开着就整帧不提交 | `lib.rs:942` `draw_frame` 的 `overlay_open`(plan 214 只放过了计划审批) | bash 审批时，上方的长说明被剪 |

plan 99/121/214 都是在这四道闸上开口子。本条把规则反过来。

**闸 1 不是多虑。** 2026-09-29 实测:`- a\n- b\n\n` 渲染成 `• a` / `• b`;下一项 `- c` 一到，整个列表变松散，
重排成 `• a` / `` / `• b` / `` / `• c`。前两行若已冻进 scrollback,封口后接缝就错位(重行或丢行)。编号列表同形。

## 二、参考与裁决

- **codex**(`refs/codex/codex-rs/tui/src/streaming/controller.rs` 开头的模块注释):每条流分"稳定区"
  (边流边进 scrollback)与"尾巴"(活动区，可变);表格从表头起整段留在尾巴里直到收尾(table holdback),
  因为加一行会改所有列宽;不变式写着 committed source 只追加。
- **grok-build**:默认 `screen_mode = fullscreen`(`xai-grok-pager/src/settings/defs.rs:492`),备用屏 + 应用
  自己的 `ScrollbackPane`,根本不用原生 scrollback,所以不存在这个问题;`--minimal`(原生 scrollback)
  与 kloop 今天同形——回答在收尾前整块留在活动区，贴底画、`skip_top` 剪顶(`xai-grok-pager-minimal/src/live.rs`
  `draw_tail`),兜底是 `/transcript` 用 `$PAGER` 打开全文(`full_view.rs`)。
- **裁决(用户定)**:A = codex 的逐行冻结。B(改全屏、自管滚动)推翻 plan 38 的内联设计，不做;
  C(`/transcript`)在 A 之后只剩"比整屏还高、还没写完的表格"一种情形，而它写完就自己进 scrollback,不做。

## 三、不变式

**渲染出来的每一行，要么在屏上，要么在 scrollback 里。** 唯一例外：还可能变的那部分(下称"未定稿")
本身就比活动区高——实际只剩一张写到一半、比屏还高的表格。

做法：一行只要不会再变，就可以冻;冻结按行进行、恰好冻掉溢出的部分;屏上只留未定稿的行和放得下的尾部。

## 四、形状

### 4.1 markdown 渲染器：前缀稳定(先做，其余都建在它上面)

性质 **P**:对任意全文 `T` 与它的任一前缀 `T[..k]`,`assistant_stream(T[..k])` 声称已定稿的那 `s` 行，
逐行(含样式)等于 `markdown_lines(T)` 的前 `s` 行。

要满足 P,列表间距从"整张列表松不松"改成"源文本里这一块前面是不是空行":

- 事件循环改用 `Parser::new_ext(..).into_offset_iter()`;遇到块级 `Start`(Paragraph、Heading、BlockQuote、
  CodeBlock、List、Item、Table、HtmlBlock)时记下 `blank_before = 源文本里这一行的上一行是否为空`
  (把 `>` 与空白都当空;offset 针对 `normalize_nested_fences` 之后的串)。行内 tag 与表格内部的
  TableHead/Row/Cell 不更新它——`End(Table)`/`End(CodeBlock)` 时读到的仍是它们自己 `Start` 的值。
- `Tag::Item`(`markdown.rs:320`):不是本列表第一项且 `blank_before` → 先推一行空行。
- `Tag::Paragraph` 带 marker 的分支(`markdown.rs:281`):去掉 `loose` 标记与那行空行。
- `block_gap`(`markdown.rs:479`)嵌套分支:`item.loose` 换成 `blank_before`(仍要求本项已有内容)。
  引用块里照旧不加空行。
- `OpenItem.loose`(`markdown.rs:156`)删掉。

**可见变化**:全松、全紧的列表不变;混合的 `- a\n- b\n\n- c` 从三项都空开变成 `• a` / `• b` / `` / `• c`
——照着源文本的空行走。已告知用户。

### 4.2 流式：同时给出"已定稿行数"

`assistant_stream_lines`(`markdown.rs:82`)换成 `assistant_stream(text, width) -> (Vec<Line>, usize)`,
`find_stream_safe_boundary`(`markdown.rs:971`)顺带报出"结尾是否在一个没关上的围栏里":

- **不在围栏里**:照旧在最后一个顶层空行处切开，前缀按 markdown、尾巴按原文;`settled = 前缀的行数`。
- **结尾在没关上的围栏里**(新):整段按 markdown 渲染(pulldown 会在文末隐式关闭围栏，于是代码边写边
  高亮显示，不再是带 ```` ``` ```` 的原文);`settled = markdown_lines(到最后一个完整行为止).len()`。
  代码不重排，写完的每一行都定稿——长代码块也能边写边进 scrollback。
- **还没有任何边界**:原文，`settled = 0`。

### 4.3 每种 cell:能冻几行、能不能整格离开

`height` 一律是**屏上实际画出的行数**(末位流式回答用 4.2 的渲染，末位流式 thinking 是一行时钟)。
`visible_transcript` 与冻结必须共用同一个逐 cell 的显示函数(plan 74 的教训：画的和提交的只能有一份来源)。

| cell | 已定稿行数 | 整格离开 |
|---|---|---|
| 回答，已封口 | 全部 | 可以 |
| 回答，还在接收(`display_cell_live`) | 4.2 的 `settled` | 不可以(还会长) |
| Thinking,还在接收 | 0 | 不可以 |
| Thinking,已封口 | 全部 | 可以 |
| Tool / Agent,Running | 0 | 只在尾部超过 4 屏时(沿用 `hard_cap`) |
| Tool / Agent,已结束 | 全部 | 可以 |
| BackgroundTask Running、AgentMessage Queued | 0(原地替换会改行数) | **可以，需要时就走**:冻结后的终态更新本来就会另追加一行(`app.rs` 的 `frozen_background_tasks` / `frozen_agent_messages`);从"钉到 4 屏"改过来，因为钉住就是闸 3 |
| Plan,待答 | 全部 | 不可以(答复会在末尾加一行) |
| Plan,已答 | 全部 | 可以 |
| 其余(User/System/Note/TurnEnd…) | 全部 | 可以 |

### 4.4 冻结算法：一个纯函数取代三个

`commit_count` + `head_freeze_lines` + `is_committable`(`render.rs:681`–`790` 一带)合成一个:

```rust
pub struct Freezable { pub height: usize, pub settled: usize, pub leave: Leave }
pub enum Leave { Never, Whole, AtCap }   // AtCap = 只在尾部超过 4 屏时

/// (整格离开几个 cell, 新的首个 cell 已冻前几行)
pub fn freeze_target(cells: &[Freezable], frozen: usize, active_h: usize) -> (usize, usize)
```

```
over = (Σheight − frozen) − active_h;  ≤ 0 → (0, frozen)
at_cap = (Σheight − frozen) > 4·active_h
skip = frozen
逐个 cell:
  rest = height − skip
  能整格离开(Whole,或 AtCap 且 at_cap):
     rest ≤ over          → over −= rest;离开;skip = 0;继续下一个
     settled < height     → 离开(小的原地替换行不可拆：多冻几行、留几行空白，胜过剪掉);结束
  take = min(settled − skip, over);skip += take;结束
```

- plan 99 的"别留下整屏空白"自然满足：除了上面那种不可拆的小行，只冻恰好溢出的行数。
- "最后一个 cell 不碰"这条规则取消：最后一个 cell 的 `rest > over` 恒成立，永远不会整格离开，只会按定稿行数冻前缀。
- `lib.rs:878` `commit_overflow`:用 `freeze_target(freezables(app, width), app.head_skip(width), active_h)`;
  写进 scrollback 的行取自同一个显示函数;**在 `drain_committed` 之前**先取出新首 cell 要冻的那几行
  (drain 会重置 `head_frozen`)。

### 4.5 事件循环：只有补全弹窗还暂停提交

`draw_frame` 的门只留 `app.popup`(补全菜单是盖在转录上的浮层，不在高度预算里)。审批/提问面板与 fork
picker 自 plan 104 起都在 `live_chrome_layout` 的预算里，删掉。代价(已告知用户):面板关闭那一刻活动区变高，
新输出到来前视口顶部会空几行;scrollback 本身连续。

### 4.6 收编 plan 214 的特例

- 删 `App::plan_awaiting_answer` 与门里的计划例外(4.5 已覆盖)。
- `head_freeze_lines` 里给 Plan 开的口随函数一起消失(4.3 表里"待答计划：全部定稿、不可整格离开"即是)。
- **保留** `post_plan`(计划原地取代它的 Running 工具行):Running 行仍是"定稿 0 行、只在 4 屏时离开",
  不取代的话它照样挡住计划。
- plan 214 的端到端测试 `a_waiting_plan_reaches_scrollback_before_it_is_answered` 原样保留，必须仍绿。

## 五、坑

- **回答文本被"替换"而不是"追加"**:`apply_item_started` 对已存在的 id 做 `*current = text`,
  `apply_item_completed`(`app.rs:1019`)用完成时的全文替换。正常情况下完成的全文等于增量拼接;若不是它的
  延长，已冻前缀与封口渲染对不上。建议加一道便宜的保护：首 cell 已部分冻结、而新文本不以旧文本开头时，
  清掉 `head_frozen`(宁可重一段，不可丢)。
- **宽度变化**:`head_skip` 对别的宽度冻的前缀记 0,整格回来、下次按新宽度重冻——scrollback 里会重一段
  (plan 121 已接受)。流式期间每次改窗宽都会发生一次，接受。
- **已知会违反 P 的 markdown**:引用式链接定义出现在使用之后(`[x]` … `[x]: url`,渲染会补 ` (url)`);
  跨空行的 HTML 块(`<details>` 等)。模型输出里罕见;见第七节第 2 问。
- **`normalize_nested_fences`**(`markdown.rs:1041`)对前缀与全文可能配出不同的围栏对(外层围栏尚未关上时)。
  语料里放嵌套围栏;若 P 被它打破，外层带标签的围栏未关上且内部出现围栏行时，把定稿退回到围栏开头。
- **synoptic 高亮**:P 按样式逐行比。若多行 token(块注释、多行字符串)让同一行在前缀与全文里颜色不同，
  先确认 synoptic 是否逐行、从左到右无回看;不是的话，代码行按文本比较、样式差异记为已知限制。
- **缩进超过 3 格的围栏**(深层列表里)不被 `find_stream_safe_boundary` 认作围栏，里面的空行会被当边界——
  既有问题;语料里放一个列表项里 3 格缩进的围栏，确认至少这种常见形状成立。
- **Thinking 的秒数**:`seal_thinking` 找最新一个未盖秒数的 Thinking;已冻走就盖不上(与今天相同)。
- **性能**:末位流式回答每帧要多渲染一次前缀(围栏里是全文再渲染一次到最后一个完整行)。今天每帧已对尾部
  每个 cell 调 `cell_lines`,量级相同;同一帧里流式 cell 的渲染尽量只算一次。

## 六、测试与验收

- `make check` 全绿。
- **P 的性质测试**(`markdown.rs`):手写语料——紧/松/混合列表、嵌套列表、列表项里的段落与 3 格缩进围栏、
  长代码块、嵌套围栏、表格、引用、标题、分隔线、跨行强调、中英混排;宽度 40 与 80;**每个字符边界**切一刀，
  断言 `lines[..settled] == markdown_lines(T)[..settled]`,且 `settled` 随 `k` 单调不减。
- 混合列表的渲染用例;围栏里边写边定稿的用例。
- `freeze_target` 表驱动单测：放得下、已封口 cell 按行切、流式 cell 冻到定稿为止、Running 工具行钉住直到
  4 屏、后台任务行整格离开(含多冻几行的情形)、待答计划作为末位 cell、末位 cell 永不整格离开。旧的
  `commit_count_*`、`head_freeze_*`、`running_background_row_pins_tail_until_hard_cap_forces_freeze` 改写成
  新规则下的对应用例(最后一条的结论反过来)。
- **端到端**(`draw_frame` + TestBackend,**断言 `scrollback()` 的逐行内容**,照 plan 214 那条的写法):
  1. 一条 3 屏高的回答逐段流入：每一帧"scrollback 里本条的部分 + 屏上"逐行等于当时的显示;封口并收尾后
     等于 `markdown_lines(全文)`,无重无漏。
  2. 一个比屏高的代码块边写边进 scrollback。
  3. bash 审批面板开着，上方一条比屏高的已封口回答：面板在时顶部已在 scrollback。
  4. 一个 Running 的后台任务行之后流入一条长回答：不被剪;任务结束的那一行追加在后面。
  5. plan 214 的计划用例仍绿。
- 反证：撤掉 4.1 的列表改动，性质测试必红(用第一节那个 `- a\n- b\n\n- c` 样例);撤掉 4.5,用例 3 必红。

## 七、开工时定(问用户，一次一个)

1. **Running 的 Tool/Agent 行要不要也改成"需要时整格离开、结束时另追加一行 ✓/✗"?** 推荐**不改**,沿用 4 屏
   硬顶：前台工具与子 agent 运行时模型在等，它后面不会长出高东西;唯一的例外(`exit_plan_mode`)已由
   plan 214 用原地替换解决。改的话要给 Tool/Agent 补一套 frozen 集合与追加行，收益近零。
2. **引用式链接 / 跨空行 HTML 块这类违反 P 的输入怎么办?** 推荐**接受并写进 DESIGN**(罕见，后果是接缝处
   重或丢一两行);另一选择是检测到定义行形状就把定稿退回到它之前。

**开工时的答复(2026-09-30):** 两问都照推荐——Running 的 Tool/Agent 行不改(`Leave::AtCap`);违反 P 的
两类输入接受，写进 DESIGN。

## 八、收尾清单

- DESIGN.md 要改写(先读现状再改，不追加):TUI 一节讲提交的那段(`The commit only freezes a leading
  prefix…` 到 plan 121 的按行冻结);面板一段里 plan 214 加的 `While a panel is up the commit pauses…`;
  Plan mode 一段里 plan 214 的 "the one panel that does not pause the commit" / "the one last cell whose
  overflowing top may freeze";markdown 流式那段(`blank line, or a closed code fence) is rendered as markdown;
  the forming tail shows raw…`)。
- plan 99、121、214 的相关段落各补一行"→ 被 plan 215 改写"。
- HANDOFF:本条的状态行改 ✅ + 提交号;新教训写进第三节。

## 九、✅ 完成

2026-09-30 当次会话做完，一次提交(SHA 以本条所在提交为准)。`make check` 全绿。

**照做的:** 4.1 列表间距按源文本局部空行;4.2 `assistant_stream(text, width) -> (lines, settled)`;4.3 每种
cell 的定稿行数与 `Leave` 由 `render::shown_cells` 一处给出，画与冻共用;4.4 `freeze_target` 取代
`commit_count`/`head_freeze_lines`/`is_committable`;4.5 `draw_frame` 只剩补全弹窗暂停提交;4.6 删
`plan_awaiting_answer`,保留 `post_plan`;第五节的"文本被替换"保护(`App::replace_answer`:头 cell 已部分冻结、
新文本不以旧文本开头时清 `head_frozen`)。

**与 plan 的出入(都是实现时发现、写进了代码注释与 DESIGN):**

- **只有完整的行才算边界。** 旧 `find_stream_safe_boundary` 把"最后一段只有空格、还没换行"的行当空行边界,
  `para\n ` 之后来个 `more` 就并回同一段——显示时错一帧无所谓，冻进 scrollback 就错了。收紧后性质测试才绿;
  反证：撤掉这条，性质测试在 "lists" 语料 `…**bold\n ` 处红。
- **围栏配对与 `normalize_nested_fences` 同一套栈规则**(带信息串的一律是开，裸的关兼容的栈顶，否则开),
  二者共用 `parse_fence_line`。旧扫描器用"开了就等同长关"的简单规则，遇到嵌套示例会判错"是否在围栏里",
  而 4.2 的新分支会把整段(含尚未封口的段落)都算定稿。
- **嵌套围栏的退路比 plan 写的更窄:** plan 说"退回到围栏开头";实现是**停在外层开着的围栏里第一条围栏行
  之上**(`StreamCut::Split(nested_at)`)。围栏开头之后、第一条内层围栏行之前的代码行在最终渲染里完全相同,
  退回开头反而会让定稿行数变少(已冻的行就对不上了),所以只退到必要处，且保持单调。反证：把这一支改回
  `OpenFence`,性质测试在 "fences" 语料的 ```` ```md ```` 例子处红。
- **`blank_before` 也在 `Event::Rule` 上更新**(分隔线也是块，plan 的列表里漏了);**`OpenItem` 记下开项时的
  引用深度**,项里引用块内部的块不再各加一行空行——旧渲染对 `- a\n\n  > q` 输出两行空行，是既有 bug,
  顺手修掉("引用块里照旧不加空行"的本意)。
- **`AtCap` 的"4 屏"按"从这个 cell 起的尾巴"逐个算**,不是 plan 伪代码里开头算一次的总量——后者在 Running
  行上方有一大段已封口内容时，会把它跟着冻走，哪怕它后面只有几行。与旧 `hard_cap` 的 `remaining` 语义一致。
- **末位 cell 并非绝对不整格离开:** 一个不可拆的状态行(后台任务 Running / 排队消息，定稿 0 行)本身比活动区
  还高时(极小终端)照样整格离开——守的是不变式;表里有专门一例。
- **不是末位、仍在接收的回答**(交错的 item,见 `interleaved_display_*` 用例)照旧按整段 markdown 画，定稿行数
  取 `assistant_stream(text).1`;由 P(取 k = 全长)这两者前 `settled` 行相同。
- 性能：画与冻各调一次 `shown_cells`(冻结要用确认后的几何),与旧版同量级;围栏分支里整段与"到最后一个完整
  行"各渲染一次，未另做缓存。

**测试:**

| 测试 | 锁住什么 |
|---|---|
| `markdown::tests::settled_lines_are_the_finished_render_at_every_cut` | 性质 P:5 份语料(紧/松/混合列表、嵌套列表、列表项里的段落与 3 格缩进围栏、跨行块注释与多行字符串的长代码、嵌套围栏两种写法、表格、引用、标题、分隔线、setext、缩进代码、跨行强调、中英混排、链接)× 宽 40/80 × **每个字符边界**:`lines[..settled] == markdown_lines(全文)[..settled]`,且 `settled` 单调不减 |
| `markdown::tests::list_gaps_follow_the_blank_lines_in_the_source` | `- a\n- b\n\n- c` → `• a`/`• b`/``/`• c`;编号列表同形;后来变松的列表下紧的子列表仍紧;`>` 单独一行等于空行 |
| `markdown::tests::a_quote_in_an_item_is_set_off_once` | 上面那个双空行 bug |
| `markdown::tests::stream_cut_*`、`an_open_fence_renders_as_code_and_settles_line_by_line` | 只认完整行;嵌套围栏停在内层行之上;围栏里代码边写边高亮、逐行定稿 |
| `render::tests::freeze_target_freezes_exactly_what_overflows` | 21 例表驱动：放得下、整格离开、按行切、只补新溢出、plan 99 的高消息、流式冻到定稿、开着的回答挡住后面、定稿少于已冻、live thinking、Running 行 4 屏内钉住/超了就走/4 屏按从它起的尾巴算、后台行需要时就走(结论与旧 `running_background_row_pins_tail_until_hard_cap_forces_freeze` 相反)、不可拆行整格走、待答计划作末位、末位不整格离开及其例外 |
| `render::tests::shown_cells_say_what_each_cell_may_freeze` | 4.3 的表逐行(真实事件驱动出 live thinking 与流式回答) |
| `app::tests::an_answer_replaced_by_other_text_gives_up_its_frozen_prefix` | 延长保留接缝，换成别的文本放弃接缝 |
| `lib::tests::a_streaming_answer_reaches_scrollback_as_it_settles` | 端到端 1:三屏回答 5 字节一段流入，**每一帧** `scrollback() ++ 屏上 == 当时的显示` 且活动区不溢出;封口、收尾后等于 `markdown_lines(全文)` + 收尾线 |
| `lib::tests::a_tall_code_block_reaches_scrollback_while_it_is_written` | 端到端 2:围栏还没关，代码行已在 scrollback |
| `lib::tests::an_open_approval_does_not_hold_back_the_answer_above_it` | 端到端 3:bash 审批面板开着，上方 30 行回答的顶部已在 scrollback |
| `lib::tests::a_running_background_row_does_not_hold_back_a_long_answer` | 端到端 4:Running 后台行之后流入长回答，逐帧不剪;任务结束的那一行追加在最后 |
| `lib::tests::a_waiting_plan_reaches_scrollback_before_it_is_answered` | plan 214 那条原样保留(只把对已删函数的断言换成等价 `matches!`),仍绿 |

**反证(逐道撤回，各自必红):** 列表改回整张松紧 → 性质测试在 `…- second…\n\n` 处红(正是第一节的样例形状);
撤"只认完整行"、撤嵌套围栏退路 → 性质测试各红一次;面板重新暂停提交 → 端到端 3 红;后台行改回 `AtCap` →
端到端 4 红;流式回答定稿记 0 → 端到端 1 红;围栏分支改回旧的"围栏里没有边界" → 端到端 2 红。四条端到端
都红在"活动区放不下、顶部被剪"那句断言上。

**另做的一次真二进制检查(未提交):** 用 `tests/tui_pty.rs` 的脚本 SSE 夹具起真 `kloop`(14×80 pty),
一条 20 段落 + 30 行代码 + 20 组列表的回答切成 23 字节一段流入，把原始输出重放进带 scrollback 的 vt100,
每个标记恰好出现一次、次序正确。夹具一次发完整个响应体，所以它验的是真终端里的终态;逐帧的性质由上面的
TestBackend 端到端保证。

### 没做 / 顺带发现

- **HTML 块会并进下一段(既有 bug,未修)。** `Start(HtmlBlock)` 没有分支,`Event::Html` 的文本进了行内缓冲,
  直到下一个段落 flush:`<div>\nhi\n</div>\n\npara` 渲染成一行 `<div>hi</div>para`。与本条无关、模型输出里
  少见，记在这里，要修另立。
- 违反 P 的两类输入(引用式链接定义在使用之后、跨空行的 HTML 块)按第七节第 2 问接受，已写进 DESIGN。

