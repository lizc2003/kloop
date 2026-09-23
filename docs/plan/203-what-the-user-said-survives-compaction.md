# Plan 203 — 用户说过的话,压几次都还在

> 来源:2026-09-23 读 `refs/chord`(`keakon/chord@cce05db`,MIT)后,用户点名「1,2,3,4 都立 plan」,
> 这是第 3 条。出处见 `refs/README.md`「chord 固定源码调研(2026-09-23)」。两片,可分两次提交。

## 一、为什么

kloop 的压缩把 `messages[..keep_from]` 整段交给摘要模型,**上一代摘要也在这段里**
(`core/src/compact.rs:267` 的 `plan_compaction`:`request = messages[..keep_from]`,
`messages[0]` 就是上一代 `ContextSummary`)。于是每压一次,上一代摘要被再摘要一次。
用户的原话只靠 `COMPACT_INSTRUCTION` 第 2、3 节"逐条引用 / 逐字复制"(`compact.rs:92`)去保——
**那是请求模型遵守,不是保证**。长会话压到第三、四代,最早的原始请求与中途的纠正会被一代代
改写成越来越短的转述;而"用户说过不要做 X"一旦被转述丢了,模型就会去做 X。

第二个缺口:压缩折掉的是工具结果,**刚读过、正在改的文件内容也一起没了**,而保留尾巴只有
`KEEP_RECENT_TOKENS`(2 万)。模型压缩后第一件事往往是把刚才那几个文件重读一遍。
这一条挂在 `docs/capability-report.md` 生存层表里("压缩后重注入最近读过的 ≤5 文件")。

chord 的两招(`internal/agent/compaction_anchors.go:13-50`、`compaction_file_context.go:215-272`):
用户原话由**程序**逐字搬进每一代 checkpoint,不经摘要模型;压缩后把关键文件的头部重新放回上下文。

## 二、片 1:用户原话由运行时逐字继承

### 2.1 形状

压缩的替换结果从 `[DroppedPrefix?, ContextSummary, tail…]` 变成
`[UserAnchors, DroppedPrefix?, ContextSummary, tail…]`。`UserAnchors` 是一条新的
`Injected::UserAnchors` 消息(`protocol` 的 `Injected` 枚举加一个变体;不考虑旧会话兼容),
内容**由运行时拼出来**,摘要模型不碰它:

```
[What the user said earlier in this session, verbatim — carried across compactions]

Original request:
<会话第一条用户消息,原文>

Later messages (oldest first):
- <用户消息原文>
- …
```

**谁算"用户说的"**:role 为 user、不含 `ToolResult`、且 `injected` 为 `None` 或
`Injected::Steering` 的消息。子 agent 回灌、定时任务、peer message、hook 输出一律不算——
它们不是用户的话,混进来就是 plan 116 那类"把管道当成对话"的错。

### 2.2 预算与继承

- **原始请求**:永远保留,单条上限 4000 字符(超出保留头尾、中间标省略)。它来自上一代
  `UserAnchors`;没有上一代时,取本次被折叠段里的第一条用户消息。
- **之后的消息**:上一代 `UserAnchors` 里的"Later messages" + 本次被折叠段里新的用户消息,
  按时间排,**从最新往回装**,总预算 4096 估算 token(chord 的 `retain_recent_tokens` 默认值);
  装不下的最旧几条丢掉,并在列表开头写明"N earlier message(s) omitted"。单条上限 2000 字符。
- **保留尾巴里的用户消息不进 anchors**——它们原样就在上下文里,重复只是白花 token。
- 继承**只从上一代 `UserAnchors` 的结构化来源拿**,不从摘要文本里解析。开工时定:
  是在内存里解析上一条 `UserAnchors` 消息的固定格式,还是给 `Message` 挂一个结构化字段。
  倾向前者(格式是运行时自己写的,解析它不是"猜模型输出"),但要有往返测试钉住格式。

### 2.3 与 `plan_compaction` 的关系

`plan_compaction` 现在只认 `messages[0]` 是不是 `ContextSummary`(`is_existing_summary`)。
改成识别**开头连续的压缩产物**(`UserAnchors`、`DroppedPrefix`、`ContextSummary`),
`fold_start` 越过它们。上一代 `UserAnchors` 仍然在发给摘要模型的请求里(让它有上下文),
但替换结果里的新 `UserAnchors` 由运行时重建,不取模型输出。

## 三、片 2:压缩后把正在用的文件放回来

### 3.1 选哪些文件

从**被折叠段**里的 `read_file` 调用往回找(只看历史,resume 后结果一致),按最近一次读的先后排,
按规范化路径去重(`file_state::normalize_absolute_path`):

- 保留尾巴里已经读过的路径跳过——它原样还在上下文里;
- 已不存在、不再是普通文件、或读权限被拒的跳过(见 3.3);
- notebook 跳过(cell 结构另有读法,先不做)。

### 3.2 放多少

- **至多 5 个文件**(cc 的取值,capability-report 那行写的就是它);
- **单文件至多 12000 字符**(chord 12KB),从文件头读;没读完就在末尾带上 `read_file` 现成的
  `[showing lines X-Y of N; call read_file with offset=Y+1 to continue]`;
- **总量至多 40000 字符,且不超过压缩后剩余窗口的 1/4**(chord 的比例)。按上面的顺序装,装不下为止。

### 3.3 怎么读

**走真实的 `read_file`**,用 `ToolCtx::harness`(`core/src/tools/mod.rs:449`,plan 199 提出来的
最小顶层上下文)——权限门、沙箱、敏感路径一道不绕。副作用正是想要的:

- 在 `FileState` 里留下一次 observation,模型可以直接 `edit_file` 这些文件(plan 195"读过就放行");
- `forget_context_reads()`(`compact.rs:450`)之后重新登记这几段的上下文驻留,重读判断不误报;
- 之后文件被外部改了,plan 197 的 `remind_changed_reads` 自然会提醒。

### 3.4 放在哪

作为一条 `Injected::RestoredFiles` 消息,紧跟在 `ContextSummary` 之后、保留尾巴之前,
**写进替换结果、随 `Compacted` 行持久化**:

```
[Files that were in use before this compaction, re-read just now]

<path> (lines 1-Y of N):
<内容>
…
```

**为什么持久化,而不是 chord 那样每次请求临时注入**:kloop 的注入层(`injected_context`)在请求
**最前面**,每次都重读磁盘的内容放在那里,文件一变就从第一个字节起击穿 prompt cache;而压缩时读一次、
写进替换结果,之后每次请求逐字节不变,缓存友好,resume 后也一模一样。代价是它会过时——这正是
3.3 里 plan 197 接手的部分。

## 四、与其他 plan 的关系

- **plan 200(请求级裁剪)**:`RestoredFiles` 与 `UserAnchors` 都是注入的 user 消息,不是
  `ToolResult`,裁剪白名单碰不到它们。两条互不依赖,顺序随意。
- **plan 202(压缩断路器)**:不相交。
- 两个新 `Injected` 变体要在各前端的重放/投影里有去处(plan 116 的"重放显示对话而不是管道"):
  开工时 grep `Injected::ContextSummary` 的所有匹配点,每一处都补上,match 保持穷尽。

## 五、开工时必须问用户的点(只有一个)

**有了运行时逐字保留之后,`COMPACT_INSTRUCTION` 第 2 节("逐条引用每一条用户消息")要不要收窄?**

- **收窄(推荐)**:改成"意图如何变化、为什么变",并明说原话已由运行时逐字保留、不必重抄。
  否则同一批用户消息在上下文里出现两次(anchors 一份、摘要里一份),白花 token。
- 不动:最保守,摘要质量不受任何影响,只是多花一些 token。

## 六、测试

- 片 1:
  - 第一次压缩:`UserAnchors` 的原始请求 = 第一条用户消息原文;折叠段里的用户消息逐字出现;
    保留尾巴里的不出现。
  - 连续压缩三次:原始请求逐字节不变;"Later messages"新的在、超预算的最旧几条被丢且有
    "omitted"计数;**摘要模型的输出里即使完全没提用户消息,anchors 也完整**(mock 让摘要返回一句话)。
  - 子 agent 回灌、定时任务、hook 输出、peer message 不进 anchors;steering 进。
  - 超长单条的头尾截断。
  - anchors 格式往返(写出 → 解析 → 再写出逐字节相同)。
  - `plan_compaction` 越过开头连续的压缩产物。
- 片 2:
  - 折叠段读过 7 个文件 → 只放最近的 5 个;尾巴里读过的跳过;删掉的、被 deny 的跳过。
  - 单文件截断带 continuation 标记;总量与 1/4 窗口两道上限各一条。
  - 放回的文件可以直接 `edit_file`(不报"must read before modifying")。
  - 放回后外部修改该文件 → 下一轮收到 plan 197 的提醒。
  - resume 后替换结果逐字节相同(持久化)。
- 两片都:rollout 往返;替换结果整对象断言。

## 七、完成时要一起做的

- `rust/DESIGN.md` 压缩一节:先读现在怎么写替换结果的形状,改写成新形状并讲清"运行时写、模型不碰"
  与"持久化而不是每次注入"的理由。
- `docs/capability-report.md`:"压缩后重注入最近读过的 ≤5 文件"一行销账。
- `refs/README.md` chord 一节第 2 条标注已由本 plan 吸收。

## 八、完成记录

(未开工)
