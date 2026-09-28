# Plan 207 — 换了 provider,工具调用的 id 也得能过关 ✅

> **已完成(2026-09-28)**,提交号见第七节。开工实测推翻了第二、三节的几处设计,**以第七节为准**。

> 来源:2026-09-24 读 `refs/pi`(`earendil-works/pi@d5629e2`,MIT)后与用户逐条定的,
> 出处见 `refs/README.md`「Pi 全面复查(2026-09-24)」第 3 条。

## 一、为什么

kloop 的 canonical 历史是 Anthropic 形的,三条 rail 各自在请求投影时翻译它,**工具调用 id 一律原样透传**:

- Anthropic:`anthropic::messages_value`(`provider/src/anthropic.rs:63`)把 `message.content`
  直接序列化,`tool_use.id` / `tool_result.tool_use_id` 都是历史里的字节。
- Responses:`responses::to_input_items`(`provider/src/responses.rs:76`)出 `function_call.call_id`
  (`:108`)与 `function_call_output.call_id`(`:148`)。
- Chat:`openai::to_openai_messages`(`provider/src/openai.rs:67`)出 `tool_calls[].id`(`:82`)、
  `tool.tool_call_id`(`:146`),还有搬走图片时那句 `Tool output for call_id {tool_use_id}:`(`:152`)。

入站只查"非空、同一条消息里不重复"(`provider/src/lib.rs:323-362`、`core/src/agent.rs:1269-1281`),
**不查字符集和长度**——各家给什么就存什么。同一 rail 上这没问题:id 是那家自己发的,它当然认。
问题出在 `/provider` 切换(plan 92)或以别的路由 reopen 之后:`provider_request_view`
(`core/src/history.rs:894`)只按 provenance 处理 reasoning,工具 id 照搬到新 rail。

- Anthropic 要求 `tool_use.id` 匹配 `^[a-zA-Z0-9_-]+$`(pi 注释称上限 64,`transform-messages.ts:59-63`)。
  Kimi 一类 Chat 兼容服务发 `functions.read_file:0` 形的 id,带 `.` 和 `:`,切到 Anthropic 就 400——
  而且是**整个会话从此每一轮都 400**,因为那段历史不会自己消失。
- 同一类服务的 id 按"本轮第几个调用"编号,**跨 turn 会重复**:第 3 轮和第 7 轮都可能有
  `functions.read_file:0`。kloop 只拦同一条消息内的重复,跨消息的重复原样进了历史。

pi 的做法:每条 rail 带一个 `normalizeToolCallId`,`transformMessages` 只在"来源不是同一模型"时调用它,
并把 tool_call 的新 id 记进一张表,再改写对应的 toolResult(`transform-messages.ts:82-89,136-142`)。
Anthropic 那一份是 `id.replace(/[^a-zA-Z0-9_-]/g, "_").slice(0, 64)`(`anthropic-messages.ts:1216`)。

**不照抄的地方**:

1. pi 的规范化**不是无碰撞的**——`a.b` 与 `a:b` 都变 `a_b`;两个 70 字符、前 64 位相同的 id 截断后相同。
2. pi 的表按原 id 做 key,**跨消息重复的 id 会互相覆盖**(后一个的映射盖掉前一个)。
3. **"Responses 要 `fc_` 前缀"这条前提对 kloop 不成立。** `fc_` 约束的是 `function_call` 输入项的
   `id` 字段(pi `openai-responses-shared.ts:165-177` 处理的是它自己存的 `call_id|item_id` 复合 id
   的后半段);kloop 入站只留 `call_id`、丢掉 item id(`responses.rs:792`),出站也只发 `call_id`
   不发 `id`(`responses.rs:106-111`)。所以 Responses 这一侧没有 `fc_` 问题,只剩 `call_id` 本身的
   字符集/长度,这一点需要核(见第四节)。

## 二、形状

### 2.1 映射放在 provider 适配器,按目标 rail 的规则,只改不合规的

每条 rail 一个纯函数 `wire_tool_id(rail, original, occurrence) -> String`,在上面三个投影函数里
对 `ToolUse.id` 与配对的 `ToolResult.tool_use_id` 同时使用:

- **合规且在本请求里第一次出现 → 原样。** 于是同一 rail 上的会话、以及 Anthropic `toolu_…`、
  OpenAI `call_…` 这类本来就合规的 id,**请求字节与今天完全相同**,prompt cache 不受影响,
  现有测试不用改。
- **不合规,或是本请求里第 k 次(k≥1)出现的重复 id → 改写**为
  `{前缀}_{16 位十六进制}`:前缀是原 id 把非 `[A-Za-z0-9_-]` 换成 `_`、截到
  `64 − 1 − 16 = 47` 字符(可读,排查时认得出来源);十六进制取 `sha256("{original}\n{k}")` 的前 8 字节
  (provider crate 已依赖 `sha2`,`lib.rs:449` 在用)。
- **只看 id 本身和它是第几次出现,不看来源 provenance。** 规则是 rail 的知识,放在知道 rail 的地方;
  合规 id 不动,所以不需要 pi 那个"是不是同一模型"的判断。compact 的摘要请求
  (`core/src/compact.rs:439` → `stream_attempt`)与子 agent 请求都走同一个投影,自动覆盖。

**确定性与 cache**:映射是 (rail, 原 id, 序号) 的纯函数,请求逐轮追加时前面的映射不变,字节稳定。
序号只在压缩改写历史后才可能变,而那时前缀 cache 本来就断了。

**无碰撞**:投影完成后,对整个请求里的 tool_use id 做一次唯一性检查;若映射结果与另一个 id 相撞
(要 64 位哈希前缀碰撞,或改写结果恰好等于某个原样保留的合规 id),**在发出前以 protocol 错误失败**,
不静默发送一个配错对的请求。

### 2.2 tool_result 跟着它的 tool_use 走,不查全局表

跨消息重复存在,所以不能像 pi 那样用"原 id → 新 id"的全局表。配对按位置:一条 user 消息里的
`tool_result` 用**紧邻的上一条 assistant 消息**的映射查;查不到(孤儿结果,kloop 压缩不拆对,
正常不会出现)就按 `occurrence = 0` 直接算。Chat 那句搬图片的说明文字用映射后的 id。

### 2.3 不动的东西

- **canonical 历史、rollout、`History::items` 一个字节都不改。** 映射只活在 `serde_json::Value` 里,
  与 `cache_control` 同一待遇(`anthropic.rs:58-62` 的注释说的就是这条边界)。
- **工具执行、UI、事件、`request_reduction` 的桩**(按 `tool_use_id` 冻结,DESIGN「Request-time
  reduction」)都用原 id。模型在新 rail 上看到改写后的 id,但它新发的调用用的是新 rail 自己的 id,
  不会回头引用旧 id;即便引用,也只是文本。
- **Mock rail 不映射**:它没有规则,`MockRequest` 继续记录 canonical 消息。
- **入站不收紧**:不拒绝、不改写 provider 发来的 id。那是那家的合法输出,只是换 rail 时要翻译。

## 三、各 rail 的规则(开工时核定后写死成常量与注释)

| rail | 约束 | 依据 |
|---|---|---|
| Anthropic Messages | `^[A-Za-z0-9_-]{1,64}$` | 官方文档的字符集 + pi 的 64 上限 |
| OpenAI Responses | 待核:`call_id` 的字符集与长度 | pi 对它用同一套 sanitize + 64(`openai-responses-shared.ts:153-157`) |
| Chat Completions | 待定:见第四节第二问 | pi 只在 `provider === "openai"` 时截到 40(`openai-completions.ts:1216`) |

所有 rail 都做"跨消息重复 → 改写",这一条不依赖任何文档:重复 id 在哪家都是在赌对方的配对逻辑。

## 四、开工时必须问用户的点

**1. 映射放在 provider 适配器(推荐),还是 core 的 `provider_request_view`?**

- **适配器(推荐)**:规则是 rail 的知识;合规 id 不动,所以不需要 provenance;compact、子 agent
  一并覆盖;core 的 request view 仍然等于 canonical 历史,`projected_continuity`
  (`history.rs:398-418`)对 `Preserved/Filtered` 的判定不受影响。
- core:能拿到来源 provenance,可以做到"只在跨 rail 时改",但得把三家的规则搬进 core,而且
  `projected_continuity` 会因为 id 改写误判成 `Filtered`,要另外区分。

**2. Responses 与 Chat 两条 rail 的规则,要不要先用真实 API 核一次?**

- **推荐:核。** 用户给代理与 key,开工时各发一条带 `functions.x:0`、70 字符 id 的最小请求,
  看 Responses 的 `call_id`、OpenAI 官方 Chat 的 `tool_calls[].id` 实际拒什么,再定第三节两行。
  核不了就退到保守做法:Responses 与 Anthropic 同规则(pi 也这么做),Chat 只做重复消解、不收紧字符集
  ——Chat 兼容服务五花八门,收紧反而可能把某家自己认的 id 改掉。

## 五、测试

provider 单元测试(整对象断言 JSON):

- `messages_value`:历史含 `functions.read_file:0` 的 tool_use + 配对 tool_result → 两处都变成
  同一个 `functions_read_file_0_<hex>`;断言整个 `messages` 数组。
- 同上历史,两个 turn 各有一个 `functions.read_file:0` → 第一次 k=0、第二次 k=1,两对分别配对、互不相同。
- 合规 id(`toolu_01…`、`call_abc`)→ 输出与现有快照逐字节相同(直接复用 `anthropic.rs:524-539`、
  `responses.rs:1433-1476`、`openai.rs:598-636` 的现有断言,不改期望值)。
- 70 字符合规 id → 改写后长度正好 64,前缀 47 字符。
- `a.b` 与 `a:b` 同一请求 → 两个不同的改写结果(pi 会撞成 `a_b`)。
- 映射结果与请求里一个原样 id 相撞(测试里用 `#[cfg(test)]` 注入固定哈希)→ 发出前 protocol 错误,
  wiremock 收到 0 个请求。
- Chat:tool_result 带图片 → 搬图说明文字里是映射后的 id,与 `tool_call_id` 一致。
- 同一段历史投影两次 → 字节相同(确定性)。

集成测试(`provider/tests/anthropic.rs` 的 wiremock 模式,`received_requests()` 取请求体):

- 一段 Chat 形 id 的历史经 `stream_attempt` 发到 Anthropic → 请求体里 tool_use / tool_result id 合规且配对。

core 测试:

- 会话在 Chat 路由上产出 `functions.x:0` 形的 tool_use,`/provider` 切到 Anthropic 后
  `provider_request_view` 仍等于 canonical(无 reasoning 时),rollout 里的 id 未变;
  切换结果仍是 `Preserved`。

## 六、完成时要一起做的

- `rust/DESIGN.md`:先读 provider switch 那段(grep
  `Canonical history is never rewritten by a switch`)和 provider rail 注释块(grep
  `cross-provider/family/model reasoning`),看"request view 只因 reasoning 而与历史不同"的说法是否仍成立;
  再在 provider 适配器的描述处补一段:工具 id 在线上按目标 rail 规则翻译、合规不动、跨消息重复消解、
  canonical 不改。已有句子若把"投影 = 透传"写死了就改写,不要只追加。
- `refs/README.md` Pi 一节第 3 条:把"Responses 要 `fc_` 前缀"更正为"对 kloop 不适用(不发 item id)",
  并注明已由 plan 207 吸收。
- HANDOFF.md:若开工时核 API 得到了新事实(第四节第二问),记一条教训。
- 本文件补 ✅ 与提交号。

## 七、落地(2026-09-28)✅

**开工两问**:① 映射放 provider 适配器(照推荐);② 用真实 API 核(照推荐)。用户指定用本机
config 里已配的网关 provider:Messages、Responses 各一个,Chat 走 Responses 那个网关的
`/chat/completions`。

### 实测(每个用例重复 4 次;同一网关会把同一请求分到不同上游,结果不总一致)

| rail | 被拒 | 收 |
|---|---|---|
| Messages | `.` `:` `\|`——只有一家上游校验,haiku 次次拒、sonnet 次次收、opus 两拒一收 | 长度到 4096 |
| Responses(gpt 线) | 无 | 任何字符、长度到 4096 |
| Responses(国产模型线) | 65 字节:一家上游明说长度须在 1–64,4 次拒 3 次 | 任何字符 |
| Chat(glm / deepseek) | 无 | 任何字符、长度到 4096 |
| 跨消息重复 id | **三条 rail、所有上游都收** | — |

### 由此改掉的设计(与用户逐条确认过)

- **第三节的表**:Messages = `[A-Za-z0-9_-]` + 64(字符集实测;64 沿用 pi,未测到但改写无代价);
  Responses = **只限 ≤64 字节,字符不限**;Chat = **不映射**,`openai.rs` 一行没改逻辑。
- **不做跨消息去重**(推翻第三节末句与 2.1 的"第 k 次出现改写"):没有一家拒;Chat 兼容服务按轮
  编号,跨轮重复是它自己的格式,去重会在同一 rail 上改掉它自己发的 id。于是 `occurrence` 参数没了,
  改写只是 id 的纯函数 `{前缀≤47}_{sha256(id) 前 8 字节 hex}`。
- **2.2 整节不需要**:tool_use 与 tool_result 各自对同一个原 id 算出同一个值,自然配对;孤儿结果也一样。
- **碰撞检查改为单射检查**:允许同一原 id 多次出现,只拒"两个**不同**原 id 算出同一个线上 id"
  (64 位哈希碰撞,或历史里已原样存着另一个 id 的改写结果)。测试不用注入哈希:`a.b` 与字面的
  `a_b_2e7336dc8eba87ef` 同时出现即可。
- 用户追问"中途换 provider 呢":映射不存档、每次请求从 canonical 现算,A→B→A 不累积,回到原 rail
  时发出的字节与从未离开时相同。

### 代码与测试

- 新 `provider/src/tool_id.rs`(`ToolIdRule::{Anthropic,Responses}` + `WireToolIds`);
  `anthropic::messages_value`、`responses::to_input_items` 改为返回 `Result`,`stream_attempt`
  在拼请求体前失败(与 Chat 投影失败同一条路),wiremock 收不到请求。
- 测试:`tool_id` 5 条(只改拒收的、64 边界、`a.b`/`a:b` 分开、重复同值、碰撞报错);
  `anthropic` 2 条(外来 id 改写且配对 + 两次投影字节相同;碰撞);`responses` 1 条;
  `openai` 1 条(Chat 原样,含跨轮重复);`tests/anthropic.rs` 集成 1 条(改写后的请求体 + 碰撞时 0 请求);
  core `history.rs` 1 条(Chat→Anthropic 切换仍 `Preserved`、request view 与 rollout 等于 canonical)。
  现有断言期望值一个没改,只加了 `.unwrap()`。

**提交**:见 git log `feat(plan207)`。
