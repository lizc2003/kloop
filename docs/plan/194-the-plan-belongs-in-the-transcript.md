# Plan 194 — 计划属于转录,不属于弹层

> 来源:2026-09-22 一次审查对话。用户让审查 kloop 在另一个项目上跑的一个真实会话
> ("目的是优化 kloop"),查出 `exit_plan_mode` 那一段连续两次被拒、模型盲猜着重写了
> 三份方案。第一版结论是"拒绝不带理由,所以模型只能猜"。用户看完补了一句
> 「用中文重写后,看不到完整的重写内容」——把诊断推到前面一层:**用户不是不同意方案,
> 是根本没看到方案**。三个修法(换渲染器 / 放宽面板高度 / 计划不进面板)里用户选了最大的
> 那条:「3」。

## 一、那一屏,用户实际看到了什么

会话里第一份中文方案,`exit_plan_mode` 的 `plan` 参数:**2547 字符 / 25 源行 /
100 列宽下 47 显示行**(中文双宽)。同一轮模型还在正文里写了一份 3249 字符 / 58 行的
完整版。面板给了它们多少:

- `tui/src/render.rs:96` `PANEL_MAX_ROWS = 20`——**整个面板**的硬顶,不是 body 的。
- `tui/src/choice.rs:119` 起自下而上分配:hint 1、Yes/No 2、header 1、subject 1
  (`Exit plan mode and start on this plan?`)、prompt 1(`Do you want to proceed?`)、
  4 个分隔空行。剩给 body **9 行**,而 body 是"唯一会滚动的部分",也是终端变矮时
  **第一个**被压缩的。
- 再扣一行:`render.rs:1111` 把 `preview` **无条件当 diff 渲染**,
  `diff_stats_line`(`render.rs:1320`)数出 7 行以 `-` 开头的 markdown 列表项,
  在顶上放一行 `+0 -7`。

**剩 8 行,方案 47 行,一屏 17%。** 那 7 个列表项被 `diff_preview_lines`
(`render.rs:1342`)染成**红色**(diff 删除色),其余每一行 DIM。另外两份方案同形:
35 行 / 5 红、41 行 / 9 红。

正文那份也看不到:`render.rs:122` 给面板的预算是 `transcript_capacity -
(activity_rows + 2)`,注释自己写着"两行held back 让 separator 和至少一行 transcript
活下来"——面板在屏上时转录就剩那一行,而 PgUp/PgDn 这时归面板。

于是那段对话的形状是:**提交 → 拒 → 猜 → 提交 → 拒 → 猜 → 用户打断插话说"要 B、
wire 也改" → 第三份通过**。两次拒绝各烧掉一份 2000–3400 output token 的完整重写,
用户真正的意思最后是靠 esc 打断送达的。

**同一个错误假设在三个前端各犯了一遍**,这是本条的真正范围:

| 前端 | 位置 | 现在怎么错 |
|---|---|---|
| TUI | `render.rs:1111` | `preview` 一律走 `diff_stats_line` + `diff_preview_lines` |
| server | `server/src/lib.rs:2044` | **拿 `preview.is_some()` 当 command/fileChange 的判别式**,注释写着"A file change carries a diff preview; nothing else does"——`exit_plan_mode` 正是那个 else,于是计划审批被当成文件改动报给客户端 |
| plain | `cli/src/ui.rs:24` | `color_diff(preview)`,同样按 +/- 着色 |

## 二、裁决

**计划是转录的一部分,不是弹层的一部分。**

弹层只留它擅长的:一句问话、两个答案。方案本身进转录,按 markdown 渲染,用户用平时
那套上下翻。三条推论:

1. body 对 plan 为空 → 面板从 20 行降到 ≈8 行,**不再压着转录**。
2. 方案走 `tui/src/markdown.rs:64` 的 `markdown_lines`,不是 diff 着色。列表就是列表,
   没有假的 `+0 -7`。
3. **一份被拒的方案留在转录里**——它发生过。下一份排在它下面,两份都看得见,这正是
   用户比较"我否掉的那份和它现在给的差在哪"所需要的东西。

**本条不做"拒绝附理由"。** 那是同一次审查的另一半结论:`core/src/permissions.rs:193`
的 `PlanExitOutcome::Declined` 和 `Decision::Deny` 都不带理由字段,于是
`core/src/tools/plan_mode.rs:95` 只能告诉模型"refine the plan and call exit_plan_mode
again"。**先让人看得见——多数拒绝根本不会发生**;落地之后如果仍有盲猜,再立一条。

## 三、形状

### 3.1 `preview` 要分种类

`ConfirmRequest.preview: Option<String>`(`core/src/permissions.rs:114` 那个结构体)
现在只有一种消费方式,谁拿到都当 diff。换成带种类的一个值,大意:

```rust
pub enum ConfirmPreview {
    /// 文件改动的 diff:`+N -M` 统计 + 绿红着色,今天的行为原样保留。
    FileChange(String),
    /// 一份待批准的计划:markdown,属于转录。
    Plan(String),
}
```

`confirm_exit_plan`(`permissions.rs:822`)填 `Plan(plan)`,三个写工具填 `FileChange`。
**种类由 core 给,不由前端猜**——server 那条 `preview.is_some()` 判别式正是猜错的
代价。

### 3.2 TUI:收到就进转录

`app.rs:766` 那个 `interactions.push_back(PendingInteraction::Confirm { .. })` 的分支里,
若 preview 是 `Plan`,先 `cells.push(Cell::Plan(text))`(`Cell` 在 `app.rs:80`),
面板那边不再把它放进 body。

新 `Cell::Plan(String)` 用 `markdown_lines` 渲染,要和 `Cell::Assistant` 在视觉上
分得开——它是一份**待批准**的东西,不是模型的一句话。

### 3.3 server 与 plain

- server:`kind` 改成按种类判(`fileChange` / `plan` / `command`),`params.preview`
  跟着带上种类。**那条注释要一起改掉**,它现在是错的。
- plain(`cli/src/ui.rs:55`):`Plan` 不走 `color_diff`,原样打印(plain 没有 markdown
  渲染器,也不该为这个引入一个)。

### 3.4 工具描述加一句

`exit_plan_mode` 的描述里写明:**方案只放进 `plan` 参数,不要在正文里再写一遍**。
今天模型两份都写(正文 3249 + 参数 2547),因为参数那份它知道会被塞进一个小窗口。
转录里能看全之后,重复就纯是浪费。

## 四、坑

- **`body_rows = 0` 的布局分支要过一遍。** `choice.rs:156` 的 `separate` 在
  `body_rows > 0` 与否时走两条路,面板从"有 body"变成"没 body"是这条 plan 的常态,
  不是边角情况。
- **`diff_stats_line` / `diff_preview_lines` 不许顺手删**,`FileChange` 还在用它们。
- **`PANEL_MAX_ROWS = 20` 本条不动。** 计划出去之后,20 行对一个大 diff 仍然可能太小,
  但那是另一件事,**分开做**——本条的判据是"计划该不该在弹层里",不是"弹层该多高"。
- **scrollback 冻结(plan 99)**:新 cell 变体要确认走同一条 cell→lines 的分派,
  否则它在原生 scrollback 里会是空行。
- **`app.rs:2351`** 按 `req.description` 取值的地方(状态/测试)不受影响,但改
  `ConfirmRequest` 的字段会牵动所有构造点(`events.rs` 那几处 `preview: None`、
  `headless.rs:250`、各测试 fixture),编译器会全指出来。
- 一轮里可以有多个 pending confirm 排队(`interactions` 是 FIFO),计划 cell 的
  push 时机要和面板弹出对齐,**不能在队列里排到第三位时才进转录**。

## 五、验收

- `make check` 全绿。
- TUI:一份 47 显示行的计划提交后,**转录里完整可见**(markdown,不带 diff 色),
  面板 ≤ 10 行且只有问句与 Yes/No;拒绝之后计划仍在转录里。
- server:计划审批的 `kind` 不再是 `fileChange`。
- plain:计划不再按 +/- 着色。
- 三个前端各有一条测试钉住"计划不是 diff"。
- 一条布局测试钉住"面板没有 body 时不塌"。

## 六、开工时定(问用户)

1. `Cell::Plan` 的视觉:用什么标题词、要不要像 `SessionHeader` 那样带框。
2. 计划被拒之后,转录里要不要补一行"未批准"——不补的话,转录里会躺着一份看不出
   结局的方案。
3. server 的 `kind` 新增一个值,有没有外部客户端需要照顾(若没有,按仓库惯例
   不考虑兼容性,直接改干净)。

## 七、开工时定的三个点(2026-09-22,用户逐条拍板)

1. **`Cell::Plan` 的视觉**:不带框,一行 `▌ Plan` 标题 + markdown 正文。`SessionHeader`
   的框是一次性横幅、四行定长字段;计划是几十行带列表/代码块/表格的东西,加框要在每行
   左右各吃两列。落地时标题直接复用 `choice::header_line` 的那套(`▌ ` + BRAND + BOLD)——
   同一套视觉词汇,不新造第二种竖条。
2. **拒绝之后留不留痕**:**两边都补**。计划 cell 在弹层弹出之前就进转录,于是"没有标记"
   本来就表示"还没答";只标拒绝的话,"没标记"要同时表示"批准了"和"还没答",而这两态
   确实会同屏出现。做成 `Cell::Plan { text, status: PlanStatus }`,答完就地改。
3. **server 的 `kind`**:仓库内只有 DESIGN.md 的协议表和 server 自己的契约测试读它,
   没有外部客户端。按仓库惯例不考虑兼容性,直接加第三个值 `plan`。

## 八、✅ 完成

2026-09-22 当次会话做完,一次提交。`make check` 全绿。

### 与 plan 的两处偏离

- **`is_committable` 不给 `Cell::Plan` 开特例。** 原打算"待答的计划不许冻进 scrollback",
  写完发现它会连带关掉 plan 99 的 `head_freeze_lines`(那个函数第一句就是
  `!is_committable(&cells[0])` 直接返回)——于是一份比视口高的计划顶部会被 `draw`
  剪掉**且进不了 scrollback**,正是本条要消灭的那个失败。取舍:**能看见 > 有标记**。
  一份在答复之前就滚进 scrollback 的计划会少一个 ✓/✗,而它已经滚出视线了。
- **`settle_plan` 用"最老的待答计划",不用 id 映射。** `exit_plan_mode` 的
  `is_concurrency_safe` 是 false,同一时刻不可能有两个计划弹层排队,所以"最老的待答"
  就是精确匹配;计划 cell 已被冻进 scrollback 时它是 no-op——和迟到的 ToolEnd 落在
  已提交行上同一个惯例。

### 测试

| 测试 | 锁住什么 |
|---|---|
| `permissions::tests::confirm_request_carries_a_change_preview` | 三个写工具的 preview 是 `ConfirmPreview::FileChange`,非文件调用仍是 `None`——**种类跟着文本走** |
| `permissions::tests::confirm_exit_plan_switches_back_or_stays` | `confirm_exit_plan` 填的是 `ConfirmPreview::Plan`,不是裸串 |
| `render::tests::a_plan_goes_to_the_transcript_and_leaves_the_panel_a_question` | 面板 body 为空 + 整 10 行逐行断言(一句问话两个答案);计划整段在转录里、无 `+0 -N`、**没有任何一个 span 是红的** |
| `render::tests::a_plan_cell_marks_how_it_ended` | 三态各自的末行:待答无标记、`✓ approved`、`✗ not approved — still planning` |
| `render::tests::draw_shows_a_tall_plan_in_the_transcript_under_a_ten_row_panel` | 真 `TestBackend` 一帧:40 条 step 一条不少地在屏上,`▌ Plan` 在,面板从 header 到 hint 正好 10 行、不带滚动提示 |
| `app::tests::plan_cells_are_posted_on_arrival_and_stamped_in_queue_order` | 两份计划都在**答复之前**就进了转录;Esc/Enter 各自盖到自己那份(整对象断言两个 cell) |
| `app::tests::a_turn_ending_declines_the_plan_it_left_unanswered` | 轮次死掉 → sender 被丢 → core 读作拒绝,转录里同步盖上 ✗(否则它会永远停在待答态) |
| `choice::tests::a_panel_with_no_body_keeps_its_separators_and_its_height` | 无 body 的布局分支:分隔空行还在,10 行就是 10 行,给 20 行也不涨 |
| `server.rs::plan_approval_is_not_reported_as_a_file_change` | 端到端(scripted provider 真走 `enter_plan_mode` → `exit_plan_mode`):`kind == "plan"`、preview 逐字等于计划原文、decline 之后 tool_call 是 `completed` 不是 `failed` |
| `ui::tests::a_plan_preview_is_printed_as_written_and_a_diff_is_coloured` | plain:计划原样打印,diff 仍上 ANSI,无 preview 仍是空串 |

### 没做(有意)

- **拒绝附理由**——本条第二节已经写明不做,落地后仍成立。顺带一个读数:TUI 的拒绝行
  现在就写着 `No, and tell kloop what to do differently`,入口本来就在。
- **`PANEL_MAX_ROWS = 20`** 一个字没动。
- **一份待答的、比视口还高的计划,顶部仍会被剪掉**(它是最后一个 cell,而 plan 99 的
  `head_freeze_lines` 明说"最后一个 cell 一律不碰")。这是 plan 99 划的边界,对任何一个
  高过视口的末位 cell 都成立,答完之后它就正常冻进 scrollback。要改是另立一条。
