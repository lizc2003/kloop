# Plan 253 — provider 流式读取与请求构造

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：✅ 已完成(2026-11-05)。行号是 2026-10-28 的读数,开工先自己量;标「待复核」的条目
> 开工第一步先读代码确认，不成立就在本文件划掉并写明原因。三条轨的去重与解析性能另见 plan 254。

## 问题

1. **Chat 轨一见 `finish_reason` 就停止读流，尾部 usage 帧读不到**(已复核)。
   `provider/src/openai.rs:520-522` `if outcome.is_some() { frames.stop(); }`;`SseFrames::stop`
   (`stream.rs:286-311`)只交付当前 chunk 里已解析的帧。`include_usage` 的权威 usage 在之后
   单独的 `choices: []` 帧里(`openai.rs:445-449` 的注释自己也这么说),上游分开 flush 时
   Terminal 的 usage 为 None,`note_usage`、`/cost` 与预测压缩都退回估算。plan 163 那个网关碰巧
   把两帧放在同一 chunk;`tests/openai.rs` 的夹具都用 `set_body_raw` 一次性下发。
2. **压缩请求在 Chat 轨上发 `"tools": []`**(疑似)。`provider/src/lib.rs:688-702` 无条件发
   `tools`;`compact.rs` 的 `sample_summary` 传 `&[]`。按 schema 校验的端点会以 400 拒绝空数组，
   不可重试，Chat 轨上的压缩(含溢出后的被动压缩)整条失败。
3. **缓存计数字段为 null 或缺失时整条流判协议错误**(待复核)。`openai.rs:253-257`、
   `responses.rs:372-376`、`anthropic.rs:318-327`:`prompt_tokens_details: {"cached_tokens":null}`、
   `input_tokens_details: {}`、`"cache_read_input_tokens": null`。前两种发生在全部输出之后，
   整轮 after_semantic_output 失败;Anthropic 那种发生在 message_start,每次请求不可重试地失败。
   与 `lib.rs:709-715` 注释"缺失即 0"的意图不一致。
4. **Anthropic 只从 message_start 读 input/cache 用量**(疑似)。`anthropic.rs:311-327`、
   `436-439`:message_delta 分支只读 `output_tokens`。有的转换网关在 message_start 填 0、
   真实值放在 message_delta 的累计 usage 里，input 被记成 0,压缩触发过晚。
5. **配额正则里的 `billing` 太宽**(疑似)。`classify.rs:78-83` + `stream.rs:377-379`:免费层
   429 限流文案带 `…/account/billing` 链接，命中后 `HttpRetry::Never`,且排在 `x-should-retry: true`
   之前。DESIGN 约 170 行明确列了 billing。
6. **只有 `event:` 没有 data 行的帧也被派发**(待复核)。`sse.rs:58`
   `if event.is_some() || !data_lines.is_empty()`;`event: ping\n\n` 这种心跳让 Anthropic 与
   Responses 轨 `parse_sse_json("")` 失败。SSE 规范：data 缓冲为空时不派发。
7. **Chat 轨总发 `max_tokens`**(疑似)。`lib.rs:690`、`706-708`:对官方端点的推理模型同时发
   `reasoning_effort` 与 `max_tokens`,会被拒("use max_completion_tokens");仓库里没有
   `max_completion_tokens`。兼容网关一般接受 `max_tokens`。
8. **文档与注释**:`stream.rs:410-412` 注释说错误体读取"多挂 45 秒",实际超时是
   `STREAM_OPEN_TIMEOUT`(300s),错误响应最多多卡 5 分钟;`responses.rs:1420-1422` 测试注释写
   "signature-less thinking dropped",断言的却是照常发送(实际以 `encrypted_content: ""` 回放)。

## 开工先问用户

- 第 5 条怎么收窄：(a) `billing` 改成 `billing_hard_limit`、`check your plan and billing` 这类
  短语;(b) status==429 时要求同时命中更强的配额标记。推荐 (a)。→ 用户选 (a)。
- 第 7 条做不做、怎么做：(a) 发 `reasoning_effort` 时改用 `max_completion_tokens`;(b) 做成
  provider 配置项。推荐 (b),默认保持 `max_tokens`(兼容网关是主要用户)。→ 用户选 (b)，
  键名定为 `output_cap_field`。
- 第 8 条错误体读取超时要不要单独设短(如 30s)?推荐要。→ 用户同意，取 30s。

## 做法

1. Chat 轨 finish_reason 之后不调 `stop()`,继续读到 `[DONE]` 或 EOF,给尾帧一个短等待上限
   (超时就以已有结果结束、usage 为 None,不报错)。
2. `tools` 为空时省略字段(Chat 与 Responses 两条轨)。
3. 辅助缓存计数为 null/缺失一律按 0;只有类型错误(如字符串)才报错，或降级为 usage=None
   而不是让流失败。
4. message_delta 的 usage 里出现 `input_tokens`/`cache_*_input_tokens` 时覆盖 message_start 的值。
5. 第 5、7、8 条按问答。
6. SSE 解析改为只在 `!data_lines.is_empty()` 时派发。
7. 修 `stream.rs` 注释与 responses 测试注释。

## 验收

- 单测/集成(旧代码先红):分块下发(finish_reason 帧与 usage 帧分两个 chunk)时 usage 不丢;
  空 tools 请求体不含 `tools`;三条轨的 null/缺失缓存字段;message_delta 累计 usage;带
  billing 链接的 429 按限流重试;`event: ping` 无 data 不派发。
- provider 集成测试夹具补"分块下发"能力(夹具层改动，后续 plan 可复用)。
- 外层执行沙箱之外 `make check`;真实 API 是否实测开工问用户(代理和 key 问用户要);DESIGN
  约 170–180 行先读再改;HANDOFF 记进展与教训。

## 完成记录(2026-11-05)

八条全部成立(含五条「疑似/待复核」)，无划掉项。

1. Chat 轨收到 `finish_reason` 后不再 `stop()`：继续读到 `[DONE]`/EOF，
   `SseFrames::finish_after` 给尾帧 5s **绝对**宽限(不是每次读重置)；终端之后尾帧读取失败
   (超时、断连、未终止的 `[DONE]` 残片)都以已有结果收尾，只丢 usage；已经读到的坏帧或
   语义帧仍然 fail-closed。provider 集成测试新增原块传输夹具 `tests/support/mod.rs`
   (独立 flush + 50ms 间隔)，`usage_flushed_after_finish_reason_is_not_lost` 覆盖旧代码会丢
   usage 的形状。
2. `tools` 为空时整个字段省略。Anthropic 轨同样处理——压缩在三条轨上都不带工具，空数组
   对按 schema 校验的端点一律是 400。
3. 三条轨的辅助缓存计数(`cached_tokens`、`cache_read_input_tokens`、
   `cache_creation_input_tokens`)缺失或 null 一律按 0；类型错误仍报协议错。
4. Anthropic `message_delta` 的 usage 里出现 `input_tokens`/cache 字段即覆盖 `message_start`，
   与 Chat「后到的 usage 覆盖先到的」同规则。
5. 配额措辞收窄为 `insufficient_quota`、`out of budget`、`quota exceeded`、
   `billing_hard_limit`、`check your plan and billing`；带 `/account/billing` 链接的免费层
   429 恢复为按限流重试。DESIGN 的措辞清单同步。
6. SSE 只在有 `data:` 行时派发；`event: ping\n\n` 不再进入 JSON 解析。空的 `data:` 行仍派发。
7. Chat 轨输出上限字段改由 `Provider::OpenAiCompat.output_cap_field` 决定；profile 键
   `output_cap_field = "max_tokens" | "max_completion_tokens"`，默认前者，仅 chat 轨合法，
   写错值或写错轨在启动时报错。配置 demo 与 DESIGN 同步。
8. 错误体读取独立 30s 超时(`HTTP_ERROR_BODY_TIMEOUT`)，不再借用 300s 的打开超时；
   `stream.rs` 的过时注释、`responses.rs` 的「signature-less thinking dropped」注释已改正。

新增 14 条测试(SSE 1、stream 1、openai 5、responses 3、anthropic 3、cli 配置 1)，另扩展
4 条既有测试(三轨空 tools/缓存计数、429 分类表、请求体整对象断言)。外层执行沙箱之外
`make check` 全绿(fmt、clippy、workspace debug 全量测试、本机 parity baseline verified)。
真实 API 未实测：本机没有代理和 key，验收以夹具和单测为准。

**审查跟进(2026-11-05)**：三条 P2 都成立。`serde_json::Map` 的下标在缺键时 panic，
message_start/message_delta 的 required usage 改回 `get(...).unwrap_or(&Value::Null)`，
`usage: {}` 现在是协议错误而不是进程 panic；尾帧宽限从"忽略一切读错误"收窄为只忽略
Transport/Timeout/可重试的 EOF 残片，完整坏帧(非法 UTF-8)、超限帧与超限响应仍然 fail-closed；
缓存明细先验证父字段是对象或 null，`"prompt_tokens_details": "bad"/[]` 恢复为协议错误而不是
静默按 0。新增 4 条回归(missing required usage、非法 UTF-8 尾帧、两条非对象明细)。

## 另议(记一笔，不在本条做)

- `openai.rs:290-303` 的 delta 字段白名单是有意的 fail-closed,但 OpenRouter 推理模型的
  `reasoning_details`、部分网关在 `message` 里带的 `annotations: []` 会在第一帧让每次请求都失败。
  要支持这类网关时再评估白名单。
