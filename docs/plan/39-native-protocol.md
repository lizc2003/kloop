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
6. **审批单套 decision**:reverse request,`accept / acceptForSession / decline / cancel` 一套。保留 codex 的健壮性约定——**对未知 reverse 方法 auto-answer**,引擎发啥都挂不死前端。

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
| `turn/interrupt` | `{threadId, turnId}` | `{}` |

其余方法先返回**降级空响应**(不卡),切片 3/4 再实现。

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

reverse request `approval/request {threadId, turnId, itemId, kind:"command"|"fileChange", description, command?, changes?, rememberRules?}` → `{decision: accept|acceptForSession|decline|cancel}`。丢失/EOF = decline。比 codex 的 command/fileChange 两方法更简(去碎片化)。

### 3.6 入口

`kloop app-server`(位置子命令,app 起的形态) + 保留 `kloop --serve` 别名。验证用 `ENGINE_BIN` 指向 kloop 二进制。

### 3.7 切片 1 完成标准

fmt/clippy/test 全绿;duplex 契约测试(握手/版本不匹配报错/thread-start/turn-start/item 事件序列/审批往返/interrupt/坏 JSON 不崩);`--mock app-server` 管道冒烟;真 key 冒烟(单 thread 发消息→item 流→工具→审批→completed)。

## 四、切片规划(0 → N)

| 切片 | 内容 | 验证 |
|---|---|---|
| **0 ✅** | core 事件模型重构(§二) | 现有测试全绿=无回归 |
| **1** | 原生 wire v1 + 握手 + 主链 + 统一审批 + `app-server` 入口(§三) | 契约测试 + 真 key 冒烟 |
| **2** | **app 分支适配主链**:`桌面前端仓库` 开分支,改 `worker.rs`(换协议)+ 前端 `chatIngest`(消费新事件)+ 指向 kloop 引擎 | **真 app 端到端**:发消息→流式→工具→审批→结果 |
| **3** | 会话管理:`thread/resume|list|read|fork|rollback|compact/start|name/set|goal/*|archive|search` + app 适配 | 契约 + app 会话列表/恢复/fork |
| **4** | config/model/skills/mcp 只读 + 降级兜底:`model/list`/`config/read|write`/`mcpServerStatus/list`/`skills/list` + app 适配 | app 模型选择器/设置面板不卡 |
| **5** | 打磨:reasoning delta 细分、`turn/diff/updated`、`turn/plan/updated`、review mode、边角 item 变体 | 真 app 各面板 |

## 五、开工时定 / 问用户的点

1. **协议版本从 `1.0` 起** —— 我定,除非你要别的。
2. **审批统一成单方法 `approval/request` + `kind` 判别**(而非 codex 的 command/fileChange 两方法) —— 提议统一(更简),你拍。
3. **旧 `--serve` 自创协议**:切片 1 直接替换,不并存 —— 你之前默认同意。
4. **app 分支名**(建议 `kloop-engine`) —— 开工切片 2 时定。
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
