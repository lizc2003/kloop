# Plan 254 — provider 三条轨去重与流解析性能

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：待实现。行号是 2026-10-28 的读数，开工先自己量。**先做 plan 253**:它会改到
> 这里要合并的几个函数(缓存计数宽松化只需在合并后的一处改)。

## 背景

纪律照 plan 177–186:**行为零变更**。性能项与去重项分开提交;第 6 条(缓存断点)是行为
变更，单独评估。

## 清单

1. **SSE 解析每帧整体前移缓冲**。`provider/src/sse.rs:26-40` 每帧 `drain(..end).collect()`,
   一个大 chunk 里有几千个小帧时是 O(n²) 的 memmove;每帧还有 Vec、String、Vec<String>、join
   共 4 次分配。→ 维护读偏移,feed 结束时统一 `drain(..consumed)`;单行 data 直接借切片。
   (mcp 的 SSE 解析同类问题在 plan 260。)
2. **每个事件深拷贝 JSON**。`anthropic.rs:341-344`、`370-373`、`417-419`;`responses.rs:1060-1063`:
   `start_block(&Value::Object(content.clone()))`、`Value::Object(delta.clone())` 只为换类型;
   `completed.push(block.clone()); sink.block_done(block)` 把大块 write_file 入参整份复制，副本
   只用于最后的 id/object 校验。→ 校验 `is_object()` 后借用;completed 只存 (id, name, is_tool)。
3. **Chat 轨 `text` 缓冲只写不读**。`openai.rs:410`/`331`:每个 content delta 同时进 `text` 与
   `display_text`,结束只用后者。→ 删掉。
4. **通用校验函数逐份复制**。`anthropic.rs:135-159` 与 `responses.rs:254-301` 的 `protocol`、
   `required_str`、`required_u64`、`event_type` 只差错误前缀;两份 `lock_identity` 只差空串策略;
   "同类显示块不能并发打开"检查在 `anthropic.rs:345-356` 与 `responses.rs:1021-1032`。
   → `lib.rs` 放一个带 rail 名的小上下文(或函数带前缀参数);`lock_identity` 用参数表达空串策略。
5. **OpenAI 两条轨的翻译成对复制**。`openai.rs:246-267` 与 `responses.rs:362-386` 的 usage 解析
   只差字段名;`openai.rs:29-38` 与 `responses.rs:39-46` 手拼 `data:{media_type};base64,{data}`;
   `[error] ` 前缀出现 3 次(`openai.rs:140`、`responses.rs:66`、`:141`);工具定义 JSON 在
   `lib.rs:655-660` 与 `693-700` 各拼一次。→ `openai_usage(v, prompt_key, details_key)`、
   `image_data_url(source)`、`error_prefixed(text, is_error)`、`tool_def_json(t)`。
6. **(行为变更，单独评估)Anthropic 只用了 3 个缓存断点**。`anthropic.rs:92-110`:tools、system、
   末条消息;上限 4。一轮新增超过约 20 个块(10 个以上并行 tool_use + result)时上一轮的断点
   超出回看窗口，整个前缀重新写缓存。→ 第 4 个断点标倒数第二条消息。

## 开工先问用户

- 第 6 条要不要做?它需要真实 API 量命中率(代理和 key 问用户要)。推荐先量一次 10 个并行工具
  调用场景的 `cache_read_input_tokens`,确认有损失再做。

## 验收

- 去重提交：现有 `provider/tests/*` 一行不改全部通过;diff 除调用点外接近零改动。
- 性能提交：补一条"一个 chunk 含几千帧"的解析测试(断言结果，不断言耗时)。
- 外层执行沙箱之外 `make check`;HANDOFF 记进展。
