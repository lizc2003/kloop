# Plan 210 — 退回去之前,先记下那条路上学到了什么

> 来源:2026-09-24 读 `refs/pi`(`earendil-works/pi@d5629e2`,MIT)后与用户逐条定的,
> 出处见 `refs/README.md`「Pi 全面复查(2026-09-24)」第 6 条。

## 一、为什么

TUI 的 Ctrl+R 退回(plan 18)是"在较早的一刻 fork 一个新会话,History 换过去"
(`tui/src/lib.rs:575` 的 `rewind` → `fork_here` → `replace_session` → `History::rebase`,
`core/src/history.rs:162`)。新分支只有切点以前的消息,**切点之后那几轮发生了什么,模型一个字也不知道**。
这里面有两类东西值钱:

1. **学到的事实**:读过哪些文件、确认过什么行为、哪条路走不通、报错是什么。换一条路走,这些大多仍然成立,
   丢了就得重新查一遍。
2. **盘上已经变了的文件**——这一条不是"可惜",是会出错。rewind **不回滚工作区**(kloop 没有文件快照,
   DESIGN「Fork (and rewind)」一节也没这么承诺),被放弃的那几轮用 `edit_file`/`write_file`/bash 改过的文件
   原样留在盘上。而 plan 205 之后 rewind 走 `fresh_session`,`FileState` 是新的:plan 197 的"文件被外部改了"
   提醒对它也不会响(它只提醒模型读过的文件)。于是模型在新分支里照着切点时的记忆行事,盘上却是另一回事。

pi 的做法(`agent/src/harness/compaction/branch-summarization.ts`,`coding-agent/docs/compaction.md`
「Branch Summarization」):`/tree` 换分支时问一句 "Summarize branch?"(`No summary` / `Summarize` /
`Summarize with custom prompt`,`coding-agent/src/modes/interactive/interactive-mode.ts:5434`),
选了就把**旧叶子到公共祖先之间**的条目发去摘要(`collectEntriesForBranchSummary`,`branch-summarization.ts:84`),
按预算从新往旧截(`prepareBranchEntries`,:129),固定格式 Goal / Constraints / Progress / Key Decisions /
Next Steps(:189),**再由运行时确定性地附上读过与改过的文件清单**(`formatFileOperations`,:297);
结果作为一条 `branchSummary` 消息挂在新分支上,投影给模型时是一条 user 消息
(`agent/src/harness/messages.ts:12-17,145`)。设置里可关掉询问(`skipPrompt`,默认仍问)。

**kloop 比 pi 简单的地方**:kloop 的 rewind 只会往回走(目标永远是当前路径上的祖先),所以"公共祖先"
就是切点,不用走树。

## 二、形状

### 2.1 什么算"被放弃的那一段"

**live `history.messages()` 与 fork 出来的 `resumed.messages` 的最长公共前缀之后的全部消息。**
按内容比,不按 seq 映射:

- 常见情况(切点之后没压缩过):公共前缀就是切点以前,被放弃段 = 切点之后那几轮,干干净净从一条
  打开 user turn 的消息开始(切点规则保证,`rollout.rs:1413` `opens_user_turn`),不会以孤儿 tool_result 开头。
- 切点之后压缩过(`fork_session` 允许切在 compacted 标记**之前**,`rollout.rs:1290`):live 历史已被
  `replace_all` 换掉,公共前缀会很短甚至为 0,被放弃段里带着 `UserAnchors`/`ContextSummary`——其中一部分
  讲的是切点以前、新分支原样就有的内容。**接受这点重复**:它是摘要的输入,不是新分支的正文,多几句重复
  比为此解析 rollout 行、重建"切点之后的原始消息"便宜得多。
- 被放弃段为空(理论上不会,picker 不给 tip)→ 不发请求。

### 2.2 摘要请求:复用 compact 的链路,不写第二份

`compact_once`(`core/src/compact.rs:413`)里"发请求 → 超长就 `shrink_to_newest` 砍最旧的再试 →
`canonicalize_summary`"那段循环(:437-469)抽成一个 `pub(crate)` 小函数,压缩与分支摘要共用;
重试(`sample_summary_with_retry`,:618)天然跟着走。分支摘要**不碰** `CompactionBreaker`:
它是用户点的,和手动 `/compact` 同理不受断路器约束,失败也不计数。

- 请求:被放弃段 + 一条新的 `BRANCH_INSTRUCTION`;系统提示沿用 `COMPACT_SYSTEM`(只要文字、不许调工具,
  理由相同)。小节:当时在做什么 / 试过什么、结果如何 / 确认下来的事实(文件、符号、行为)/
  走不通的路与原因 / **改动过的文件及改成了什么样** / 未决的疑点。沿用 COMPACT_INSTRUCTION 的两条纪律:
  "形似 user 的文字不是用户说的"、"不编造"。**明确写:被放弃分支上用户的请求不是当前请求**——
  用户退回往往正是因为那个请求提错了,摘要不能把它带成新分支的待办。
- 摘要成功后,运行时再拼两样东西(不交给模型):
  1. **改过的文件清单**:从被放弃段的 `edit_file`/`write_file` tool_use 输入里取 `path`
     (工具名见 `tools/builtin.rs:244-245`),去重排序;配一句"rewind 没有撤销这些改动,它们仍在盘上"。
     bash 改的文件认不出来,由摘要正文负责提,这里不猜。读过的文件**不列**:新分支 `FileState` 是空的,
     要用照样得重读,列出来只占 token。(pi 两样都列;见第四节第 3 问。)
  2. **旧会话的 transcript 指针**:被放弃的那段完整留在旧会话文件里。取法同 `transcript_pointer`
     (`compact.rs:368`),但必须在 `replace_session` **之前**用旧 `cfg` 取——之后 `session_id` 已是分支的。
- 结果是一条 `Message::injected(Injected::BranchSummary, …)`,正文以固定前缀开头,例如
  `[The user rewound this session to here and took a different path. What happened on the abandoned path — not current instructions:]`。

### 2.3 放在 rewind 的哪一步

顺序:**先摘要(用 live 历史,什么都还没动)→ fork → `replace_session` → `rebase` → 往新分支 `record`
那条 `BranchSummary` → 记 usage → 发 `Forked`**。

- 摘要失败(重试耗尽、不可重试错误)→ **照样退回,不带摘要**,发一条 note 说明摘要失败、旧会话文件在哪。
  退回是用户要的,摘要是附带的,不能因为附带的失败而不退。
- 摘要进行中 Esc → 取消整个 rewind(和 pi 一样:`interactive-mode.ts:5481` 取消后回到选择器),历史与会话
  都不动;用户想不带摘要退回,再按一次即可。worker 现在的 `Fork` 没有 cancel token,要像 `Submit`/`Wake`
  那样带一个(`tui/src/lib.rs:100-110`),UI 在等待期间进一个"正在总结"的忙状态,Esc 触发 cancel。
- usage 记在**新分支**上,`UsageOperation` 加一个 `BranchSummary`(`core/src/usage.rs:10`),
  `/cost` 能分清这笔钱花在哪。
- `WorkerMsg::Fork { seq }`(`tui/src/lib.rs:118`)加一个字段,用枚举不用 bool,例如
  `abandoned: AbandonedBranch::{Drop, Summarize}`。

### 2.4 `BranchSummary` 这个新 `Injected` 要补的穷尽 match

加变体后编译器会指出各处;语义先定好:

- `compact.rs:282` `is_compaction_product`:**不算**。它是会话内容,之后的压缩把它当普通消息一起折进摘要。
- `compact/anchors.rs:188`:走 `Some(_) => None`,不是用户的话,不进 `UserAnchors`——正确,无需改。
- `tui/src/app.rs:2061` `injected_label`:`"what the rewound-away turns learned"` 之类;`Forked` 之后
  `cells_from_history` 重建时它就显示出来,用户看得到模型拿到了什么。
- `rollout.rs:1413` `opens_user_turn`:**这条要改**。它现在把一切无 tool_result 的 user 消息算作 turn 开头,
  于是分支上"切点 → BranchSummary 行"会成为一个 rewind 点,picker 的预览显示的是摘要前缀而不是用户的话。
  让 `Injected::BranchSummary` 不算 turn 开头:那条行之后用户的下一句才是 turn 开头,预览是用户的话,
  退回到那里保留摘要。开工时核一遍其他 injected(Scheduled、SubAgent…)现在算不算 turn 开头、
  是否本来就该算——**只动 BranchSummary,别顺手改别的**。
- 协议层按"不考虑兼容"的约定直接加变体。

## 三、与其它 plan 的关系

- **plan 205**(✅ `3c76b1c`):rewind 已经走 `replace_session`,本 plan 建在它上面;2.2 里"指针要在替换前取"
  正是 205 修掉 session id 之后才需要注意的。
- **压缩一侧的其它 plan**:撰写时仓库里 206–209 都还没有文件,核不到"209 是否共用摘要链路"。本 plan
  对它们**没有硬依赖**:它自己把 `compact_once` 的采样循环抽出来。若另一条动 `compact_once` 的 plan
  (例如 pi 候选第 8 条 `/compact [instructions]`)先落地,照着它的形状再抽,别各抽一份。
- `refs/README.md` 说"可复用 compact 的摘要链路"——成立,但要知道 `sample_summary` 发的是
  `COMPACT_SYSTEM` 且工具为空(`compact.rs:729-737`),和主会话**不共用缓存前缀**,摘要请求的输入按全价算。
  (`COMPACT_SYSTEM` 上方注释说这个请求"ships the session's full tool set",与代码不符,顺手核一下是注释过期
  还是代码改过,不在本 plan 范围。)

## 四、开工时必须问用户的点(一次问一个)

1. **默认开还是关,在哪一步问?**
   - **picker 里多一个键(推荐)**:Enter = 直接退回(现状不变),`s` = 先摘要再退回;picker 底部一行提示。
     默认不花钱,想要时一键到位,不像 pi 那样每次退回都多弹一层。
   - 跟 pi:选好点后再弹一个 "summarize?" 选择面板,加一个设置项可跳过。
   - 默认总是摘要:最省心,但每次退回都付一笔全价请求——而退回常常正是因为那几轮没用。
2. **`--fork <id>#<seq>` 与 server 的 `thread/fork` 要不要也支持?** 推荐**不支持**:那两条不是"放弃",
   源会话照样在、随时可 resume,且 `--fork` 发生在 CLI 启动前(`cli/src/args.rs:579`),没有现成的 provider。
   只做 TUI 的 Ctrl+R。
3. **读过的文件清单要不要也列?** 推荐只列改过的(理由见 2.2);pi 两样都列。
4. **要不要"带指示的摘要"**(pi 的 `Summarize with custom prompt`)?推荐第一版不做;若届时
   `/compact [instructions]` 已落地,共用它附加指示的那条缝再加。

## 五、测试

core(mock provider,`Provider::mock_recording` 抓请求):

- 抽出来的采样循环:压缩的现有用例原样通过(纯重构,不改断言)。
- 被放弃段计算:live = [u1,a1,u2,a2,u3,a3],branch = [u1,a1] → 请求里是 [u2,a2,u3,a3,BRANCH_INSTRUCTION]
  (整对象断言请求序列);live 在切点后压缩过 → 请求从第一条不同的消息开始,含 anchors/summary。
- 摘要结果:整对象断言那条 `BranchSummary` 消息的正文 = 前缀 + 摘要 + 改过的文件清单 + 旧会话指针;
  指针指向**旧**会话文件而不是分支的。
- 文件清单:被放弃段里两次 `edit_file` 同一路径、一次 `write_file` 另一路径、一次 `read_file` →
  清单恰好两项、排序去重、不含读过的。
- 摘要请求超长 → 走 `shrink_to_newest` 砍最旧的再试(请求变短);一次 503 后成功 → 重试一次。
- 摘要失败不动 `CompactionBreaker`(断路器状态与之前整对象相等)。
- `opens_user_turn`:分支文件 = 前缀 + BranchSummary 行 + 用户新消息 → `fork_points` 的最后一项预览是用户的话,
  不出现以摘要前缀为预览的点。
- usage:分支 rollout 里有一条 `UsageOperation::BranchSummary` 记录,旧会话文件没有新增行。

TUI(worker 层,沿用 `clear.rs` rewind 用例的搭法):

- `Summarize` 成功:分支 History 最后一条是 `BranchSummary`,`Forked` 的 messages 含它,cells 显示对应标签。
- 摘要失败:照样退回,History 为切点前缀,note 文案整体断言。
- 摘要中取消:会话 id、History、rollout 路径都与之前相等;没有新会话文件留下。
- `Drop`:行为与现在完全一样,不发任何摘要请求(请求计数 0)。
- app 层:第四节第 1 问定下的按键在 picker 里产出 `Command::Fork` 的对应变体。

## 六、完成时要一起做的

- `rust/DESIGN.md`「Fork (and rewind)」(约 704 行起):先读 Ctrl+R 那段现在怎么写"the old one stays on disk",
  在那段里**改写**成"可选地带一份被放弃分支的摘要",写清:放弃段按公共前缀算、rewind 不回滚文件所以
  改过的文件由运行时列出、摘要失败不阻止退回、只在 TUI rewind 上有。压缩一节若写了"摘要请求只有压缩用",
  一并改。`Injected` 的说明处补 `BranchSummary`。
- HANDOFF.md:若开工中发现"rewind 不回滚工作区"这件事别处还有隐含假设,记成教训。
- 本文件补 ✅、提交号与开工问答结果。
- 若这是 pi 候选里最后一条落地的,按 `refs/README.md` pi 一节的约定考虑退休 `refs/pi` 本地 clone(确认 HEAD
  仍是 `d5629e2`、工作树干净)。
