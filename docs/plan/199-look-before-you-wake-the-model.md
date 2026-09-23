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

✅ 2026-09-23,用户选**做法甲**(每次触发都走完整 bash 门)。提交号见本条所在提交。

- **bash 工具拆出带状态的前台运行**:`bash_tool` 原来只返回文本,成败只靠文本末尾的
  `[exit …]` 表达。拆出 `run_foreground_bash` → `ForegroundRun { text, success }`
  (escalation 重跑时取重跑那次的状态),`bash_tool` 取 `.text`,行为不变。check 拿
  `success`,**不解析文本**。
- **调度器**:`ScheduledJob.check`(`serde(default)`,旧 store 照读);`CheckRunner` trait +
  `CheckOutcome::{Passed, Failed, Unavailable}`;worker 对带 check 的到点任务 `start_check`
  单独 spawn,`checks_running` 按 job id 去重(锁跨 spawn 持有,任务自己的 remove 不会先于
  insert);`shutdown` 取走 runner 并 abort 在跑的 check——**这是打断
  `Config → Scheduler → runner → Config` 引用环的唯一一处**,有测试钉 `strong_count`。
- **runner**:`tools/scheduler.rs` 的 `GatedCheck`,在第一次调度工具调用的 `bind` 里装上
  (`bind_check_runner` 只装一次)。`inject.rs` 原来内联构造的最小 `ToolCtx` 提成
  `ToolCtx::harness`,两处共用。
- **第四节"runner 持 `Weak`"没有照做**:前端在 `/model`、`/provider` 之后会重新冻结出一个新的
  `Arc<Config>`,旧的那个随即被丢掉,持 `Weak` 的 runner 会在一次换模型之后悄悄失效(每次
  触发都变成 "could not run")。改为持强引用、由 `shutdown` 断环。持的是绑定那一刻的 Config
  也没关系:bash 用到的权限、沙箱、shell、worktree 状态都是 clone 间共享的 `Arc`。
- 超时:bash 默认 60 秒,超时是 `bail!` → `Unavailable`,与第二节一致,未另写。
- **第四节"CLI plain 也要接"是错的**:`main.rs` 那处 `bind_owner` 在测试里,生产里 worker
  只由工具的 `bind` 启动,runner 装在同一处就够了。
- 测试:调度器 6 条(通过即跳过并重排、失败带输出投递、被拒/无 runner 也投递、上次未完成
  则跳过、shutdown 释放 runner、持久 store 往返 + 空 check 拒绝),工具层 3 条(真 bash
  退出码 → Passed/Failed、deny 规则 → Unavailable、`cron_create` 端到端:真 bash 失败 →
  inbox 里一条逐字断言的 prompt、`cron_list` 显示 check)。`make check` 全绿。
- 没做真实 provider 下的交互验证(触发时审批弹窗在 TUI 里的样子);门的行为由 deny 规则
  那条测试钉住。
