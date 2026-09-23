# Plan 204 — 开始了,不等于做完了

> 来源:2026-09-23 读 `refs/chord`(`keakon/chord@cce05db`,MIT)后,用户点名「1,2,3,4 都立 plan」,
> 这是第 4 条。出处见 `refs/README.md`「chord 固定源码调研(2026-09-23)」。
> **排在 plan 182(`rollout.rs` 拆分)之后**,见第六节。

## 一、为什么

进程在一轮工具执行中途死掉(崩溃、被杀、断电),resume 时 `rollout.rs` 的配对修复
(`core/src/rollout.rs:1595-1636`)给每个没有结果的 `tool_use` 补一个同样的结果:
`interrupted`,`is_error: true`(`core/src/tools/mod.rs:959`)。

**模型从这一个词里分不出两种完全不同的处境:**

- 这个调用**根本没开始**——什么都没发生,重来是对的;
- 这个调用**已经在跑**——`git push`、`rm`、一次数据库迁移、一次发消息,可能已经生效了一半。
  这时原样重来可能是错的,应该先看一眼现状。

一轮里的工具是**全部跑完才一起记录**的(`agent.rs:923` 的 `self.history.record(Message::tool_results(results))`),
所以一轮 5 个串行调用里第 3 个跑到一半时崩溃,前 2 个已经真的做完了,修复后也同样只是 `interrupted`。

chord 的做法(`internal/recovery/manager.go:365-409`、`internal/agent/restore_normalize.go:94-195`):
每个工具开始执行前先追加一条 `started` 记录并 fsync;恢复时,没有这条记录的标 `not_started`
(可以安全重来),有的标 `outcome_unknown`(提示先核实现状)。

kloop 的意图屏障已经有了:带 `tool_use` 的 assistant 消息在派发前就已记录(`agent.rs:910` 的注释)。
缺的只是"每个调用开始了没有"这一位。

## 二、形状

### 2.1 rollout 加一种行

`RolloutLine` 加 `ToolStarted { tool_use_id }`(`rollout.rs:331` 那个枚举),在**真正开始执行一个
有副作用的调用之前**追加。它不是消息,不进 `History::items`,不影响请求;只在修复时被读。

**只给非并发安全的调用记**(`tools::is_concurrency_safe(name, input, …)` 为 false 的,
`core/src/tools/mod.rs:780`)。只读调用重来没有任何代价,"开始了没有"对它们不重要——
一律按"没开始"处理即可;而且只读调用正是并发批里的那一大片,给它们每个都写一行 + 落盘是白花。

### 2.2 修复时的三种结果

配对修复补结果时,按 `tool_use_id` 查这次会话里有没有 `ToolStarted`:

| 情况 | 补的结果(`is_error: true`) |
|---|---|
| 并发安全的调用,或者没有 `ToolStarted` | `not run: the session ended before this call started. Nothing happened; calling it again is safe.` |
| 有 `ToolStarted` | `interrupted: the session ended while this call was running. It may have taken effect, fully or partly. Check the current state before repeating it.` |

措辞开工时再磨,但两条的**可执行结论**要照上面:一条说"可以重来",一条说"先核实"。

**进程内的 Ctrl+C 取消不走这里**——那条路径由 `dispatch_tools` 自己产结果,工具知道自己跑到哪了。
本 plan 只改崩溃后 resume/fork 时的配对修复。

### 2.3 落盘

`Rollout::append_line`(`rollout.rs:709`)现在是 `writeln!` 不 `fsync`。对**进程崩溃**这已经够了——
`write` 返回后数据在内核页缓存里,进程死了也会落盘。只有**断电/内核崩溃**会丢。

丢了 `ToolStarted` 的后果是**往危险方向错**:把一个已经开始的调用说成"没开始、可以重来"。
所以这一行的落盘要求比普通行高。见第四节。

## 三、不做

- **不记每个调用的完成结果**。那样能把"前 2 个已经做完"的真实结果也救回来,但要在 rollout 里把
  结果内容写两遍(journal 一份、结果消息一份),还要处理一轮预算 spill(`enforce_round_budget`)
  发生在整条结果消息记录时、单条 journal 看不到的问题。代价与收益不成比例;"先核实"已经能让模型
  自己去确认那 2 个调用的效果。
- 不改 `interrupted` 在其他地方的用法(取消路径、`tools/mod.rs` 其他调用者)。
- **旧会话**:没有 `ToolStarted` 行的旧 rollout 修复时一律按"没开始"——按用户定的"不考虑兼容"
  (2026-09-23),不做版本判断。这意味着老会话在这种极少见的中途崩溃下会给出偏乐观的措辞,接受。

## 四、开工时必须问用户的点(只有一个)

**`ToolStarted` 这一行要不要 fsync?**

- **要,用 `sync_data`(推荐)**:只对非并发安全的调用写这一行,而这类调用本来就是串行、
  且通常本身就比一次 `sync_data` 慢得多(一次 shell 命令、一次写文件);换来的是断电后也不会把
  "已经开始"说成"没开始"。
- 不要:与其他 rollout 行一致,零额外开销;断电时退化成今天的行为(甚至略差:明说"可以重来")。

## 五、实现要点(开工时核)

- **在哪里写**:`dispatch_tools` 里,每个非并发安全调用**通过了权限门与 hook、即将真正执行**的那一刻。
  被权限门拒绝、被 hook block 的调用没有开始,不写。开工时看 `dispatch_tools` 的结构,找到
  "门已过、执行未开始"的唯一那一点;若并发批与串行调用走两条路,两处都要覆盖(并发批按 2.1 本来不写)。
- **`History` 怎么写这一行**:`History` 已经持有 `Rollout`,加一个 `note_tool_started(&str)`,
  走现有的 `persist`(`history.rs:512`,失败即降级为无持久化并告警)。`dispatch_tools` 拿不到
  `&mut History` 的话,开工时定是经 `ToolCtx` 传一个回调,还是在 `dispatch_round` 里按"即将执行"
  的顺序预先写——后者更简单,但要保证"写了 = 真的开始了",被门拒的不能预写。
- **修复侧**:加载 rollout 时收集所有 `ToolStarted` 的 id,传给配对修复;修复只看这个集合,
  不看别的。`validate` 与各前端对 rollout 行的遍历要认识新行(match 保持穷尽,不要用 `_`)。
- **子 agent** 有自己的 rollout,同样适用。

## 六、与其他 plan 的关系

- **plan 182 要把 `rollout.rs` 拆成 `rollout/{sessions,line,validate}.rs`**。本 plan 动的正是
  `RolloutLine`、加载与修复——两条改同一批代码,**182 先做**,本 plan 跟着新位置改。
  若决定先做本 plan,就要在 182 的 plan 文件里记一笔新增的行类型。

## 七、测试

- 串行 3 个写操作,第 2 个执行中途"崩溃"(测试里在该工具执行时截断 rollout、不写结果)。
  一轮结果是整体记录的,所以已经做完的第 1 个也没有结果。resume 后补的三条依次为
  `interrupted…`(1,有 `ToolStarted`)、`interrupted…`(2,有)、`not run…`(3,没有)。整对象断言。
- 并发安全的调用(只读 bash、`read_file`)没有 `ToolStarted` 行,修复后是 `not run…`。
- 被权限门拒绝的调用没有 `ToolStarted` 行。
- `ToolStarted` 不进 `History::items`、不出现在任何请求里。
- rollout 往返与 `validate` 接受新行。
- fork 与 resume 两条路径都走新修复。
- 若第四节选了 fsync:用可注入的写入器断言 `sync_data` 只在 `ToolStarted` 行上调用。

## 八、完成时要一起做的

- `rust/DESIGN.md` 会话持久化 / 配对修复那段:先读现在怎么描述 `interrupted` 的,改写成三种结果与判据。
- `refs/README.md` chord 一节第 5 条"可以对照的小件"里的意图屏障与 started journal,标注已由本 plan 吸收。

## 九、完成记录

(未开工)
