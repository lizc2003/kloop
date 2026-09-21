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
- 上限沿用:`MAX_TASKS` 256、subject 200 字符单行。
- 描述目标:**一个工具 < 400 字符**(现在五个合计 1597)。

`description` 字段留不留见第五节,**这是开工前要问用户的点**。

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

## 五、开工时定(问用户)

**`description` 字段留不留?**

倾向**删掉,只留 `subject`**:整表覆盖每轮重发全表,而 `description` 上限 8 KiB,
256 条就是 2 MB 级别的表——这个形状撑不住。删掉之后 task 就是一行标题加一个状态,
`task_get` 消失得也更自然(它存在的唯一理由就是「list 省略 description,要全文去 get」)。

但这是**产品面收窄**:模型再不能给一条任务写详细指令。如果你要留,那 `task_write`
的表必须限长(比如 description 降到 512 字符),不能沿用 8 KiB。

## 六、不在本 plan 内

- **缺 reminder** 那条(187 第六节记的)。做完 188 之后它反而更重要:整表覆盖的价值
  要靠「模型知道当前表长什么样」兑现,而 task 图至今不回灌上下文。**那条 plan 要连
  回灌粒度一起定**(整表含 status 回灌,还是只在表非空/全完成时提醒一句)。
- 187 里记过的那份 `session_picker_18x60.snap.new`,仍然不动。

## 七、验收

- `make check` 全绿;`make mock` 跑通(demo 脚本已改)。
- `task.rs` code 行 **< 200**(187 之后约 480,现 628)。
- 工具定义:**1 个**,描述 **< 400 字符**(现 5 个 / 1597 字符)。
- `grep -rn "task_create\|task_get\|task_update\|task_list\|task_clear" rust/crates/`
  只剩 `tools/mod.rs` 的 reserved 名单一处。
- `builtin::ALL` 的长度 −4;`builtin_all_is_complete` 那条测试自动守住变体数。
- DESIGN.md **2243–2295 整段重写**(不是追加):五工具 → 一工具、ID/rollover/高水位
  三段删掉、`todo_write` 那句的措辞要改——**它现在说「不做 alias、同步或迁移」,
  而本条等于回到它的形状,得把这段历史讲对**。2865–2895 对照差异段同步。
