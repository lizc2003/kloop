# Plan 206 — 超限、限流、欠费,三件事要分开认

> 来源:2026-09-24 读 refs/pi(earendil-works/pi@d5629e2,MIT)后与用户逐条定的,出处见
> refs/README.md「Pi 全面复查(2026-09-24)」第 1、2 条。pi 路径相对 `refs/pi/packages/`。

## 一、为什么

provider 报错时,kloop 要回答三个不同的问题,现在都答得太粗:

1. **是不是上下文超长?** 是,就走被动压缩(`SampleError::Overflow`,`core/src/agent/sampling.rs:281`)。
   判据只有 `is_overflow_message`(`provider/src/lib.rs:236-241`)里的 3 个子串:
   `prompt is too long` / `context_length_exceeded` / `maximum context length`。Anthropic、OpenAI
   与多数走 LiteLLM/OpenRouter 的服务能认出来;Gemini(`input token count … exceeds the maximum`)、
   xAI(`maximum prompt length is N`)、Together(`is longer than the model's context length`)、
   llama.cpp、LM Studio、Kimi、Mistral、DashScope、Ollama 等认不出——**认不出就不压缩**,
   这一轮直接以 400 失败。pi 在 `ai/src/utils/overflow.ts:37-62` 攒了约 25 条正则。
2. **是不是暂时的?** HTTP 路径的重试只看状态码(`provider/src/failure.rs:123-131`:408/429/5xx)。
   响应体写着 `insufficient_quota`、`quota exceeded`、`billing` 的 429 是**欠费/配额用完**,
   重试不会好,却照样按 `MAX_ATTEMPTS = 3`(`sampling.rs:84`)白等两轮退避;遇到带大 `Retry-After`
   的配额 429,每次还要等满 60s 上限(`provider/src/stream.rs:38, 444-455`)。流内错误那条路已经拦了:
   `is_fatal_stream_error`(`lib.rs:306-320`)里有 `insufficient_quota` 与 `billing_error`。
   两条路判据不一致。
3. **服务端自己说了什么?** OpenAI/Anthropic 的 SDK 都尊重 `x-should-retry`(显式要/不要重试)和
   `retry-after-ms`(毫秒级等待),pi 照抄了 SDK 策略(`ai/src/utils/provider-retry.ts:23-35, 51-67`)。
   kloop 只读 `Retry-After`(`stream.rs:355-359`)。

反过来还有一个坑:模式表越宽,越容易把**限流**认成超长。pi 的例子是 Bedrock 的
`Too many tokens, please wait before trying again`——命中 `/too many tokens/`,却是节流。
把限流当超长,就是用一次昂贵的摘要去"修"一个等几秒就好的问题。所以 pi 另有一张反向排除表
(`overflow.ts:75-79`)。

## 二、形状

### 2.1 一个分类入口,两条路共用

新建 `provider/src/classify.rs`(名字开工时可调),替掉 `is_overflow_message`,对外只暴露:

- `is_overflow_text(text) -> bool`:先过**排除表**(命中即 false),再过**超长表**。
- `is_quota_text(text) -> bool`:配额/计费表。

四个调用点全部改用它,不留旧函数:

| 调用点 | 现在 | 改后 |
|---|---|---|
| HTTP 错误体 `stream.rs:361` | 任何状态码都查 | 见 2.3 |
| Anthropic 流内 `error` 帧 `anthropic.rs:450` | 查 `error` 的 JSON 串 | 同,换函数 |
| Chat 流内 `error` `openai.rs:436` | 同上 | 同,换函数 |
| Responses `response.failed` / `error` `responses.rs:914, 923` | 同上 | 同,换函数 |

流内错误还要多一道:**label 本身就说是限流/过载**(`rate_limit_error`、`overloaded_error`、
`rate_limit_exceeded`)时不判超长,直接交给 `stream_error`。label 比 message 可靠。

### 2.2 超长表与排除表

从 `overflow.ts:37-62` 移植,**去掉两条**:

- **`/request_too_large/`**:Anthropic 的 413 `request_too_large` 是**请求体字节**超限(图片、大附件),
  不是 token 超限;压缩按 token 估算裁历史,未必能把字节降下来,还白付一次摘要。kloop 现在把它当
  不可重试的普通错误(HTTP 413 不在白名单;流内 label 在 `is_fatal_stream_error` 里),**保持不变**。
  这里是有意与 pi 分歧。
- Cerebras 的 `400/413 (no body)` 特判(`overflow.ts:64, 145`):依赖 provider 身份,kloop 的
  OpenAiCompat 不知道自己连的是谁。不搬。

其余照搬,大小写不敏感;注释写上每条对应哪家(与 pi 一样),便于以后补。现有 3 个子串都被新表覆盖
(`context_length_exceeded` 对应 `/context[_ ]length[_ ]exceeded/`,`maximum context length`
对应 LiteLLM/OpenRouter 那两条)——开工时逐条对照,确认没有回退。

排除表照搬 `overflow.ts:75-79`:`^(Throttling error|Service unavailable):`、`rate limit`、
`too many requests`。

### 2.3 HTTP 路径的判定顺序(`stream.rs` 错误分支)

拿到 status、headers、原始 body(脱敏前)后,依次:

1. **超长**:`is_overflow_text(body)` 且状态码是 4xx 但**不是 429**(见问题 1)→ `context_overflow()`。
2. **`x-should-retry: false`** → 不可重试。
3. **配额**:`is_quota_text(body)` → 不可重试(状态码保留,仍是 `Http { status }`)。
4. **`x-should-retry: true`** → 可重试(包括不在白名单里的状态码,如 409)。
5. 否则走现有状态码白名单。

配额表照 pi `ai/src/utils/retry.ts:7-24` 取通用的四条:`insufficient_quota`、`out of budget`、
`quota exceeded`、`billing`;OpenCode 专属的 `GoUsageLimitError` 等不搬。

`failure.rs` 的 `ProviderFailure::http(status, message, retry_after)` 保留"只看状态码"的语义
(core 测试在用,`agent/tests.rs:2613`、`compact.rs:1024`);另加一个 crate 内构造器接收已判好的
可重试性。**不要用 bool 位置参数**——用一个小枚举(如 `HttpRetry { ByStatus, Never, Always }`)。
`retry_after` 仍只在可重试时保留(`failure.rs:129` 那条 filter 不变)。

### 2.4 `retry-after-ms`

`stream.rs:355-359` 先读 `retry-after-ms`(非负浮点毫秒),有效就用它,否则回落 `Retry-After`。
两者都受 `MAX_RETRY_AFTER`(60s)封顶。解析函数与 `retry_after_delay` 并排,同样纯函数可单测。
core 侧不用改:`retry_delay`(`sampling.rs:89-96`)已经让 provider 给的延迟优先。

### 2.5 不在本 plan

- **静默溢出**(`overflow.ts:151-168`:请求成功但 `usage.input` 超过窗口;或 `length` 停止且输出 0):
  provider 层不知道窗口大小,要在 core 拿到 usage 后判;而且请求已经成功、回复可能是截断输入上的
  胡话,处理方式(重试?压缩后重发?)本身要另议。低优先,需要时另立 plan。
- **409 默认重试**:pi 跟 SDK 重试 409,kloop 不。本 plan 只在服务端显式 `x-should-retry: true`
  时放行,不改白名单。

## 三、开工时必须问用户的点

1. **HTTP 路径上,429 和 5xx 的错误体还查不查超长?**
   - **不查(推荐)**:超长是请求本身的问题,服务端一律回 400/413/422 一类;429 的
     `Too many tokens, please wait` 恰恰是 pi 排除表漏掉的形状(排除表只认 Bedrock 的前缀),
     按状态码挡掉最稳。流内错误没有状态码,仍靠排除表 + label。
   - 照 pi 不分状态码:与 pi 一致,但依赖排除表写全。
2. **模式表用 `regex` crate 还是手写匹配?**
   - **加 `regex`(推荐)**:`Cargo.lock` 里已有 `regex 1.12.4`(传递依赖),不引入新下载;
     25 条带 `\d+`、可选词的模式手写会又长又容易错,和 pi 逐条对照也更直观。用 `RegexSet`
     一次编译(`LazyLock`)。
   - 手写子串/小解析器:provider crate 不加直接依赖,代价是和 pi 的表无法逐条对照。
3. **配额 429 带的 `Retry-After` 超过 60s 时怎么办?** 现在是封顶到 60s 照等(`stream.rs:454`)。
   配额体已由 2.3 第 3 步挡掉,剩下的是"体里没写 quota、但让你等一小时"的 429。
   - **维持封顶等待(推荐)**:本 plan 只管"认",不改等待策略;真遇到了再议。
   - 照 pi(`provider-retry.ts:37-49`)超过上限立即失败并在消息里写"服务端要求等 Ns"。

## 四、测试

整对象断言优先;provider 层用 wiremock(`provider/tests/*.rs` 现有写法),分类函数用单测。

**`classify.rs` 单测**(表驱动,输入列表映射成 `Vec<bool>` 后一次 `assert_eq!`):

- pi 注释里每家的示例消息(`overflow.ts:11-35`,Anthropic `request_too_large` 那条除外)→ 全为 true。
- 反例 → 全为 false:`request_too_large` 的 413 体、`Throttling error: Too many tokens, please wait`、
  `Rate limit reached for gpt-4o`、`429 Too Many Requests`、一段普通的 400 `invalid_request_error`。
- 配额表:`insufficient_quota`、`You exceeded your current quota … billing details` → true;
  `rate limit exceeded` → false。
- `retry-after-ms`:`"1500"` → 1.5s;`"0.5"` → 0.5ms;`"-1"`、`"soon"` → None;`"600000"` → 60s。

**wiremock(每条断言整个 `ProviderFailure` 的 kind / retryable / retry_after)**:

- Chat 400,体为 Together 形 `The input (300000 tokens) is longer than the model's context length (262144 tokens)`
  → `ContextOverflow`(旧实现是 `Http { status: 400 }`,这条是回归证明)。
- Anthropic 413 `request_too_large` → `Http { status: 413 }`,不可重试,**不是** `ContextOverflow`。
- Chat 429 `insufficient_quota` 体 + `Retry-After: 30` → `Http { status: 429 }`,不可重试,`retry_after == None`。
- Chat 429 体 `Too many tokens, please wait` → 可重试,不是超长(随问题 1 的答案定预期)。
- Chat 429 + `x-should-retry: false` → 不可重试;409 + `x-should-retry: true` → 可重试。
- Chat 503 + `retry-after-ms: 1500` + `Retry-After: 20` → `retry_after == 1.5s`(ms 优先)。
- Anthropic 流内 `error` 帧,`type: rate_limit_error`、message 含 `too many tokens` → 可重试的
  `Protocol`(`incomplete_protocol`),不是超长。
- Responses `response.failed`,message 为 Gemini 形 `input token count (1196265) exceeds the maximum`
  → `ContextOverflow`。
- 现有 `http_overflow_maps_to_overflow_error`(`provider/tests/anthropic.rs:722`、`openai.rs:482`)、
  `http_status_and_retry_after_remain_typed`(`openai.rs:629`)、`failure.rs:237` 的白名单单测保持通过。

**core**:不可重试的 429 只发一次请求——若 core 测试能直接构造分类后的失败就在
`agent/tests.rs` 加一条请求计数断言;构造器是 crate 内的就不加,由 `sampling.rs:165` 现有
`!is_retryable()` 分支与其测试覆盖。

## 五、完成时要一起做的

- `rust/DESIGN.md` 两处,**先读现在写的还成不成立**:
  - 第 150-158 行附近 "Provider sampling is bounded and typed" 一段:现写 "HTTP 408/429/5xx … retry"
    与 "`Retry-After` is honored up to 60s",要改写成"状态码白名单 + 配额体排除 + `x-should-retry`
    优先 + `retry-after-ms` 优先于 `Retry-After`",并说明 HTTP 与流内两条路的配额判据现在一致。
  - 第 298-300 行 **Reactive** 一条:现写 "(`prompt is too long` / `context_length_exceeded`)",
    改为"按一张各家超长文案表认、另有限流排除表;`request_too_large` 是字节超限、有意不算",
    以及问题 1 定下的状态码边界。
- HANDOFF.md:若有新教训则记(候选:"模式表越宽越要配反例表"、"字节超限与 token 超限是两回事")。
- 本 plan 补 ✅ 与提交号。
- `refs/README.md` Pi 节第 1、2 条标注"已由 plan 206 吸收";**若 Pi 节候选均已定案**,按该节
  约定退休 `refs/pi` 本地 clone(确认 HEAD 仍是 `d5629e2`、工作树干净后删除)。

## 六、完成记录

✅ 2026-09-28,提交 `099587e`。开工问答三条都照推荐(用户逐条「同意」):① HTTP 上 429 与 5xx 的
错误体不查超长;② 加 `regex`(workspace 登记一行,`Cargo.lock` 只多 provider 的一条依赖,无新下载);
③ 超过 60s 的 `Retry-After` 维持封顶照等。

- **`provider/src/classify.rs`**:超长表、排除表、配额表(`RegexSet` + `LazyLock`,大小写不敏感)与
  `stream_failure`。原 `lib.rs` 的 `stream_error` / `is_fatal_stream_error` 搬进来,并把超长判断合进去,
  四个流内调用点各变成一次调用;`is_overflow_message` 删除。
- **HTTP 路径**(`stream.rs`):照 2.3 的顺序;可重试性经 `ProviderFailure::http_decided(…, HttpRetry)`
  (crate 内),`http()` 保留"只看状态码"并委托给它。`retry-after-ms` 在 f64 里先封顶再转 `Duration`,
  `1e300` 不 panic,`NaN`/`inf`/负数视为无效、回落 `Retry-After`。

与 plan 的出入(都是开工核对时发现的):

1. **超长表多留一条 kloop 旧子串 `maximum context length`**。第 2.2 节说"现有 3 个子串都被新表覆盖",
   逐条对照后不成立:pi 里含这几个字的三条都要求具体结构,`maximum context length is 128,000 tokens`
   这种千分位写法 `\d+` 就不认。为不回退保留旧子串,注释写明它比那三条宽。
2. **排除表第一条没有照搬锚点**。pi 的 `^(Throttling error|Service unavailable):` 锚的是 pi 自己渲染
   Bedrock 错误时加的前缀;kloop 匹配的是原始 wire 文本(多为 JSON 串),`^` 永远不中。改成不锚定的
   `throttling`、`service unavailable` 两条。
3. **流内路径也读配额表**。2.1 的表只写了"换函数",但第五节要 DESIGN 写"两条路配额判据一致":
   label 不致命(如 `server_error`)而文本是配额的,现在也不可重试。
4. `x-should-retry: true` 挡不住配额体(2.3 第 3 步先于第 4 步),测试钉住了这一条。
5. core 没加测试:`http_decided` 是 crate 内构造器,按第四节约定由 `sampling.rs` 现有 `!is_retryable()`
   分支及其测试覆盖。
6. DESIGN 那段 open 超时写的是 45s,实际早已是 300s(`STREAM_OPEN_TIMEOUT`),改写时一并更正。

测试:`classify.rs` 三条表驱动(24 种各家超长写法全 true,真实测试后加到 27;`request_too_large` 体、Bedrock 限流、
`Rate limit reached…`、`429 Too Many Requests`、普通 400 全 false;配额三 true 一 false);`stream.rs`
`retry-after-ms` 九种输入整体断言;wiremock:Chat 一条八个 HTTP 响应的 (kind, retryable, retry_after)
整体断言(Together 400 → 超长、配额 429 丢 `Retry-After`、`Too many tokens` 429 可重试、503 带超长字样
不算超长、`x-should-retry` 两向、`true` 挡不住配额、`retry-after-ms` 优先)、Chat 流内 `server_error`
带配额文本不可重试、Anthropic 413 `request_too_large` 不是超长、Anthropic 流内 `rate_limit_error` 带
`too many tokens` 可重试且不是超长、Responses `response.failed` Gemini 写法 → 超长。原有的超长、
`Retry-After`、白名单测试原样通过。`make check` 全绿。

**真实测试(2026-09-28,用户「真实测试了吗」「同意」,随后点名再测两个模型)**:默认 provider
(Responses 线路)下配置的三个模型，约 400 万字符的随机词输入。

| 模型 | 状态码 | 文案 | `099587e` 认得出吗 |
|---|---|---|---|
| `deepseek-v4.1-flash`(默认) | 400 | `Input exceeds the context limit (N tokens). Please shorten the input.` | 否 |
| `deepseek-v4-flash-0731` | 400 | `Input length N exceeds the maximum length M.` | 否 |
| `glm-5.3-flash` | 400 | `Total prompt tokens exceed max_prompt_tokens.` | 否 |

三个都经火山方舟，都没有 `x-should-retry` / `retry-after-ms`。**新表和旧的 3 个子串一条都不中**——提交
`099587e` 之后，这三个模型超窗口照样是普通 400、这一轮直接失败。状态码都是 400,第三节问题 1 的状态码
边界对它们成立。补三条(`exceeds the context limit`、`input length \d+ exceeds the maximum length`、
`prompt tokens exceed max_prompt_tokens`,后两条写具体，免得"某参数超过最大长度"一类校验错误被当成超长),
提交 `fix(plan206): recognize Volcengine Ark's overflow wordings`。

- 用 release 构建跑 `--headless`:修之前默认模型报 `provider http error … 400`;修之后三个模型都先出
  `context window exceeded; compacting and retrying`,再以 `reactive compaction made no changes` 结束
  (只有一条消息，没得折)——认出超长、进被动压缩这条链路在真实 provider 上通了。换模型是临时 HOME 里放一份
  改了 `model` 行的配置副本(kloop 只读 `~/.kloop/config.toml`),测完即删。
- 同一网关的另一条上游对超长输入**不报错**,等首包超时后回 504;请求随机落到哪条上游。这种没有任何超长
  信号，只能按 5xx 重试、白等两次。本 plan 不处理，记在这里。
- 没做的：带可折叠历史的完整被动压缩(要真付约百万 token 的输入费;压缩机制本身本 plan 没动)。413、
  配额用完没法在真实环境里造。

- `refs/pi` **未退休**:207–212 尚未完成。
