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
读那条 plan 怎么说。

**查下来没有跨进程需求的话,直接删掉这段,不要留着"以防万一"。** 一段没人需要、也没人
解释得清的定时磁盘 IO,留着只会让下一个读者再查一遍。

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
  正确的形状是把"这一天合不合格"当成**一个**判定(`matches` 已经是了),按**天**跳而不是
  按 dom 跳:日这一层最多 366 次迭代,它上面的月、下面的时/分照跳。这不是折中,是这条
  规则本来的样子——dom 与 dow 的或语义决定了"日"是原子的。
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
  `0 0 13 * 5`),对照用的朴素扫描函数放在 `#[cfg(test)]` 里——它是测试的 oracle,不是
  生产代码的退路,生产路径上**只有**跳跃实现;
- 若做了 mtime 短路:一个测试证明"store 未变 → 不读文件"(可以数
  `DurableStore::load` 的调用,或临时把文件权限设成不可读再断言 worker 不报错);
- fmt / clippy(`-D warnings`) / `cargo test --workspace` 各自单独跑、当场取退出码。

## ✅ 已完成(2026-09-14;提交 SHA 以本条所在提交为准)

只动 `crates/core/src/scheduler.rs` 一个文件与 README 的 Scheduler 段。

### 一、每秒一次磁盘 → 只有真有 durable job 的会话才轮询

`git log -S 'store_poll'` 只有一条提交(9824ab3,plan 58),plan 58 正文里那句
「两个同 owner runtime 并存时,store claim 在跨进程锁内线性化,只投递一次。worker 每秒
重读 durable store」——**前半句是契约,后半句只是当时的实现**。单次投递靠的是 flock +
`transaction_if_changed`,与轮询间隔无关;轮询提供的只是"更早看到对方的 job"。

于是这一节实际做了三件事,而不是 plan 预设的"三选一":

1. **按需轮询。** 原来的条件是 `self.store.is_some()`,而 `startup.rs:934` 对每个非
   `--mock` 启动都构造 store——于是一个从没建过 scheduled job、磁盘上那个 json 根本不
   存在的会话,也整天每秒去看一次。新条件是**本会话确实持有 durable job**
   (`Horizon::durable_jobs`)。没有的会话睡在 `notify_change` 上,**零磁盘 IO**。
2. **间隔 1 秒 → 15 秒**,并且第一次给了它一个写得下来的上界:
   `requires_missed_confirmation` 的 60 秒。durable 一次性 job 迟到 ≥ 60 秒才会被判成
   missed、才要回头问用户确认,所以间隔只要明显小于 60 秒就没有用户可感知的出口
   (cron 的分辨率本来就是分钟)。1 秒不是热路径,是一开始就多了 60 倍。
3. `next_deadline` 换成 `horizon`,一次 `list()` 同时给出"何时醒"和"值不值得轮询"。

**中途推翻的一版。** 先按 plan 建议做了 mtime 短路(`StoreFingerprint` + `StoreCache` +
`STORE_SETTLE`,约 60 行),用户一句「代价这么大」问回来,重算这笔账:短路省下的是
~1.5 秒 CPU/天,换的是 60 行代码加一个需要自己证明的静置窗口。而按需轮询直接把绝大多数
会话的这项开销**降到零**,还净删代码。教训 134 记的是这个,不是那套缓存。

**顺带,plan 第一节里的 mtime 方案本身也是错的**(即便采纳):它写着"同一秒内的两次写会
漏掉一次,但下一次轮询会补上"——不会补上,fingerprint 不变,以后每次轮询都继续短路,
那次写永久丢失。指纹短路的漏检不会自愈,这一条留在教训 134 里。

### 二、逐分钟 → 按字段跳跃

`next_after` 的 `for _ in 0..MAX_SCAN_MINUTES` 换成 `while candidate <= limit`,
`MAX_SCAN_MINUTES` 从"循环次数"变成"时间窗口"(`limit = start + (MAX-1) * MINUTE_MS`,
与原来检查的候选区间逐字节相同)。新增两个私有方法:

- `skip_minutes(parts)` —— 给一个已经 `matches` 失败的候选,返回**可证明无匹配**的分钟数
  (恒 ≥ 1):月或日不合格 → 推到本地次日 00:00;时不合格 → 推到下一个允许小时的 :00
  (没有更大的允许值就推到 24:00);否则只能是分不合格 → 推到同小时下一个允许分钟。
  **月不匹配也按天跳**,不单独跳月:日这一层本来就封顶 366 次迭代,为再省十几次而引入
  年份字段和月长计算不划算。`matches` 里的 dom/dow 或语义拆成 `day_matches`,skip 与
  match 用**同一个**判定,不存在"对 dom 单独跳"的机会。
- `skip(timezone, candidate, parts)` —— 跳跃是按本地日历算的,只在本地时间与 UTC 同步
  前进时成立。`LocalParts` 新增 `offset_seconds`;跳到目标后 offset 若变了,就把步长
  **折半**重试,直到落回同一 offset 或退化成 +1 分钟。这是 Lord Howe(DST 只偏移 30 分钟)
  这种时区的正确性前提,也让 DST 边界附近只多花十几次查询,而不是退化整段扫描。

`Scheduler::create` 里那次"先 `next_after` 校验再 `next_cron_fire`"的重复调用删掉——
`next_cron_fire` 返回 `None` 就是校验失败,错误信息本来就一样。最坏一次 `cron_create`
从三遍降到两遍(recurring 要 nominal + following 算 period,省不掉)。

### 三、验收

- `scheduler` 的 14 条既有测试**一行没改**全绿,含 `timezone_scan_handles_dst_gap_and_fold`
  (gap 跳过、fold 触发两次)、`month_end_and_leap_year_are_scanned`、
  `dom_and_dow_use_cron_or_semantics`;
- 新增 `never_matching_cron_is_rejected_without_walking_the_year`:`0 0 30 2 *` 的
  `create` 在 recurring 两种取值下都 < 100ms 返回,且错误文案逐字不变;
- 新增 `field_skipping_agrees_with_a_minute_by_minute_scan`:朴素逐分钟扫描作为
  `#[cfg(test)]` oracle(生产路径上**没有**这条退路),在 UTC / America\_New\_York(春秋
  两个转换)/ Australia\_Lord\_Howe(30 分钟 DST,春秋两个转换)五个 (时区, 起点) 上,对
  `*/15 * * * *`、`0 9 * * 1-5`、`0 0 * * 0`、`30 3 1 * *`、`0 0 13 * 5` 连续求解并逐次
  比对;再单独比对三个长扫描:`0 0 1 3 *`、`0 0 29 2 *`(闰年)、`0 0 30 2 *`(永不匹配);
- 新增 `only_a_session_holding_durable_jobs_polls_the_store`:没有 durable job 时,
  把 store 写成坏 JSON 再把时钟推 600 秒,inbox 里**一条 `SchedulerFailure` 都没有**
  (worker 根本没醒);建一个 durable job 之后,同样的坏 store 在**一个 `STORE_POLL_MS`
  之内**被读到并报 failure。两个方向都实测过会变红:退回 `self.store.is_some()` 第一段
  红,完全去掉轮询第二段红;
- `cargo fmt --all --check` = 0、`cargo clippy --workspace --all-targets -D warnings` = 0、
  `cargo test --workspace` 全绿,各自单独取的退出码。

### 四、非目标守住了

`parse_field` 一行未动;jitter 三个常量与 `id_fraction` 未动;durable store 的格式、
`transaction_if_changed` 的事务语义、`Clock` / `ManualClock` 全部未动。
