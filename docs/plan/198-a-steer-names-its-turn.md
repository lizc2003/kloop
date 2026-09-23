# Plan 198 — 插话要说清楚它插的是哪一轮

> 来源:2026-09-23,读一个驱动 codex app-server 的私有 IM 网关客户端(只读调研,结论见
> 当天会话)。它在生产里踩过的一条是:**客户端以为某一轮还在跑,发了插话;那一轮其实
> 已经结束,插话被塞进了下一轮**,于是用户对上一件事的补充,变成了一件新事的开头。
> 它的修法是插话带 `expectedTurnId`、不匹配就拒绝、客户端自己决定改成排队还是丢弃。
> 用户看完对照后一句「开个 plan 做 steer 校验」。

## 一、现状(读代码得来,开工先复核)

- `turn/steer`(`server/src/lib.rs` 的 `turn_steer`)**无条件** `inbox.push(InboxItem::Steer)`,
  push 之后再读 `handle.turn` 返回 `{turn_id}`——空闲时是 `null`。
- 它的 doc comment 已经写着 *"`expected_turn_id`, if the client sends it, is accepted and
  ignored for now"*——**这个参数的位置早就留好了,本条是把它兑现**。
- **同一段 doc comment 有一半是过期的**:它说空闲时的插话"等下一次 `turn/start` 顶部
  drain,client-driven server 没有 autowake"。但 `thread_worker` 的 `select!` 里有
  `inbox_activity.changed()` 分支:inbox 非空且 `running` 抢得到,就**当场分配一个
  `delivery_only` 的新 turn**。DESIGN.md 的 Steering 一节写的是后者(对的),
  HANDOFF.md 的 server 能力条目和 plan 22 条目写的是前者(错的)。**开工第一件事:写一条
  测试钉住"空闲插话 → 立刻起一个 delivery turn",再动任何东西。**
- core 里插话能折进当前轮的最后一个点是 `keep_going_for_late_work`(`core/src/agent.rs`,
  回合准备结束前再 drain 一次,命中就 Retry)。另一个 drain 点是每一轮顶部。
  中断/出错结束的回合走不到这个兜底 drain。

## 二、问题:两个场景,第二个更隐蔽

**A. 轮次已经结束。** 客户端看到的最后状态是"turn N 在跑",发插话;服务端这时已空闲,
插话立刻起了 delivery turn N+1。客户端本意是改 N,结果启动了一个只有这句补充的新回合,
模型看到的是没头没尾的一句话。

**B. 轮次"还没结束,但已经不收了"。** 最后一次 drain 之后、`handle.turn` 清成 `None` 之前,
中间有 `turn/completed` 通知、usage 事件、rollout 收尾。插话落在这个窗口里:
**响应告诉客户端 `{turn_id: N}`,实际上它被 N+1 消费了。响应在说假话。**
这个窗口不带 `expected_turn_id` 也存在,所以本条修的不只是新参数。

## 三、要做的

1. **`expected_turn_id` 可选。** 带了:只有该轮正在跑**且还收插话**时才入队,否则**不入队**,
   返回错误,`data.kind` 区分"没有在跑的轮次"和"在跑的不是这一轮"
   (沿用 `thread/provider/switch` 的 `data: {kind}` 形状,名字开工时定)。
   不带:行为照旧(空闲插话仍起 delivery turn),兼容现有客户端和 `steer_folds_into_a_running_turn`。
2. **"还收插话"要和最后一次 drain 原子。** 这是本条的难点,也是**开工必须问用户的点**:
   - **做法甲(推荐):动 core 的 `Inbox`。** 给它一个"关门"的原语:回合决定结束时,
     在同一把 `items` 锁里 drain 并关门;server 的插话在同一把锁里检查门并 push。
     于是一条插话要么在关门前进来、被兜底 drain 捡到(回合续跑),要么被拒——**没有窗口**。
     下一轮开始时开门。代价是 core 的 `Inbox` 多一个状态,且要想清楚它和子 agent 的
     fresh inbox、TUI 的 Enter-while-running 是否需要同样的语义(大概率不需要,TUI 没有
     "期望轮次"这回事)。
   - **做法乙:只改 server。** 在 `turn/completed` 通知前关门。改动小,但最后一次 drain
     到关门之间仍有窗口(只是更窄),场景 B 只是变少、没有消失。
   - 问法:"要不要为了关掉 B 窗口去动 core 的 Inbox?"——不要写成多选问卷。
3. **不带参数时的返回值也要说实话。** 做法甲落地后,门关了的那一刻返回 `null`
   (它确实会进下一个 delivery turn),而不是 N。这算不算破坏兼容,开工时和用户确认。
4. **中断的回合:** 取消时门应当立即关(取消后的插话不该被当成"折进了 N")。开工时读
   `turn_interrupt` 与 cancel 路径确认。
5. 顺手修掉第一节列的三处过期描述(lib.rs doc comment、HANDOFF 的两条),DESIGN.md
   的方法表和 Steering 一节补上 `expected_turn_id` 与错误 kind。**先读那两段现在还成不成立。**

## 四、不做

- 不做服务端重试,也不做"拒绝后自动改成排队"——那是客户端的决定,服务端只负责不说假话。
- `turn/interrupt` 不加 `expected_turn_id`(同类问题,但另一件事;记一笔)。
- 不改 wire 字段的 snake_case(HANDOFF 里写的 `{turnId}` 是旧描述,代码是 `turn_id`,顺手改文档)。
- 不碰 TUI 与 plain 的插话路径。

## 五、测试(server 集成测试,mock provider)

- 空闲插话起 delivery turn(第一节的钉子,先写)。
- 带 `expected_turn_id` = 正在跑的轮次 → 入队、折进该轮、返回该 id(改写现有那条,或并列一条)。
- 带 `expected_turn_id`、空闲 → 错误,kind = 无活跃轮次;**之后没有 delivery turn 被起**,
  rollout 里没有这句话。
- 带 `expected_turn_id`、不匹配(旧 id)→ 错误,kind = 轮次不符,不入队。
- 场景 B 的确定性复现:只有做法甲能写成确定性测试(用 core 层单测驱动"drain+关门"与 push
  的交错);做法乙只能断言窗口变窄,写不出确定性测试——**这本身也是选甲的一个理由**。
- 中断后带旧 id 插话 → 拒绝。
- 整对象断言错误响应(`code`、`message`、`data`)。

## 六、与其他 plan 的关系

- **plan 185 要拆 `server/src/lib.rs`**(`turn_steer` 会搬进 `methods/turn.rs`)。两条改同一个
  文件,**不要并行开 worktree**;谁后做谁跟着新位置改。
- plan 22(steering 机制)与 plan 26(delivery turn 的来路)是背景,不需要重读。

## 七、完成记录

✅ 2026-09-23,用户选**做法甲**(动 core 的 `Inbox`)。提交号见本条所在提交。

- **第一节的钉子先确认了**:空闲插话当场起 delivery turn,代码注释与 HANDOFF 两条写的
  "等下一次 `turn/start`"是错的。现在由 `steer_expecting_a_finished_turn_is_refused_and_starts_nothing`
  钉住(无条件插话 → `turn_id: null` → 下一个 turn id 被 delivery turn 用掉)。
- **core**:`Inbox` 的队列与 `steer_window: Option<u64>` 同锁(`Pending`)。`push_steer(text,
  expected)` 在同一临界区准入并入队,返回窗口值;`drain_or_close_steer_window` 取空时同锁关窗;
  `open/close_steer_window`。`SteerRefused::{NoActiveTurn, TurnMismatch{active}}` 带 `kind()`。
- **收尾 guard 改了顺序**:`keep_going_for_late_work` 原来先 drain 再问"本地 agent 能不能结束";
  现在先问后者(不能结束就普通 drain + Retry,不关窗),最后一次 drain 才用关窗版本。
  两种顺序对原有行为等价,但旧顺序会"关了窗又续跑"。
- **server**:`turn/start` 与 delivery turn 开窗;`turn/interrupt` 立刻关窗;worker 在每个回合
  结束时再关一次(错误、max rounds、中断这些走不到 guard 的结束方式)。`turn/steer` 与
  provider switch 一样在分发里单独处理,拒绝时 `-32000` + `data.kind`(`turn_mismatch` 另带
  `active_turn_id`)。
- **第三节第 3 点(不带参数时返回值说实话)**:返回的 `turn_id` 就是准入时的窗口值,关窗之后
  返回 null。旧客户端原来在空闲时本来就拿到 null,只是那个空档里的回答从 N 变成了 null。
- **"不做"照旧**:不重试、`turn/interrupt` 不加 `expected_turn_id`、TUI/plain 路径不动
  (它们从不开窗,guard 的关窗对它们是 no-op)。
- 测试:core 3 条(`push_steer` 准入矩阵、最后一次 drain 只在结束回合时关窗、agent 层"准入的
  插话由该轮回答、之后该轮拒绝")+ server 3 条(运行中:错号拒绝整对象断言、对号折入;结束后:
  带号拒绝且不起回合、不带号 → null + delivery turn;中断后带号拒绝,对完成通知与拒绝的先后
  不做假设)。`make check` 全绿。
- **一个测试坑**:中断那条第一版在 `recv_until` 里等 `turn/completed`,但它可能先于插话的
  响应到达、被前一次 `recv_until` 吃掉,于是超时。两个事件的先后本来就不确定(两处关窗都能
  让插话被拒),测试改成两者都收到为止、不假设顺序。
