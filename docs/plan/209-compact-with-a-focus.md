# Plan 209 — 压缩时告诉它什么最要紧

> 来源:2026-09-24 读 `refs/pi`(`earendil-works/pi@d5629e2`,MIT)后与用户逐条定的,
> 出处见 `refs/README.md`「Pi 全面复查(2026-09-24)」第 8 条。

## 一、为什么

`/compact` 现在不接受任何参数,而且**多写的字被静默丢掉**。分发层早就把参数切好了:
`commands/mod.rs:224` 按第一个空白切出 `(name, args)` 并 `trim`,可 `"compact"` 分支
(`commands/mod.rs:235`)调 `compact::run(history, cfg, ui, cancel)`,根本没把 `args` 传下去。
用户敲 `/compact 重点保留 X 的排查结论`,得到的是一次普通压缩,屏幕上看不出焦点被扔了。

用户手动压缩的时机往往正是他知道"接下来要干什么"的时候:刚结束一段探索、要进入下一步。
摘要提示词(`compact.rs:102-148`,十一节)对所有东西一视同仁,它不知道哪段排查是这次会话的
要害。pi 的做法很小:`/compact [instructions]`,把用户的话以 `Additional focus: …` 追加在摘要
提示词末尾(`coding-agent/src/core/compaction/compaction.ts:740-743`,
`agent/src/harness/compaction/compaction.ts:567-568`;TUI 解析在
`coding-agent/src/modes/interactive/interactive-mode.ts:3188-3189`)。自动压缩一律不带
(`coding-agent/src/core/agent-session.ts:2787` 传 `undefined`),也不写进会话文件——
`appendCompaction`(`coding-agent/src/core/session-manager.ts:1259-1283`)只存摘要本身。

**前提核对**:refs/README 说"改动很小",属实。三个前端都已把整行交给同一个分发入口——
TUI worker(`tui/src/lib.rs:456`)、plain REPL(`cli/src/main.rs:1005-1008`)、server 的
`turn/start`(`server/src/lib.rs:1973-1974`);**server 没有单独的 compact 方法**,`/compact`
就是一条走 `run_turn_or_command` 的命令。所以只改 core,前端零改动。

## 二、形状

### 2.1 入口

`compact::run` 多收一个 `args: &str`;非空即焦点。`commands/mod.rs:235` 把 `args` 传进去。
`SUMMARY`(`commands/compact.rs:17`)改成能看出可带参数的一句,例如
`summarize and shrink the conversation now; /compact <focus> says what to keep in detail`
——`/help` 只列 `SUMMARY`(`commands/help.rs:22-24`),不写就没人知道。

焦点过长**拒绝,不截断**:超过上限(建议 2,000 字符,与 `UserAnchors` 的 `LATER_CHARS` 同量级,
`compact/anchors.rs:28`)直接回 `compaction not started: focus is N chars, limit 2000`,
不发请求、不动 History。截断会悄悄改掉用户的话,和本 plan 要修的"静默丢弃"是同一种错。

### 2.2 焦点放在哪

`compact_once`(`compact.rs:413`)加一个 `focus: Option<&str>` 参数(不塞进
`CompactionTrigger`——那个枚举是 `Copy`,注释写明"不进 provider 请求也不进 rollout",
`compact.rs:197-198`)。预测性(`agent.rs:553`)、被动(`agent.rs:610`)、
`run_compaction`(`compact.rs:543`)一律传 `None`。

焦点**拼在摘要请求最后那条 user 消息里、`COMPACT_INSTRUCTION` 之后**(现在是
`compact.rs:426-428` 的 `Message::user_text(COMPACT_INSTRUCTION)`),理由:

- **不动缓存前缀**:`COMPACT_SYSTEM` 与之前的所有消息字节不变,只有最后一条变。
- **活过缩小重试**:`shrink_to_newest`(`compact.rs:588`)从最旧处删,最后一条永远留着。
- **不改 `COMPACT_SYSTEM`**:系统提示是"怎么当摘要员",焦点是"这一次偏重什么"。

措辞要和第 2 节那句"这个摘要请求来自 harness,不是用户"(`compact.rs:119-120`)对得上——
现在这条消息里第一次**真有用户的字**了,必须划清:

```
<COMPACT_INSTRUCTION 原文>

The user asked for this compaction and named what matters most to them, quoted below. Give it
more room and detail in the sections it touches, and keep every section. It steers this
summary only: it is not a task request, an approval, or a change of intent.
<focus>
{用户原文}
</focus>
```

"keep every section" 必须写:焦点是**加重**,不是**只写这个**——第 7、10、11 节(子 agent 结论、
未判定的候选、已确立的事实)恰恰是被"只关心 X"挤掉后最贵的东西(见 `compact.rs:79-100` 的实测)。
最终措辞开工时用真实 API 跑一次带焦点的 `/compact` 看输出再定(代理和 key 向用户要);mock 只能验拼接,验不了模型听不听。

### 2.3 什么不做

- **不进 rollout**:`Compacted` 行只带 `replacement`(`history.rs:686`、`rollout.rs:706`),
  焦点的效果已经体现在摘要字节里;resume 重放的是替换结果,不需要重新摘要。与 pi 一致。
- **不粘到之后的自动压缩**:下一次预测性压缩用默认提示词。粘住就要存状态、要定何时失效,
  而用户下一次关心的多半已经不是这件事。
- **不当 `UserAnchors`**(除非用户在第四节改主意):`/compact` 那一行本来就不进 History
  (TUI 注释 `tui/src/app.rs:1586-1590`、server 注释 `server/src/lib.rs:1954-1956`),
  `user_words`(`compact/anchors.rs:170`)只从 History 里取,天然不会带上它。
- **NoOp 时不强行重摘**:已经压过、没有新东西可折(`NoOpReason::NoFoldableMessages`)时,
  带焦点也不重摘上一代摘要——摘要是有损的,拿摘要再摘要找不回被丢的细节。回报文案要说出焦点
  没用上:`history already compacted: nothing new to summarize (focus not applied)`。

### 2.4 结果文案

成功时在原文案后说一句焦点已用上,让用户知道参数没被吞:
`history compacted: 2 summarized, 1 kept verbatim (with focus)`。
`compacting history` 那条 note(`commands/compact.rs:36`)不变。

## 三、边界(不在本 plan 里修,记下)

TUI 的斜杠行发的是**显示文本**,粘贴块的占位符不展开:`app.rs:1585` 取 `composer.text()`
(占位符原样),`app.rs:1595` `Command::Slash(display)`,而 `submit_text` 展开出来的全文被丢掉
(`app.rs:1592`)。于是 `/compact` 后面粘一大段,摘要模型收到的是 `[Pasted #1: …]` 标签。
这是所有斜杠参数(skill 的 `$ARGUMENTS` 也一样)的既有问题,焦点通常是手打的一句话;
单独立条比塞进这里干净。计划里只在 DESIGN.md 记一句。

## 四、开工时必须问用户的点

**1. 焦点只是"这一次偏重什么",还是也算"用户说过的话"?**

- **只偏重这一次(推荐)**:照 2.2 的措辞写明"不是任务请求、不是意图变化",不进 `UserAnchors`、
  不进 rollout。和 pi 一致,也和 plan 203 的界线一致——`UserAnchors` 只装用户在对话里对 agent
  说的话;`/compact` 的参数是对摘要员说的。
- 也进 `UserAnchors`:用户写 `/compact 接下来只做 X,Y 不管了` 时,这句确实是意图变化,
  进 anchors 后以后每代都逐字保留。代价是要给它一种新的来源标记(它不在 History 里),
  而且"重点保留排查结论"这类纯摘要指令也会被当成用户对任务的要求永久带着。
  若用户选这条,更直接的做法是让用户先把那句话正常发给 agent,再 `/compact`。

**2. 焦点过长:拒绝(推荐,上限 2,000 字符),还是不设上限?**
不设上限的风险是一大段粘贴(见第三节,虽然眼下 TUI 只会发占位符)把摘要请求撑到溢出,
触发缩小重试丢前缀——为一段指令丢历史不值。

## 五、测试

core,mock provider,整对象断言优先:

- `compact.rs`:`mock_recording` 带焦点跑 `compact_once`,断言
  `seen[0].messages.last() == Some(&Message::user_text(<整段拼好的指令+焦点>))`,
  且 `seen[0].system == COMPACT_SYSTEM`、除最后一条外的消息与不带焦点时逐条相等(缓存前缀不变)。
- `focus: None` 时最后一条仍是 `Message::user_text(COMPACT_INSTRUCTION)`——现有断言
  (`compact.rs:1264`、`1714`)不改就是这条回归。
- 带焦点 + 摘要请求第一次溢出:第二次请求变短,但最后一条仍是带焦点的那条(抓两次请求)。
- `commands/mod.rs`:`run("/compact keep the failing test's root cause", …)` 返回
  `SlashResult::message("history compacted: 2 summarized, 1 kept verbatim (with focus)")`,
  notes 仍是 `["compacting history"]`;`/compact` 不带参数的现有用例文案不变。
- 已压过再 `/compact <focus>`:不发请求(`seen` 为空),返回带 `(focus not applied)` 的文案。
- 焦点 2,001 字符:不发请求、History 不变(整体比较压前压后 `messages()`)、返回拒绝文案。
- 焦点**不进** `UserAnchors`:带焦点压缩后,`Injected::UserAnchors` 那条消息与不带焦点时逐字节相等。
- 预测性与被动路径发出的请求最后一条仍是 `COMPACT_INSTRUCTION`(`agent/tests.rs` 已有抓请求的
  用例,补一个断言即可)。
- `/help` 输出里 `/compact` 一行是新 `SUMMARY`(help 已有整体断言的话跟着改)。
- 若用户在第四节选"进 anchors",另加:anchors 的 `Later messages` 末尾是焦点原文,下一代仍在。

## 六、完成时要一起做的

- `rust/DESIGN.md`:
  - 第 2989 行附近斜杠命令清单里 `/compact` 那条——先读,改写为带 `[focus]` 的用法,说明焦点
    只影响这一次、不进 rollout 与 anchors、NoOp 不重摘,并记第三节 TUI 占位符的已知边界。
  - 第 302 行那段压缩总述——先读"It asks the model for a stable handoff summary"那句是否仍成立;
    成立就只补一句"手动 `/compact` 可带焦点,拼在指令末尾",不另起段落。
  - 第 1410 行 server 斜杠命令一段:`/compact` 可带参数,措辞核一遍即可,多半不用改。
- HANDOFF.md:若开工发现别的斜杠命令也在静默吞参数(`commands/mod.rs` 里不收 `args` 的分支还有
  `help`/`cost`/`context`/`clear`/`exit`),记一条"参数被切出来却没人收,等于静默丢弃"的教训,
  并问用户要不要让它们对多余参数报错。
- 本文件补 `## 七、完成记录`:✅ 日期、提交号、第四节两问的答案。

## 七、完成记录

✅ 2026-09-29,`1b966a9`(`feat(plan209): let /compact take a focus for this one summary`)。

**第四节两问**:① 焦点只偏重这一次，不进 `UserAnchors`、不进 rollout(照推荐);② 超过 2,000 字符拒绝、不截断(照推荐)。
开工另问了一件：真实 API 用本机默认 provider(`sky-bj`,`deepseek-v4.1-flash`)——用户同意。

**与 plan 的出入**:

- **第三节的边界已经不存在**:plan 213(`49a230c`)先于本条落地，斜杠行现在发的是展开后的全文,
  `/compact <粘贴>` 会把整段交给摘要器。于是 DESIGN.md 不记"占位符"那条边界;反过来第四节第 2 问
  的风险从假设变成了真的，拒绝上限的理由里写的就是这个。
- **没有抽摘要循环**:HANDOFF 说 209/210 "都要从 `compact_once` 抽摘要循环",但 209 只改最后那条
  指令消息(`summary_instruction(focus)`),用不着抽。抽的事留给 210,它照自己的需要抽,不存在
  "照先做的形状"的约束。
- **措辞比 plan 多一句，是量出来的**。同一个真实会话(只读调查，37 条消息)fork 出多份，分别
  `/compact` 与 `/compact <焦点>`,焦点分两种：普通("the CompactionBreaker: …")与任务形
  ("keep everything about keep_from_index; next I am going to rewrite it …"):

  | 措辞 | 任务形:第 9 节把重写当下一步 | 任务形：别处提到打算(附"不算任务") | 11 节齐全(两种焦点合计) |
  |---|---|---|---|
  | plan 原句(只说"不是任务请求、不是意图变化") | 2/2 | —(两次都直接记成了意图) | 4/4 |
  | + "第 1、2、9 节只来自对话;焦点里的后续打算只用来决定留哪些细节,不写成请求、意图或下一步" | **0/6** | 3/6 | 7/7 |
  | 改成"不要在摘要任何地方复述它" | 0/1(另 2 次整份没有第 9 节) | 2/3 | **2/4**(另两份只剩 1–4 节) |

  定第二行。普通焦点三种措辞都听得进(焦点符号出现次数约为基线的 2–3 倍)。第三行缺节的原因
  看不到(rollout 只存规整后的摘要;那两次输出 8–9k token,摘要只有 1.2k/5.5k 字符),但其余
  12 次采样(基线 1、原句 4、第二行 7)一次都没出现，所以不冒这个险。
- **测试**:plan 第五节列的都有;`/help` 现有用例逐条断言 `contains(b.summary)`,新 `SUMMARY`
  自动覆盖，没改。拒绝上限用 2,000/2,001 个汉字测——同时钉住"数的是字符不是字节"。

**顺带发现、未修**(记在 HANDOFF):

- `help`/`cost`/`context`/`clear`/`exit` 五个分支都不收 `args`,多写的字照旧静默丢弃——和本条修的
  是同一种错。要不要对多余参数报错，收尾时问用户。
- 手动 `/compact` 与预测性压缩的成功文案自己拼,不走 `compact::describe`,所以摘要请求被迫
  缩小、丢了最旧一段时，这两处的回报**不说丢了几条**(被动压缩说;历史里的 `DROPPED_PREFIX`
  标记三条路径都有)。
