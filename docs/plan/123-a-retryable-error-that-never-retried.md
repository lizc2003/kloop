# Plan 123 — 一个可重试的错误，两道门都没让它重试

> 来源：2026-09-07 dogfood。用户在那次代码审查会话里三次被同一条 note 打断，
> 每次都要手打「继续」：
>
> ```
> [error: provider protocol error: openai-responses stream error (server_error)]
> ```
>
> 用户问「能查到具体是什么原因吗」。查到了，而且它不是分类错误——`server_error`
> **本来就被判为可重试**，是两道门各自以正当的局部理由拒绝了它。

## 一、两道门

**门一：sampling 层的重试循环被绕过**（`crates/core/src/agent/sampling.rs:221`）

```rust
return Err(if error.after_semantic_output() {
    SampleError::AfterOutput { error, partial: replayable_partial(blocks, &text_accum) }
} else if error.is_context_overflow() {
    SampleError::Overflow
} else {
    SampleError::Provider(error)   // ← 只有这条进 3 次重试 + 指数退避
});
```

`semantic_event()`（`crates/provider/src/stream.rs:118`）把**非空 `ThinkingDelta`
也算作 semantic output**。用户跑的是 `effort = "xhigh"`，模型总是先输出大段
reasoning 才产出 text/tool_use，所以错误必然带 `after_semantic_output = true`,
直接跳过整个重试循环。

**这道门的理由是正当的**：重放一个已经产出可见内容的请求，会让用户看见重复输出。

**门二：agent 层的断点续传条件不成立**（`crates/core/src/agent.rs:616`）

```rust
if partial_landed && error.is_retryable() && stream_resumes < STREAM_RESUME_LIMIT {
```

`replayable_partial()`（`sampling.rs:344`）明确丢弃**无签名的 thinking**
（Responses 的 reasoning 没签名就不能回放，Plan 89 的约束）和 **tool_use**。
此刻模型只发过 thinking delta、一个 block 都没 done，于是 blocks 为空 →
`partial_landed = false` → 落到 `agent.rs:628` 的 `break 'turn`，整个 turn 结束。

**这道门的理由也是正当的**：没有可继续的助手轮次，"continue where you left off"
无从谈起。

## 二、交集就是 xhigh 的常态

两道门的交集是「**已经有 semantic output，但没有任何可回放的 block**」。这个窗口
从请求发出一直持续到模型吐出第一个 text 或 tool_use——在 xhigh 下这段时间很长,
而 网关 的 `server_error` 是随机抖动。于是：

- 一个被明确标注为 retryable 的错误（`crates/provider/src/lib.rs:198` 的注释：
  transient 条件"deliberately absent so they default to retryable"）
- 三次重试 + 指数退避 + `Retry-After` 的机制全都在
- 但实际重试次数是 **0**，turn 直接结束

本机证据：`20260907-091015.jsonl` 第一轮 `#29` 后、第三轮 `#197` 后各一次,
`20260903-091113.jsonl` 一次。

## 三、改法

死角的本质是：**「不能重放」和「没有内容可继续」同时成立时，剩下的正确动作是
重试整个请求**。当 `after_semantic_output` 为真而 `replayable_partial` 为空,
那些 semantic output 全部是不可回放的 thinking——**重放不会产生重复的答案**,
只会重新想一遍；而且那段 thinking 在 UI 上已经被
`finish_open_items(..., ItemStatus::Failed)` 标记为失败了。

所以在 `sampling.rs:221` 判断：`replayable_partial` 为空就不走 `AfterOutput`,
改走 `SampleError::Provider`，完整复用既有的重试语义（3 次 + 退避 +
`Retry-After`，耗尽后按既有规则切 fallback）。

> **第一版这条写错了,被既有测试当场抓住。** 让 `replayable_partial` 为空的有
> **两种**情况:无签名 reasoning,和**一个完整但未派发的 tool call**——
> `replayable_partial` 把两者都丢掉。它们不能同命:tool call 意味着模型已经
> 决定要做某件事、调用方也看到了这个决定,再问一次可能得到**另一个**决定;而
> reasoning 什么都没留下,重问不可能重复任何东西。第一版把两种空一视同仁,
> 直接破坏了 `complete_tool_block_seals_retry_without_dispatching_it` 锁定的
> 契约(只发一次请求、tool block 丢弃不执行、不切 fallback)。
> 讽刺的是**本文档第一版和 `agent.rs:616` 的注释都写着 tool call 这条**
> ("a complete-but-undispatched tool call, say"),我引用了它却没把它当成
> "这两种空不同"的信号——正是今天写进 skill 的「一条触发路径不等于全部」。
> 现修为:先看 blocks 里有没有 `ToolUse`,有就仍走 `AfterOutput`(空 partial
> → agent 层结束 turn,行为不变),只有纯 reasoning 那种空才重试。

**修在源头而不是 agent 层**：`AfterOutput` 这个变体存在的唯一理由就是"重放会
重复用户已见的输出"，理由不成立时它就不该被构造出来。在 agent 层补一个分支
只会让两处各判一半。

## 四、顺带：文案分不出可重试与永久

`incomplete_protocol()`（`crates/provider/src/failure.rs:105`）用的是
`ProviderFailureKind::Protocol` + `retryable: true`，而 `Protocol` 的 label 是
`"provider protocol error"`（`failure.rs:199`）——**和真正 fatal 的 `protocol()`
一字不差**。用户看到"协议错误"，实际是"可重试的上游抖动"，从文案上完全区分
不出来，也就无从判断该不该重跑。

Display 按 `retryable` 分开渲染即可，`ProviderFailureKind` 不动（它进 rollout
的 `typedError`，加 variant 会改 schema）。

## 四之二、同一个模式的第二个实例：重复的生命周期帧

> 用户在实施过程中报的第二条：`[error: provider protocol error: openai-responses
> received duplicate response.in_progress]`,「新会话里,老报这个错,并且 turn
> 被打断」。

`response.created` / `response.in_progress` 是**纯生命周期 metadata**——它们开启
响应,不携带任何语义内容。Plan 65 第 282 行自己写着「created/in_progress 等纯
metadata 可忽略」,但实现走到了相反方向:`response_id.is_some()` 或
`in_progress_seen` 一旦为真,再来一帧就 `protocol()`——**fatal、不可重试**,整个
turn 当场结束。而网关内部重试或合并上游流时重放开场帧是常见行为,那一帧里没有
任何第一帧没说过的东西。

改法是把检查从「来过几次」换成「身份对不对」:

- 重复的 `created` / `in_progress`,**id 与 status 一致 → 忽略**;
- **id 不同 → 仍然 fatal**——那是两个响应被复用到一条流上,之后所有内容都会
  记到错的响应上。

`in_progress_seen` 唯一的用途就是那个计数检查,已删除;`response_id` 保留,终态
校验仍要用它。

### 用最新版跑，仍然中断：`identity or status changed`

> 用户装上上面那版之后报的:`[error: provider protocol error: openai-responses
> response.in_progress identity or status changed]`。

上一版把「来过几次」换成了「身份对不对」,但那个身份检查本身有两个问题:

**其一,`status` 根本不该被检查。** kloop 从这两个开场帧里唯一需要的是"这还是
同一个响应";`status` 在下面**没有任何一处被读**(终态事件的 status 是另一条
路径,那里确实消费它)。而 OpenAI Responses 的开场 status 合法取值就包含
`queued`——网关排队或重试时发 `queued` 完全正常。**检查一个不消费的字段,是把
上游的用词变成了这里的协议违约。**

**其二,同一个"新 id"在两个时点意味着相反的事。**

- **在任何 output item 之前**:这是上游重启——网关重试后开始转发真正的响应,
  此时没有任何内容被归属过,跟随新 id 是安全的,turn 保住;
- **在 output 之后**:两个响应共用一条流,之后所有内容都会记到错的响应上,
  仍然 fail closed。

顺带把错误信息拆开并带上实际值(`from r to other ... after output had landed`)。
上一版把两个条件合并成一句 `identity or status changed`——**用户报这个错时,
无法知道触发的是哪一半**,只能靠推理。这本身就是诊断缺陷,与本文件第四节的
`error_detail` 同源。

**契约被重新决定,所以旧测试删掉而不是放宽。**
`a_second_response_identity_still_fails_closed`(上一版刚加的)钉的正是"无 output
时 id 冲突要失败",而这一版认定那种情况应当跟随。它被删除并留下一行说明,由
`a_new_identity_before_any_output_is_followed` /
`a_new_identity_after_output_still_fails_closed` 两条分别承接两半——**一个场景
被重新裁定之后,不该让旧测试留着旧名字和旧主张。**

### 第三条:可选字段被当成必填 —— `final reasoning parts were not an array`

`finish_reasoning` 对 `item["summary"]` 和 `item["content"]` 都要求必须是数组,
但 **`content` 在 reasoning item 里是可选字段**:一个只有 summary 的 reasoning
item 根本不发这个 key,`item["content"]` 于是是 `Null`,`as_array()` 直接失败,
turn 在模型已经把活干完之后被杀掉。

**为什么一直没暴露**:现有测试的 fixture 全都显式写了 `"content": []`——
**测试构造的是"理想形状",不是真实上游会发的形状**。

改法:抽出 `parts_array()`,reasoning parts 与 message content 两处都走它——
缺席按空处理,因为「没有 content」与「content 为空」在这里语义等价。不放松任何
真正的校验:

- 缺席 + 流里也没有 parts → 正常通过;
- 缺席 + 流里有 parts → 仍被既有的 `count did not match` 抓住;
- present 但不是数组(比如字符串)→ 仍然失败,且**现在会说出实际类型**
  (`got string`)。

**这些都是同一个模式的实例**:上游一个合法但非典型的选择,被 kloop 当成协议违约
杀掉整个 turn。区别只在于是哪个字段——`server_error` 那条是「可重试却没重试」,
重复开场帧那条是「根本没被当成可重试」,身份校验那条是「校验了一个不消费的字段」,
可选字段那条是「把可选当成了必填」。**共同的教训在 HANDOFF 112。**

## 五、非目标

- 不改 `is_fatal_stream_error` 的名单（`server_error` 的分类本来就是对的）。
- 不改 `semantic_event` 把 thinking 算作 semantic output 的判定（它对 Anthropic
  签名 thinking 的回放是必需的）。
- 不动 `STREAM_RESUME_LIMIT` 与有可回放内容时的续传路径。

## 验证

- `cargo fmt` + `clippy -D warnings` + 21 个测试二进制逐个跑（教训 107(f)）。
- 新增回归：只发 thinking delta 后遇到 retryable 流错误 → 请求被重试而不是结束
  turn；有可回放 text 时仍走既有的续传路径（不回归）。
- 文案回归：retryable 与 fatal 的 protocol 失败渲染成不同前缀。

## ✅ 已完成（2026-09-07；提交 SHA 以本条所在提交为准）

**主修**（`crates/core/src/agent/sampling.rs:221`）：先算 `replayable_partial`，
只有它非空才构造 `SampleError::AfterOutput`；为空时落到 `SampleError::Provider`，
完整复用既有的 3 次重试 + 指数退避 + `Retry-After`、耗尽后按既有规则切 fallback。
`is_context_overflow` 的判定顺序不变（overflow 仍先于普通重试）。

**文案**（`crates/provider/src/failure.rs:176`）：`Display` 按 `retryable` 分渲，
retryable 的 Protocol 失败显示为 `provider stream interrupted`，fatal 的仍是
`provider protocol error`。`ProviderFailureKind` 不动，rollout 的 `typedError`
schema 不变。

### 测试

- `a_stream_that_dies_during_reasoning_is_retried_not_ended`：mock 先发一个
  无签名 `Thinking` 的 delta、再抛 retryable 流错误，断言 turn 正常完成、
  **两次请求的 messages 完全相同**（是重试不是续传：没有 partial assistant 轮次、
  没有 resume nudge），且丢弃的 reasoning 不在 history 里留下空的 assistant 轮次。
- `a_retryable_protocol_failure_does_not_read_as_a_permanent_one`：两个构造函数
  共用 kind，但渲染出不同前缀。
- `repeated_lifecycle_frames_are_tolerated_when_the_identity_holds` /
  `a_second_response_identity_still_fails_closed`：重放的开场帧不再失败，而 id
  冲突仍然 fail closed。
- `a_relayed_message_never_carries_the_key_back`：relay 把请求凭据回显进错误
  消息时，渲染出的错误里不含 key、含 `[redacted]`。
- `partial_stream_seals_the_open_item_then_continues_from_it` 未改动仍通过——
  有可回放 text 时走的仍是续传路径，不是重试。
