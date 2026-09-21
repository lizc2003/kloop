# Plan 181 — 一个 cron 解析器、一把文件锁、一个调度器,挤在一个文件里

> 本批判据与统一纪律见 `HANDOFF.md` 第〇节。

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
- 两个新文件 ≤800;`scheduler.rs` 降到 ≈680 后跑 `make arch-baseline`,它会从基线里被摘掉。
