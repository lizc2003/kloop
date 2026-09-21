# Plan 187 — 一张图,只剩一个写入者

> 来源:2026-09-21 一次「为什么模型列了六条候选却没调 task 工具」的对话。直接原因是
> 没有 reminder(另记,见第六节),但翻 plan 71/72/73 时翻出了更深的一层:**依赖图是为
> plan 71 那个多 agent 协调的世界建的,plan 72 把那个世界抽走了,图留了下来**。
> 这条 plan 只做一件事:按 plan 73 已经用过的判据,把没有执行语义的依赖图删掉。
>
> **不属于架构重整那一批(177–186)**,与它们没有文件重叠,可任意顺序插入。

## 一、为什么现在能删

plan 71 建这套图时,它是**多 agent 协调机制**:共享 `Arc<TaskRegistry>`,child agent
各自认领,dependent 在 blocker 未完成时被 `task_update` 挡回去。那次真实验收跑的就是
这个——两个后台 child,一个被 gate 住,blocker 完成后才放行。环检测、blocker gate、
正反向投影,在那个世界里全都有执行语义。同一个 plan 顺手删掉了旧的 `todo_write`。

**plan 72 把图收紧成 depth-0 root-owned,child agent 不再看见 `task_*`**。协调对象没了,
只剩 root 一个写入者。**plan 73 已经按这个判据砍过一次**:owner 被删,理由是
「kloop 没有 Team、assignment、claim、权限、路由,owner 没有可执行语义;只在 description
里劝模型不要乱填,不能形成运行时不变量」。

同一个判据现在原样指向 `blocked_by`。一个只有 root 写入、自己创建自己更新的图,
环检测在防它自己成环,blocker gate 在拦它自己往前走。**唯一的真实消费者是
`rust/crates/tui/src/render.rs:151` 的 `open_blockers`**——渲染时把未完成的 blocker
显示成一行。代价是 `task_update` 472 字符描述里的大半,和模型每次调用都要维护的一套约束。

量出来的现状:`rust/crates/core/src/tools/task.rs` **1568 总行 = 628 code + 940 test**,
`blocked_by` 在 code 区 41 处、test 区 38 处。五个工具描述合计 **1597 字符**。

## 二、删除面(core)

`rust/crates/core/src/tools/task.rs`,整函数删除的:

| 行 | 行数 | 是什么 |
|---|---|---|
| 20 | 1 | `MAX_BLOCKERS` |
| 274–297 | 24 | `validate_dependencies` — 越界/自依赖/重复/缺失四种拒绝 |
| 298–318 | 21 | `creates_cycle` — DFS 环检测 |
| 319–336 | 18 | `validate_blocker_statuses` — blocker gate |
| 360–367 | 8 | `blocks_for` — 反向投影 |
| 368–371 | 4 | `ids_as_strings` — 只有 344/355 两个调用点,都是 blocked_by/blocks |
| 582–604 | 23 | `optional_blocked_by` — 解析 |

字段:`StoredTask.blocked_by`(57)、`TaskView.blocked_by/blocks`(66–67)、
`TaskGraphTask.blocked_by/blocks`(75–76)。

改动点:

- **`create`(107–139)**:删 `rollover` 条件里的 `input.blocked_by.is_empty()`(151)、
  `validate_dependencies` 调用(123)、`StoredTask` 构造里的字段。
- **`update`(147–199)**:删 `patch.blocked_by` 分支(164–166)、三个校验调用(176–179)、
  `display_changed` 里的 `blocked_by` 比较(190)。删完这个函数少掉约三分之一。
- **五个 `*_def`(393–465)**:`task_create` schema 去一项 + 描述尾句;`task_update`
  描述里依赖那一大段;`task_get` 的「computed reverse blocks projection」;
  `task_list` 的 blocked_by/blocks。**目标:`task_update` 从 472 字符降到 150 以内,
  五个合计降到 900 以内**——这个数就是本 plan 的验收之一。
- `parse_create`(501)/`parse_update`(517) 里的 blocked_by 分支。

## 三、删除面(core 之外,七个文件)

- **`rust/crates/tui/src/render.rs` — 唯一的真消费者,三处**:`open_blockers`(151–160)
  整个删;`task_line`(162–)里的 `" › blocked by {}"` 分支(188–195);`tasks_panel`
  的四分组排序(264–281)里 `pending` / `blocked` 两组合并成一组。
  **这是本 plan 唯一用户可见的行为变更**:任务面板不再标注 blocker,pending 不再
  把「被挡住的」排到后面。
- **`rust/crates/cli/src/startup.rs:966`** — `mock_demo_turns()`(945) 的演示脚本讲的
  正是「blocker 挡住 dependent → 完成 blocker → 放行」这个故事,**整段要重写**。
  它是 `make mock` 的门面,重写成一个两三条任务顺序推进的演示即可。
- 四处只是测试里构造 `TaskGraphTask { blocked_by: Vec::new(), .. }`,删字段跟着删一行:
  `tui/src/app.rs:2138`、`tui/src/lib.rs:1380`、`server/src/wire.rs:474`、`cli/src/ui.rs:401`。
- `core/src/tools/plan52_parity_tests.rs:608` 一处。

**没有 wire 契约要改**:`TaskGraphUpdated` 在 `core/src/event.rs:309` 和
`server/src/wire.rs:281` 都返回 `None`,`cli/src/ui.rs:393` 那条测试的名字就叫
`task_graph_snapshot_has_no_plain_projection`。这个事件只给 TUI。
**也没有 insta 快照受影响**:两张 `tui/src/snapshots/*.snap` 里 `blocked` 计数为 0。

## 四、坑

1. **`rollover` 条件看着被放宽了,其实等价。** `create` 的 epoch 滚动条件现在是
   「图非空 + 全完成 + 新任务无依赖」。删掉第三项后,读 diff 的人会以为放宽了——
   删掉依赖之后根本不存在「带依赖的新任务」,语义等价。**在提交信息里写这一句**,
   否则复审会卡在这里。
2. **`validate_dependencies` 在 create 里的位置是有意的**(123 行,在 `next_id` 之后、
   mutation 之前),它保证了「失败不消耗 ID」。删掉之后 `create` 里只剩 text 校验和
   `checked_add` 两个失败点,而 **`failed_create_does_not_consume_an_id_and_clear_keeps_high_water`
   (836) 现在正是用一个坏依赖触发的**——这条测试必须保留,触发方式换成超长 text。
   **这是本 plan 最容易做错的一处。**
3. **`task_line` 的宽度计算**:`" › blocked by {}"` 参与截断,删掉分支后截断逻辑
   要跟着简化,别留下一个永远为空的 suffix 变量。
4. **测试按「测的是依赖还是状态机」分**,不要按名字猜:
   - `graph_constraints_are_atomic`(768) — 整条删。
   - `combined_dependency_and_status_patch_validates_the_candidate_atomically`(912) — 整条删。
   - `blockers_gate_forward_status_and_status_never_moves_backward`(795) — **一半删一半留**:
     blocker gate 那半删,状态不可倒退那半保留并改名。
   - helper `create(ctx, subject, blocked_by)`(656) 去掉第三个参数,全部调用点跟着改;
     `snapshots_revision_and_epoch_rollover_are_atomic`(1052) 里的
     `input(subject, blocked_by)` 同理。
5. **开工第一步先核实 parity 生成物**:`plan52_parity_tests.rs` 里 64 处 `task_`,
   DESIGN.md:2870 一段写着 plan 74「不改 218 份 pinned raw/normalized capture」。
   先跑 `make test` 看这条路径会不会动 matrix 行数/cells 数;**如果会动,停下来问用户**
   ——改 parity 生成物不在本 plan 范围内。
6. **不碰**:`TaskStatus::rank`(42) 与状态倒退检查(见第五节)、`task_clear`、epoch、
   revision、ID 高水位、`MAX_TASKS`/`MAX_SUBJECT_CHARS`/`MAX_DESCRIPTION_BYTES` 三个上限。

## 五、开工时定(问用户)

**状态倒退检查(`pending → in_progress → completed` 不可逆)留不留?**

本 plan 默认**保留**,只删依赖图。但它和 `blocked_by` 是同一类东西:约束的是模型自己,
没有第二个写入者要防。真实场景里模型发现「这条其实没做完」时无法把 completed 改回
in_progress,只能新建一条。如果决定一起删,删除面多出 `TaskStatus::rank`(42–49)、
`update` 里的倒退检查(170–175)、`status_name`(372,只服务那条错误信息)、
以及第四节 4 里说的那半条测试——**改动性质相同,规模很小,但这是行为放宽,要用户点头**。

## 六、不在本 plan 内

- **缺 reminder。** 全仓 `<system-reminder>` 用了四处(`skills.rs:378`、
  `tool_search.rs:195`、`fs.rs:70/386/427`),唯独 task 一处没有;task 图也不回灌上下文,
  `TaskGraphUpdated` 只进事件流给 TUI。**模型写下候选的那一刻,列表已经在它自己的
  上下文里了,调 task 换不回任何它还没有的东西。** 这是「为什么不调用」的直接原因,
  值得单独一条 plan,但和本条的删除面不重叠。
- **五个 CRUD 要不要合成一个全量覆盖的写入工具。** 砍掉图之后 task 退化成一张表,
  那时候才谈得上——也就是 plan 71 当年删掉的形状,但这次是有理由地回去。**先做完本条再判。**
- `rust/crates/tui/src/snapshots/kloop_tui__session_picker__tests__session_picker_18x60.snap.new`
  是一份被提交进仓库的 insta 未接受快照,**和本 plan 无关**,顺手记一笔别顺手删。

## 七、验收

- `make check` 全绿。
- `make mock` 跑通(demo 脚本已重写)。
- 五个工具描述合计 **< 900 字符**(现 1597),其中 `task_update` **< 150**(现 472)。
- `task.rs` code 行从 628 降到 **500 以下**;`blocked_by` 在全仓 `--include=*.rs` 的
  出现次数为 **0**。
- `grep -rn "TaskGraphTask\|TaskGraphSnapshot\|TaskView" rust/crates/` 逐个核对字段使用。
- DESIGN.md 两处同步——**先读那一段现在还成不成立,再决定改写还是追加**:
  - **2243–2295「Root-owned session task graph (Plans 71–74)」**:三条工具描述、
    「A task cannot enter a non-pending state until all blockers are completed」、
    missing/self/duplicate/cycles 那句、「256 blockers per task」——**改写,不追加**。
  - **2865–2895** 对照差异那段里提到 dependency 的一句。
