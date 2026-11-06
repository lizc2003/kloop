# Plan 254 — provider 三条轨去重与流解析性能

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：✅ 已完成（2026-11-06）。第 1–5 条于 2026-11-05 完成，第 6 条于 2026-11-06 补齐；真实流式验证限制见末节。
> 行号是 2026-10-28 的读数，开工先自己量。**先做 plan 253**:它会改到
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
6. **(行为变更)Anthropic 补第 4 个缓存断点**。tools、system、末条消息原来共占 3 个，
   API 上限为 4；单个断点只有有限的回看窗口。→ 最近两条消息各标最后一个可缓存的
   顶层块，工具和 system 的断点保留；thinking、redacted_thinking 和空文本不标记。
   当前官方 API 将连续 `tool_use` 和连续 `tool_result` 各计为一个回看位置，因此原先
   “10 个以上并行调用必然越过窗口”的判断不成立，另用多块消息尾部验证。

## 实施决定

- 第 6 条最初按用户决定跳过；2026-11-06 用户授权补做，并已授权使用本机配置做真实验证。
- 先测并行工具调用和多块消息尾部的 `cache_read_input_tokens`，再按四断点上限实现。
  规则核对：[Anthropic prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)。

## 验收

- 去重提交：现有 `provider/tests/*` 一行不改全部通过;diff 除调用点外接近零改动。
- 性能提交：补一条"一个 chunk 含几千帧"的解析测试(断言结果，不断言耗时)。
- 第 6 条：完整 HTTP 请求约束工具、system、最近两条消息的四个断点；补大批工具调用和
  多块尾部、不可标记块、空消息、不回填更早历史和缓存关闭的回归；使用现有配置真实验证。
- 外层执行沙箱之外 `make check`;HANDOFF 记进展。

## 完成记录（2026-11-05）

- 第 1 条：`SseParser::feed` 改为读偏移，帧在缓冲上就地校验/解析，消费前缀结束时一次
  `drain`；每个 payload 只分配一个 `String`。新增 5000 帧单 chunk 回归与“完整帧先于残片”
  回归。
- 第 2 条：Anthropic 的 `content_block`/`delta` 直接借用；三条轨的完成块改为
  `CompletedToolCalls`（id、name、输入是否对象），Responses 另记 `landed_output` 布尔值。
- 第 3 条：Chat 轨删除只写不读的 `text` 缓冲。
- 第 4 条：`lib.rs` 新增 `Rail` 上下文、`display_conflict`/`DisplayKind`、带
  `EmptyIdentity` 的 `lock_identity`。
- 第 5 条：`openai_usage`/`UsageFields`、`image_data_url`、`error_prefixed`、`tool_fields`
  统一两条 OpenAI 轨的翻译。
- 第 6 条：按用户决定跳过（需要真实 API 观测缓存命中率）。
- 验证：现有 provider 测试未改全部通过；外层执行沙箱之外 `make check` 全绿（workspace
  2257 项通过、6 项 ignored，本机 parity baseline verified）。

## 第 6 条补做（2026-11-06）

- `messages_value` 只在发送视图中，为最近两条消息各标记最后一个可缓存块；有工具时
  加上 tools、system 正好四个，缺少合适块时留空，不把空位回填到更早消息。
- thinking、redacted_thinking 和空文本跳过，原始正文与签名保留；缓存关闭时不注入任何
  消息断点，默认五分钟 ephemeral TTL 不变。DESIGN 和 provider 注释同步。
- 更新完整 HTTP 请求期望，并新增两条序列化回归；HTTP 请求回归修复前先红，缺少的正是
  倒数第二条消息的断点。
- 真实三/四断点对照已完成：四断点请求能正常读取缓存；纯并行工具场景没有稳定优势，
  多块消息尾部的缓存读取有差异，但总输入也有波动，尚不能据此量化额外断点的收益。
  缓存对照关闭 thinking 以隔离签名问题，没有改动本机配置。
- 外层执行沙箱之外 `make check` 全绿（fmt、clippy、workspace debug 全量测试、本机
  parity baseline verified），其中 provider 单元测试 61 项、Anthropic HTTP 契约测试 34 项。
- 当前 provider 的真实流式缓存采样两次都在首次预热时被上游无签名 thinking 阻断，
  关闭 thinking 的控制请求也收到该形状；尚未进入新增第四断点的工具结果请求。
  直接 API 四断点缓存读取已验证，完整流式工具循环未计为通过，签名保护未放宽。
