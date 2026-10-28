# Plan 250 — agent 主循环与上下文估算

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：待实现。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
> 先读代码确认，不成立就在本文件划掉并写明原因。

## 问题

1. **`run_turn` 被提前中断时 hook 上下文留在 staged**(已复核)。`core/src/agent.rs:313-318`
   把 pre_turn hook 的上下文 `stage` 进去;`:356` 的"整 turn 撤回"条件是
   `Aborted && has_staged() && inbox.is_empty()`。`run_turn` 的调用方(TUI `tui/src/lib.rs:522`、
   `:607` 的 skill `/name`,server `worker.rs:297`、`:344`,`cli/src/plain.rs:252`、`:297`)
   都是先 `history.record(prompt)` 再调，staged 里只剩 hook 上下文：
   - 这一 turn 的输入已落盘，却没写 terminal 行;
   - 下一次 `run_turn_with_input` 时 staged 是 `[旧 hook ctx, 新输入]`,若又被提前中断，
     `take_staged().into_iter().next()`(`:228`)拿回的是旧 hook 文本——TUI 把它放回输入框，
     用户刚打的字丢了;若没中断，旧 hook 上下文排在新输入前面提交。
   `:356` 的注释"nothing has been written all turn"也不准：预测式压缩的 `replace_all`、
   `record_provider_usage`、round 0 的 todo/changed-read 提醒都会写。
2. **图片 token 按 base64 字节/4 估**(已复核)。`core/src/history.rs:1123`
   `estimate_message_tokens` 原样序列化 `ImageSource::Base64{data}`。一张 400KB PNG 估成约
   13 万 token,下一轮 `predicted_overflow` 在 200k 窗口下判溢出;`keep_from_index`
   (`compact.rs:224`)对它也按 13 万计，整段会话被摘要掉;`/context` 与用量表一起虚高。
   `image.rs` 头注释承认 API 对视觉 token 有上限。测试
   `ascii_messages_keep_the_bytes_over_four_estimate`(`history.rs:1236` 起)把错误行为钉成了期望。
3. **主请求因超长被拒时不记实测上限**(已复核)。全仓唯一的生产调用 `note_overflow_at` 在
   `compact.rs:599`(摘要请求自己被拒时)。主请求的 `Sampled::Overflow` → `recover_from_overflow`
   (`agent.rs:613-659`)不记，拿不到本轮的 `estimated`。配置窗口高于真实窗口时，每次涨回
   上限都要再被拒一次;同一 turn 内第二次被拒直接 Error。与 DESIGN 约 405 行
   "costs one rejected request before the measured ceiling takes over"不符。
4. **截断续写后夹一轮工具调用，`final_text` 漏字并错拼**(已复核代码)。`agent.rs:844`、
   `:876-881`(流中断续写 `:693` 同形):第 1 轮 OutputLimit 文字 A → 第 2 轮文字 B + tool_use →
   第 3 轮 EndTurn 文字 C,结果 `final_text = A + C`,B 丢了、A 与 C 无分隔。子 agent 的
   `final_text` 就是交付物。
5. **`shrink_to_newest` 退到 0 被当成缩不动**(疑似)。`compact.rs:702-707`:第二次 shrink 时
   `request[1]` 是 tool_result,回退到 0 后返回 0,报 "summary request too large to shrink
   further";丢掉 `[0,1]` 这一对本可继续。
6. **结构化 turn 已接受的值被迟到消息丢掉**(疑似)。`agent.rs:1002-1005`:`structured_output`
   通过校验后，同一时刻来了 peer 消息或后台 shell 通知，`keep_going_for_late_work` 返回
   Retry,`value` 丢弃;模型以为已交付、回一句纯文本，走 `nudge_structured`,消耗与之前失败
   共享的 `structured_failures`(满 3 次失败)。

## 开工先问用户

- 第 2 条图片按多少计：(a) 固定值(如 1600,按 API 文档的单图视觉 token 上限取);
  (b) 读图片头拿宽高按像素公式算(要解 base64 头部，每轮每图一次)。推荐 (a),估算只用于
  溢出预测，偏高一点无害。
- 第 6 条：已接受的值存进 `Turn`,之后 EndTurn 时模型没再调用就用它完成——要不要?推荐要。

## 做法

1. 在 stage hook 之前记下 `input_staged = history.has_staged()`,`:356` 以它为条件;不满足
   "整 turn 撤回"时先丢弃 staged 里的 hook 上下文再写 terminal。改写 `:356` 的注释，写明
   哪三处会提前写、为什么可接受。
2. `estimate_message_tokens` 对 `ContentBlock::Image`(含 `ToolResultContent::Blocks` 里的图)
   按问答计，不再序列化 data;改写那条测试。
3. 把 `turn_rounds` 算出的 `estimated` 传进 `settle_sample`/`recover_from_overflow`,Overflow
   分支先 `history.note_overflow_at(estimated)` 再压缩。
4. ToolUse 轮出现时把 `truncated_prefix` 清空(其文字已在 `produced_text` 里);EndTurn 只在
   上一轮恰是截断/续写时才拼 prefix。
5. `shrink_to_newest` 退到 0 时改为向前跳过这一对，只有确实没东西可丢才返回 0。
6. 按问答处理已接受的结构化值。

## 验收

- 单测(旧代码先红):pre_turn hook + `run_turn` + 首字前中断 → 写 terminal、staged 为空，
  下一轮中断拿回的是新输入;一张大 base64 图不触发预测压缩;主请求 Overflow 后
  `effective_window` 收到实测值;A/B+tool/C 序列的 `final_text`;首条为大 tool_use 时两次
  shrink 仍能继续;结构化值接受后遇迟到消息仍以该值完成。
- DESIGN 改写约 405 行与 staging/中断撤回一段;外层执行沙箱之外 `make check`;HANDOFF 记
  进展与教训。
