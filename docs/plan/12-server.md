# Plan 12 — server 模式(多会话 JSON-RPC)✅(3bedbd1;真 key 验收已过)

> 一个会话完成。开工前先读 docs/plan/HANDOFF.md。参考:codex `app-server` 一族(调研结论:上游 proto 已删,一切前端收敛到 app-server 的类 JSON-RPC;kloop 用户拍板要多会话形态,不要单会话 JSONL)。

## 目标

`kloop --serve`:stdio 上的类 JSON-RPC 服务,多会话并行——每个 thread 一份 History + rollout + 权限缓存,turn 事件按 thread 标记流出,审批走 server→client 请求往返。给未来 IDE/自动化客户端一个可编程入口。

## 形态(仿 codex,做最小闭环)

- **信封**(仿 `app-server-protocol/rpc.rs`:类 JSON-RPC 2.0 但不带 `"jsonrpc"` 字段):
  - client→server 请求 `{id, method, params}`;server 回 `{id, result}` 或 `{id, error:{code,message}}`
  - server→client 通知 `{method, params}`(无 id)
  - server→client 请求 `{id, method, params}`(审批),client 回 `{id, result}`;id 用 `srv-{n}` 计数器,与 client 的 id 空间隔离
  - `id` 接受 string 或 int(untagged)
- **方法**(v1 就这些,thread 命名对齐 codex 以保可跟随性):
  - `thread/start {}` → `{threadId}`(新会话,落 rollout)
  - `thread/resume {threadId}` → `{threadId, messageCount}`(rollout 重放,复用孤儿修补)
  - `thread/list {}` → `{threads: [{id, messages, snippet}]}`(按最近修改排序)
  - `turn/start {threadId, input}` → `{}`(受理即回;同 thread 已有 turn 在跑 → error)
  - `turn/interrupt {threadId}` → `{}`(CancellationToken,沿用中断语义)
- **通知**(全部带 threadId;形态即 tui AgentEvent 的序列化):
  `turn/started`、`text/delta {text}`、`note {text}`、`tool/started {callId, name, summary}`、`tool/completed {callId, ok}`、`turn/completed {reason}`(reason: completed/maxRounds/aborted/error)
- **审批往返**:server→client 请求 `approval/request {threadId, description, rememberRules?}`,client 回 `{decision: "allow"|"allowSession"|"allowAlways"|"deny"}`;回复丢失/连接断 = deny(与 tui oneshot 语义一致)。exec 式无人值守不在本 plan(客户端不回就永远挂着,可先 interrupt)。

## 结构

- **新 crate `crates/server`**(kloop-server),依赖 core/protocol;cli 加 `--serve` 分发。依赖链变为 protocol ← provider ← core ← {tui, server} ← cli。
- **每 thread 一套**:独立 tokio task 持有 History(同 tui 的 worker 形态);独立 Permissions(会话缓存不跨 thread);Config 由 cli 传入的工厂闭包按 thread 构造(approver = 该 thread 的路由适配器,note = 通知)。
- **单写者 stdout**:所有出站消息经 mpsc 汇到一个 writer task,每行一条 JSON。stdin 读取循环解析:带 method = 请求,无 method 有 id = 审批回复(路由到 pending map 的 oneshot)。
- 会话 id 生成(UTC 时间戳)现在在 cli 里,server 也要用 → 挪进 kloop-core rollout 模块,cli 改引用。

## 测试

wire 信封 serde 契约(request/response/notification/审批回复的区分);服务循环用 `tokio::io::duplex` + Mock provider 做进程内契约测试:start→turn→通知序列断言、审批往返(允许/拒绝)、并行两 thread 事件不串、interrupt、错误路径(未知方法、重复 turn、坏 JSON 行不崩)。

## 完成标准

fmt/clippy/test 全绿;`--mock` 不受影响;真 key 手工验收:管道驱动 `--serve` 跑两个并行 thread(含一次审批往返 + 一次 interrupt);README 更新;HANDOFF 更新。

## 完成记录

**实现**(新 crate `crates/server`,依赖 core/protocol;cli `--serve` 分发):

- 按 plan 形态落地,无偏差。`serve()` 对读写流泛型(契约测试走 `tokio::io::duplex`);单写者 stdout task;`ThreadUi` 同时实现 Ui+Approver(与 tui `ChannelUi` 同构,只是渲染换成 serde)。
- 会话工具(id 生成、路径、按新旧列表、摘要)从 cli 挪进 `core/src/rollout.rs` 公共函数,cli/server 共用。
- **实现中抓到的坑**:同一秒内两次 `thread/start` 会拿到同一 id——rollout 文件懒创建,`new_session_id` 的 exists() 查不到。修法:选定 id 后立即 `File::create` 占位。契约测试与真 key 验收都覆盖了同秒双开(后者拿到 `-2` 后缀)。
- 无人值守语义:审批回复丢失/输入流关闭 = deny(与 tui oneshot 语义一致);stdin EOF 时 cancel 所有在飞 turn,writer 排空后退出。

**验证**(fmt/clippy -D warnings/test 全绿,126 个测试,server 新增 9):

- wire 契约:信封形状、string/int id、请求 vs 审批回复的结构区分。
- duplex 协议测试(真 serve 循环 + Mock provider):流式 delta 与收尾、审批 deny/allow 往返(文件确实未写/已写)、并行双 thread 事件不串且同秒 id 不撞、忙 thread 拒绝第二个 turn、审批挂起时 interrupt→aborted、协议错误(坏 JSON/未知方法/坏参数/幽灵 thread/无主审批回复)后服务器照常工作、跨重启 list/resume/续跑(4 条消息全在)。
- 真实 binary 冒烟:`--mock --serve` 管道驱动(5 个工具调用含 offload+子 agent,10 条消息落盘,exit 0)。
- 真 key 验收(claude-sonnet-5,代理):两 thread 并行(同秒 id 得 `-2`)、t2 审批 `write_file: s2.txt` allow 后内容精确、t1 长 turn interrupt→aborted、thread/list 计数正确、exit 0。
