# Plan 65 — Provider 消息语义、严格终态与 Partial 一致性

> 状态：✅ 已完成（2026-08-07；提交 SHA 以本文件所在提交为准）
>
> 依赖：Plan 60、Plan 64
>
> 施工关系：Plan 64 已完成 transport guard、typed failure、consumer-drop 取消、semantic retry watermark 与单 terminal seam；本计划只收紧该 seam 上方的消息语义，不重做 timeout/cap/retry。Plan 63 已固定 native protocol 1.0 exact handshake；本计划在 1.0 现有 envelope、method、turn status 与错误字符串形状内兼容修正，typed public terminal/独立 partial 状态留给未来显式版本升级。

## 背景

当前三条 provider rail 最终都投影为：

```text
TextDelta / ThinkingDelta
BlockDone(ContentBlock)
Done { stop_reason: Option<String>, usage }
```

这条 seam 已能可靠区分正常 terminal、transport failure 与 premature close，但仍把两类事实混在一起：

1. stream 是否按 provider 协议完整结束；
2. provider 为什么结束，以及这个结果能否被 agent 当成成功、工具请求、可恢复截断、拒绝或不完整响应。

由此存在以下确定性缺口：

- Anthropic `refusal`、Chat Completions `content_filter`、Responses 非 output-limit 的 `response.incomplete` 都可能落成 `EndReason::Completed`。
- Responses 可以先发 `response.output_text.delta`，不发对应 `response.output_item.done`，再发 `response.completed`；UI 看见文字，history 没有该文字，turn 却成功。
- adapter 对 block index、tool id/name/call_id 和事件顺序大量使用空字符串、index 0 或忽略作为默认值；malformed provider output 可能变成可 dispatch 的工具调用。
- `BlockDone(ContentBlock)` 在类型上允许 `Image`、`ToolResult` 等 provider assistant output 不应产生的状态。
- provider path 会持久化空 `Message::assistant([])`。
- 一个仍处于 delta 阶段的 assistant/reasoning item 遇到 stream error 时，以 `item/completed` 内的 `status:"completed"` 收口；turn 随后才报 error，item 自身语义误导。
- SSE frame 当前使用 lossy UTF-8 解码；非法 bytes 可能被替换为 U+FFFD 后继续成为合法 JSON、文本或 tool arguments。
- output-limit 连续恢复用尽后，现有 core 会把仍然截断的最后一轮标成 completed。

本计划的目标不是把某一家 provider 的 wire 变成全局模型，而是让每个 adapter 在边界上完成严格解析，再把只有 kloop 能安全消费的 assistant blocks 与 typed outcome 交给 core。

## 回源结论

### codex codex-rs

可借鉴：

- Responses delta、`output_item.added`、`output_item.done` 与 completion 分开建模，并携带 item identity。
- EOF 早于 `response.completed` 明确报错。
- `response.incomplete` 与 `response.failed` 不伪装为 completion。
- turn loop 只在显式 `ResponseEvent::Completed` 后结束 sampling。

不可照搬：

- 它面向 Responses-first 的宽事件面，不能直接替代 kloop 的三 rail canonical seam。
- 部分未知/解析失败事件仍只记 debug 或忽略；kloop 的 tool/history 边界需要更严格。
- 它的 output item 与 app-server vocabulary 属于自身产品协议，不应拉进 kloop native protocol 1.0。

### 当前 Claude Code 与官方 Claude API

可借鉴：

- 官方 stop reason 已有明确语义：`end_turn`、`max_tokens`、`stop_sequence`、`tool_use`、`pause_turn`、`refusal`；`refusal` 不是普通成功。
- Claude Code 会显式把 refusal 与 max-output terminal 投成错误消息，并检测“有 message start、无 completed block、也无 stop reason”的不完整 stream。
- completed block 与最终 usage/stop reason 分开更新，说明 display block completion 和 turn outcome 是两个层次。

不可照搬：

- Claude Code 的 Anthropic-first message/event 对象、ACP 映射和 UI transcript mutation 不是 kloop 的稳定内部协议。
- 其 OpenAI/Responses adapter 仍有缺字段 fallback、把任意 `incomplete` 映为 `max_tokens`、terminal 时主动闭合未完成 block 等兼容性放宽；这些会掩盖 kloop 已确认的 UI/history 分裂。
- 本地快照只能作为实现证据；当前 Claude stop reason 以官方文档为准。

### claw-code

可借鉴：

- OpenAI-compatible tool calls 按 index 建状态，而不是用单一 current tool slot。
- canonical output block 类型只含 assistant 可产生的 Text/Thinking/RedactedThinking/ToolUse。
- `stop`/`tool_calls` 先在 adapter 归一化为 `end_turn`/`tool_use`。

不可照搬：

- EOF 时若缺 finish reason 会补 `end_turn`，缺 tool id 会合成 id；这会把不完整 wire 伪造成合法历史。
- finish 时主动补所有 block stop，没有证明 provider 真的发完。
- stop reason 仍是 free-form string，`content_filter` 等没有进入明确的 agent terminal 语义。

### CodeWhale

可借鉴：

- 三 adapter 共享 stream-open/idle guard，而 wire parser 留在各自边界；该方向已由 Plan 64 落地。
- 多 tool-call 需要按 block index 路由，不能靠一个 current slot。
- runtime envelope 的稳定 identity/sequence 思路适合未来 replay protocol，但不属于本计划。

不可照搬：

- Responses 把 `[DONE]` 当 loop 结束，并在任何退出后补 `MessageStop`；error 后仍可能继续出现 success-like terminal。
- `response.completed` 中未知 status 回退 `end_turn`，`response.incomplete` 的处理路径互相矛盾。
- malformed SSE JSON、unknown semantic event 和缺失 required tool 字段多处只 warn/ignore/default。
- turn loop 的 `any_content_received` 把任意 non-start event 当 semantic watermark，粒度不如 Plan 64 已有的 text/reasoning/completed-block 规则。

## 已确认的产品决定

1. **Transport terminal 与 semantic outcome 分型。** `StreamEvent::Done { stop_reason: Option<String> }` 改为 mandatory typed terminal；adapter 不得把 free-form provider reason交给 core猜。
2. **Provider assistant output 使用窄类型，并统一归一化空内容。** stream 只能完成 Text、Thinking、RedactedThinking、ToolUse；Image 与 ToolResult 只属于 input/history，不再是 provider output seam 的可构造状态。空 Text、空 RedactedThinking、thinking/signature 都为空的 Thinking 不构成 semantic block，不提升 watermark、不落 history；thinking 可空但 signature 非空的 Thinking 是可回放 semantic block，必须保留。
3. **合法 terminal outcome 固定为：**自然结束、工具请求、output limit、拒绝、内容过滤、其他明确不完整。未知 stop/status 不是成功，按 protocol failure fail closed。
4. **Refused/Filtered/Incomplete 是语义终态，不是 transport failure。** 它们不透明 retry、不切 client-side fallback、不 dispatch tool；已完整或可恢复的 assistant content可以落 history，turn 以 error 结束。
5. **OutputLimit 是显式可恢复 outcome。** 保持“记录截断 assistant → 注入 continue nudge → bounded continuation”，不把它当同一 request 的透明 retry；恢复预算耗尽后 turn 必须 error，不能 completed。
6. **ToolUse outcome 与 tool blocks 双向一致。** 有 tool block 必须是 ToolUse；ToolUse 必须至少有一个合法 tool block。任何 mismatch 在 dispatch 前失败。
7. **Required tool identity fail closed。** Anthropic `id/name`、Chat `id/name/index`、Responses `call_id/name/output identity` 到 item 完成时必须存在、非空、无冲突；不合成 id，不用空字符串或 index 0 掩盖缺字段。
8. **Tool arguments 必须是完整 JSON object。** 空/纯 whitespace 兼容为 `{}`；非空必须 parse 且 top-level 为 object。partial、scalar、array 或冲突 fragment 都不能 dispatch。
9. **Responses 必须证明每个 output item 与 content part 闭合。** `output_item.added` 注册 item，已支持的 `content_part.added/done`、text/refusal/reasoning/function-arguments delta/done 都按 item + content identity 路由，`output_item.done` 精确完成同一 item；`response.completed|incomplete` 前 open set 必须为空。最终 item/part 与已显示 delta 矛盾时 fail closed。
10. **Unknown semantic event fail closed。** 每条 rail维护显式 supported/ignorable allowlist；ping、注释、已知 usage/metadata 可忽略，未知 block/content/delta/terminal 不能静默丢弃。未来 provider 加事件时在 adapter 中显式升级。
11. **SSE 必须 strict UTF-8，包括 EOF residual。** 完整 frame bytes 在 JSON parse 前精确 UTF-8 校验；EOF时若仍有未闭合frame，先校验残余bytes：非法UTF-8是non-retryable protocol failure，合法但未闭合才是Plan 64的incomplete protocol。任何路径都不做lossy replacement。Plan 64 的 chunk split、CRLF、multiline、frame/response cap不变。
12. **Provider path 不持久化空 assistant。**合法空 EndTurn 可以结束 turn，但不写 `Message::assistant([])`；其他无内容 outcome按各自语义处理，不用空 assistant占位。
13. **Display lifecycle、history 与所有 frontend 一致。** 完整 block 即使没有 delta，也必须生成一对 item start/completion；仍 open 的 delta item遇到 cancel/error时以 failed 收口，并用收到的 partial text；已经由完整 block正常完成的 item不因之后 turn error被追溯改成 failed。plain/TUI必须按item id跟踪是否已有delta：无delta的completion创建最终输出，有delta的completion只校正/封口而不重复打印；server/headless投影同一core terminal status。
14. **Native protocol 1.0 不升版。** JSON-RPC envelope、method 名、`turn/completed` shape、`status:"error"` + string error、thread/read snapshot shape保持；open partial item复用现有 item `status:"failed"` token，不新增 `partial` token或 typed error字段。
15. **未来 protocol 2.0 不在本计划预埋半套双栈。** typed public refusal/filter/incomplete、独立 partial/cancelled item status、provider diagnostic object若需要，另立版本与 Desktop migration；1.0 不加客户端必须理解的新字段。
16. **Rollout 继续 append-only。** assistant semantic content与 display-only turn terminal分开记录；terminal、provider diagnostic、retry state不进入下一次 provider messages。
17. **不因兼容性放宽正确性。** 对常见 provider alias做显式映射；不能确认等价的值进入 Incomplete/error或 protocol failure，而不是猜成 EndTurn。

## 必须保持的不变量

- 单 sampling attempt对 consumer只能有一个 terminal：typed outcome或 typed failure，不能两者都发。
- EOF、`[DONE]`、channel close和 producer task结束都不能自行代表 semantic success。
- Plan 64 的 semantic watermark继续单调：首个 non-empty text/reasoning delta或首个完整 semantic block后，任何失败都禁止透明 replay/fallback。
- 完整 tool block本身就是 semantic output；即使尚未 dispatch，也封死 retry。
- malformed tool JSON、缺 identity、事件乱序和 item closure mismatch都在 tool dispatch前失败。
- display delta不能成为唯一历史事实：成功 terminal前必须有对应完整 semantic block；失败时只能按 partial recovery规则持久化。
- 一个 assistant/reasoning item最多一个 start和一个 lifecycle terminal；没有 delta的完整 block也不能在 UI 消失。
- 一轮 provider assistant message内 tool-use id唯一，tool result仍与该 id一一配对。
- provider 返回的 assistant output不能构造 Image/ToolResult。
- `Refused`、`Filtered`、`Incomplete`、恢复用尽的 `OutputLimit`绝不映为 `EndReason::Completed`。
- 普通空 EndTurn不伪造文本、不写空 assistant；thread仍只产生一个 turn terminal。
- turn terminal与 rollout snapshot绝不进入 provider replay history。
- protocol 1.0仍 exact-match；不接受 2.0、不做版本协商或 alias。

## 1. Canonical stream 类型

在 `kloop-protocol` 引入只用于 provider assistant output 的窄类型。名称开工时可按周边风格微调，但职责固定：

```rust
pub enum AssistantBlock {
    Text { text: String },
    Thinking { thinking: String, signature: String },
    RedactedThinking { data: String },
    ToolUse { id: String, name: String, input: Value },
}

pub enum OutputLimitKind {
    MaxOutputTokens,
    ModelContextWindow,
}

pub enum IncompleteReason {
    PauseTurn,
    Provider(String),
}

pub enum AssistantOutcome {
    EndTurn,
    ToolUse,
    OutputLimit(OutputLimitKind),
    Refused,
    Filtered,
    Incomplete(IncompleteReason),
}

pub enum StreamEvent {
    TextDelta(String),
    ThinkingDelta(String),
    BlockDone(AssistantBlock),
    Terminal {
        outcome: AssistantOutcome,
        usage: Option<Usage>,
    },
}
```

约束：

- `AssistantBlock` 由一个共用 normalizer 判定是否具有 semantic payload：非空 Text/RedactedThinking、thinking 或 signature 至少一个非空的 Thinking、以及任意结构合法的 ToolUse 才能进入 sink；空 display block只完成 adapter内部 wire lifecycle，不发 `BlockDone`。
- signed-empty Thinking（thinking为空、signature非空）必须保留并提升 watermark；unsigned-empty Thinking必须丢弃。该规则不能由笼统的 `blocks.is_empty()` 或 `text.trim().is_empty()`代替。
- `AssistantBlock -> ContentBlock` 只有一个无损转换点，由 sampling/history边界调用。
- 不提供 `ContentBlock -> AssistantBlock` 的宽松 blanket conversion；测试/mock必须显式构造合法 output。
- `StreamCompletion` 的 outcome必填；删除 `Option<String>` 和“None 也算结束”。
- 每个 `BlockDone` 发布前必须已完成该block的required field与结构校验；一经发布立即提升Plan 64 semantic watermark。terminal到达时再把outcome与本attempt已发布/缓存的block facts做一致性校验，失败则发附带既有watermark的`ProviderFailure`，绝不能为了等待stop reason而让已完整ToolUse失去retry seal。Chat这条rail本来就把全部完整block缓存到finish_reason：缓存阶段不属于consumer-visible `BlockDone`，finish/outcome校验通过后逐个发布，每次发布都同步封死watermark；Anthropic的content_block_stop和Responses的output_item.done则在各自block合法闭合时立即发布。
- `IncompleteReason::Provider` 只保存有界、secret-safe 的 provider reason token，不保存整份 response/body。
- `AssistantOutcome` 是内部 semantic contract，不直接 serde成 native protocol 1.0。
- `ProviderFailure` 继续表达请求未取得合法 semantic terminal；outcome不塞进 failure taxonomy。

在 provider stream consumer seam继续统一维护 semantic watermark和terminal_seen。`Terminal` 之后 `recv()`返回 None；producer提前关闭仍物化为 Plan 64 的 incomplete typed failure。

## 2. Outcome 映射

### Anthropic Messages

显式映射：

| wire stop reason | canonical outcome |
|---|---|
| `end_turn` | `EndTurn` |
| `stop_sequence` | `EndTurn` |
| `tool_use` | `ToolUse` |
| `max_tokens` | `OutputLimit(MaxOutputTokens)` |
| `model_context_window_exceeded` | `OutputLimit(ModelContextWindow)` |
| `refusal` | `Refused` |
| `pause_turn` | `Incomplete(PauseTurn)` |
| 其他/缺失 | protocol failure |

`refusal` 的 `stop_details` 可用于 bounded diagnostic/log，但 protocol 1.0只投稳定、无敏感细节的错误文案。当前 kloop不声明可产生 `pause_turn` 的 server tools；收到时保存合法内容并 error，不假装已经会 resume。未来若实现 pause continuation，只改 core 对 typed outcome的策略，不回退 adapter strictness。

### OpenAI-compatible Chat Completions

兼容 alias显式映射：

| wire finish reason | canonical outcome |
|---|---|
| `stop`、`end_turn` | `EndTurn` |
| `tool_calls`、legacy `function_call`、明确兼容的 `tool_use` | `ToolUse` |
| `length`、明确兼容的 `max_tokens` | `OutputLimit(MaxOutputTokens)` |
| `content_filter` | `Filtered` |
| `refusal` | `Refused` |
| 其他/缺失 | protocol failure |

`[DONE]` 仍只表示 transport tail。允许 finish_reason同一 body chunk后紧随 usage-only frame或 `[DONE]`；finish_reason一旦 latch，后续 semantic delta/tool fragment/第二个 finish reason必须失败。为兼容 Plan 64 的低延迟行为，不要求再等一个可选 transport-tail chunk才发布 terminal。

OpenAI Chat 的拒绝也可能出现在 `delta.refusal`/final `message.refusal`，而 finish_reason仍为 `stop`。adapter必须单独累积非空 refusal payload，将它像assistant text一样显示并落为Text block，同时把`stop + refusal`提升为`Refused`；refusal与tool call冲突时失败。只有没有refusal payload时，才单独按上表解释`stop`。

### OpenAI Responses

- `response.completed` 且只有普通 message/reasoning output → `EndTurn`。
- `response.completed` 且至少一个 function call、无 refusal/冲突 output → `ToolUse`。
- completed response中出现 refusal content → `Refused`；不得把 refusal text 当普通 EndTurn。
- `response.incomplete` + `max_output_tokens` → `OutputLimit(MaxOutputTokens)`。
- `response.incomplete` + `content_filter` → `Filtered`。
- 其他已解析 incomplete reason → `Incomplete(Provider(reason))`。
- `response.failed`、`error` → typed provider failure。
- completed/incomplete event中的 response status必须与 event type一致；缺失、冲突或未知 status失败。

一个 response同时含 refusal与tool call、incomplete与可 dispatch tool call、或其他互斥语义时按 protocol failure，不猜优先级。

## 3. Adapter 严格状态机

### 3.1 共用规则

每条 rail在 adapter内部维护：

- message/response是否开始；
- open/closed block或output item集合；
- terminal是否已 latch；
- text/reasoning/tool argument accumulator；
- tool identity第一次出现的值与后续一致性；
- completed assistant blocks与 outcome所需事实。

所有 required 数字/字符串字段使用显式 parser；不再用 `unwrap_or(0)`、`unwrap_or_default()`表达 wire缺失。可选字段仍按 provider规范可选。

显式 ignorable allowlist只包括该 rail已知不会改变 assistant semantic output的事件。unknown event默认 protocol failure；不能用“未来兼容”作为静默丢 content、tool或terminal的理由。

### 3.2 Anthropic

- `message_start` exactly once；content/message terminal事件不得越过 start。
- content index必须存在、唯一 start、delta/stop引用已知 open block、stop exactly once。
- Text/Thinking/RedactedThinking/ToolUse逐类型校验 required shape；unknown content block type失败。
- ToolUse在 start时锁定非空 id/name；input fragment只进入同一 index；stop时严格 parse object。
- Thinking signature按 wire增量累积；需要回放的 Thinking没有合法 signature时仍可显示，但 partial history继续遵守既有 replay安全规则。
- `message_delta` 提供唯一可映射 stop reason；`message_stop` 前全部 block closed且 stop reason已知。
- `message_stop` 是唯一成功 stream terminal；缺它的 EOF继续是 Plan 64 incomplete failure。
- ping/明确 metadata可忽略；Anthropic SSE `error`立即 typed failure。

### 3.3 Chat Completions

- 请求只允许一个 logical choice；usage-only empty choices合法，其余缺 choice/index冲突失败。
- tool calls用 map按 wire index维护；不要求 index物理连续，但同 index的 id/name只允许“先缺后补”或重复相同值，冲突值失败。
- arguments只追加 string fragment；terminal时每个 call必须有非空 id/name，empty args转 `{}`，非空 args必须是完整 object。
- `delta.refusal`（以及等价final字段）进入独立accumulator和display text，不与普通content或tool call静默合并；`stop + refusal`归一为Refused。
- 收到finish_reason后，先解析合法outcome、完成required field/tool JSON与outcome/block一致性校验；全部通过后才按Thinking → Text/refusal → ToolUse的稳定顺序发布non-empty `BlockDone`，最后返回typed completion。invalid/unknown finish不能先泄出完整tool block。
- tool block按稳定 index顺序完成；一轮内 id必须唯一。
- finish_reason只 latch一次。latch后只接受同一已解析 chunk里的 usage-only tail或 `[DONE]`，不接受新 semantic content。
- text/reasoning/tool blocks完成后才发布 typed terminal；unknown finish reason失败。
- `[DONE]` 或 EOF先到且无 finish_reason继续是 incomplete failure。

### 3.4 Responses

以 provider output identity（output_index + item_id，并按类型附加content_index/call_id）维护 open item与part：

- lifecycle allowlist显式支持 `response.created|in_progress`、`response.output_item.added|done`、`response.content_part.added|done`、`response.output_text.delta|done`、`response.refusal.delta|done`、`response.function_call_arguments.delta|done`、当前请求可能产生的 reasoning summary/text part/delta/done、`response.completed|incomplete|failed` 与 `error`。created/in_progress等纯metadata可忽略；所有added/delta/done都必须推进或校验状态，不能走泛化no-op。
- `response.output_item.added` 注册唯一 item及类型；duplicate/conflicting added失败。`content_part.added`在所属message/reasoning item下注册唯一content_index和output_text/refusal等part类型；如果官方fixture证明某类无delta final item合法省略part lifecycle，只允许`output_item.done`从final item一次完成它，不能让orphan delta隐式创建part。
- output-text/refusal/reasoning/function-call delta必须引用已注册、类型匹配、仍 open的 item/part；orphan delta失败。各自`*.done`校验final text/arguments并关闭对应streaming field，`content_part.done`关闭part；duplicate/wrong-order done失败。
- refusal按item+content_index独立累积、向display发text delta，并在final message item中与refusal part逐字核对；普通output_text与refusal可以同属一个message供展示，但任何refusal事实都会把completed response归一为Refused，且与function call互斥。
- message content part只接受已支持的 output_text/refusal；unknown content-bearing part失败。
- function call要求非空 call_id/name；item id按 wire要求校验，但不能替代缺失 call_id。arguments delta/done与final item必须一致并最终parse为object。
- reasoning summary/text的added/delta/done按item+summary/content index路由；encrypted content继续进入canonical Thinking signature。没有可安全回放密文时partial replay规则不放宽。
- `response.output_item.done`必须引用唯一 open item，要求其已出现的parts/fields全部closed，并从完整 final item生成一个或多个non-empty `AssistantBlock`；unknown/duplicate done失败。
- 若同一 item已有 display delta，final item中的对应text/refusal/reasoning/arguments必须与 accumulator一致；不一致按 semantic-output-after-error路径保存可恢复 partial并失败。
- `response.completed|incomplete` 前 item/part open set必须为空；不能像参考 adapter那样在 terminal主动补任何done/block stop。
- allowlist与required/optional事件顺序以本地loopback的官方wire fixture固定；未来新增事件必须先分类为metadata或semantic并补fixture，不得恢复`_ => {}`。
- `[DONE]`、EOF、HTTP body结束都不能替代 Responses semantic terminal。

## 4. Core sampling、history 与 turn 决策

### Block 与 UI 生命周期

- delta首次到达时照旧打开 assistant/reasoning item。
- `BlockDone(Text|Thinking)` 若此前没有 delta，也要分配 turn-unique item id并依次发 start + completion；只有归一化后non-empty的可展示Text/Thinking才建item。RedactedThinking仍可作为semantic/history block，但永不创建或渲染assistant/reasoning item。
- core lifecycle terminal显式携带 Completed/Failed；实现可将`ItemCompleted`重命名为`ItemFinished`，但不能继续让wire层从事件名猜status。completion payload始终带完整 final text，可修正 start/delta阶段的展示。
- provider error/cancel时，仅当前仍 open的 display item用收到的 accumulator收口为 failed；已收到合法 `BlockDone` 的 item保持 completed。
- reasoning partial即使因缺 signature不进入 replay history，也要以 failed item向 UI收口，不能悬挂。
- plain/TUI按item id维护是否已渲染delta：completion命中live item时只以final payload校正并封口；没有live item时创建最终text/reasoning cell或一次性输出。TUI当前忽略assistant/reasoning start/completion的行为必须改掉；plain不能只打印delta。
- server/headless直接投影core terminal status；Desktop/client以`item/completed` payload做最终校正，不能因没有delta而丢项，也不能把failed渲染成completed成功。

上述是实施契约，不是可选frontend美化；否则“无delta完整block”和“partial failed”仍会在不同surface分叉。

### History 记录规则

- adapter/sink先经共用normalizer剔除空Text、空RedactedThinking与unsigned-empty Thinking；sampling不得只靠`Vec::is_empty()`判断消息是否为空，且不得对文本使用`trim()`改变合法whitespace内容。signed-empty Thinking仍是non-empty semantic message。
- `AssistantBlock` 在 sampling成功取得 terminal后转换为 `ContentBlock`。
- 归一化后的`blocks.is_empty()`时绝不调用 `history.record(Message::assistant(...))`。
- `EndTurn` + empty blocks是合法空回复：turn completed、final_text为空、history不写占位。
- Refused/Filtered/Incomplete若有完整或按既有规则可恢复的 text/signed-thinking blocks，先 append assistant，再 append error terminal；无内容则只 append terminal。
- protocol/transport/cancel partial继续沿用 Plan 64：只保存 Text、RedactedThinking、有 signature的 Thinking；ToolUse永不作为 partial replay block。
- terminal snapshot只用于 display/read，不加入 `History::messages()` provider replay。

### Outcome 与 agent loop

sampling返回：

```text
{ blocks, usage, outcome }
```

core在任何 history写入/tool dispatch前做 defense-in-depth校验：

- `ToolUse` outcome iff blocks含至少一个 ToolUse；
- 非 ToolUse outcome不能含 ToolUse；
- tool id/name非空且id唯一；input为object；
- provider output只含窄 assistant blocks。

决策：

- `EndTurn`：普通完成。
- `ToolUse`：记录 assistant并 dispatch；工具存在与否仍由既有 dispatcher返回 tool result，不把“模型调用未知工具”误分类成 wire malformed。
- `OutputLimit`：记录非空 partial，按现有上限注入 continuation；不透明 retry/fallback。若最后仍 output-limited，保存最后内容并返回稳定 error terminal。
- `Refused`：记录可用内容，返回 `EndReason::Error("model refused the request")`。
- `Filtered`：记录可用内容，返回 `EndReason::Error("provider filtered the response")`。
- `Incomplete`：记录可用内容，返回带稳定前缀、bounded reason的 `EndReason::Error`。

semantic outcome不进入 primary retry/fallback分支。尤其 refusal不能自动换模型重放；若未来产品要 refusal fallback，必须另立显式、按是否已有 semantic output裁决的计划。

usage只要来自合法 terminal即可记账，包括 Refused/Filtered/Incomplete；malformed/early failure没有可信 final usage时不伪造。

## 5. Native protocol 1.0 与 rollout 兼容

### Native protocol 1.0

保持：

- `PROTOCOL_VERSION = "1.0"` 与 exact handshake；
- JSON-RPC 2.0 envelope；
- `item/started`、`item/delta`、`item/completed`、`turn/started`、`turn/completed` method；
- `turn/completed.params.turn = { id, status, error? }`；
- Refused/Filtered/Incomplete继续投成 `status:"error"` 与 string error；
- `thread/read` terminal仍为现有 `{status,error?}`。

修正但不扩 schema：

- open partial assistant/reasoning在 `item/completed` 内使用现有 `status:"failed"`；正常完整 block仍为 `completed`。
- `item/completed` method表示 item lifecycle已终止，nested status表达成功/失败；不新增 `item/failed`。
- cancel也只能在1.0复用 failed item + aborted turn；独立 `cancelled` item status留给未来版本。

如果 Desktop当前 adapter对 assistant/reasoning status写死为 completed，实施时同步其 kloop专用分支和 contract tests；不为旧错误行为保留 alias。

### Rollout

- 不修改 `Message` JSON shape。
- 不修改 `TurnTerminal {status,error?}` 与 snapshot shape。
- 不写空 assistant line。
- non-success semantic outcome先写非空 assistant内容，再写 terminal；读取顺序稳定。
- fork/resume复制 terminal record但 provider history继续只取 message/compaction line。
- 旧 rollout可继续读取；新实现不要求 migration。

Typed public terminal、provider code、refusal detail、partial/cancelled独立状态都不是本计划的 rollout/native 1.0字段。

## 6. 错误与兼容性策略

### Stable public text

protocol 1.0至少固定以下 category前缀，测试不绑定 provider整段原文：

```text
model refused the request
provider filtered the response
provider returned an incomplete response: <bounded reason>
response remained truncated after <N> continuation attempts
```

未知 event/stop、missing field和closure mismatch沿用 `ProviderFailure::Protocol`，错误只含 rail、字段/事件类别和bounded identity，不回显完整 SSE body、prompt、tool arguments或凭据。

### Backward compatibility

- 请求 wire不变。
- rollout旧数据可读。
- native 1.0 shape不变。
- 正常成功、工具调用和已由 Plan 64处理的 transport retry保持原行为。
- 行为变化只发生在此前被错误标成功、静默忽略、写空 assistant或误标 partial completed的路径。
- provider-specific aliases必须有 fixture证据；不能为“兼容更多代理”恢复默认空 id/name/index/stop reason。

## 7. 实施切片

所有切片在同一 kloop main working tree完成；最终一次 `plan65` commit，不开 feature branch。

### 切片 0：基线与 regression fixtures

- 固定 Plan 64 timeout/cap/single-terminal/retry watermark现状。
- 为已确认缺口先加负例：refusal/filter/incomplete成功误判、Responses delta无done、空 assistant、partial item status、invalid UTF-8、tool required field缺失、truncation恢复用尽。
- 确认正常三 rail、server partial recovery和rollout resume基线不被测试误改。

### 切片 1：Canonical types 与 stream seam

- 新增 `AssistantBlock`、`AssistantOutcome`、output-limit/incomplete reason。
- `StreamEvent::BlockDone`收窄，`Done`改 mandatory typed `Terminal`。
- 更新 `StreamCompletion`、`StreamSink`、`ProviderStream`和mock fixtures。
- 保持 Plan 64 producer ownership、failure terminal和semantic watermark。

### 切片 2：SSE 与三 adapter 状态机

- strict UTF-8 frame decode。
- Anthropic index/block/message terminal严格化与stop mapping。
- Chat choice/tool/finish state严格化与alias mapping。
- Responses output item registry、delta/done一致性、refusal/incomplete mapping。
- 三 rail unknown semantic event与required field fail closed。

### 切片 3：Core sampling/history/outcome

- 窄 block转换、terminal outcome消费与defense-in-depth校验。
- 无delta完整 block补 item lifecycle。
- open partial item以failed收口。
- 空 assistant不落history。
- refusal/filter/incomplete与truncation exhaustion返回正确 error；semantic terminal不进retry/fallback。

### 切片 4：Server、rollout、frontends

- wire层从core item terminal status投影，不再按事件名硬编码 completed。
- 保持native protocol 1.0 shape，补failed partial与stable error E2E。
- thread/read、resume/fork验证message + terminal顺序。
- plain/TUI确认失败partial不悬挂、不重复结束；如需同步Desktop，仅改其kloop专用分支。

### 切片 5：文档与总验收

- 更新 README provider reliability、semantic terminal、empty/partial和protocol 1.0说明。
- 更新 HANDOFF完成记录；若实现产生新的跨层教训，写入交接。
- 跑focused tests、workspace gates、mock smoke与至少三条真实 rail acceptance。
- 回填本计划完成事实、真实验证与提交号；一次提交，不push。

## 8. 测试矩阵

### Canonical seam

- `AssistantBlock`只能转换成四类合法 assistant `ContentBlock`。
- `Terminal`/failure各自exactly once；terminal后consumer为None。
- producer premature close仍产生typed incomplete failure。
- Text/Thinking delta及完整ToolUse都提升semantic watermark。
- Refused/Filtered/Incomplete不会进入retry/fallback。

### SSE

- UTF-8 codepoint跨chunk正常。
- 完整frame内非法UTF-8失败且不产生replacement character。
- EOF残余非法UTF-8分类为non-retryable protocol；合法未闭合frame才分类为incomplete protocol，并继续受semantic watermark约束。
- CRLF、multiline data、comment/keepalive正常。
- unfinished frame/response cap、open/idle/wall guard不回退。
- malformed JSON错误有界，不含完整敏感payload。

### Anthropic

- Text、Thinking+signature、RedactedThinking、ToolUse完整流。
- end_turn/stop_sequence/tool_use/max_tokens/model-context/refusal/pause映射。
- 缺message_start、缺message_delta stop、缺message_stop、stop时open block。
- duplicate/missing index，delta/stop引用unknown或closed block。
- missing/empty tool id/name，invalid/non-object input，duplicate tool id。
- unknown block/event、SSE error。

### Chat Completions

- normal stop、tool_calls、legacy function_call、length、content_filter、finish_reason refusal。
- `delta.refusal + finish_reason:stop`映为Refused并保留拒绝文本；refusal与tool call冲突失败。
- unknown/invalid finish在任何完整BlockDone发布前失败；先前display delta仍按partial semantic failure收口。
- usage-only empty-choice tail与finish同chunk `[DONE]`。
- `[DONE]`/EOF无finish reason失败；unknown/duplicate finish失败。
- finish后semantic delta失败。
- 多tool call交错fragment按index稳定完成。
- missing/conflicting id/name/index、duplicate id、invalid/scalar/array arguments失败。
- reasoning/text/tool mixed合法顺序与outcome/block一致性。

### Responses

- message text、reasoning+encrypted content、function call、多output item。
- refusal content与`refusal.delta/done` + completed → Refused，display/final item逐字一致。
- `content_part.added/done`、`output_text.done`、`function_call_arguments.done`与reasoning part/done的正常顺序；orphan、duplicate、wrong-order逐类失败。
- max_output_tokens/content_filter/其他incomplete reason。
- response.failed/error与completed status mismatch。
- orphan delta、duplicate added/done、wrong item type、terminal时open item。
- delta text/arguments与final item mismatch。
- missing call_id/name/output identity、invalid arguments。
- unknown content part/semantic event。
- `[DONE]`/EOF无completed|incomplete失败。

### Core/history

- 普通empty EndTurn（包括只含被normalizer剔除的空Text/空RedactedThinking/unsigned-empty Thinking）completed但不增加assistant message/rollout line或semantic watermark。
- signed-empty Thinking保留、提升watermark并可进入replay history；合法whitespace Text不被`trim()`误删。
- 非空EndTurn正常final_text和history。
- ToolUse outcome/block双向mismatch在dispatch前失败。
- duplicate tool id与non-object input不dispatch。
- output limit按typed outcome continuation；恢复用尽后error且partial内容可读。
- Refused/Filtered/Incomplete不completed、不fallback；有内容时可恢复，无内容时不写空assistant。
- 完整block无delta仍产生一次start+completed。
- stream error/cancel时open text/reasoning一次failed；已完成block不降级。
- unsigned partial thinking不进replay history，signed thinking保留。
- usage只在可信terminal记账。

### Server/native 1.0

- initialize仍只接受1.0；2.0 mismatch。
- 正常item/turn JSON shape逐对象不变。
- partial open item：一个start、若干delta、一个`item/completed`且nested status failed、一个error turn terminal。
- refusal/filter/incomplete：turn status error、稳定string error、无typed新增字段。
- thread/read能读回非空assistant partial及紧随terminal；空response只有terminal。
- disconnect/cancel/producer failure仍只有一个turn terminal。

### Rollout/resume/frontends

- 旧rollout fixture继续读取。
- 新non-success message/terminal顺序稳定，terminal不进provider history。
- fork复制display terminal但不污染消息前缀语义。
- plain/TUI对“无delta但有non-empty完整block”创建最终输出，对已有delta的completion不重复打印；failed partial保留已收内容、失败样式收口且不悬挂。
- Desktop kloop adapter若受影响，补existing failed status contract与真实binary duplex fixture。

### Retry/fallback回归

- zero-output retryable transport/408/429/5xx仍总计3 attempts。
- first text/reasoning/completed tool后任何failure不replay。
- terminal Refused/Filtered/Incomplete即使zero-output也不fallback。
- output-limit continuation是新round并带recorded history/nudge，不算transparent attempt。
- invalid protocol zero-output不retry，context overflow仍走既有reactive compaction。

## 9. 关键文件

核心修改：

- `rust/crates/protocol/src/lib.rs`
- `rust/crates/provider/src/{lib.rs,stream.rs,sse.rs,failure.rs}`
- `rust/crates/provider/src/{anthropic.rs,openai.rs,responses.rs}`
- `rust/crates/core/src/agent.rs`
- `rust/crates/core/src/agent/sampling.rs`
- `rust/crates/core/src/event.rs`
- `rust/crates/core/src/history.rs`
- `rust/crates/core/src/rollout.rs`
- `rust/crates/server/src/{lib.rs,wire.rs}`

测试：

- `rust/crates/provider/tests/{anthropic.rs,openai.rs,responses.rs,stream_guard.rs}`及同模块unit tests
- `rust/crates/core/src/agent/tests.rs`
- `rust/crates/core/src/rollout.rs` tests
- `rust/crates/server/tests/server.rs`
- plain/TUI受item terminal status影响的render tests
- 如受影响，`桌面前端仓库` kloop专用分支的adapter/contract tests

文档：

- `rust/README.md`
- `docs/plan/HANDOFF.md`
- 本计划完成记录

## 10. 验证

实施阶段至少运行：

```bash
cd kloop
cargo test -p kloop-protocol
cargo test -p kloop-provider
cargo test -p kloop-core agent::tests
cargo test -p kloop-core rollout::tests
cargo test -p kloop-server
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

真实 rail acceptance使用仓库根 `.kloop/env.local`，不得把 key、proxy credential、raw authenticated response或含secret header写入日志/fixture/提交：

- Anthropic：普通text、tool use、output limit或可控fixture代理的refusal terminal。
- Chat Completions：普通text、tool use、finish reason映射。
- Responses：普通text、tool use、`output_item.done`/completion closure。

无法稳定由真实模型触发的 refusal/filter/incomplete/malformed wire必须用本地 loopback fixture权威覆盖；不得把“真实调用没触发”冒充负路径已验收。真实API若因rate limit失败，按项目已有fallback纪律记录实际执行的rail/model，不把替跑说成原模型通过。

## 完成记录（2026-08-07）

- Canonical seam 已收窄为四类 `AssistantBlock` 与 mandatory `AssistantOutcome`；mock 与 core 在发布、落 history 和 dispatch 前都复核 tool identity、唯一 id、object input 及 outcome/block 双向一致性。
- 三条 adapter 已改为显式状态机：Anthropic验证 message/block index 与 stop；Chat验证 single choice、tool index、placeholder identity、finish alias 与 `[DONE]` 顺序；Responses验证 output item/part、refusal字段、arguments done、final identity/accumulator与terminal status。合法 semantic terminal保持低延迟；只有 terminal 前真实 EOF 的 residual 才按 strict UTF-8/incomplete分类。
- Core、server、plain、headless与TUI统一了无delta completion和failed partial lifecycle；空EndTurn不写空assistant，所有Text block按顺序组成final text，transport/semantic partial在human headless可读，TUI不会把仍live但非末尾的display cell冻结进scrollback。
- Desktop `kloop` 专用分支提交 `70db26456323cab78c043dfb5f63a7c0546977e3`：normalizer按wire item status闭合reasoning，terminal snapshot与history调用同步；4组kloop Bun contract共19 tests和`vue-tsc --noEmit`通过，未修改app脏main。
- Rust focused provider/core/server/CLI/TUI tests、`cargo fmt --all --check`、workspace all-targets Clippy、`cargo test --workspace`、mock headless smoke与`git diff --check`均通过；provider suite为防连接复用回归连续运行两次通过。
- 从仓库根`.kloop/env.local`仅source凭据、用隔离HOME执行真实验收：Anthropic、OpenAI-compatible Chat、OpenAI Responses的普通text与`todo_write` tool-use共六条均通过；未记录key、header或raw authenticated response。refusal/filter/incomplete/malformed wire由本地loopback fixture覆盖。

## 非目标

- 不重做 Plan 64 timeout、cap、HTTP taxonomy、Retry-After、attempt budget或producer ownership。
- 不引入统一所有provider wire event的大而全AST。
- 不把 Responses item identity直接暴露成native item id。
- 不实现server tools、`pause_turn` continuation、refusal fallback或跨provider自动重放。
- 不新增native protocol 2.0、版本协商、双栈、typed public error/provider diagnostic。
- 不迁移rollout schema，不重写旧terminal记录。
- 不改变工具permission/sandbox/approval语义。
- 不因兼容代理而恢复synthetic tool id、missing finish→end_turn、unknown status→completed或lossy UTF-8。

## 完成标准

- 三 rail只在收到各自合法semantic terminal且状态机全部闭合后发布typed `Terminal`。
- core不再读取free-form stop reason决定成功/工具/截断/拒绝。
- refusal/filter/incomplete/truncation exhaustion不会成为completed turn。
- Responses delta/history闭合一致，orphan/mismatch不能成功。
- malformed tool structure在dispatch前失败，provider output类型不再允许Image/ToolResult。
- provider path不写空assistant；合法empty EndTurn仍能稳定结束。
- open partial item在native 1.0以failed收口；正常block、turn与thread/read shape保持兼容。
- Plan 64 retry watermark、single terminal、guard与取消不变量全部回归通过。
- README/HANDOFF/本计划完成记录同步，全部门禁与真实rail acceptance按实际结果记录。
- kloop main一次提交，信息包含`plan65`；不push。
