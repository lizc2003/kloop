# Plan 190 — 没有人告诉模型那张表还在

> 来源:2026-09-21,和 187/188 同一次对话的起点。用户看到模型列了六条候选、
> 一条 task 都没建,问「是不是这个系列工具太复杂了」。187/188 回答的是「复杂」那一半,
> **这条回答「为什么不调用」那一半**。
>
> **不硬依赖 187/188,但建议排在它们之后**:那时回灌的是一行标题加一个状态,
> 而不是一张带依赖边的图。编号跳过 189(已被占用)。

## 一、缺什么

全仓 `<system-reminder>` 用在四处:skills 目录(`skills.rs:378`)、延迟工具清单
(`tool_search.rs:195`)、重读同一文件的劝阻与图片/空文件提示(`fs.rs:70/386/427`)。
**task 一处都没有。**

而且 todo 表**不回灌上下文**:`TodoUpdated`(`event.rs:129`)在
`event.rs:309` 和 `server/src/wire.rs:281` 都返回 `None`,只有 TUI 投影它。
模型写完一张表之后,**下一轮它对这张表的全部认知,就是自己几轮前那条 tool_result**,
中间隔着几十 K 的工具输出。

于是两件事各缺一半:

- **它不知道表现在长什么样**(几轮之后,那条 tool_result 早被挤到上下文深处)。
- **它想不起来有这个工具**。`BASE_SYSTEM`(`context.rs:119-121`)只有一句
  「For multi-step tasks, track the work with the task tools」,埋在 `# Using your tools`
  第三条。更要命的是那句话的后半:「**send those updates in the same round as the work
  they describe, never as a round of their own**」——模型在「先列候选、下一轮再查证」
  的那一刻,**这一轮没有 work 可以捎带,按字面它就不该发**。行为完全合规,结果是永远不发。

## 二、绝对不能放哪

**不能放进 `injected_context`(`agent.rs:1302`)。**

那是合成的**第一条 user 消息**,装着 plan-mode reminder、项目指示、skills 目录、
延迟工具清单。它的 doc 注释写明了为什么这些东西能放在那里:
「Project instructions and the skills catalog are **session-stable**」,
只有 MCP 目录「may replace the deferred-tools notice **at a round boundary**」。

第一条 user 消息在**缓存前缀的最前面**。每轮变化的 todo 表放进去,
**整个会话的缓存每轮全废**——不是掉几个百分点,是前缀从头断。

plan 120 用 2019 轮 usage 采样量过这件事:单轮增量 0–2k 的轮次命中率中位数 **97%**,
>20k 掉到 **14%**;而且一轮大增量会让**随后几轮**都读不到本来就没写进去的缓存条目。
一张几行标题的表落在最好那一档——**只要位置对**。位置不对,增量再小也没用。

## 三、放哪、什么时候放

**放哪:`drain_inbox`(`agent.rs:1213`)那个 round-boundary 注入口。**

它的注释正好写着需要的保证:「Called only at round boundaries (top of the loop, and
just before the turn would end) — **never mid-request**, so an in-flight sampling never
sees a partial write and **tool_result blocks are never interleaved** with the injected
user message.」注入的内容作为 **user message 追加在历史末尾**,在缓存前缀之后,
只计入增量。`history.rs:417` 已有 `offload_text` 给机器产生的文本封边界。

两条实现路线,**推荐第二条**:

1. 新增一个 `InboxItem` 变体,复用 `into_message` 的加框。代码最少,
   但语义别扭——inbox 装的是**外部塞进来的**东西(steering、子 agent 结果、后台 shell
   终止通知),而这条提醒是 agent 对自己状态的判断。
2. 在 round 边界直接判断并 push 一条 user message,和 `drain_inbox` 并列调用。
   语义干净,且节流状态(见下)本来就该住在 agent 的 turn 状态里,而不是 inbox 里。

**什么时候放——节流是这条 plan 的全部难点。** 每轮都发,历史里就堆起十份互相矛盾的
过期快照,既占上下文又误导模型。建议的触发条件:

- **表非空,且距上次注入已过 N 轮**(N 开工时定,建议 3),**且表在这期间没有变化**。
  「表没变」正是该提醒的信号:模型在干活,但没有回来更新状态。
- 注入后记下 `revision`(`TodoSnapshot.revision`,现成的),
  **同一个 revision 只注入一次**。
- **表为空时不提醒**,或至多在会话首轮之后提醒一次。空表提醒最容易变成噪音——
  它要求判断「这次的活该不该记清单」,而那个判断模型自己做得比一条固定规则好。

同时**改 `BASE_SYSTEM` 那句**(`context.rs:119-121`):把「never as a round of their own」
这条限制说清楚它管的是**更新**,不是**开列**——计划成形的那一刻就该记下来,
那时本来就没有 work 可捎带。这一句的改动比整个注入机制更可能立刻见效,
**而且它是 session-stable 的,不伤缓存**。

## 四、坑

1. **别把回灌做成第二份真相。** 面板(TUI)、tool_result、注入的 reminder 三处都在讲
   同一张表。注入的那份必须是 `TaskRegistry::snapshot()`(`task.rs:206`)的直接投影,
   **不要另写一套格式化**,否则三处会漂。
2. **`/clear` 之后要清节流状态**。`commands/clear.rs:16` 调 `cfg.tasks.clear()`
   并无条件推进 revision(教训 87 的 reset fence)。上次注入的 revision 记录必须跟着复位,
   否则清空后第一张新表会因为「revision 变了但没到 N 轮」被吞掉,或者反过来立刻重发。
3. **子 agent 不注入。** `todo_write` 是 `Gate::Depth0`(`builtin.rs`),
   depth>0 根本看不见这张表,给它们注入是纯噪音。
4. **这是行为改动,不是删代码,测试断言只能守住机制**(注入位置在历史末尾、
   同 revision 不重复、`/clear` 后复位、depth>0 不注入)。**它到底有没有用,
   测试答不了**——见验收。

## 五、开工时定(问用户)

**N 取几轮,以及空表要不要提醒一次。**

建议 N=3、空表不提醒。但这两个数字**没有可以推导出来的正确答案**,
而且调错方向就是往每一轮上下文里加噪音。开工时按第六节的方式先量一版再定。

## 六、验收

- 机制侧(测试守得住的):注入落在历史末尾而非 `injected_context`;同 revision 只注入一次;
  `/clear` 后节流状态复位;depth>0 不注入;`make check` 全绿。
- **效果侧(测试守不住的,必须真实 dogfood)**:拿一个真会用到清单的任务跑几个会话,
  看两个数——**模型是否在计划成形时就建表**(而不是根本不建),
  以及**未完成任务在表里停留的轮数**(它是否回来更新)。对照组是改动前的会话。
- **缓存侧**:按 plan 120 的口径取 usage,确认命中率**没有**因为这条改动下降。
  如果掉了,第一嫌疑是注入位置错了(见第二节),不是注入内容太大。
- DESIGN.md:task 那一段(2243–2295)补注入契约;**先读现在那段还成不成立再决定改写还是追加**。

## 八、188 之后的形状更新(2026-09-21)

本 plan 立于 plan 187/188 之前,正文里的 `task_*` 措辞已按落地结果改过。开工时的
事实是:

- 工具只有一个 **`todo_write`**,整表覆盖,`{"todos":[{"subject","status"}]}`,
  两字段都必填。没有读工具——**这正是本 plan 存在的理由**:模型除了自己上一次的
  写入,没有任何途径知道表现在长什么样。
- 回灌的内容因此很小:一行 subject 加一个 status,256 行封顶,subject 200 字符封顶。
  不再有 `description`(188 删掉)、没有 ID、没有 `blocked_by`。
- 类型名:`TodoSnapshot { revision, todos }`、`TodoItem { subject, status }`、
  `TodoStatus`、`Event::TodoUpdated`、`Config.todos: Arc<TodoRegistry>`。
- `BASE_SYSTEM`(`context.rs`)那句已经是 `todo_write` 单数措辞,
  **但「never as a round of its own」那半句仍未动**,留给本 plan(第五节)。

## ✅ 完成(2026-09-21)

一次提交(SHA 即本条所在提交),`make check`(fmt + clippy -D warnings + 全量测试)全绿。

**第五节那两个数,先查参考项目再定**(用户问「参考项目怎么做的」):

- `refs/codex` 的 `update_plan` 根本不存状态——`core/src/tools/handlers/plan.rs` 的
  handler 只发一个 UI 事件,工具结果是一句 `"Plan updated"`;`core/src/context/` 那一排
  注入片段(时间、token 预算、guardian…)里**没有 plan**。它的赌注全在
  `core/gpt_5_2_prompt.md:290-298` 那一节规矩上。
- cc **做了**这件事,形状和第三节几乎重合:ephemeral 附件包成 `<system-reminder>` 的
  meta user 消息;触发是两个计数器——距上次**调用**工具 ≥10 个 assistant 轮 **且** 距上次
  提醒 ≥10 轮;**空表照样提醒**(正文固定,表非空才把表附在后面);同一张表可以反复提醒。
- codewhale 一条都没有(整个 TUI 没有回灌通道);另一个参考产品没有 todo 回灌,
  但有同形状近亲(「当前会话目标状态」作 model-only 合成 user 消息注入 + 投影层过滤)。

于是两个数都改了原来的建议:**N=3 → 8**(向 cc 的 10 靠,但我们同 revision 只发一次,
比它严),**空表不提醒 → 空表提醒一次**(空表恰恰是本 plan 起因的那一档)。

**落地**:

- `tools/todo.rs`:`ReminderState`(seen_revision / rounds_unchanged / announced_revision /
  announced_empty)住进 `TodoRegistryState`,`round_boundary_reminder()` 是唯一入口;
  `render_reminder()` 直接投影 `TodoSnapshot`(坑 1:不另写一套格式化);
  `clear()` 连节流一起复位(坑 2)。
- `agent.rs`:`remind_todos()` 和 `drain_inbox`/`drain_local_mailbox` 并排在 round 边界,
  depth>0 直接返回(坑 3),文本过 `history.offload_text` 封上限。走第三节的**路线 2**。
- `context.rs`:`BASE_SYSTEM` 那句拆成「计划清楚就开列(值得单开一轮)」+「之后的更新
  和工作同轮」。
- 测试(坑 4,只守机制):registry 三条(同 revision 只发一次 / 空表一会话一次 /
  `clear` 复位),agent 两条——单元一条(落在历史末尾、depth>0 不注入且不推进计数、
  说过一次不再说),**端到端一条**(真 `run_turn` 过 mock provider 跑满 10 轮:提醒作为
  **独立的 user 消息**落在某轮 `tool_result` 与下一条 assistant 之间,不与 `tool_result`
  同块,整轮只出现一次)。
- `make mock` 照不到这条路:那个脚本 7 轮,兜底要 8 个边界。**没有为了演示去加轮数**——
  smoke 的五条主线不该被一条兜底稀释,端到端测试才是钉它的地方。
- DESIGN.md「Root-owned session todo list」一节补注入契约,顺手改掉过期的
  `Arc<TaskRegistry>` 和「`TodoUpdated` 从不进上下文」那半句。

## 真机 dogfood 第一批(2026-09-21,三个会话,n 很小)

用户的默认 provider(一个 flash 档模型),`--headless --permission-mode bypass`,
在 scratchpad 的空目录里跑,不碰仓库。**三个会话都留了 rollout,下面是从 rollout 读的,
不是从终端输出猜的。**

1. **单步偏多的任务**(统计 notes.txt 词频、存 top5、报第一名),新构建,9 轮:
   **没建表**。空表提醒按预期在第 8 个边界发出(rollout 里是一条独立的 user 消息,
   夹在某轮 `tool_result` 与下一条 assistant 之间),模型读到后又跑了一轮就收尾,
   **没有建表也没有提这条提醒**。提醒正文自己写着「不需要就忽略」,这一轮它确实快做完了,
   所以这不算违规——但也说明**空表提醒不会把一个不想记清单的模型劝回来**。
2. **真多步任务**(做成带 stopwords / `--top N` / 单元测试 / README 的小工具),新构建,8 轮:
   **建了**。第 4 轮探完环境、动手之前,`todo_write` 写下五行,**独占一轮**;
   最后一轮在收尾前把五行全部改成 completed。中间三轮干活没碰表,提醒没触发
   (表在第 4 轮变过,计数重新起算,整轮只有 4 个静止边界)。
3. **同一个任务的对照组**,plan 190 之前那个提交(`f90ace5`)编译出来的二进制,11 轮:
   **也建了表,但形状不同**——它把 `todo_write` **和第一次 `write_file` 捆在同一轮**发,
   之后**再也没有回来更新**:多跑了六轮、活全干完,表还停在「1 条 in_progress + 4 条 pending」,
   turn 结束时是一张过期的表。

**读数**(注意 n=1/组,同一个模型、同一句 prompt,采样噪音足以解释一部分):

- 「模型根本不建表」不是普遍现象,**是任务相关的**——真多步的任务两个构建都建了。
  起因那次对话里的现象,更可能出在「候选还没成形就该不该记」这一档,而那一档正是
  空表提醒管的,而这一批显示**空表提醒对它没用**。
- 新旧的差别落在**什么时候建、建完还回不回来**:新的在动手前独占一轮建表、收尾前改完成;
  旧的把建表捎在第一轮工作里、之后再没更新。这正好是第六节点名的那两个数
  (「是否在计划成形时就建表」「未完成任务停留几轮」),方向都对。
- **缓存侧还没取**:三个会话都没长到触发几次注入,按 plan 120 的口径取 usage 对照
  还没做。

**结论**:机制在真机上确认可用(注入位置、内容、一次性都对),提示词那半句看起来是
**真正起作用的那一半**,但 n 太小,**这不是第六节要求的那次对照**——要下结论还得多跑几组,
最好换一个模型档位再看一次。
