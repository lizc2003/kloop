# Plan 77 — Native event sequence/cursor/snapshot projection

> 状态：✅ 已完成（2026-08-11）
>
> 基线：`1ab5d7e`（Plan 76）
>
> 依赖：Plan 39、Plan 74、Plan 76

## Context

Prime Agent 调研暴露了 kloop 当前原生协议的一个真实缺口：`ThreadUi::notify` 能把 core `Event` 投影成 per-thread JSON-RPC notification，但客户端只能按到达顺序消费；notification 没有序号，断流或 adapter 丢事件后无法检测 gap，也没有一个与 live 流原子衔接的 full snapshot。现有 `thread/read` 只读取 rollout 的 messages/runtime/terminals，`thread/list.cursor` 只是列表 offset；两者都不是事件恢复协议。

Plan 77 只实现 native app-server 的 generation/sequence/cursor/snapshot projection，并同步专用 Desktop `kloop` 分支的消费端。Usage ledger 留给后续独立 plan；Task Graph 继续是 root-owned、TUI-only 的内部 full snapshot，绝不借本计划公开化。

采用 **generation-scoped、server-memory projection + bounded replay ring + rollout-seeded snapshot**，而不是新增 durable public-event journal：同一 active thread generation 内 `seq` 跨 turn 单调；server/thread resume 后 generation 更新，旧 cursor 自动落到 snapshot。这样既能恢复客户端显示，又不会复制 rollout 真值、把 display event 混入 provider history，或承诺跨崩溃重放 tool/approval/side effect。

开工基线以 Plan 76 完成后的 HEAD 为准；实施前先读最终 `docs/plan/76-*.md` 并复核其是否触及 server/wire/app adapter，解决冲突后再落 Plan 77。

## 产品与协议契约

### 1. 直接收敛 native protocol 1.0 当前契约

修改 `rust/crates/server/src/lib.rs` 的 initialize 与通知契约：

- 按当前项目没有外部协议用户的既有纪律，直接更新 native protocol `"1.0"` 的唯一实现；不保留旧 notification shape、双向 opt-in、兼容 adapter、fallback 或双栈。kloop 与 Desktop 专用 `kloop` 分支必须同步升级。
- server 固定广告并要求 `events:{sequence:true,sync:true,snapshot:true}`；Desktop initialize 对缺失或错误 capability hard-fail。该 capability 是当前协议的必备能力声明，不是兼容开关。
- 所有经 `ThreadUi::notify` 发出的 public notifications 必须增加：
  - `eventGeneration`：当前 active thread projection 的 opaque string；
  - `seq`：十进制字符串，generation 内从 `"1"` 起、跨 turn 不复位。
- `seq` 用字符串，避免 JavaScript safe-integer 边界；内部仍用 checked `u64`，溢出时终止该 projection，不能 wrap/reuse。
- sequence 覆盖 turn bracket、item lifecycle、background/mailbox/scheduler、token usage、cwd、note/system、`thread/cleared`。JSON-RPC response/error 与 `approval/request`、`question/request` reverse request 不参与 sequence；现有 `srv_seq` 改名为 `reverse_request_seq`，继续只分配 reverse request id。
- `wire::project_event` 继续只负责 `Event -> (method, params)` 静态投影；server 层注入 thread/generation/seq。headless `--json` 不获得 cursor/snapshot 语义，也不被迫增加 seq。

### 2. 一个原子恢复入口：`thread/events/sync`

新增必备 RPC：

```jsonc
thread/events/sync {
  "threadId": "...",
  "eventCursor": {
    "threadId": "...",
    "generation": "...",
    "seq": "42"
  } // optional
}
```

cursor 是 typed consistency token，不是认证凭据，也与 `thread/list.cursor` 的 string offset 完全不同。参数 strict parse：坏类型、空值、foreign thread、非十进制/溢出 seq、同 generation 的 future seq 都返回 `INVALID_PARAMS`。

返回两种互斥结果：

```jsonc
// cursor 仍在 bounded ring 内
{
  "mode": "replay",
  "generation": "g",
  "highWaterSeq": "57",
  "events": [{"method":"item/completed","params":{...}}],
  "eventCursor": {"threadId":"...","generation":"g","seq":"57"}
}

// 无 cursor、generation 已更换或 cursor 已被 retention 淘汰
{
  "mode": "snapshot",
  "reason": "initial" | "generationChanged" | "cursorExpired",
  "generation": "g",
  "highWaterSeq": "57",
  "snapshot": {...},
  "eventCursor": {"threadId":"...","generation":"g","seq":"57"}
}
```

- sync 只作用于 active thread；dormant session 先走现有 `thread/resume`，再 sync。
- cursor 恰好位于 high-water 时 replay 为空，属于正常幂等调用。
- replay 只返回 `(cursor.seq, captured high-water]` 的连续 public events；事件 envelope 复用 live method/params，可进入同一 ingest path。
- 不另增 `events/read`、`events/replay`、`events/snapshot` 三套重叠方法；一个 sync 入口统一 gap recovery、resume 和 initial attach。

### 3. Client-side 原子 handoff

`ThreadProjection` 的同一 mutex 串行执行 per-thread seq 分配、projection reducer、ring append和 outbound notification enqueue。sync 在该 mutex 下捕获 snapshot/replay 与 high-water，然后释放；不要求 response 在 writer queue 中抢在新 notification 前面。

Desktop 必须在 sync request pending 时缓存该 thread 的 sequenced notifications：

1. 安装 snapshot 时 full replace native thread view，设置 `(generation, highWaterSeq)`；replay 则从已有 state 继续。
2. 丢弃相同 generation 下 `seq <= highWaterSeq` 的重复事件。
3. 将缓存事件按 seq 应用，只接受严格连续的下一项。
4. 发现 gap 时暂停后续渲染并再次 sync；cursor expired 自动获得 snapshot。
5. generation 改变时只接受 snapshot full replace，不能把新 generation 与旧 tail 拼接。
6. 未知但合法 sequenced notification 可以继续由业务 adapter 忽略，但 sequencer 仍须推进 seq，避免未来新增 method 制造假 gap。
7. 任一 public notification 缺失或携带错误的 `eventGeneration` / `seq` 都必须 fail closed；不存在 legacy direct-ingest 降级路径。

这样无论 snapshot/replay response 与 `H+1` notification 谁先到，客户端都能无 gap、无重复地衔接。

### 4. Snapshot 是只读 public projection，不是第二执行真值

新增 snapshot schema v1，server 在 thread spawn 时用现有 `SessionSnapshot` seed 持久部分，之后只从 accepted input 与 public notification reducer 更新 volatile tail：

```jsonc
{
  "schemaVersion": 1,
  "thread": {"id":"...","cwd":"...","model":"...","resumable":true},
  "history": {"messages":[],"terminals":[]},
  "tail": {
    "turns": [{
      "id": 3,
      "status": "inProgress" | "completed" | "maxRounds" | "aborted" | "error",
      "input": [],
      "items": [],
      "error": null
    }],
    "notices": [{"turnId":3,"kind":"note"|"system","text":"..."}],
    "backgroundTasks": [],
    "agentMessages": [],
    "scheduledTasks": [],
    "tokenUsage": null,
    "cwd": {"path":"...","branch":null}
  },
  "recovery": {"source":"fresh"|"resumed","volatileState":"live"|"reset"}
}
```

具体 DTO 名可按现有 Rust 风格微调，但边界固定：

- `history` 复用 rollout `load_session_snapshot` 的 messages/runtime/terminals，不将 sequence/cursor/event records 写入 rollout，也不进入 provider replay/token accounting。
- `tail` 是本 generation 以来的 materialized display state，不是 event patch：turn/input 在 server 接受 `turn/start`/steer 时记录；item delta 追加到对应 typed item，completion 用完整 item替换；background/message/scheduler 按稳定 id保存最新状态；token usage/cwd保存最新值；notice保序。
- `thread/cleared` 只清 transcript/history/turn/notices projection，不重置 generation/seq，也不停止或清除 session-scoped background/scheduler/mailbox 状态。
- snapshot/replay 中的 tool item、background state 都是只读数据，永远不能重新 dispatch tool、恢复 process、重发 mailbox body或触发 side effect。
- restart/resume 创建新 generation：snapshot 从现有 rollout 恢复可持久 messages/runtime/terminals，volatile maps为空并标 `volatileState:"reset"`；旧 cursor返回 `generationChanged` snapshot。不伪装恢复 running turn、open tool、pending approval/question、process/kernel、scheduler delivery或 usage ledger。
- turn/item identity仍遵守现有 wire contract。Desktop 内部 key 增加 generation scope（例如 `<generation>:<turnId>:<itemId>`），解决 resume 后当前 `turn_seq` 从 1 重建时与旧历史碰撞；本计划不为此污染 rollout或改变既有 numeric turn id。
- `Event::TaskGraphUpdated` 继续在 `wire::project_event` 返回 `None`；snapshot schema、ring、capability和 Desktop adapter 都不出现 Task、taskId、owner或 graph revision。

### 5. Retention 与资源边界

在 `rust/crates/server/src/events.rs` 使用 `VecDeque<PublicEventEnvelope>`：

- 每个 active thread 最多保留 4,096 条或 16 MiB encoded event tail，先达到哪个阈值就从最旧端淘汰。
- ring 淘汰只影响 replay；materialized snapshot state仍完整。
- event envelope/单字段继续服从既有 wire/tool-output边界；snapshot不引入 raw provider response、credentials、approval payload缓存或 message body之外的新敏感数据。
- generation、ring和 materialized projection均为进程内状态，不新增 `.kloop/events` sidecar，不做 per-delta fsync，也不宣称 crash-safe/exactly-once public event replay。跨进程恢复明确走 rollout-seeded snapshot。

## 实施步骤

### 1. 建立 projection 模块

- 新增 `rust/crates/server/src/events.rs`：
  - `EventCursor` strict serde DTO；
  - `PublicEventEnvelope`、`ProjectionSync`、snapshot v1 DTO；
  - `ThreadProjection` mutex state、checked seq allocator、materializing reducer；
  - count/byte bounded ring和 sync decision；
  - fresh/resumed seed与 generation-scoped cursor validation。
- 单测 reducer 不从 tool output文本猜状态；unknown method只作为 sequenced envelope保留，不扩张 snapshot schema。

### 2. 接入 server 的唯一 public notification 出口

修改 `rust/crates/server/src/lib.rs`、`wire.rs`：

- `Server` 固定启用 event projection；initialize 广告必备 events capability，不维护 client opt-in 状态。
- `ThreadHandle`/`ThreadUi` 持有同一个 `Arc<ThreadProjection>`；`srv_seq` 重命名，明确 reverse request和event seq所有权分离。
- `ThreadUi::notify` 无条件完成 thread id注入、projection apply、ring append和带 seq通知入队；删除旧 params发送分支。
- `thread/start|resume|fork` 给 `spawn_thread` 传精确 seed：fresh为空；resume用已加载 snapshot；fork重新读取新 fork的 snapshot，不能误用 source snapshot。
- `turn/start` 成功 admission后记录 accepted user input；scheduler delivery、slash `system`/`thread/cleared`、config note仍全部汇入同一 projection出口。
- 增加 `thread/events/sync` dispatch和 strict参数验证；dormant/foreign thread、future cursor均 fail closed。
- 不改 `kloop-core` Event、History provider语义或 rollout line格式；不增加 durable event record。

### 3. Desktop `kloop` 专用分支适配

只在 `~/work/桌面前端仓库` 的专用 `kloop` worktree/branch工作，根 app `main` 与 `codex` gitlink不动；app改动独立提交并把 SHA记入 Plan 77。

- `src-tauri/src/worker.rs`：initialize硬校验 server events capability、提供 `thread/events/sync` RPC；缺失能力时拒绝启动 native session，reverse request流保持独立。
- `src/kloop/dto.ts`、`capabilities.ts`：加入 string seq、generation、typed cursor、sync union和snapshot v1，严格验证 advertised-new路径。
- 在 `chatIngest.ts` 前增加 per-thread sequencer/sync coordinator；business ingest仍处理现有 method。unknown sequenced event推进cursor后再忽略。
- snapshot走 dedicated full-replace reducer，复用现有 thread/read/history normalizer处理 `history`，再安装 materialized tail；不能把 snapshot伪装成一串 synthetic live events。
- thread start/resume/fork 后先进入 buffering/sync，再开放 live render；gap→sync，expired/generationChanged→snapshot。不保留无 events capability 的旧 server行为。
- generation进入 app内部 item scope，protocol原有 `<turnId>:<itemId>` identity语义不变。

### 4. 文档同步

- 更新 `rust/README.md` native protocol：协商规则、notification sequence、typed eventCursor、sync replay/snapshot、retention、restart generation、client handoff和非恢复边界。
- 更新 `docs/plan/HANDOFF.md`：区分 rollout/provider history、public display projection和execution state；记录“generation + snapshot基线优先于伪造durable replay”的教训。
- 在 `refs/README.md` 追加 Prime Agent固定 SHA `e9ef5777409001faf91382227b12bf09496078fa` 的相关取舍：借 daemon sequence/cursor/snapshot，不照搬无沙箱执行或弱持久化；不要求本计划克隆/vendoring该仓库。
- `docs/plan/39-native-protocol.md` 只追加当前演进指针，不重写历史完成事实。仅在 capability report存在已失真的活跃 claim时做最小同步；不改 fixed parity fixture来伪造外部协议事实。

## 非目标

- Usage ledger、provider cost、own/total attribution、quota或billing。
- Task Graph public wire、snapshot字段或Desktop面板。
- Durable public event journal、跨进程增量 replay、exactly-once side effect、pending approval/process checkpoint。
- 持久 turn-id watermark；跨 generation身份由 generation scope和full replace解决。
- HTTP/SSE、旧 Codex/Codex wire、protocol 2.0双栈。
- 可修改 History 的hook、持久IPython/kernel或第二套parent/execution registry。

## 关键文件

- `docs/plan/77-native-event-sequence-cursor-snapshot.md`
- `rust/crates/server/src/events.rs`（新增）
- `rust/crates/server/src/lib.rs`
- `rust/crates/server/src/wire.rs`
- `rust/crates/server/tests/server.rs`
- `rust/README.md`
- `docs/plan/HANDOFF.md`
- `refs/README.md`
- Desktop 专用分支：`src-tauri/src/worker.rs`、`src/kloop/{dto,capabilities,chatIngest,normalize}.ts`及相应 tests

## 验证

### Engine 定向契约

- protocol仍只有一个 exact `1.0` 当前契约；server固定广告 events capability，Desktop硬校验。每条 public notification都有同 generation、严格递增 string seq，不存在旧 params或compat测试分支。
- turn/item/background/mailbox/scheduler/usage/cwd/note/system/clear全覆盖；reverse requests无 seq且使用独立id。
- 同 thread跨多 turn不复位；多 thread各自独立。u64溢出fail closed。
- cursor strict负向：malformed、foreign thread、bad/future/overflow seq；current high-water空replay；bounded gap精确连续replay；retention过期与旧 generation返回snapshot。
- snapshot reducer覆盖 accepted input、delta materialization、completion replacement、notice顺序、keyed session state、`thread/cleared`边界；snapshot安装后buffered live event无gap/duplicate。
- resume/server restart建立新 generation并从 rollout seed history；旧cursor得到 generationChanged snapshot；不恢复running tool/approval/background side effect。
- `TaskGraphUpdated`继续无wire、ring、snapshot；token usage只有现有total，不出现ledger字段。
- `thread/list.cursor`与typed `eventCursor`无法混用；`thread/read`既有shape不变。
- `load_session`/provider messages/rollout JSONL不出现generation、seq、cursor或public event envelope。
- 复用 `crates/server/tests/server.rs` 现有 duplex harness和event-order测试，并给 `events.rs` 增加纯 reducer/ring单测及并发 barrier handoff测试。

### Desktop

- 缺失 events capability、seq或generation时adapter hard-fail；正常路径在sync完成前buffer，不测试或保留旧 server fallback。
- initial snapshot→live、same-generation replay→live、duplicate drop、gap→sync、expired/old generation→full replace。
- snapshot/full replace不重复 assistant/tool cards；unknown sequenced method推进cursor但不渲染；malformed advertised-new payload fail closed。
- resume后 generation-scoped item key不与旧 turn/item冲突；TaskGraph仍不可见。
- 全量 `bun test`、`bun run ts:check`、`bun run build:test`，Tauri Rust fmt/clippy/tests；使用 `ENGINE_BIN` 指向本地 kloop跑真实 native reconnect E2E，至少覆盖两轮→中断/重启 app-server→resume/snapshot→新一轮，无丢失、重复或side-effect重放。

### kloop 总质量门

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test --workspace`
- server focused suite
- `cargo run -p kloop -- app-server --mock`
- `git diff --check`
- 真 provider/Tauri验收只记录脱敏 lifecycle和判据，不保存key、endpoint、Authorization、raw response或完整transcript。

完成时在 Plan 77 补 `✅`、kloop commit、Desktop companion commit和全部验证结果；kloop行为/docs一次提交，Desktop仍独立提交。

## 完成记录（2026-08-11）

已按唯一当前 protocol 1.0 契约完成；没有保留旧 notification shape、events opt-in、fallback、compatibility adapter 或双栈。

- kloop server：新增 `events.rs`，落地 generation、checked string sequence、typed cursor、4,096 条 / 16 MiB bounded ring、materialized snapshot 和 `thread/events/sync`；`ThreadUi::notify` 成为唯一 sequenced public notification 出口，reverse request ID 独立。
- snapshot：fresh/resumed 从各自 rollout seed，accepted input与 public event reducer更新当前 generation tail；mailbox等 keyed state只保留语义字段，不保留 envelope metadata；resume/restart只恢复 persisted history并明确 reset volatile state。
- Desktop：companion commit `e23322b4`；initialize hard-require三项 events capability，Rust reader拒绝 malformed sequence，TypeScript coordinator主动 attach、buffer、连续 replay、gap recovery、duplicate drop和 generation full replace；draft先迁移到 canonical thread ID，item key带 generation scope。
- Task Graph仍只在TUI内部；Usage Ledger、durable event journal、pending approval/process checkpoint和side-effect replay均未加入。
- kloop提交：以本完成记录所在提交为准。

验证结果：

- `cargo fmt --all --check`：通过。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：通过。
- `cargo test --workspace`：通过。
- `cargo test -p kloop-server --lib`：24 passed。
- `cargo test -p kloop-server --test server`：29 passed。
- `cargo run -p kloop -- app-server --mock </dev/null`：通过。
- real-binary native reconnect E2E：`start → turn → restart → resume → generationChanged snapshot → turn`通过；两代各46条连续event，replay与live envelope精确相等，sync前后rollout SHA-256不变。
- Desktop `bun run ts:check`：通过。
- Desktop `bun test`：893 passed / 52 skipped / 0 failed（945 tests，131 files）。
- Desktop `bun run build:test`：通过。
- Desktop Tauri `cargo fmt --all -- --check`：通过。
- Desktop Tauri `cargo clippy --all-targets --all-features -- -D warnings`：通过（测试时以 `TAURI_CONFIG` 清空bundle sidecar，仅绕开不存在的打包资源，不改变Rust代码路径）。
- Desktop Tauri `cargo test`：230 passed / 0 failed；首次全量运行有1个临时worktree测试因系统临时目录瞬态失败，单测重跑与随后全量重跑均通过。
- Desktop focused event tests：14 passed；snapshot/full replace、unknown method cursor推进、initial attach、gap replay、duplicate、generation change和strict malformed路径均覆盖。
- `git diff --check`：两个仓库均通过。

未伪报GUI手工检查：本次自动验收以真实kloop stdio binary reconnect harness、Desktop Tauri transport tests和TypeScript coordinator tests分层闭合；没有记录key、endpoint、Authorization、raw provider response或完整transcript。

