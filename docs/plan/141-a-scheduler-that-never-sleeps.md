# Plan 141 — 一个不睡觉的调度器

> 来源:plan 136 的全仓通读,第五节非目标里的一条(三.3)。2026-09-11 用户要求把 136 的
> 非目标排成计划;本 plan 是其中一条,与 138–140、142 同批,可独立开工。**读完即可动手。**
>
> plan 136 把它挂进非目标的理由是"两者都要动调度语义(跳跃式扫描的正确性、轮询间隔与跨
> 进程可见性的权衡),不该混在清账里"。这条 plan 就是去做那两个权衡。

## 一、每秒一次磁盘

`crates/core/src/scheduler.rs:1047` 的 `run_worker`:

```rust
let deadline = if self.store.is_some() {
    let store_poll = now.saturating_add(1_000);
    Some(deadline.map_or(store_poll, |job| job.min(store_poll)))
} else {
    deadline
};
```

有 durable store 时,worker 每秒醒一次。每次醒来会走 `claim_due` → `store.transaction_if_changed`
→ `with_lock`:`ensure_private_dir` + `reject_symlink` + `private_open` + **flock** +
`read_to_end` + `serde_json::from_slice` + **对每个 job 重新 `CronSpec::parse`**(见
`read_locked` 的校验循环)。一个开着的会话整天如此,哪怕一个任务都没有。

**这段代码没有注释说明这 1 秒从哪来。** 合理推测是"感知其它进程对同一个 store 的写入"
(durable job 按 project 分区,另一个 kloop 进程可能在同一分区里 create/delete)。开工第一步
是**确认这个推测**:`git log -S 'store_poll' ` 找到引入它的提交与 plan(应在 plan 58 附近),
读那条 plan 怎么说。**如果它其实没有跨进程需求,直接删掉这段是最好的结果。**

若跨进程可见性确实要保:三条路,按代价排序——

1. **按 mtime 短路**:醒来先 `symlink_metadata(path).modified()`,与上次读到的 mtime 相同就
   不开锁、不解析。代价是一次 stat,省掉 flock+read+parse。轮询间隔可以维持 1 秒。
   **风险**:mtime 粒度在某些文件系统上是秒级,同一秒内的两次写会漏掉一次——但下一次
   轮询会补上,而"补上"对调度语义无害(任务只是晚一秒被认领)。
2. **拉长间隔**到 5–15 秒,并在 `create`/`delete`/`schedule_wakeup` 之后 `notify_change()`
   (进程内已经这么做了),只让**跨进程**的变更承担这个延迟。
3. 两者都做。

## 二、一次 `cron_create` 最多扫 156 万分钟

`CronSpec::next_after`(`:189`)逐分钟前进,上限 `MAX_SCAN_MINUTES = 527_040`(约 366 天),
每一分钟做一次 `timezone.local_parts(candidate)`——chrono / chrono-tz 的时区换算。

一个**合法但永不匹配**的表达式(`0 0 30 2 *`,2 月 30 日)会把 52 万次换算全部跑完才返回
`None`。而调用它的是:

- `Scheduler::create` 先调一次做"这个表达式有没有下一次"的校验;
- 紧接着 `next_cron_fire` 再调一次拿 nominal;
- recurring 时 `next_cron_fire` **内部还要再调一次**拿 following(算 period 做 jitter)。

**最坏一次 `cron_create` 三遍 = 约 156 万次时区换算**,而这是模型输入直接驱动的同步路径。

**做法**:把逐分钟改成**按字段跳跃**。标准做法(cron 实现的通用形态):从候选时间起,
依次检查 月 → 日 → 时 → 分,**任一不匹配就把该字段推进到下一个允许值、并把更低的字段
归零**,而不是 +1 分钟。这样 `0 0 30 2 *` 在几十次迭代内就穷尽了一年。

**必须保住的语义**(现有测试锁着,改完要一条不落地绿):

- `matches` 的 dom/dow 规则是 Vixie cron 的:两者都是 `*` → 真;只有一个是 `*` → 用另一个;
  **都不是 `*` → 取或**(`dom || dow`)。跳跃实现最容易在这里错——不能对 dom 单独跳。
  安全的折中:**只在"日/周至少一个是 `*`"时跳日**,两者都受限时对日回退到逐日扫描
  (一年 366 次,仍比 52 万次好四个数量级)。
- 时区:`local_parts` 可能对某个瞬间返回 `None`(DST 折叠/跳跃),现在的代码用 `.ok()?`
  **直接放弃整个查找**。跳跃版要保持同样的保守性,别把 DST 边界变成无限循环。
- `MAX_SCAN_MINUTES` 的"一年上限"语义要保留(错误信息里写着"does not match any calendar
  date in the next year"),只是它不再是循环次数,而是时间窗口。

**另外**:`create` 里那次"先校验再 `next_cron_fire`"的重复调用可以直接去掉——
`next_cron_fire` 返回 `None` 就是校验失败,错误信息已经一样。

## 三、非目标

- 不改 cron 表达式的**语法**(`parse_field` 一行不动)。
- 不改 jitter 的算法(`RECURRING_JITTER_*` / `ONE_SHOT_JITTER_MAX_MS` / `id_fraction`)。
- 不动 durable store 的**格式**或 claim 的事务语义(那是 plan 58 的地基,教训 63)。
- 不动 `ManualClock` / `Clock` trait——测试靠它,改了就没法确定性地验。

## 四、开工时问用户

第一节的三条路选哪条(mtime 短路 / 拉长间隔 / 两者)。**建议**:先按 `git log -S` 查清 1 秒
的来历;如果确有跨进程需求,做 mtime 短路(语义不变、只省 IO),间隔维持 1 秒——拉长间隔
是能被用户感知的行为变化,而短路不是。

## 五、验收

- `scheduler` 的既有测试**一条不改**地全绿(它们用 `ManualClock`,对时间推进很敏感);
- 新增:`0 0 30 2 *` 这类永不匹配的表达式,`create` 在**毫秒级**返回错误(用
  `std::time::Instant` 断言上界,例如 < 100ms——不是精确计时,是数量级);
- 新增:跳跃实现与逐分钟实现在一组代表性表达式上**结果相同**(`*/15 * * * *`、
  `0 9 * * 1-5`、`30 3 1 * *`、`0 0 * * 0`、`0 0 29 2 *`(闰年)、dom+dow 都受限的
  `0 0 13 * 5`),用一个保留下来的朴素扫描函数做对照——这是唯一能证明跳跃没跳错的办法;
- 若做了 mtime 短路:一个测试证明"store 未变 → 不读文件"(可以数
  `DurableStore::load` 的调用,或临时把文件权限设成不可读再断言 worker 不报错);
- fmt / clippy(`-D warnings`) / `cargo test --workspace` 各自单独跑、当场取退出码。
