# Plan 253 — provider 流式读取与请求构造

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：待实现。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
> 先读代码确认，不成立就在本文件划掉并写明原因。三条轨的去重与解析性能另见 plan 254。

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
  短语;(b) status==429 时要求同时命中更强的配额标记。推荐 (a)。
- 第 7 条做不做、怎么做：(a) 发 `reasoning_effort` 时改用 `max_completion_tokens`;(b) 做成
  provider 配置项。推荐 (b),默认保持 `max_tokens`(兼容网关是主要用户)。
- 第 8 条错误体读取超时要不要单独设短(如 30s)?推荐要。

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

## 另议(记一笔，不在本条做)

- `openai.rs:290-303` 的 delta 字段白名单是有意的 fail-closed,但 OpenRouter 推理模型的
  `reasoning_details`、部分网关在 `message` 里带的 `annotations: []` 会在第一帧让每次请求都失败。
  要支持这类网关时再评估白名单。
