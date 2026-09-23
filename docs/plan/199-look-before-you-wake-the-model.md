# Plan 199 — 叫醒模型之前,先看一眼

> 来源:2026-09-23,读一个私有 IM 网关时看到的做法(只写形状):定时任务可以先跑一个探测脚本,
> 退出码为 0 就什么都不做,非 0 才把输出交给 agent。用户在对照清单里选了它,「做1,2」
> (2 是 `/context`,已于同日 `2ce2a53` 落地)。

## 一、为什么

"每 10 分钟看一眼 CI / 部署 / 某个日志"这类定时任务,绝大多数时候什么都没发生。现在
kloop 的 `cron_create` 只有 `prompt`,每次到点都是一次完整的模型调用:整份 system + 工具
定义 + 历史(`/context` 在本仓库量出来,光固定开销就约 1 万 token),换来一句"没有变化"。
一个 shell 命令就能回答"要不要叫醒模型",而且回答得更准。

## 二、形状

`cron_create` 加一个可选字段 `check`(一条 shell 命令):

- 到点先跑 `check`。**退出码 0 → 本次静默跳过**,照常推进下一次触发时间,不进 inbox、
  不起 delivery turn、不写历史。
- **非 0 → 照常投递** `ScheduledPrompt`,并在 prompt 后附上 check 的退出码与输出(截尾,
  上限复用 bash 工具的输出上限)。
- **check 自己跑不起来**(被权限门拒绝、超时、spawn 失败)→ **也投递**,附上原因。
  判据写死:**不确定就叫醒模型,绝不静默吞掉**——一个坏掉的 check 静默吞掉,等于这个
  定时任务悄悄死了。
- `cron_list` 显示 check;持久任务把 `check` 写进 store(`#[serde(default)]`,旧文件照读)。
- 与 cc 的 schema parity:plan 58 的 parity 测试只锁了 `required` 与
  `additionalProperties: false`,加一个**可选**字段不破它;`required` 仍是 `["cron","prompt"]`。

**不做**:`schedule_wakeup`(`/loop` 的动态唤醒)不加 check——那是模型自己排的下一步,
不是无人值守的轮询。

## 三、开工时必须问用户的点(只有一个)

**check 在触发时面对什么权限?** 这是一条没有人在场时执行的 shell 命令。

- **做法甲(推荐):每次触发都走完整的 bash 门**——deny / 安全检查 / ask / approver / 沙箱,
  和模型自己调 bash 一模一样。先例是 `tools/inject.rs` 的 `expand_slash_injections`:
  `/name` 命令里的 `` !`cmd` `` 就是用一个最小的顶层 `ToolCtx` 走真实 bash 门执行的,
  模块注释写明"这些门是安全边界,这里从不绕过"。后果:没有被允许过的 check 会在触发时
  弹审批;用户选一次"本会话/本项目总是允许"之后就不再问。审批被拒 → 按第二节"跑不起来"
  处理,叫醒模型并说明。
- 做法乙:创建时审批一次,之后免审跑。**不推荐**——等于给一条字符串发了一张长期通行证,
  而持久任务还会被另一个进程认领执行。
- 做法丙:只允许只读分类器自动放行的命令,其余在创建时拒绝。最安全,但 `curl`、`gh run
  list` 这类最常见的 check 大概率进不了只读名单,功能就没了。

## 四、实现要点(开工时核)

- **check 在哪里跑**:调度 worker(`Scheduler::run_worker`)只持有 `Inbox`,没有 `Config`。
  worker 是由第一次调度工具调用 `bind_owner` 懒启动的(`tools/scheduler.rs` 的 `bind`),
  那里有 `ctx.cfg`——在这里给 worker 挂一个 check runner。**注意引用环**:
  `Config → Scheduler → runner → Config`,runner 要持 `Weak`,升级失败就当 session 已结束。
  CLI plain 也在 `main.rs` 里直接 `bind_owner`,两处都要接。
- **不能阻塞 worker 循环**:check 可能跑几十秒,worker 还要服务别的任务的到点和 store
  轮询。每个 check 单独 spawn,结果回来再 push;同一个任务上一次 check 没跑完时,这次
  触发跳过(不排队、不并发同一个 check)。
- **超时**:默认 60 秒,超时按"跑不起来"处理。开工时看 bash 工具现成的超时与进程组清理,
  复用,不另写。
- 持久任务被另一个 runtime 认领时,check 在认领方跑、走认领方的门——这是对的,记进 DESIGN。

## 五、测试

- 退出 0 → inbox 为空、`next_fire_at_ms` 前进、没有 delivery turn。
- 退出非 0 → inbox 里一条 `ScheduledPrompt`,prompt 带退出码与输出(整对象断言)。
- 权限门拒绝 / 超时 → 投递,并附原因。
- 上一次 check 未完成时再次到点 → 跳过,不并发。
- 持久 store:带 `check` 的任务往返;没有 `check` 字段的旧文件照读。
- parity:`required` 仍是 `["cron","prompt"]`。
- 用 `ManualClock` 驱动,不睡真时间。

## 六、与其他 plan 的关系

- **plan 181 要把 `core/src/scheduler.rs` 拆成 `scheduler/{cron,store}.rs`**。两条改同一个
  文件,不要并行;谁后做谁跟着新位置改。

## 七、完成记录

(未开工)
