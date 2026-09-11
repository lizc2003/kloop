# Plan 133 — 被截断的一轮,被读成了协议损坏

> 来源:2026-09-10,用户 dogfood 的真实会话
> `~/.kloop/projects/v1/p1_434f98.../sessions/20260910-154903.jsonl`:第 39、41 两条
> `turn_terminal` 都是
> `provider protocol error: openai-responses final output item was not completed`,
> 而且第 40 条是用户输入的「继续」——**两轮各烧掉 60.3s 和 60.9s,一个字都没留下**。
> 原话:「kloop 的最新对话provider出错」「那个最新对话,用继续,可以复现出这个错误」。
>
> provider `gw_cn`(`ai-coding-sr-bj-direct`,火山 ark 后端),model
> `deepseek-v4-flash-0731`,effort `xhigh`。

## 一、根因:预算用尽被当成了协议损坏

kloop 给 Responses 请求写死 `max_output_tokens: 8192`(`protocol/src/lib.rs:812`)。
xhigh 下 deepseek 的**推理 token 就能单独吃满这 8192**(实测 `reasoning_tokens: 8192`,
41s;用户那两轮上下文更大,60s)。上游于是按 Responses 的规矩把这次响应标成截断:

```
response.output_item.done  item: {type: "reasoning", status: "incomplete"}
response.incomplete        incomplete_details: {reason: "length"}
                           usage: {output_tokens: 8192, reasoning_tokens: 8192}
```

`check_final_item_status()` 对 `output_item.done` 里的 status 只接受 `"completed"`,
于是第一行就抛不可重试的 protocol 错误,整轮丢弃。

**而 kloop 早就把这件事处理对了**:`terminal_outcome()` 认 `response.incomplete`,
`agent.rs:722` 的 `AssistantOutcome::OutputLimit` 会自动追一句「你上次被输出上限截断了,
从断点继续」,最多三次,文本前缀还会拼起来。**这条路只是永远走不到**——item 级的一个字段
在它前面把整轮打死了。这与 Plan 131 是同一族:一条契约落在了一个不该有裁判权的位置上。

## 二、第二处:`reason` 的拼法不同,恢复机制照样不生效

放行了 item 级 status,也只是走到 `terminal_outcome()` 里:那里只认
`"max_output_tokens"` 和 `"content_filter"`,而 ark 送的是 **`"length"`**,于是落进泛化的
`Incomplete(IncompleteReason::Provider("length"))`——`agent.rs` 对 `Incomplete` 的处理是
**直接以错误结束整轮**,不进续写恢复。**两处都修,才等于修好了一件事。**

## 三、第三处:被截断时,已经完整的 function_call 也全被标成 incomplete

第三次抓包(12 个文件、`parallel_tool_calls`、上限 700)显示 ark 的截断标记是**响应级的
毯子**,不是「这个 item 的内容不全」:

| 实测形态 | item.done 的 status | terminal |
| --- | --- | --- |
| 推理烧满(8192) | `reasoning: incomplete`,无 `encrypted_content` | incomplete / length |
| 正文写到一半(400) | `reasoning: completed` → `message: incomplete`,文本 1025 字节 | incomplete / length |
| 11 个并行工具调用(700) | `reasoning/message: completed` → **11 个 function_call 全 `incomplete`** | incomplete / length |

第三行是关键:那 11 个 `function_call` 的 `arguments` **逐个都是完整合法的 JSON**,
`function_call_arguments.done` 也都发了;没赶上的第 12 个干脆一个事件都没有。也就是说
**截断落在 item 边界上,`status: "incomplete"` 描述的是这次响应,不是这个 item 的内容**。
于是放行 status 之后会立刻撞下一道墙:`terminal_outcome()` 里的
`if has_tool || has_refusal { Err("incomplete response contained conflicting output") }`。

11 个经过 kloop 全套校验(参数 done、JSON 合法、终值与流式累积一致)的工具调用,是**可以
照常执行**的;执行完把结果交回模型,它自己会把第 12 个补上。把整轮打死才是有害的那一侧。

## 四、做了什么

`provider/src/responses.rs`:

1. **`check_item_status()` 换成 `item_status()`**:把值交回调用方,由调用方说明自己接受
   哪些状态、以及**实际收到的是什么**。`output_item.added` 仍然只接受 `in_progress`
   (缺失照旧按 reasoning 的既有豁免放过);`output_item.done` 接受 `completed` 与
   `incomplete`,其余值(含显式 `null`)照旧 fail closed,错误信息现在印出 item type 与
   拿到的那个值。
2. **新增 `ItemCompletion`**,由三个 `finish_*` 一路带回 `stream()`。`Truncated` 只记录
   「这次响应的预算用光了」这一件事;item 内容的校验一分没松(part 索引稠密、数量一致、
   文本逐字相等;参数 done + JSON 合法 + 终值与流式累积一致)。
3. **`OutputSummary`** 把 `has_tool` / `has_refusal` / `truncated` 收成一个结构(三个位置
   布尔参数没人读得懂),并在 terminal 处加一条交叉检查:**item 说截断、response 说
   completed,fail closed**——这是放宽换来的唯一一句新话,它必须和 terminal 对得上。
4. **`terminal_outcome()` 的 incomplete 分支**:`"length"` 与 `"max_output_tokens"` 同义,
   都映射 `OutputLimit(MaxOutputTokens)`(唯一进续写恢复的分支);`content_filter` 仍然
   优先于一切;`has_refusal` → `Refused`;**已完整落地的 function call → `ToolUse`,照常
   派发**;其余 reason 仍是 `Incomplete(Provider(...))`。refusal 与 tool call 同时出现仍
   是协议错误,与 completed 分支共用同一句文案。

`protocol/src/lib.rs`:给 `StreamEvent` 派生 `PartialEq`,新测试才能一次断言整条事件序列。

`README.md`:两处纪律改写——item status 的值域与那条交叉检查;截断响应的 outcome 与工具
派发。

## 五、非目标

- **不动 `MAX_OUTPUT_TOKENS = 8192`**。它是「为什么天天撞」的原因,但改它影响三条 rail
  的每个请求,是另一个决定,单独拍。
- **不放宽 `output_item.added` 的 `in_progress`**:没有任何实测形态需要它。
- **不放宽「terminal 到达时仍有未闭合 item」**:三次抓包里上游都规规矩矩关闭了 part 和
  item(`output_text.done`/`content_part.done`/`reasoning_summary_*.done` 一个不少),
  没有证据的地方保持 fail-closed,只把错误信息补上实际值,万一撞到能一眼定位。
- 不碰 Chat/Anthropic 两条 rail 的截断语义。

## ✅ 已完成(2026-09-11;提交 SHA 以本条所在提交为准)

### 测试

5 个新测试,三个形态用的都是抓包里的真实事件序列(不是想象出来的形状):

- `reasoning_truncated_by_the_output_budget_becomes_an_output_limit`——形态一,
  reasoning 以 `incomplete` 收口、不带 `encrypted_content`,terminal `length`;
  断言整条事件序列 = ThinkingDelta + Thinking 块(签名为空)+ `OutputLimit`。
- `a_message_truncated_mid_text_keeps_the_partial_answer`——形态二,写到一半的正文必须
  留下来,续写恢复要靠它拼。
- `complete_function_calls_stamped_incomplete_are_still_dispatched`——形态三,两个
  byte-complete 的调用被标 `incomplete`,仍然按 `ToolUse` 派发。
- `a_truncated_item_under_a_completed_response_fails_closed`——放宽换来的那句新话必须和
  terminal 对得上。
- `an_unknown_final_item_status_still_fails_and_names_itself`——`"failed"` 仍然打死整轮,
  且错误信息里印出它自己(旧文案只说"was not completed",害得复现时只能猜)。

既有的 8 个严格性 case(`output_item_status_stays_strict_outside_omitted_reasoning`)
一个没改、全部仍然失败,说明放开的确实只有 `incomplete` 这一个值。

`cargo fmt --all`、`cargo clippy --all-targets --all-features`(0 warning)、`cargo test`
全绿(provider responses 46 个;全量 workspace 0 failed)。

### 真实线路验收(gw_cn / `deepseek-v4-flash-0731` / xhigh)

抓包(`curl` 直打 `ai-coding-sr-bj-direct`)确认三种形态与 `reason: "length"`,并确认
**带空 `encrypted_content` 的 reasoning 条目回放被上游接受**(200),也就是形态一恢复后的
下一轮不会 400。

端到端:`kloop --headless "Output every integer from 1 to 5000..."`——第一轮
`output_tokens: 8192`(撞满上限),保留 14112 字符正文,stderr 打出
`[response truncated by output limit; asking the model to continue (1/3)]`,第二轮
3055 tokens 收尾,`turn_terminal: completed`,exit 0。转录见
`~/.kloop/projects/v1/p1_38ef03.../sessions/20260911-031921.jsonl`。

对照组是用户报障的那两轮(`20260910-154903.jsonl` 第 39、41 条):同一条 rail、同一个
错误、各烧掉 60.3s 与 60.9s,一个字都没留下。
