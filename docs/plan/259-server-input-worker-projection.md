# Plan 259 — server 输入、worker 与事件投影

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：待实现。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
> 先读代码确认，不成立就在本文件划掉并写明原因。`thread/fork` 的孤儿文件在 plan 249。

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
