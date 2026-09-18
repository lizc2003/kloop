# Plan 164 — 自己产的 reasoning,被自己判成不可能

> 来源:2026-09-18,[plan 163](163-one-usage-report-sent-twice.md) 修完 usage 之后,同一个
> `gw-cn` + `glm-5.3-flash` 会话再跑一轮,第二轮死在
> `provider protocol error: reasoning history block shape does not match its producing API family`。
> 当场查完做完(✅ 见文末)。

## 现状:仓库里两半代码互相不认

`crates/provider/src/openai.rs` 的 chat 适配器**明确会产 reasoning**:`reasoning_content` /
`reasoning` delta 转成 signature-less 的 `AssistantBlock::Thinking`,有测试
(`reasoning_content_becomes_thinking_block`,注释还写着 "deepseek-style")锁死。glm-5.3-flash
就是这类模型——真机一次 126 个 completion token 里 124 个是 reasoning。

`crates/core/src/provider_route.rs::validate_provenance` 的形状规则**断言它不可能**:

```rust
// Chat carries no reasoning at all; Responses carries it as an encrypted
// blob and never as a redacted block.
ReasoningShape::Plain => source.api_family != ProviderApiFamily::OpenAiChatCompletions,
```

于是第一轮存下 `thinking` + `text` + `tool_use`(provenance 写着 `open_ai_chat_completions`),
第二轮投影请求视图时这条规则炸了,整轮作废。**同一条会话在 chat 轨上永远走不过第二轮**;
更糟的是 `rollout.rs` 的落盘校验用的是同一个函数,**已经写出来的会话文件连 resume 都打不开**。

那句注释把两件事写混了:chat **不回放** reasoning(真),被写成了 chat **不产生** reasoning(假)。

## 裁决

### 一、放宽的是 `Plain`,不是整条规则

`ReasoningShape::Plain` 对所有 API family 放行;`Redacted` 维持原样(只有 Anthropic 有 redacted
reasoning,chat 和 responses 都没有这个概念,那条仍然是真的不可能)。

### 二、strip 留在它本来的位置

`history.rs` 的投影本来就写好了:`chat_target` 为真时无条件剥掉 reasoning、清掉 provenance
(`exact_replay_compatible` 与 `sanctioned_switch` 两条分支都挂在 `!chat_target` 下)。
它一直是对的,只是被排在它前面的校验挡住了,**没机会跑**。修的是校验,不是投影。

### 三、落盘保留,不在写入时剥

reasoning 要进 transcript(显示、`--resume` 重放),chat 轨也不例外。"不回放"是**请求投影**的
职责,不是存储的职责——把它提前到写入端,会让 transcript 丢掉模型真实说过的话。

## 验收

- `history::chat_produced_reasoning_is_stripped_not_rejected`:同一条 chat 路由上自产
  signature-less thinking,下一轮请求视图只剩 text、provenance 清空(不再报错)。
- `rollout::chat_reasoning_provenance_survives_read`:带 thinking 的 chat 消息落盘后原样读回
  ——即已经写坏的会话能重新打开。旁边那条 `chat_text_provenance_survives_read_without_reasoning`
  的名字正是这条错误假设留下的化石,留着不动。
- `chat_request_view_validates_source_then_removes_reasoning`(切轨到 chat 的那一半)不受影响。

## ✅ 完成

2026-09-18 完成,一次提交 `<sha>`。`cargo fmt --check` + `clippy -D warnings` + 全量 `cargo test` 全绿。
