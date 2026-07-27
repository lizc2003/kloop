# Plan 58 — 调度工具对齐

> 状态：未开工
>
> 母计划：Plan 48
>
> 依赖：Plan 51、Plan 52
>
> 固定目标：Claude Code 2.1.220；版本身份以 Plan 48 manifest 为准。

## 背景

调度能力横跨工具注册、时间计算、持久化、后台执行、通知回灌和 session 生命周期，因此必须等待 Plan 51 的后台/Monitor 契约与 Plan 52 的 Agent/Task 回灌边界稳定。

Plan 48 只证明 CronCreate、CronDelete、CronList、ScheduleWakeup 在 CC clean fixture 中注册并带 schema。kloop 当前没有同形的 model-visible scheduler；四项的 executor、permission、concurrency、output 和 lifecycle 均未运行。

## 当前证据与差距

对应 matrix 行：

- `cron-create@clean-cli`
- `cron-delete@clean-cli`
- `cron-list@clean-cli`
- `schedule-wakeup@clean-cli`

当前结论：

- 四项工具在 CC clean fixture 可见；kloop 无对应实现，因此 registration/schema 为 `missing`。
- parser、executor、permission、concurrency、output、lifecycle 全部为 `unknown`。
- 当前 fixtures 没有可控时钟、到期触发、删除/列举、重启恢复、错过触发或通知回灌。
- Cron 与 ScheduleWakeup 是否共享 registry、持久化和执行模型尚无证据。

优先复用：

- Plan 48 的 exact-binary collector、fake provider、manifest 与 verifier
- Plan 51 的后台状态、通知、父会话结束和 orphan cleanup 契约
- Plan 52 的 Agent/Task registry、结果回灌、取消和并发契约
- kloop 当前 session/config/storage seam，仅在静态定位证明适用后复用
- 临时目录、可控时钟与进程内 fake scheduler

## 目标

1. 固定 CronCreate/Delete/List 与 ScheduleWakeup 的注册条件、schema、parser 和错误。
2. 固定 cron 表达式、时区、当前时间、下一次触发、抖动和边界时间计算。
3. 固定一次性/重复、session-only/durable、删除、列举、过期和重启恢复语义。
4. 固定到期任务如何排队、执行、取消、失败、通知并回灌当前或恢复后的 session。
5. 固定并发到期、重复 ID、create/delete race 和父会话结束时的 cleanup。
6. 全部 fixture 使用可控时钟、临时存储和 fake provider；不安装系统 cron，不创建真实长期任务。

## 开工证据闸门

- 从 exact bundle 分别追四个工具的构造点、schema、parser、scheduler registry、storage、timer 和 result mapping。
- 找出 ScheduleWakeup 的真实调用上下文与 gate；不能仅凭 clean registration 推断动态 loop 行为。
- collector 注入或驱动可控时间；禁止依赖真实分钟边界、睡眠数分钟或系统时区偶然状态。
- 所有持久化写入 fixture 临时目录，case 前后记录文件 hash、job registry、timer 与通知队列。
- fake provider 驱动到期 prompt 和后续 tool_result；保存触发、排队、执行、通知、回灌的严格顺序。
- job ID、临时路径和时间只按声明规则归一化；cron 表达式、时区、状态、排序和错误文案不得归一化。
- verifier 断言没有系统 crontab、launchd、用户配置、真实持久任务或残留进程被修改。

## 实施切片

### 0. 注册、schema 与 parser

- 固定四个工具的名称、字段、required、默认值、additionalProperties 和坏类型错误。
- 覆盖合法/非法 cron、边界日期、空 prompt、未知 job ID、durable/recurring 组合。
- 区分注册可见、当前 session 可调用和实际 scheduler 可执行三个层级。

### 1. Cron registry 与时间模型

- 固定 Create/Delete/List 的 ID、排序、状态、下一次运行与删除结果。
- 采集一次性/重复任务、时区/DST、月末、闰年、错过触发、抖动、自动过期和时钟跳变。
- 判断 session-only 与 durable 是否共享 registry；在 fixture 前不预写存储格式或恢复策略。

### 2. ScheduleWakeup 与回灌

- 固定 wakeup 的延迟/时间输入、最小/最大边界、替换、停止和重复安排。
- 采集 session 忙碌、空闲、关闭、断线和恢复时的触发与投递。
- 与 Plan 51/52 对齐后台通知、agent completion、task registry 和父 turn 边界，不因内部统一改变已证明顺序。

### 3. 并发、失败与清理

- 覆盖同刻多任务、create/delete race、重复 delete、执行失败、取消、进程退出和存储损坏。
- 固定并发上限、排队、公平性和一个 job 失败是否影响其他 job。
- 验证 session exit、异常和测试结束后 timer、任务、文件锁及子进程全部清理。

### 4. 产品与回归

只实现已裁决差距；Cron 与 ScheduleWakeup 可共享底层组件，但公开 surface 和生命周期必须分别有证据。同步 matrix、fixture、static evidence、manifest、generator 与 verifier。

## 非目标与有意保留

- 不安装或修改系统 cron、launchd、systemd timer 或用户登录项。
- 不创建跨测试长期存活的真实任务，不等待真实小时/天边界。
- 不访问公网、远程队列、真实日历或用户通知服务。
- 不把 kloop 现有后台 task 自动计作 scheduler parity。
- 不从当前 Claude Code 描述、公开文档或其他版本推断 2.1.220 的时区、抖动、过期或 durability 规则。
- 不为内部统一强行合并 Cron、ScheduleWakeup、后台 Bash 和 Agent 状态机。

## Fixture 与测试

至少覆盖：

- 四工具最小成功、缺字段、错类型、未知字段和边界输入；
- 合法/非法 cron、一次性/重复、时区/DST、月末、闰年和时钟跳变；
- Create/List/Delete、未知 ID、重复删除、排序与并发 race；
- session-only/durable、重启恢复、错过触发、自动过期和损坏存储；
- ScheduleWakeup 的安排、替换、停止、忙碌/空闲/关闭/恢复；
- 到期成功、失败、取消、同刻多任务、通知与结果回灌；
- fixture 后无系统 scheduler 改动、持久任务、timer、锁、文件或进程残留。

验证：

```bash
python3 -B refs/claude-code-2.1.220/verify.py
cd kloop
cargo test -p kloop-core scheduler
cargo test -p kloop-core background
cargo test -p kloop-core tools::task::tests
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

## 文档同步

完成时同步本 plan、HANDOFF、refs/README、kloop README、capability report 与 parity 产物。明确时间模型、durability、错过触发、回灌和 session cleanup 的实际边界。

## 完成标准

- 四个 scheduler surface 的当前可运行链有 exact fixture 与 kloop golden 或明确产品裁决。
- 时间测试完全由可控时钟驱动，normalized fixture 可确定性重放。
- create/delete/list、触发、取消、失败、重启与回灌均有确定性测试。
- 不修改系统 scheduler 或用户存储，结束后无 timer、任务、文件锁或进程残留。
- 所有门禁全绿，一次提交，提交信息带 `plan58`。

## 开工时定 / 问用户

- kloop 是否需要同时提供 Cron* 与 ScheduleWakeup 两类 model-visible surface。
- session-only 与 durable scheduler 的产品范围、默认存储和恢复边界。
- 时区、抖动、错过触发、自动过期与通知投递的兼容策略。
