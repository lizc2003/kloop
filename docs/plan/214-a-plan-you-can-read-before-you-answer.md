# Plan 214 — 答之前就要能看全计划;备注步骤删掉

> 来源:2026-09-29 用户 dogfood 截图。「plan 第一次还是显示不完整，选 no 后，再调整，能显示完整的
> plan。另外，选 no 后，选项里还有个 optional,不好理解。」对照当天那次 kloop 会话的记录核实:
> 一次会话里三份计划都在 53–59 显示行之间(120 列),审批时顶部被剪;选 No 之后
> 模型调 `ask_user_question`,选项带 preview,选中后弹出 `Notes (optional) >`。用户拍板：一个 plan
> 做两件事，「简洁彻底的改正 plan 的问题」;备注「有用吗」→ 整条删，连 `QuestionAnswer.notes`。

## 一、计划为什么答完才看得全

plan 194 把计划从面板挪进了转录，末节"没做"里记了一条边界:待答的、比视口高的计划顶部会被剪。
实际是三道闸叠在一起，任何一道都足以让顶部既不在屏上也不在 scrollback:

1. **`draw_frame` 在有 interaction 时整个跳过提交**(`tui/src/lib.rs` 的 `overlay_open`)。这是
   plan 76 加的，那时面板还是盖在转录上的浮窗;plan 104 已把面板并入 `live_chrome_layout` 的同一份
   高度预算，这道闸对"内容在转录里"的计划审批已无理由。
2. **`exit_plan_mode` 的工具行在计划上方、状态 Running**,`commit_count` 在它这里停(除非积压超过
   4 屏)。而且这一行是误导的：被拒时工具结果不是 error,它会打上 `✓ Exit plan mode`,紧挨着下面
   计划的 `✗ not approved`。
3. **计划是最后一个 cell**,`head_freeze_lines` 对最后一个 cell 一律不动。

一答完，面板关、工具行结束、模型接着出新 cell,三道闸同时解除，整份计划被冻进 scrollback——
所以"选 No 之后能看全"。顺序正好反了。

## 二、改法(三处，各对一道闸)

1. **计划 cell 取代它的工具行。** 审批到达时，找到那条 Running 的 `exit_plan_mode` 行，原地换成
   `Cell::Plan`(同一个下标，其余索引表不用挪),并从 `tool_cells` 摘掉它的 id——之后的 ToolEnd
   找不到行，照既有惯例是 no-op。计划 cell 的 `✓/✗` 本来就说清了这次调用的结局。找不到这一行
   (已被冻进 scrollback)时退回 push 到末尾，即今天的行为。
2. **计划审批不挡提交。** `overlay_open` 只在队首 interaction 不是计划审批时成立;fork picker、补全
   popup 与其它审批/提问照旧。
3. **待答计划可以冻前缀，即使它是最后一个 cell。** 计划正文从到达起就不变，答复只在末尾追加一行
   `✓/✗`,前缀稳定;`head_freeze_lines` 对最后一个 cell 的禁令对它放开，对其它 cell 不变。

代价(接受):答复后面板关闭，转录区变高 ≈ 面板行数，直到模型的下一行输出到来之前，视口顶部会有
几行空白。scrollback 本身是连续的(冻结的前缀接着之后提交的剩余部分),空白只在视口里、一闪而过。

## 三、备注步骤：整条删

`ask_user_question` 选中带 preview 的选项后强制进 `QuestionPhase::Notes`(plain 同样追问一句)。
它能做的("选 A,但……")`Type something else` 已经能做;只对 preview 选项出现、看不出为什么;
那一步按 Esc 连选择一起取消。唯一剩下的产出方是 server 协议的 `notes?`,而 server 没有外部客户端
(plan 194 核过)。所以删：TUI 的 phase 与编辑器、plain 的追问、`QuestionAnswer.notes`、server
`QuestionResponse.notes`(`deny_unknown_fields`,再带就按 malformed 失败关闭)、格式化与校验分支、
DESIGN.md 两处。**不动** `ask_user_question` 输入 schema 里的 `annotations.notes`:那是照 cc schema
收下再忽略的模型参数，与用户备注无关。

## 四、验收

- `make check` 全绿。
- 端到端(`draw_frame` + TestBackend):工具行 → 一份 40 行计划的审批到达，面板仍开着时计划前缀已在
  scrollback、视口从接缝下一行开始、两者合起来一行不少;答复后末尾多 `✓ approved` 而前缀不动。
- App:审批到达后 cells 里工具行位置就是计划 cell(整对象断言),迟到的 ToolEnd 不改任何 cell。
- `head_freeze_lines`:最后一个 cell 是计划时冻溢出部分;是别的 cell 时仍为 0。
- 提问：带 preview 的选项 Enter 即提交，答案里没有备注;server 回复带 `notes` 被拒。

## 五、✅ 完成

2026-09-29 当次会话做完，一次提交(SHA 以本条所在提交为准)。`make check` 全绿。按第二节三处照做，没有偏离。

| 测试 | 锁住什么 |
|---|---|
| `lib::tests::a_waiting_plan_reaches_scrollback_before_it_is_answered` | 真 `draw_frame` + TestBackend:工具行上跑来一份 40 步计划，**面板还开着**,TestBackend 的 scrollback 逐行等于"之前的用户消息 + 计划前 34 行",视口从第 35 行起、正好填满;Enter 批准后接缝不动，末行 `✓ approved` |
| `app::tests::a_plan_takes_the_place_of_its_running_row` | 计划 cell 落在 `exit_plan_mode` 行的位置(整对象断言),拒绝后迟到的 ToolEnd 不改任何 cell |
| `render::tests::head_freeze_takes_a_waiting_plans_overflow_though_it_is_last` | 最后一个 cell 是计划时冻溢出部分;旧用例 "head is the last cell"(非计划)仍为 0 |
| `app::tests::question_single_preview_answers_on_enter_and_esc_cancels` | 带 preview 的选项 Enter 即提交，没有备注步骤 |
| `wire::tests::a_question_answer_is_a_selection_or_other_text_and_nothing_else` | server 回复带 `notes` 按 unknown field 拒绝 |

反证：三处修改逐一撤回，端到端那条每次都红(撤工具行替换时 app 那条也红，撤 `head_freeze_lines` 放开时 render 那条也红)。

**→ 被 plan 215 改写**:计划审批不暂停提交的特例(`App::plan_awaiting_answer`)与 `head_freeze_lines` 给计划开的口
一起删掉——现在所有面板都不暂停提交，待答计划按"全部定稿、不可整格离开"冻。`post_plan` 原地替换保留;
上面端到端那条原样保留、仍绿(只把对已删函数的断言换成等价的 `matches!`)。

### 没做(有意)

- **其它面板仍暂停提交。** bash 审批、提问等面板开着时，转录尾部照旧可能被剪;它们的内容在面板里，
  放开会让每次答复后视口顶部闪一段空白。要改另立一条。**→ 已立 plan 215(逐行冻结，统一处理四道闸)。**
- **resume 的计划仍是工具行。** `cells_from_history` 里 `exit_plan_mode` 依旧渲染成工具行 + 结果预览;
  从结果文案反推批准/拒绝太脆，不在本条。
