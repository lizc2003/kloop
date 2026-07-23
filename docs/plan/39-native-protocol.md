# Plan 39 — kloop 原生 agent 协议 + core 事件模型重构(替换 codex 引擎)

> 大工程,分切片。开工前读 HANDOFF.md。背景与目标见记忆 [[kloop-replace-codex-engine]]。
> 这不是"兼容 codex wire",而是 **kloop 定义一套最合理的原生协议,app 在 `桌面前端仓库` 开专门分支改造适配,kloop 引擎替换 旧引擎二进制 二进制**。codex 的 app-server 协议只作参考 + 反面教材。

## 〇、背景:为什么推翻 plan 12 的自创协议

- app(Tauri 前端)现在起的是 `旧引擎二进制 app-server [--config …]` 二进制,走 stdio 类 JSON-RPC。目标是让 kloop 引擎顶替它。
- 用户拍板:**不逐字节兼容 codex,做纯粹合理的协议,愿意改 app**。所以 wire 由 kloop 定义,app 跟着改。
- codex 协议调研结论(两轮 Explore,权威):合理的留、历史债甩掉。详见 [[kloop-replace-codex-engine]]。
- 现有 `crates/server`(plan 12)的自创 `thread/turn` 协议 + 事件形态(`text/delta`/`tool/started`)是这套的**起点但要重写 wire 层**。

## 一、大方向(已拍板,2026-07-20)

1. **wire 回归标准 JSON-RPC 2.0**(带 `jsonrpc:"2.0"`),一行一 JSON,stdio。id 统一数字。四类消息标准判别(request/response/notification/reverse-request)。
2. **握手带版本协商 + 结构化能力发现**:`initialize` 双向交换 `protocolVersion` + `capabilities` 对象;不匹配显式报错,不靠 60s 超时暴露。(补 codex 最大硬伤)
3. **领域模型 thread / turn / item 三层**,但 **turn + item 下沉进 core 成原生事件流**,thread 留会话管理层。**取消"回调→事件→wire"两跳,收敛成"core item 事件流 → 各前端投影"一跳**,server 退化成纯序列化薄壳。
4. **事件去碎片化**:started / delta / completed 三态,delta 收敛成单个 `item/delta {itemId, channel, text}`(channel ∈ text/reasoning/output),不再每种内容一个顶层方法。
5. **方法面精简,引擎只管 agent**:thread 生命周期、turn、审批、config/model 只读、mcp 状态。git / 文件读写 / 历史扫描 / 删除 / login 交前端本地做(app 现在本来就这样)。codex 那近百方法门面(fs/process/realtime/windowsSandbox/attestation)不做。
6. **审批单套 decision**:reverse request,`accept / acceptForSession / acceptAlways / decline` 四档。保留健壮性约定——**对未知 reverse 方法 auto-answer**,引擎发啥都挂不死前端;旧 `cancel` 只在 app 兼容边界 fail-closed 映成 `decline`。

## 二、切片 0 — core 事件模型重构(地基,纯重构无 wire 变化)

**目标**:把 core 的输出从离散 `Ui` 回调,改成一个统一的、core 原生的 **item 事件流**;三前端(TUI/plain/server)改为消费同一个 `Event`。行为不变——所有现有测试绿即验证通过,不碰 wire。

### 2.1 现状(已核对)

- `Ui` trait(`core/src/agent.rs:23`):宽接口 + 默认实现降级到 `note`——`text_delta`/`thinking_delta`/`note`/`tool_start(agent,id,name,summary,input)`/`tool_end(agent,id,ok,output)`/`agent_start`/`agent_end`/`todo_update`/`cwd_changed`/`mode_changed`。默认降级让 plain 前端只需实现少数方法。**这个分层降级的好处要保留。**
- TUI 有 `AgentEvent`(`tui/src/events.rs:24`):`ChannelUi` 把 Ui 回调**转成** AgentEvent 发 channel;`App` 消费。server 的 `ThreadUi` 把回调转成 JSON。→ 已经有一跳隐含的"回调→事件"转换,只是事件枚举住在 TUI crate。

### 2.2 新的 core 事件模型(`core/src/event.rs`,待细化)

```
pub enum Event {
    TurnStarted,
    TurnEnded(EndReason),
    ItemStarted   { id: ItemId, item: Item },
    ItemDelta     { id: ItemId, delta: Delta },
    ItemCompleted { id: ItemId, item: Item },   // 定稿态(含最终 output/status)
    Usage(u64),
    CwdChanged    { cwd: String, branch: Option<String> },
    ModeChanged(Mode),
    Note(String),                                // 系统提示/非模型输出(压缩、重试…)
}
pub enum Item {
    AssistantMessage { text: String },
    Reasoning        { text: String },                       // summary/content 是否分开:开工时定
    ToolCall { agent: String, name: String, input: Value,
               status: ItemStatus, output: Option<String> }, // agent="" 主,"agent-N" 子
    SubAgent { label: String, task: String, status: ItemStatus },
    Todo     { items: Vec<TodoItem> },
}
pub enum Delta  { Text(String), Reasoning(String), Output(String) }
pub enum ItemStatus { InProgress, Completed, Failed }
```

- **item id**:core 给每个 item 分配稳定 id(工具沿用 tool_use id;assistant/reasoning 用 turn 内递增)。方案开工时定。
- **History 一致性铁律**:item 事件是 **History 追加的实时投影**——`ItemCompleted` 对应落进 History 的一条 block。设计时钉死"记录"与"发事件"同源,别漂移。

### 2.3 `Ui` trait 改造

- 收敛成单入口 `fn emit(&self, ev: &Event)`(+ 保留 `Approver::confirm` 走原 seam,审批不是产出事件)。
- **保留降级便利**:给 `Event` 加 `fn as_note(&self) -> Option<String>`(或类似),plain 前端在 `emit` 里 `match` 只处理关心的(text/note),其余忽略——等价于旧默认降级,但集中在一处。
- core 内部产事件的调用点(`run_turn`/工具执行/子 agent/压缩)由 `ui.text_delta()` 等改成 `ui.emit(Event::…)`。

### 2.4 三前端适配(行为不变)

- **TUI**:`AgentEvent` 里 core-agent 事件(TextDelta/ThinkingDelta/Note/ToolStart/ToolEnd/AgentStart/AgentEnd/TodoUpdate/ModeChanged/Usage/TurnEnded)**由 core 的 `Event` 取代**;前端控制事件(System/ClearTranscript/Quit/ForkPoints/Forked/Confirm)**留在 TUI**(它们是 UI 状态流,非 agent 产出)。`ChannelUi` 变成"把 core Event 塞进 channel + 混入前端控制事件"。`App`/`render`/`Cell` 状态机基本不动,只是喂进来的类型变了。
- **plain**:`emit` 里 `match`,打印 text/note/工具行,忽略其余。
- **server**:`emit` 里把 `Event` 序列化成 wire notification(切片 1 定义)。

### 2.5 切片 0 完成标准

fmt/clippy/test 全绿(现有 TUI/plain/server 行为不变即证明重构无回归);`--mock`、`--serve`(旧协议暂留)、TUI 三条路径手工冒烟不变。**本切片不动 wire,旧 `--serve` 协议照跑**(切片 1 才替换)。

## 三、切片 1 — kloop 原生 wire 协议 v1(主链闭环)

**目标**:server crate 重写 wire 层为标准 JSON-RPC 2.0 + 版本协商 + item 事件投影 + 统一审批;新增 `kloop app-server` 子命令入口。让一个协议客户端能:握手 → 建 thread → 发 turn → 收流式 item 事件 → 审批 → 收结果。

### 3.1 envelope(`crates/server/src/wire.rs` 重写)

标准 JSON-RPC 2.0:
- request `{"jsonrpc":"2.0","id":N,"method":"…","params":{…}}`
- response `{"jsonrpc":"2.0","id":N,"result":{…}}` / `{"jsonrpc":"2.0","id":N,"error":{"code","message"}}`
- notification `{"jsonrpc":"2.0","method":"…","params":{…}}`(无 id)
- reverse request(审批):server 发 `{"jsonrpc":"2.0","id":M,"method":"approval/request",…}`,client 回 `{"jsonrpc":"2.0","id":M,"result":{…}}`。id 统一数字,server 自己的计数器空间。

### 3.2 握手

| 方法 | params | result |
|---|---|---|
| `initialize` | `{clientInfo:{name,version}, protocolVersion, capabilities}` | `{serverInfo:{name,version}, protocolVersion, capabilities}` |
| `initialized`(notif) | — | — |

- `protocolVersion` 从 `"1.0"` 起(kloop 定)。不匹配 → error,不静默降级。
- `capabilities`:结构化(如 `{streaming, subagents, mcp, images}`),而非单布尔。具体位开工时定。

### 3.3 主链方法(切片 1 就这些)

| 方法 | params | result |
|---|---|---|
| `thread/start` | `{cwd?, model?, …}` | `{thread:{id}}` |
| `turn/start` | `{threadId, input:[{type:"text",text}|{type:"image",…}], clientTurnId?}` | `{turn:{id}}` |
| `turn/steer` | `{threadId, expectedTurnId, input:[…]}` | `{turnId}` |
| `turn/interrupt` | `{threadId}` | `{}` |

切片 1 以外的方法不伪装兼容:未知方法显式报 JSON-RPC `METHOD_NOT_FOUND`;切片 2 的 app capability gate 保证主界面不调用这些延期面,切片 3/4 再逐项实现。

### 3.4 事件投影(core `Event` → wire notification,全带 `threadId`)

| core Event | wire notification |
|---|---|
| TurnStarted | `turn/started {threadId, turn:{id}}` |
| ItemStarted{item} | `item/started {threadId, turnId, item}`(item.type 判别) |
| ItemDelta{delta} | `item/delta {threadId, turnId, itemId, channel, text}` |
| ItemCompleted{item} | `item/completed {threadId, turnId, item}` |
| Usage | `thread/tokenUsage/updated {threadId, tokenUsage}` |
| TurnEnded | `turn/completed {threadId, turn:{id, status, error?}}` |
| Note | `note {threadId, text}` |
| CwdChanged | `thread/cwd/updated {threadId, cwd, branch}` |
| Error | `error {threadId, turnId?, error}` |

`item` 序列化 = `Item` 的 camelCase tag=`type` 投影:`assistantMessage`/`reasoning`/`toolCall`/`subAgent`/`todo`,带 `id`/`status`。

### 3.5 审批(统一单方法 + kind 判别 —— 待你拍板,见五)

reverse request `approval/request {threadId, turnId, itemId, kind:"command"|"fileChange", description, preview?, rememberRules?}` → `{decision: accept|acceptForSession|acceptAlways|decline}`。丢失/EOF = decline;app 旧 `cancel` 兼容值也只会 fail-closed 映成 decline。

### 3.6 入口

`kloop app-server`(位置子命令,app 起的形态) + 保留 `kloop --serve` 别名。验证用 `ENGINE_BIN` 指向 kloop 二进制。

### 3.7 切片 1 完成标准

fmt/clippy/test 全绿;duplex 契约测试(握手/版本不匹配报错/thread-start/turn-start/item 事件序列/审批往返/interrupt/坏 JSON 不崩);`--mock app-server` 管道冒烟;真 key 冒烟(单 thread 发消息→item 流→工具→审批→completed)。

### 3.8 切片 1 开工备注(承 slice 0,拍板见对话 2026-07-20)

**旧 wire 连根删,不并存不兼容**——slice 0 为把现有测试当回归网,让 server/headless 的 `emit` 把 `Event` 翻回旧通知(`text/delta`/`tool/started{callId,name,summary}`/`tool/completed`/`agent/*`/`todo/updated`/`note`/`thread/worktree`,无 `jsonrpc` 字段的旧信封)。这些是**临时脚手架**,slice 1 全部删掉:

1. **server `ThreadUi::emit`**:整段"翻回旧通知"删掉,改成把 `Event` 序列化成新 wire(标准 JSON-RPC 2.0 信封 + §3.4 的 item 事件投影)。`handle_request` 方法名接线随之重写。
2. **headless `--json`(`cli/src/headless.rs` `JsonUi`)一起换到新 item 词汇**——它现在是"复用 server 旧形状",旧 wire 一删就别让旧形状残留在这条路径上(保持"一套词汇,两前端")。
3. **`todo_write` 去双发**:现在 core 对 todo_write 既发 `ItemStarted/Completed{ToolCall}` 又发 `ItemCompleted{Todo}`,导致每个前端都写 `if name=="todo_write" { skip }`(TUI `apply_core`、`StdoutUi`;server/headless 则两个都发)。slice 1 让 **core 只发 `Todo` item、不发 todo_write 的 ToolCall**,把各前端的 skip 特判全删掉。
4. **item id turn-unique**:slice 0 的 assistant/reasoning id 是 per-round 的 `msg-N`/`reasoning-N`(会跨 round 复位),slice 1 改成 turn 内唯一(见 §五.7)。
5. **turn 括号是否走 emit(开放设计点)**:slice 0 的 TurnStarted/Usage/TurnEnded 由各前端 worker 在 `run_turn` 前后自造、不走 `emit` 缝(命令路径不跑 run_turn 也要括号,故 worker 自造更稳)。slice 1 定新 wire 时再判要不要让 core 统一发这三个走 emit、把"各前端自造"收敛掉——留意命令路径的括号来源。
6. **`as_note` / `tool_summary` 去留**:新 wire 送完整 `Item`(含全 input),`summary` 字段大概率不再上 wire;但 `Event::as_note()`+`tool_summary()` 仍是 plain/headless-text 前端的 note 降级所需,保留(它们不是 wire 包袱,是行内 UI 的真实需求)。

## 四、切片规划(0 → N)

| 切片 | 内容 | 验证 |
|---|---|---|
| **0 ✅** | core 事件模型重构(§二) | 现有测试全绿=无回归 |
| **1 ✅** | 原生 wire v1 + 握手 + 主链 + 统一审批 + `app-server` 入口(§三) | 契约测试 + `--mock` 冒烟 + 真 key anthropic 轨冒烟 |
| **2 ✅** | **app `kloop` 分支适配主链**:`worker.rs` 换原生 v1 + 独立 `kloop/` 前端 adapter + capability gate + SSO 绕过 | 真 app Eval Mode + 真协议 E2E:首/二轮、工具、审批、图片、双 cwd |
| **3（进行中：3A engine ✅）** | 会话管理:`thread/resume|list|read|fork|rollback|compact/start|name/set|goal/*|archive|search` + app 适配；3A 已完成持久化快照 + `list/read/resume/fork` engine 主干 | 契约 + app 会话列表/恢复/fork |
| **4 ✅** | config/model/skills/MCP 只读:`model/list`/`config/read`/`mcpServerStatus/list`/`skills/list` + app 适配；写入面继续 fail-closed | app 模型选择器/技能调用/MCP 状态 + 真 Tauri E2E |
| **5** | 打磨:reasoning delta 细分、`turn/diff/updated`、`turn/plan/updated`、review mode、边角 item 变体 | 真 app 各面板 |

## 五、开工时定 / 问用户的点

1. **协议版本从 `1.0` 起** —— 我定,除非你要别的。
2. **审批统一成单方法 `approval/request` + `kind` 判别**(而非 codex 的 command/fileChange 两方法) —— 提议统一(更简),你拍。
3. **旧 `--serve` 自创协议**:切片 1 直接替换,不并存 —— 你之前默认同意。
4. ✅(切片 2 定)**app 分支名**:`桌面前端仓库` 使用专用 `kloop` 分支;保留其既有 `codex` gitlink 修改且绝不提交。
5. ✅(切片 0 定)**`Ui` 收敛成单 `emit(&Event)`**,`Approver::confirm` 保留;旧默认降级集中进 `Event::as_note()`。
6. ✅(切片 0 定)**Reasoning 不拆 summary/content**:`Item::Reasoning{text}` 单字段;signature 是 protocol/provider 传输细节,不进 UI 事件。
7. ✅(切片 0 定)**item id**:tool 用 tool_use id、子 agent 用 label、todo 用 `todos`/`todos-{agent}` 固定槽、assistant/reasoning delta 驱动用 turn-local `msg-N`/`reasoning-N`(切片 1 再改 turn-unique)。

## 六、测试与验证策略

- 切片 0:纯重构,现有全套测试是回归网。补 `Event`/`Item` 序列化投影的单测。
- 切片 1:duplex 进程内契约测试(复用 plan 12 的 Mock provider 手法)覆盖握手/事件序列/审批/错误路径。
- 切片 2+:真 app 端到端(`ENGINE_BIN` 指向 kloop debug 二进制),逐面板验收。
- 真 key 从仓库根 `.kloop/env.local`(见 [[real-key-env-local]])。

## 七、完成标准(每切片)

fmt + clippy(-D warnings)+ test 全绿;一次 commit(写清验证方式);行为变更同步 README;本 plan 补 ✅ 与提交号;新教训写进 HANDOFF.md。app 分支改动在 `桌面前端仓库` 那边独立提交(不进 kloop 仓库),但在本 plan 记录分支名与 commit。

## 完成记录

### 切片 0 ✅(2026-07-20,提交见下)

core 事件模型重构落地,纯重构无 wire 变化,行为逐字节不变。

- **新 `crates/core/src/event.rs`**:`Event`(TurnStarted/TurnEnded/ItemStarted/ItemDelta/ItemCompleted/Usage/CwdChanged/ModeChanged/Note)+ `Item`(AssistantMessage/Reasoning/ToolCall{agent,name,input,status,output}/SubAgent{label,task,status}/Todo{agent,items})+ `Delta`(Text/Reasoning/Output)+ `ItemStatus`(InProgress/Completed/Failed)+ `tool_summary()` + `Event::as_note()`(集中旧默认降级)。
- **`Ui` trait**(`agent.rs`)从一堆默认降级方法收敛成单 `fn emit(&self, ev: &Event)`;`Approver::confirm` 保留。
- **core 产事件点改 `emit`**:`sampling.rs`(text/thinking delta → assistant/reasoning item 三态,**delta 驱动**——首 delta 开、BlockDone 封,不流式则无 item)、`tools/mod.rs run_one`(tool_start/end → ItemStarted/Completed{ToolCall})、`tools/task.rs`+`codemode.rs`(agent_start/end → SubAgent item,共享 `emit_agent_start/end`)、`todo.rs`(→ ItemCompleted{Todo})、`plan_mode.rs`(→ ModeChanged)、`worktree_tool.rs`(→ CwdChanged)、`hooks.rs`/`agent.rs`(→ Note)。
- **三前端投影 `Event`**:TUI `App::apply_core`(`AgentEvent::Core(Event)` 分派,cells 逻辑不变;子 agent todo 过滤从 ChannelUi 下移到 App)、server `ThreadUi::emit` + headless `JsonUi::emit`(翻回旧 wire **字节不变**)、plain `StdoutUi::emit`(text/reasoning 打印 + todo 清单 + `as_note` 兜底)。turn 括号(TurnStarted/Usage/TurnEnded)由各前端 worker 自造,不走 emit 缝。
- **验证**:fmt + clippy(-D warnings)+ 全工作区 test 全绿(core 359 / tui 126 / server 16 / cli headless 等);`cargo run -p kloop -- --mock` 与 `--mock --serve` 活体冒烟——plain 输出、server 通知 wire 逐字节与旧版一致。**唯一测试改动**:旧 `--json` 契约测试里手写的假 `summary`("ls")改成真实值(完整 input JSON 截断,真实 `run_one` 一直如此)。教训见 HANDOFF #37。
- 提交:本次(plan 39 切片 0,见 git log)。

### 切片 1 ✅(2026-07-21,提交见下)

原生 wire v1 落地——旧自创协议连根删,server/headless 重写到标准 JSON-RPC 2.0 + 握手 + item 事件投影 + 统一审批 + `app-server` 入口。开工前拍板(对话):审批 decision 用 `accept/acceptForSession/acceptAlways/decline` 四档(对齐内部 `Decision`,不丢 `AllowAlways` 持久化能力;`cancel`/未知/丢失都按 `decline`,中断整 turn 走 `turn/interrupt`)。

- **core 收尾两处 slice-0 挂账(§3.8)**:① `todo_write` 去双发——`run_one` 对 `todo_write` **不发 `ToolCall` 三态**(`tool_row = name != "todo_write"` 守卫两处 emit),只留 `todo.rs` 的 `Todo` item;各前端 skip 特判全删(TUI `apply_core`、`StdoutUi`、server/headless 天然随投影收敛)。② item id turn-unique——`item_seq` 从 `sample_once` 局部上移到 `turn_rounds` 拥有,穿过 `sample_with_retry`(`&mut *item_seq` reborrow)进 `sample_once`,`msg-N`/`reasoning-N` turn 内单调不跨 round 复位(`--mock` 冒烟见 msg-0..msg-5)。
- **`crates/server/src/wire.rs` 重写**:所有信封加 `"jsonrpc":"2.0"`;新增**共享投影** `project_event(ev, turn_id) -> Option<(method, params)>`(`item/started|delta|completed` 的 `item` 序列化成 camelCase `type` tag + `id`/`status`/字段,tool 双发都带全 `input`、`output`/`agent` present 才出;`Usage`→`thread/tokenUsage/updated`、`CwdChanged`→`thread/cwd/updated`、`Note`/`ModeChanged`→`note`;turn 括号返 `None` 由 worker 造)+ `turn_started_params`/`turn_completed_params`(`{turn:{id,status,error?}}`)。`PROTOCOL_VERSION="1.0"`。
- **`crates/server/src/lib.rs`**:`initialize` 握手(版本不匹配 `INVALID_PARAMS` 硬报错、`initialized` 门 gate 其余方法)+ 结构化 `capabilities{streaming,subagents,mcp,images,approvals}`;`turn/start` 分配数字 turn id(每 thread `turn_seq` 从 1)、published 进 thread 共享 `turn` 槽给 `turn/steer` 读、返 `{turn:{id}}`;`parse_input` 收 string 或 content-part 数组(text 拼、image 走 `user_with_blocks`);worker 抽 `run_turn_or_command`、bracket 由 worker 造(`turn/started`→…→`Usage` emit→`turn/completed`);`ThreadUi::emit` 收敛成一句 `project_event`;审批 reverse request 数字 id + `kind`(`preview.is_some()` 判 fileChange/command)+ 四档 decision 映 `Decision`。thread/start|resume|fork 返 `{thread:{id}}`。
- **`crates/cli/src/headless.rs`**:`JsonUi::emit` 换 `kloop_server::project_event`(turn id 恒 1)、bracket 用 re-export 的 `turn_started_params`/`turn_completed_params` + `Usage` emit;"一套词汇两前端"从"复用旧形状"升级成"复用同一 `project_event`"。
- **`crates/cli`**:`kloop app-server` 位置子命令(main 早分支把首 token `app-server` 改写成 `--serve`,复用全部 flag 处理;`app-server --mock` 可跑)+ `--serve` 别名保留;help/README 同步。
- **验证**:fmt + clippy(-D warnings)+ 全工作区 test 全绿(server 契约 17 + wire 单测 6 重写:握手 gate/版本不匹配/item 事件序列/统一审批四档/steer 回 turnId/interrupt/坏 JSON 不崩/fork/resume/slash);`cargo run -p kloop -- app-server --mock` 同步驱动冒烟——握手回 capabilities、thread/start 回 `{thread:{id}}`、整 demo turn 的 item 流(turn-unique msg-N、todo 单 item 无 tool 行、toolCall 全 input+output、subAgent 生命周期、tokenUsage、`turn/completed{status}`)全对。**真 key anthropic 轨冒烟已过**(Python 同步客户端驱动真 `app-server`,经代理真采样):① 默认沙箱下真 turn——握手/`{thread:{id}}`/`{turn:{id:1}}`/toolCall(bash)全 input+output/assistantMessage turn-unique `msg-0` delta+封口/`thread/tokenUsage/updated`/`turn/completed{completed}`,模型真写文件内容正确;顺带证 sandbox auto-allow 联动仍生效(写 cwd 内文件的 bash 被沙箱兜住、审批 0 次)。② `KLOOP_SANDBOX=off` 下 write_file 两次跑 **审批 reverse request 双路径**——`approval/request{kind:"fileChange",preview:"(new file)\n+1  …",rememberRules:["write_file(*)"]}`,回 `accept`→`completed`+文件写入、回 `decline`→`failed`+is_error tool_result+文件不存在,turn 续跑 `completed`。
- 提交:本次(plan 39 切片 1,见 git log)。

### 切片 2 ✅(2026-07-22,app 功能提交 `f660aca1` + kloop 本提交；2026-07-23 app 合并 main 后 head `80a4cd1e`)

真实 Codex Desktop 已改为 kloop v1 客户端,主链不再经过旧 Codex wire；延期能力在 UI、TypeScript API 和 Rust 启动热路径三层 fail-closed,不伪装兼容。

- **kloop 每线程配置**:`thread/start` 新增 `ThreadStartOptions{cwd,model}`；cwd 缺省 server 启动目录,显式相对路径按该目录解析后 canonicalize,空/坏类型/不存在/非目录均 `INVALID_PARAMS`；CLI 不改进程 cwd,而是按 thread cwd 重建 project instructions、skills、permissions、sandbox、hooks/agent types/program limits,provider 与已连接 MCP `tool_sources` 仍进程共享；model 在默认 provider/env 配好后作 thread 级 override。`resume/fork` 本切片仍用默认 options。
- **Tauri transport**(`桌面前端仓库` 专用 `kloop` 分支):子进程只起 `<ENGINE_BIN> app-server`；所有 request/reverse-response 都是标准 JSON-RPC 2.0；initialize 硬校验 name/version/protocol/capabilities；首轮只发 `thread/start{cwd}` + native `turn/start{threadId,input}`，第二轮不再 `thread/read`；Rust 保留数字 turn id,仅 TS store 边界转字符串；图片 data URL 转 canonical base64 image block；reader 只 surface `approval/request`,未知 reverse request 自动 `{}` 回包；四档 decision 全接通,旧 `cancel` 只在 Rust 边界映 `decline`。
- **独立前端 adapter/UI**:新增 `src/kloop/{dto,chatIngest,normalize,capabilities}.ts`,消费五类 item + text/reasoning/output delta + token total + string error；wire item id 是 turn-local,进入 session-wide timeline 前统一 scope 成 `<turnId>:<itemId>`,避免第二轮 `msg-0` 覆盖首轮；`system`/`note`/`thread/cleared` 明确路由；subAgent completed 省略 task 时保留 started 字段。新增通用 ToolCallCard,恢复 ReasoningCard,todo 复用 PlanCard,native subAgent 只读,approval 卡直显 description/preview/rememberRules 与四档决定。
- **明确降级**:app `KLOOP_APP_CAPABILITIES` 关闭 history/model/config/skills/MCP/goal/plan/review/compact/dynamic tools/automation/artifact/account/legacy extensions；搜索、fork、rollback、标题/侧栏会话菜单、side chat、handoff、composer model/reasoning/plan/goal/review/compact 等入口隐藏,`api/thread|git|skills|mcp` 再做 invoke 边界 guard；Rust setup 不启动 history watcher/automation scheduler。按用户明确授权,kloop 分支 `App.vue` 跳过 Codex SSO、LoginDialog、AccessGuard 和 skill-env 前置,直接启动 kloop app-server；正式 provider 凭证来自启动环境/`.kloop`。
- **事件生命周期补洞**:审查发现采样流已发 delta 后若断流,旧重试会遗留半截 item 并产生第二份回答。`sample_once` 现累积可见文本、错误/取消/异常 Done 时补 `ItemCompleted`；一旦已有可见输出,错误不自动重试(避免重复),未输出的瞬时错误仍保留三次 retry。Mock 新增 `PartialError` 锁定 started→delta→completed 且只请求一次。
- **验证(kloop)**:`cargo fmt --all --check`、`cargo clippy --workspace -- -D warnings`、`cargo test` 全绿；`target/debug/kloop app-server --mock` 同步 JSON-RPC 客户端握手/thread/完整 item 流冒烟通过；真 key 原生协议脚本同一 thread 连跑首轮/第二轮、bash tool、write_file `approval/request→decline`、base64 PNG,再开第二 thread 真实写 A/B 两个临时目录,确认 cwd/文件落点不串。
- **验证(app)**:43 个 native/eval 定向 Bun 测试全绿；真实 `bun run eval` 以 `ENGINE_BIN=target/debug/kloop` 启动完整 Tauri App,case `39-02` 得 `completed`(threadId/数字 turnId 边界/8 events),证 SSO 绕过 + worker + WebView event bridge + completion 全链；approval case `39-03` 正确得 `needs_interaction`,error=`approval required: approval/request`。2026-07-23 将最新 `origin/main@4e00b015` 合入 app `kloop` 分支后,把 main 的旧 Codex/v2 测试按 native capability 契约分流：保留并加强 disabled fallback 断言,仅对明确延期的 history/plan/review/compact/dynamic-tools/automation 行为逐 capability skip,不做全局测试 override、不恢复旧 RPC；最终全量 `bun test` **865 pass / 52 capability skips / 0 fail**(917 tests / 127 files),`bun run typecheck`、`bun run build:test`、Rust `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、单线程全量 **225 tests** 全绿。
- **app 提交**:`桌面前端仓库` branch `kloop`,native 功能提交 `f660aca1`；合并 `origin/main@4e00b015` 并收齐质量门后的 branch head `80a4cd1e`(merge parents=`f660aca1` + `4e00b015`)；既有 `codex` gitlink 修改保持未 stage/未提交。kloop 提交:本提交。

### 切片 3A engine ✅(2026-07-23,提交见下)

会话管理的 engine 地基落地：append-only rollout 现在既能恢复 provider history，也能恢复 thread 运行时和 UI 需要的 turn 终态；原生 wire 提供 `thread/list|read|resume|fork` 主干。**这只完成切片 3A engine，不代表整个切片 3 完成**——Desktop 会话 UI 接线以及 `rollback|compact/start|name/set|goal/*|archive|search` 仍挂账。

- **rollout 快照扩展**:`session` 行持久化 canonical cwd + 最终解析 model，`turn_terminal` 行持久化 `completed|maxRounds|aborted|error`；`SessionSnapshot` 将 provider 可回放 `messages`、最后生效的 `runtime`、按消息位置锚定的 `terminals` 分开返回，runtime/terminal 不进入 provider replay 或 token accounting。compaction 替换消息后清掉已经失效的 terminal 索引；fork 复制 runtime/terminal 并重写 envelope，cut 只认完整 user exchange 边界。
- **partial 输出可恢复**:采样在可见 text/reasoning 已流出后遇到错误、取消或异常 Done，会先补齐 `ItemCompleted`，并把可安全回放的 partial assistant block 落进 History；有外部可见副作用后不再 retry/fallback，避免重启前丢半截回答或当前 turn 生成第二份回答。未签名 thinking、image、tool use/result 不从不完整流中伪造回放。
- **原生会话 RPC**:`initialize.capabilities.threads={list,read,resume,fork}`；`thread/list{limit?,cursor?}` 默认 50、上限 500，隐藏 sidechain，并返回 snippet/runtime/resumable/inProgress/fork lineage；`thread/read` 返回 messages/runtime/terminals；`thread/resume` 与 `thread/fork` 恢复原 cwd/model 并直接建立 live thread。running turn 期间拒绝 fork，避免 terminal 尚未落盘时复制不完整 exchange。
- **恢复语义 fail-closed**:`thread/start` 在 ConfigFactory 解析默认 provider/model 后把实际 model 再 pin 进 rollout，后续环境变化不静默换模型。旧 rollout 仍可 `read`，但因没有 runtime，`resume/fork` 必须显式给原 cwd；成功后追加 runtime 完成一次性迁移，绝不猜成 app-server 当前启动目录。
- **验证**:`cargo fmt --all --check`、`cargo clippy --workspace -- -D warnings`、全工作区 `cargo test` 全绿；`kloop --mock` 六轮 demo 通过；同步 `app-server --mock` 客户端完成 initialize/start/turn/item stream/read/list 并核对 runtime/model/terminal；真 key anthropic 轨以 `claude-sonnet-4-6` 运行“只回复 OK”，响应 `OK`，随后 `thread/read` 恢复两条消息、正确 cwd/model 与 completed terminal。契约测试覆盖分页、runtime 保真、legacy 迁移、fork 边界/running 拒绝、partial error 恢复。
- **提交**:本次(plan 39 切片 3A engine,见 git log)。

### 切片 4 ✅(2026-07-24,app 提交 `934e325d` + kloop 本提交)

原生协议补齐严格只读的模型、配置、技能与 MCP 状态面；Desktop 只打开对应读取入口，旧配置写入、技能/plugin mutation、MCP mutation 与 personalization 继续三层 fail-closed。

- **engine 只读 RPC**:`model/list` 只列本地实际可解析的默认模型；`config/read` 只返回 cwd/model/permission/context/defer/sandbox/worktree 显式 allowlist；`skills/list` 合并项目与用户 `SKILL.md` metadata(项目同名覆盖用户、不含正文/allowed-tools/commands)；`mcpServerStatus/list` 返回进程启动时 immutable connected/unavailable snapshot + 工具 metadata。initialize 增加 `models.list/config.read/skills.list/mcpServers.status` 四组 capability。
- **scope 与安全边界**:`config/read`/`skills/list` 支持 canonical cwd 或 live `threadId` scope，thread model 取已 pin runtime；双 selector、坏类型和未知参数全部 `INVALID_PARAMS`，mutation 方法仍 `METHOD_NOT_FOUND`。MCP 响应不含 command/args/env/url/header/token，失败 message 与 stderr 都不反射原始 anyhow 链；`--mock` 的 config/skills reader 全链 hermetic，不读真实 cwd/HOME。
- **Desktop adapter/UI**:Tauri worker 新增四个 native read command 并在 initialize 硬校验四组 capability；`src/kloop/readSurfaces.ts` 把 native DTO 投影到既有 view model。模型菜单使用 native catalog、仅空 thread 可选且隐藏 reasoning；composer 按当前 cwd 加载 skills，切项目会失效缓存并丢弃陈旧请求，选择后以原生 `/<name> ` 替换 slash query；Connectors 展示 MCP transport/state/tools/safe message，隐藏 add/edit/toggle/delete；PluginCenter 会把不可见 tab 归一化，Slash 菜单渲染与键盘索引共用同一 capability 过滤结果。
- **mutation fail-closed**:`configuration/skills/mcp` umbrella 保持 false；TS API 在 `invoke` 前拒绝 personalization/skill/plugin/MCP mutation，Rust personalization command 也固定返回 `PERSONALIZATION_FAILED`，不能借 safe `config/read` 伪造旧偏好或触发旧 write/reset。设置加载失败时 instructions/memory/reset 控件全部禁用且错误可见（reset dialog 内也显示），direct mount 也不会产生不可保存的假状态；审查未发现 direct invoke 旁路。
- **验证(kloop)**:`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、全工作区 `cargo test` 全绿。同步原生协议 E2E 核对 model/config allowlist/project skill/connected+unavailable MCP、`config/write=-32601` 与 secret projection clean；安全回归另证 mock skills hermetic、未知参数 `-32602`、MCP stderr clean；真 key `/slice4-e2e` 返回 `SLICE4_SKILL_OK`。
- **验证(app)**:全量 Bun **885 pass / 52 capability skips / 0 fail**(937 tests / 130 files)，`bun run ts:check`、`bun run build:test` 全绿；Rust `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、**227 tests** 全绿。真实完整 Tauri Eval 以 `ENGINE_BIN=target/debug/kloop` 跑 case `39-04` 得 `completed`（threadId/turnId 均存在、error=null）。
- **提交**:`桌面前端仓库` 专用 `kloop` 分支 `934e325d`；既有 `codex` gitlink 修改保持 unstaged/未提交。kloop 提交:本提交。
