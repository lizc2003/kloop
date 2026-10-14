# Plan 181 — 一个 cron 解析器、一把文件锁、一个调度器,挤在一个文件里

> 本批判据与统一纪律见 `HANDOFF.md` 第〇节。
> 第一至四节保留立项时的快照；当前落点与验证见第五节。

## 一、现状

`rust/crates/core/src/scheduler.rs`,**1187 code 行 / 1864 总行**。
里面是三个**互相不需要认识**的子系统,外加一层时间抽象:

| 行 | code 行 | 是什么 |
|---|---|---|
| 1–106 | 83 | 常量 + `Clock` / `SystemClock` / `ManualClock` |
| 107–395 | 258 | **cron**:`SchedulerTimeZone`、`LocalParts`、`CronSpec`(解析 + `next_after` 的字段跳跃)、`parse_field` |
| 396–436 | 37 | `ScheduledKind` / `ScheduledJob` / `WakeupResult` 三个数据类型 |
| 437–712 | 249 | **持久化**:`StoreFile`、三平台的 `lock_store_file` / `unlock_store_file` / `sync_store_directory`、`DurableStore`、`private_open`、`reject_symlink`、`ensure_private_dir` |
| 713–1302 | 560 | **调度本体**:`Horizon`、`SchedulerState`、`Scheduler`、`advance_after_fire`、`next_cron_fire`、`id_fraction` |

cron 解析器不需要知道有没有磁盘;文件锁不需要知道 cron;两者都不需要知道 `Scheduler`。
`DurableStore` 那一段还是**第三次**出现的平台三胞胎(和 plan 178/179 同一个形状),
而且和 `cli/src/private_store.rs` 干的是同一件事(0600 私有文件 + 锁 + 目录 durable)。

## 二、切法

| 新文件 | 内容 | 预估 |
|---|---|---|
| `scheduler/cron.rs` | 107–395:`SchedulerTimeZone` / `LocalParts` / `CronSpec` / `parse_field` | ≈258 |
| `scheduler/store.rs` | 437–712:`StoreFile` / `DurableStore` / 三平台锁与目录同步 | ≈249 |
| `scheduler.rs`(留) | `Clock` 家族、三个数据类型、`Scheduler` 本体 | ≈680 |

**`CronSpec::next_after` 的 DST 折半(plan 141 的产物)整块跟着 cron 走**,
它是这个子系统里最难的一段,单独成文件之后才有机会被单独读懂。

## 三、坑

- **不要顺手把 `scheduler/store.rs` 和 `cli/src/private_store.rs` 合并。**
  它们像,但一个在 core、一个在 cli,core 不依赖 cli;真要合并得先决定这层住哪个 crate,
  那是另一条 plan。这次只在本文件记一笔"两处同形"。
- `#[cfg(test)] mod tests` 在 1303,**1864 总行里有 561 行是测试**。cron 的测试
  (字段解析、`next_after` 的跨月/DST)跟着 `cron.rs` 走,`DurableStore` 的锁测试跟着
  `store.rs` 走,调度的端到端留在 `scheduler.rs`。**这是本条工作量的大头**,别低估。
- `MAX_SCAN_MINUTES` / `RECURRING_*` 那批常量要分家:cron 用的跟 cron 走,
  调度策略用的留下。分不清的宁可留在 `scheduler.rs` 再 `pub(super)` 给子模块。
- plan 141 把轮询从 1s 改到 15s、把 cron 扫描改成字段跳跃。**重构不能把这两个决定改回去**,
  `STORE_POLL_MS` 与 `next_after` 的实现原样搬。

## 四、验收

- `make check` 全绿;scheduler 的 561 行测试**一条不少**,只是换了文件。
- `scheduler.rs` 降到 ≈680。

## 五、✅ 完成（2026-10-14）

实现提交：本节所在提交（`refactor(plan181): separate cron parsing and durable scheduler storage`）。
提交号可用 `git log -1 --format=%h --grep='^refactor(plan181):'` 查询。

开工时文件已增至 **1295 生产 code 行 / 2246 总行，21 条测试**，包含 plan 199
加入的 check 执行、跳过重叠触发与 shutdown 释放逻辑。按职责拆分，保留这些新增
行为；父模块不为追到旧估算的 ≈680 行再拆调度本体。

| 文件 | 职责 | 生产 code 行 / 总行 | 原有测试 |
|---|---|---|---|
| `scheduler.rs` | 时钟、任务数据类型、调度策略、check 生命周期与端到端测试 | 781 / 1490 | 15 |
| `scheduler/cron.rs` | 五字段解析、时区、字段跳跃与 DST offset 折半 | 264 / 445 | 5 |
| `scheduler/store.rs` | 私有文件、三平台锁、存储校验与原子持久化 | 264 / 342 | 1 |
| `scheduler/testutil.rs` | UTC 时间构造与测试临时目录序号 | 仅测试 / 12 | 共用夹具 |

- 两个实现子模块保持私有，`CronSpec`、`SchedulerTimeZone`、`DurableStore` 从父模块
  重导出，公开声明与调用方路径不变。
- `LocalParts`、时区转换方法及调度器使用的 minute/hour 字段只提至 `pub(super)`；
  `DurableStore` 的 load 与两个事务方法同样只供父模块使用，其余实现保持私有。
- `MINUTE_MS` 仍由父模块提供；扫描上限跟 cron 走，存储版本、字节上限和生产
  临时文件序号跟 store 走。测试使用独立的共用序号，不为夹具公开生产内部常量。
- 原计划的“互相不需要认识”只适用于职责划分：store 加载时本来就用 cron 解析器
  校验持久任务，原样保留这条依赖；不抽象新契约，不与 CLI 私有存储合并。
- 5 条纯 cron 测试和 1 条存储测试随实现搬走；不可能日历表达式的 create 拒绝、
  durable check 的创建与回读、跨 runtime 的 owner 分区与单次 claim 仍测调度器，
  留在父模块。21 条测试的名称和属性、98 个函数的正文逐一核对一致；两块搬出
  的生产实现除必要可见性外与原文一致。

在外层执行沙箱之外运行完整 `make check`，fmt、clippy、workspace debug 全量测试
与本机 parity 全绿；core 1124 条测试通过，含本次全部 21 条 scheduler 测试。
本机语料未修改。提交前同时核对完整差异并运行 `git diff --check` 与
`git diff --cached --check`。设计落点已同步 DESIGN，交接见 HANDOFF 教训 235。

Windows 验证使用独立临时 workspace 与 target 目录，真实源码不作 Rust stub：

```sh
cargo +1.98.1 check --offline --locked -p kloop-core \
  --target x86_64-pc-windows-msvc --all-targets --all-features
```

检查通过。沿用教训 201，仅为 Windows triple 的 C 编译与归档生成空产物；该检查
覆盖 Rust 类型、名字解析和测试目标，不代表 C 代码、链接或 Windows 上执行通过。
编译器仍报告已有 `TestConfig::login_env` dead_code 告警，定义所在文件未改动。
