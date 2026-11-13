# Plan 259 — server 输入、worker 与事件投影

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：✅ 已完成（2026-11-13）。行号是 2026-10-28 的读数，保留为当时的依据。
> `thread/fork` 的孤儿文件在 plan 249。

## 问题

1. **worker 的 inbox 唤醒信号会被吞，steer 卡住**(已复核)。`server/src/worker.rs:55-65`:
   activity 分支在 `running` 为真时 `continue`,watch 已标为已读。`turn/start` 与
   `thread/provider/switch` 的处理方在把消息送进 `turns` 之前就 CAS 了 running=true;这期间来的
   inbox 动静被吞掉。若接下来处理的是 `SwitchProvider`(`:79-134`)或 `/provider` 命令(`:139-156`),
   结束时只 `running.store(false)` 再 `continue`,没人回头查 inbox,steer、后台任务或 cron 的投递
   一直留着，直到下一次新动静。普通 turn 不受影响(agent 循环会排空 inbox)。
2. **一行非 UTF-8 输入让整个多会话 server 退出**(已复核)。`server/src/lib.rs:129`
   `while let Some(line) = lines.next_line().await?`:`InvalidData` 让 `serve` 立即返回 Err,
   取消 turn、`pending.clear()`、`writer.await`、各线程的后台收尾与 worktree finish 全部跳过，
   客户端收不到 PARSE_ERROR。`lines()` 对单行长度也没有上限。
3. **`/provider` 走的 turn 在快照里永远 in_progress**(待复核)。`worker.rs:139-156` +
   `methods/turn.rs:43-45`:`turn_start` 先 `projection.record_input(Some(N))` 建出 turn N,
   provider 分支按设计(DESIGN 约 1856 行)不发 turn/started/completed,但也没 `discard_turn(N)`;
   `thread/events/sync` 的 `tail.turns` 一直挂着它，重连客户端显示一个永不结束的 turn。
4. **每个 delta 复制整段累积文本，每个事件序列化两次**(优化)。`server/src/events.rs:684`
   `item[field] = Value::String(format!("{current}{text}"))`,100KB reasoning 按 20 字符一个 delta
   约复制 250MB,在 projection 锁内;`:398` `publish` 先 `to_vec` 算大小、`params.clone()` 进 ring,
   write_loop 再 `to_vec` 一次。
5. **所有方法在唯一的读循环里同步做文件 IO**(优化)。`lib.rs:133`/`274` + `methods/threads.rs:256-304`:
   `handle_line` 是同步函数;`thread/list {limit:500}` 逐个 `load_session_snapshot` 全量反序列化，
   只为计数和取摘要，这期间审批回复与 `turn/interrupt` 排队。`locate_thread` 的 bucket 扫描、
   config/skills reader 同样。
6. **小重复与不一致**(重复)。"从持久化 route 记录 + `default_effort` 构造 `ActiveProviderRoute`"
   在 `methods/query.rs:200-207`、`threads.rs:285-292`、`320-328` 逐字三份;`SwitchError` → kind 在
   `query.rs:85-95` 与 `params.rs:82-97` 两份;`model` 参数解析在 `params.rs:250-257`(trim)与
   `query.rs:54-67`(不 trim)两份，`" m "` 在 provider/switch 得到 unknown_model。
7. **out 通道无界**(观察)。`lib.rs:102`:客户端不读 stdout 时内存持续增长。

## 开工先问用户

- 第 7 条要不要改成有界通道?有界后 worker 在客户端不读时会被背压挂住(取消也可能卡住)。
  推荐先不改，只在 DESIGN 写明;或设一个很大的上限、超出时断开该客户端。

## 做法

1. 非模型操作(switch、provider 命令、不调 `run_turn` 的 slash 命令)结束后，若
   `!inbox.is_empty()` 就 `inbox_activity.mark_changed()`(或在 activity 分支因忙而跳过时也补一次)。
2. 按字节读行并设上限(照 mcp 的 `read_bounded_line`),坏行或超长行回 `PARSE_ERROR(id:null)` 并
   继续;只有真正的 IO 错误才 break,并走正常收尾路径。
3. provider 分支补 `ui.projection.discard_turn(turn.id)`;更好的是在 `turn_start` 里就识别
   `/provider`,不分配 turn、不打开 steer 窗口(开工时选一种并记下)。
4. 拿到 `&mut Value::String(s)` 后 `push_str`;publish 只序列化一次，结果既用于算大小也直接交给
   writer(通道改为预序列化字节)。
5. 列表只读头部或元数据(复用 `session_digest`);重的读方法放进 `spawn_blocking`,结果经 out
   通道回送(JSON-RPC 允许乱序响应)。
6. 抽 `public_route_from_record`;`switch_error_kind` 拆出只处理 `SwitchError` 的函数供两处用;
   `model` 解析共用 `optional_nonempty_str(params, key)` 并统一 trim。

## 验收

- 集成(`server/tests/server.rs`,旧代码先红):switch 与 steer 连发后 steer 被投递;一行非 UTF-8
  得到 PARSE_ERROR 且后续请求正常;超长行被拒;`/provider` 之后的 events/sync 没有 in_progress turn;
  `thread/list` 期间 `turn/interrupt` 不被阻塞(用大量会话夹具);provider/switch 的 model 被 trim。
- 外层执行沙箱之外 `make check`;DESIGN 的 server 一节先读再改;HANDOFF 记进展与教训。

## 实施结果（2026-11-13）

七条开工复核全部成立。第 7 条按推荐不改：out 通道保持无界，只在 DESIGN
写明「客户端不读 stdout 时内存增长，不做背压」。

1. worker 在每次把 `running` 放回 false 后（普通 turn、switch、`/provider`
   命令）检查 inbox，非空就 `notify_activity()` 补一次唤醒；普通 turn 结束
   同样补，覆盖 last drain 之后到达的 steer。
2. serve 改成按字节读行并设 8 MiB 上限（照 mcp 的 `read_bounded_line`）：
   超长行排到换行后回 `PARSE_ERROR(id:null)`，非 UTF-8 同样处理并继续；
   只有真正的 IO 错误才结束循环，且仍走取消 turn、清 pending、等 writer 的
   正常收尾。
3. 选「在 `turn_start` 里识别」：新增 `ThreadWorkerMsg::ProviderCommand`，
   `/provider` 不再 `record_input`、不开 steer 窗口、不进入事件投影；响应仍带
   一个只用于应答的 turn id，任何事件都不使用它。
4. `item/delta` 直接对 `Value::String` `push_str`；out 通道改成预序列化字节，
   writer 只补换行，事件 ring 的留存大小与实际写出的字节一致，`publish` 不再
   克隆 params 做第二次序列化。
5. `session_digest` 扩出 messages / runtime / 最后一个 route，`thread/list`
   改用它；`thread/list`、`thread/read`、`config/read`、`skills/list` 都移到
   `spawn_blocking`，响应可以乱序。配置/技能读取前的 scope 解析也带进阻塞任务，
   包括休眠会话的 path 定位与重放。
6. 抽 `public_route_from_record`、`route_switch_error_kind`、
   `optional_nonempty_str`；`model`/`provider_id` 统一 trim，
   `" m "` 在 provider/switch 不再报 unknown_model。

回归：server 集成 49 项（含 `/provider` 无 in_progress turn、`/provider` 期间
steer 仍投递、非 UTF-8/超长行后继续服务、`thread/list` 期间 interrupt 先回、
`" m "` 被 trim），server lib 27 项，core lib 1429 项（1 ignored），CLI bin
167 项通过；`clippy -p kloop-server -p kloop-core -p kloop --all-targets -D warnings`
通过；未跑全量 `make check`、parity 和真实 API。
