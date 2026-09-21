# Plan 188 — 一张表,一次调用

> 来源:2026-09-21,和 187 同一次对话。用户的原话是「为了这个功能,需要一系列的 tool,
> 大模型是否能理解」。
>
> **硬依赖 187**。187 砍掉依赖图和状态机之后,task 退化成一张平表,这条才谈得上;
> 反过来先合并工具、依赖图还在,整表覆盖就得处理 `blocked_by` 的全量替换语义,是更难的一次改。

## 一、为什么

plan 71 第 27 行把换成五个 CRUD 工具的理由写得很清楚:

> 旧 `todo_write` 是每个 Agent 各自持有的**无 ID 整表 checklist**,
> 不能承担**稳定 ID、owner 或依赖图**。

三个理由,今天**一个不剩**:owner 被 plan 73 删了(「没有可执行语义的字段不该留在公开
schema 里」),依赖图被 plan 187 删了,而**稳定 ID 是前两个的附庸**——ID 的用处是让
`task_update` 寻址、让 `blocked_by` 引用。整表覆盖不需要寻址。

模型**能**用这五个工具:plan 74 的真实验收里,Anthropic 那条跑出 create IDs `1,2,3`、
完成 `1,2` 后 `task_list == [3]`;OpenAI Chat 跑出 `1,2`、中间 `task_clear` 返回
`cleared_count=1`。**单次调用没问题,难的是跨轮次维持一致**:

- CRUD 要求模型记住 ID、记住哪条在 `in_progress`、在对的时机发 `task_update`。
  每步都是独立决策,**错一步图就漂**——而且没有任何东西告诉它图现在长什么样,
  `TaskGraphUpdated` 只进 TUI,不回灌上下文。
- 整表覆盖把一致性变成**一次输出的产物**:每次重述完整状态,漏了哪条、哪条该标完成,
  写的时候全在眼前。
- 成本差一个量级:N 条任务,CRUD 至少 N 次 create + 2N 次 update;整表覆盖每次一调。
- 还有一笔固定成本:五个工具定义常驻工具数组,**每轮请求都发**(定义源码 3962 字符),
  而实际使用中模型一次都没调。

plan 73 记过一次实证:同一个显式 Task 生命周期,模型**一次主动写 `owner:"assistant"`,
另一次保持 `owner:null`**。schema 上多一条约束,理解就往下掉一档。

## 二、新工具形状

**一个 `task_write`,整表覆盖,替换现有五个。**

```json
{"tasks": [{"subject": "...", "status": "pending|in_progress|completed"}, ...]}
```

- `tasks` 为完整替换列表,`[]` 清空。返回写入后的表。
- **没有 ID**:模型面不出现,内部也不需要(见第三节「顺带消失的三样」)。
- **没有 blocked_by / blocks**(187 已删)、**没有 owner**(plan 73 已删)、
  **没有状态倒退约束**(187 已删,整表覆盖必须能把写错的 `completed` 改回去)。
- **没有 `description`**(第五节,用户已拍板):一条任务就是一行标题加一个状态。
- 上限沿用:`MAX_TASKS` 256、subject 200 字符单行。
- 描述目标:**一个工具 < 400 字符**(现在五个合计 1597)。

## 三、删除面

### core

`rust/crates/core/src/tools/task.rs`(187 之后约 480 code 行,本条要降到 **200 以下**):

- `TaskRegistry` 的 `create`/`get`/`update`/`list` 四个方法 → 一个 `write`。
  **`clear()`(211) 和 `snapshot()`(206) 必须保留**——见下面第一条坑。
- 五个 `*_def` + 五个 `*_tool` handler → 各一个。
- `parse_create`/`parse_update`/`required_task_id`/`parse_task_id`/`optional_string` 等
  一串 strict parser 收成一个数组解析。
- **顺带消失的三样**:`next_id` 与 ID 高水位、epoch rollover(`create` 里那段
  「图非空 + 全完成 → 清表换代」的逻辑)、`TaskView` 这个类型。
  整表覆盖时模型直接写新表,旧表被覆盖,**rollover 天然就发生了**,不需要这个概念。
- **`description` 的删除面**(第五节已定):`MAX_DESCRIPTION_BYTES`(19);
  `validate_task_text`(241–256) 去掉三段 description 校验后只剩一行
  `validate_single_line(subject, …)`,**整个函数并掉、调用点直接调 `validate_single_line`**;
  字段 `StoredTask.description`(55)、`TaskCreateInput.description`(382)、
  `TaskPatch.description`(388);schema 的 `required`(404)、strict key 列表(504)、
  `required_string` 取值(508)。**TUI 零影响**——`TaskGraphTask`(70–77)本来就不带 description。
- **`revision` 保留**:它是 TUI 的 stale fence(乱序/重复 snapshot 丢弃),和 ID 无关。

### builtin.rs — 十处 match 臂

`rust/crates/core/src/tools/builtin.rs` 里五个变体出现在十个地方,逐个收成一个:
枚举(56–60)、`ALL`(101–105)、`from_name`(257–261)、`name`(299–303)、
`Gate::Depth0`(340–344)、显示名(432–436)、超时分类(519–523)、
只读分类(568–576,现在 Get/List 为 true、其余 false → **`task_write` 归 false**)、
副作用分类(637–641)、`def` 分发(670–674)。

**`ALL` 的顺序决定 provider 请求字节**(文件头 38–43 行的注释写着,重排会让 prompt cache
失效)。本条删四加一,**请求字节必然变、缓存必然失效一次**——这是预期成本,不是 bug,
但要在提交信息里写一句,免得下次有人对着缓存命中率查半天。新工具放在原来 `TaskCreate`
的位置(`Grep` 之后),保持 catalog 分组不乱。

### 退役名进 reserved

`tools/mod.rs:552–563` 已有先例(`task`、`wait`、`kill_bash`、`todo_write`)。
**五个旧名全部加进去**——resumed history 里的旧调用不能被 MCP 工具冒名顶替。
注意 `todo_write` 就躺在那张表里:这次等于**回到它的形状,但名字不复用**。

### 其余

- `permissions.rs` 两处测试的五元组列表(2465、3783)→ 一个名字。
- `plan52_parity_tests.rs` 64 处 `task_` — **开工第一步核实它会不会动 parity 生成物**
  (同 187 坑 5;187 已经趟过一次,把结论抄过来即可)。
- `cli/src/startup.rs:945` 的 `mock_demo_turns` — 187 已经重写过一次,这次再改一次,
  改成一次 `task_write` 写三条、下一轮重写同一张表把第一条标完成。
- TUI:`render.rs` 的面板(187 之后只剩 subject/status 渲染)、`app.rs`/`lib.rs` 的
  `TaskGraphTask` 构造点跟着去掉 `id`。

## 四、坑

1. **`TaskRegistry::clear()` 不能跟着 `task_clear` 工具一起删。**
   `commands/clear.rs:16` 的 `/clear` 斜杠命令在调它,`tui/src/lib.rs:199` 在调
   `snapshot()`。**工具面删掉,registry 的方法留着**——这是「模型面」和「用户命令面」
   的区别,最容易在删干净的冲动里一起删掉。
2. **`/clear` 的 reset fence 依赖 revision 无条件推进**(DESIGN.md 2285 附近、教训 87)。
   删 ID 高水位时不要顺手把 revision 也简化掉,那会让 TUI 收到乱序 snapshot 时闪回旧表。
3. **整表覆盖的幂等判定**:现在 `update` 用 `candidate == current` 判无变化、
   `display_changed` 决定发不发 snapshot。整表写入要保留同样的语义——
   **写入内容与当前表逐字段相等时不推进 revision、不发 snapshot**,否则模型每轮重写
   同一张表都会让 TUI 面板闪一次。
4. **`MAX_TASKS` 的检查时机变了**:现在是 create 时「已有数 ≥ 256 就拒」,
   整表覆盖是「提交的数组 > 256 就拒」,**在任何 mutation 之前**。
5. 187 的坑 2 同样适用:失败必须**完全不改状态**。整表覆盖天生容易做对(先全量校验、
   再一次性替换),但别写成边校验边 push。

## 五、`description` 删掉,只留 subject(2026-09-21 用户拍板)

**一条任务 = 一行 subject + 一个 status。**

理由是整表覆盖的形状决定的:每轮重发全表,而 `description` 上限 8 KiB,
256 条就是 MB 级的表,撑不住。删掉之后 **`task_get` 最后一点存在理由也没了**——
它存在的唯一原因就是「`task_list` 省略 description,要全文去 `get`」。

这是**产品面收窄**,要认:模型不能再给一条任务写详细指令。判断是——
清单的用处是「让模型和用户都看见还剩什么」,不是承载任务说明书;
真要写详细指令,那是消息正文或 plan 文件的事,不是面板上一行。

**记一笔:`subject` 从此是唯一的信息通道,上限 200 字符单行。**
本 plan **不动这个上限**——一行标题 200 字符够用,放宽它等于把 description
从后门放回来。若开工时发现模型频繁撞上限,那是「它在往 subject 里塞说明书」的信号,
该在工具描述里说清楚,不是抬高数字。

## 六、不在本 plan 内

- **缺 reminder** 那条(187 第六节记的)。做完 188 之后它反而更重要:整表覆盖的价值
  要靠「模型知道当前表长什么样」兑现,而 task 图至今不回灌上下文。**那条 plan 要连
  回灌粒度一起定**(整表含 status 回灌,还是只在表非空/全完成时提醒一句)。
- 187 里记过的那份 `session_picker_18x60.snap.new`,仍然不动。

## 七、验收

- `make check` 全绿;`make mock` 跑通(demo 脚本已改)。
- `task.rs` code 行 **< 200**(187 之后约 480,现 628)。
- `description` 在 `task.rs` 里只作为 `ToolDef.description` 与 schema 里的字段说明
  出现,**不再是任务的字段**;`MAX_DESCRIPTION_BYTES` 归零引用。
- 工具定义:**1 个**,描述 **< 400 字符**(现 5 个 / 1597 字符)。
- `grep -rn "task_create\|task_get\|task_update\|task_list\|task_clear" rust/crates/`
  只剩 `tools/mod.rs` 的 reserved 名单一处。
- `builtin::ALL` 的长度 −4;`builtin_all_is_complete` 那条测试自动守住变体数。
- DESIGN.md **2243–2295 整段重写**(不是追加):五工具 → 一工具、ID/rollover/高水位
  三段删掉、`todo_write` 那句的措辞要改——**它现在说「不做 alias、同步或迁移」,
  而本条等于回到它的形状,得把这段历史讲对**。2865–2895 对照差异段同步。

## 八、完成记录(2026-09-21,提交 a942f93;改名见第九节)

**已落地。** 删除面按第三节执行完毕,验收逐条核对:

| 验收项 | 目标 | 实际 |
|---|---|---|
| `make check` | 全绿 | ✅ fmt + clippy + 1600 test |
| `make mock` | 跑通 | ✅ demo 两轮改成写表 / 重写同一张表 |
| `task.rs` code 行 | < 200 | **197**(187 之后 424) |
| 工具定义数 | 1 | **1**(`task_write`) |
| 工具描述字符 | < 400 | **241**(原五个合计 1597) |
| `builtin::ALL` 长度 | −4 | **36 → 32**,`builtin_all_is_complete` 守住 |
| `description` 作为任务字段 | 0 | ✅ `MAX_DESCRIPTION_BYTES` 已删,零引用 |

工具形状:`{"tasks":[{"subject","status"}]}`,`subject` 与 `status` **都必填**——
整表覆盖本来就要求模型重述每一行的状态,让 status 可省而默认 pending,等于给
「忘了写」准备一条静默回退路径。

### 与 plan 预期不同的四处

1. **五个退役名不止进 reserved,还进了断言。** 验收原文写「grep 只剩 `tools/mod.rs`
   一处」,实际是五处:reserved 名单本身、它的 `reserved_name_tests`、`task.rs` 的
   「退役工具不可调用」测试、以及 plan52 原生报告的 `retired_tool_gate`。判据同 187:
   **名字没了,断言它没了的测试留着**——reserved 这件事只有被断言才不会烂掉。
2. **server 的 `task_graph_is_isolated_per_server_thread` 删了。** 它靠「每个线程都拿到
   ID `1`」证明 registry 不共享;整表覆盖之后,**共享与不共享返回的是同一张表**
   (调用方刚发的那张),这条性质从工具面**不再可观测**。没有把它改写成一条永远为真的
   断言,而是删掉并在 `task_write_surfaces_as_an_ordinary_tool_call` 的文档注释里
   写明它去哪了;线程级 Config 隔离另有 `thread_start_resolves_per_thread_cwd_and_model`
   和 `parallel_threads_do_not_cross_streams`,registry 独立性由 core 单测钉住。
3. **`BASE_SYSTEM` 顺带改了一句**(`context.rs:119-121`)。原文是「track the work with
   the task tools and keep their state current」,工具只剩一个,复数就是错的。只改了这
   半句为 `task_write` 并点明「每次重写整张表」;**「never as a round of their own」那半句
   留给 plan 190**——190 第六节已经写明它管的是更新不是开列,要连回灌一起改。
4. **`concurrency_safe` 从「Get/List 安全、其余不安全」收成一条 false。** 没有只读的
   task 工具了,整张表的写入必须串行。`readonly`(权限面)仍是 true:改的只有会话内存。

### `make parity`(本机语料,教训 156 的第二次实践)

红了,原因与 187 同:static-evidence 把行号区间钉进 `task.rs`(1190 → 702 行)和
`plan52_parity_tests.rs`;验证器另有一整段硬断言旧报告结构(`task_schemas` 五个 schema、
`task_graph` 的 blocker/rollover/high-water、`owner_field_gate`、`child_task_gate` 数组)。
已当场改完:行号重新对齐、原生报告断言改成整表写入的五个场景(首写/重写/同表重写不发
快照/空数组清空/strict 拒绝)、负向探针重写成七条(伪造 owner schema、缺 schema、child
catalog 提权、child 运行时绕过、事件乱序、无变化却发快照、**发出的快照与返回的表不一致**
——整表版的「非原子」、接受退役字段、复活退役工具名),matrix 的四条 CC 行与那条
kloop-only 行改指 `task_write` 后重新生成。`make parity` 全绿。
**这些改动全在 `refs/` 语料里,被 `.gitignore` 排除,不在版本控制中,没有对应提交。**

### 下一条

第六节那条「缺 reminder」已立为 **plan 190**,现在是它最该做的时候:整表覆盖的价值
全靠「模型知道当前表长什么样」兑现,而这次之后回灌的内容正好是一行标题加一个状态。

## 九、工具改名 `task_write` → `todo_write`(2026-09-21,提交 f50dec0)

第八节落地后用户一句「task 这个名字太重了」,复核后认同并改掉。**这是名字的问题,
不是形状的问题**——第二节定下的整表覆盖、两字段必填、幂等、校验时机全部不变。

### 判据

`task` 在 kloop 里**已经指三样东西**:`BackgroundTask`(后台执行)、`ScheduledTask`
(定时任务)、和这张清单。超载的代价是**每次调用都在付**:工具描述要花字符写
"it starts nothing, assigns nothing",DESIGN 要专门写一句把清单状态和执行生命周期
分开。`todo` 只指一样,而且先验最强——cc 的 `TodoWrite` 就是这个形状,kloop 自己
plan 20(提交 `63209d4`)也用过这个名字两个月。

**复用退役名的顾虑核查过,不成立**:担心的是 resumed history 里的旧调用被静默错认,
但旧 `todo_write` 是 `{"todos":[{content, activeForm, status}]}`,新的是
`{"todos":[{subject, status}]}`,**item 字段名不同,strict parser 会当场拒绝**,
不会半应用。两者本来也不是同一个工具:旧的**每个 agent 各自一张表、全 depth 露出**,
新的是 depth-0 root-only 一张表(plan 72 的结果)。这层历史写进了 DESIGN。

### 改动面

| 层 | 改动 |
|---|---|
| 模型面 | `task_write` → `todo_write`;参数 `tasks` → `todos` |
| reserved | `todo_write` **移出**退役名单(回到服役,由 `builtin::ALL` 自动保留);五个 `task_*` 留着 |
| 类型 | `TaskRegistry`→`TodoRegistry`、`TaskGraphSnapshot`→`TodoSnapshot`、`TaskGraphTask`→`TodoItem`、`TaskStatus`→`TodoStatus`、`Event::TaskGraphUpdated`→`Event::TodoUpdated`、`Builtin::TaskWrite`→`Builtin::TodoWrite`、`Config.tasks`→`Config.todos` |
| 文件 | `tools/task.rs` → `tools/todo.rs`(`git mv`,保住历史) |
| TUI | `app.todos` / `show_todos` / `live_todos()` / `todo_panel_lines` / `todo_line` / `TODO_PANEL_*`;工具行 "Todo write";footer 提示 "ctrl+t to hide todos" |
| 文案 | `BASE_SYSTEM` 的 `todo_write`、`--mock` 的 "root-owned todos"、`builtin` 标题 "Write todo list" |
| 措辞 | 全仓 "task graph" → "todo list"——`graph` 是 187 删掉的依赖图时代的遗留词,已经名不副实两条 plan 了 |

### 两个真实的坑(教训 160)

机械改名用 `\btask\b → \btodo\b` 跑,**在 `app.rs` 和 `render.rs` 上误伤了**:
`Item::SubAgent { label, task }`、`Cell::Agent { agent, task }`、
`Cell::BackgroundTask(task)`、`tokio::task`、`Injected::Scheduled` 的 "scheduled task"
文案,全被改成了 todo。编译器抓住了结构体字段那几处,**但注释和字符串文案不会报错**
——`"scheduled todo · {id}"` 会一路静默到用户屏幕上。处理方式:**先全量还原,再逐条
精确改回该改的那四五处**,而不是在误伤堆里逐个挑。

第二个坑:`todo_write` 从「退役名」变回「在役名」,三处列表要同时调整
(reserved 的构造、`reserved_name_tests` 的断言、`retired_tool_names_...` 的
catalog-absence 列表),漏一处测试就红。`retired_tool_names_stay_reserved_and_legacy_migrations_are_directed`
里那条 `assert_eq!(output, "unknown tool: todo_write")` 现在指向五个 `task_*`。

### 验证

`make check`(fmt + clippy + **1600 test**)全绿、`make mock` 跑通、`make parity` 全绿
(本机语料第三次跟着改:工具名、字段名、static-evidence 的文件名与行号区间、
matrix 的 `kloop_name`)。

## 十、兼容包袱删干净(2026-09-21,同日第三次提交)

用户:「代码删干净,不要兼容,我喜欢干净的代码」。第九节改完名后盘了一遍,
**兼容设施有四处,全删**。

### 删的

1. **`reject_unavailable` 的改名指引**(`tools/mod.rs`):
   `task → run_agent`、`wait → wait_for_activity`、`kill_bash → stop_bash` 三条
   `bail!`,以及一条 `bash.run_in_background → background`。删掉后这些名字统一走
   `unknown tool: <name>`。**顺带发现那条 `run_in_background` 是重复的**——
   `bash.rs` 的 `parse_bash_input` 里有一模一样的一条。
2. **`bash.rs` 的 `run_in_background` 改名 `bail!`**:`BashInput` 本来就
   `deny_unknown_fields`,删掉手写分支后它报 ``unknown field `run_in_background` ``,
   **一样 fail-closed,只是不再手把手指路**。
3. **`reserved_names()` 里的八个退役名**(`task`、`wait`、`kill_bash` + 五个 `task_*`)。
   留下的只有 `builtin::ALL` 派生的在役名和 `structured_output`(内部协议名)。
4. **只为上面三条存在的测试**:`retired_tool_names_stay_reserved_and_legacy_migrations_are_directed`、
   `the_retired_tools_and_their_fields_are_gone`、reserved 测试里的退役名块、
   `the_definition_is_one_strict_root_only_tool` 里两个退役枚举、plan52 的 `retired_tool_gate`。

`reject_unavailable` 的 `input: &Value` 参数**只为那条 `run_in_background` 检查存在**,
一并删掉,调用点跟着改。

### 判据

reserved 名单的理由是「MCP 工具不能冒名顶替 resumed history 里的旧调用」。这条理由
经不起推敲:**模型是按当前工具数组决定调什么的**,不是按历史;真误调了,
`unknown tool: X` 已经是足够清楚的答复;而这份名单**只会增长**——plan 188 一次就加了五个。
换来的是「一个模型没理由发起的调用」的错误文案好一点点。

同一判据也适用改名指引:`task → run_agent` 是 plan 66 那次改名的产物,两条 plan 之前的事。

### 保留的三样(不是兼容包袱,别顺手删)

- **`builtin::ALL` 派生的 reserved**:在役工具名不能被外部源占用,这是当前保护。
- **`structured_output` reserved**:内部补全协议名,从来不是可配置工具。
- **`rollout.rs` / `codemode.rs` 的 "legacy … cannot be resumed"**:那是**拒绝**旧数据
  的 fail-closed,不是接受旧数据的兼容层。方向相反。

### 两条负向断言留着,并写明它们守什么

`the_reserved_set_covers_...` 里加了「`task`/`task_create` **不在** reserved」,
`an_unknown_tool_name_fails_closed_with_no_rename_table` 拿 `task`/`task_create`/
一个从未存在的名字当探针。**它们守的是「兼容包袱不要再长回来」**——删掉守卫,
下一个人很容易又往 reserved 里加一行。这不违背「不要兼容」,是执行它。

### 顺带收敛的两处历史枚举

- `todo_write` 的行 schema 已经是 `additionalProperties:false` + `required:[subject,status]`,
  再列一份 `["id","task_id","description","owner","blocked_by"]` 说明它们不在,是把历史
  钉进断言。改成直接断言 `properties` 的键集合就是 `{subject,status}`。
- plan52 的 `retired_field_gate` **改名 `foreign_field_gate`**:那几个字段是
  **对照产品的 Task 记录带、kloop 的行不带**的,这是 parity 报告该说的差异证据,
  不是 kloop 自己的历史。框架从「我们退役了什么」换成「对方有什么我们拒什么」。

### 账

`task.rs`(现 `todo.rs`)code 行 197 → **194**;`tools/mod.rs` 净减约 60 行;
测试 1600 → **1599**(删两条、加一条)。`make check` / `make mock` / `make parity` 全绿。
