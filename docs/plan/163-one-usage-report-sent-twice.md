# Plan 163 — 同一份用量,网关发了两遍

> 来源:2026-09-18,用户把 `~/.kloop/config.toml` 里的 `gw-cn` 从 `wire_api = "responses"`
> 改成 `"chat"`、默认模型换成 `glm-5.3-flash`,改完开的第一个会话、第一轮就死在
> `provider protocol error: openai-compat received duplicate usage`,一个字都没回出来。
> 当场查完做完(✅ 见文末)。

## 现状

`crates/provider/src/openai.rs` 的 chat 流解析照 OpenAI 的 `include_usage` 契约写:usage 只在
`[DONE]` 之前那一帧 `choices: []` 里出现一次。于是见到第二个非空 `usage` 就
`protocol("received duplicate usage")` —— 协议错误、`retryable: false`、整轮作废。

这条严格性从建仓起就在,**一直没被触发**:9/17 到 9/18 白天的全部会话走的是 responses 轨 +
`deepseek-v4-flash-0731`,压根不经过这段代码。chat 轨是这次改配置才第一次真正跑起来。

## 证据:网关真的发两遍(真机三次,次次如此)

对 `gw-cn` 的 `/chat/completions` 直接 curl(`stream_options.include_usage = true`,与 kloop
发的 body 同形),尾部永远是这个形状:

```
#125 choices=1 finish=["stop"]  usage={prompt_tokens:17, completion_tokens:126, ...}
#126 choices=[]  finish=[]      usage={prompt_tokens:17, completion_tokens:126, ...}   ← 逐字节相同
#127 [DONE]
```

带 usage 的永远是最后两帧,`router_detail.router_name` 是 `aicoding-volcengine`。两份内容完全
一样——它不是增量、也不是修正,就是同一次统计发了两遍。

**顺带查到、但不在本 plan 修的一件事**:`models` 白名单里的 `deepseek-v4-flash-0731` 在 chat 轨上
直接 403 `api key purpose is not allowed on this route`(`purpose_mismatch`)。这是网关对 key 的
路由限制,不是 kloop 的事;但那条白名单在 `wire_api = "chat"` 下是死的。

## 裁决

### 一、重复的 usage 不再是协议错误,以**最后一份**为准

尾帧是契约定义的权威帧,后到覆盖先到,对"两份相同"和"前一份是半成品"两种情况同时正确。

**真正的理由不是宽容,是这个字段的分量**:usage 不参与任何语义输出,它只喂 `/cost` 的账本和
压缩预算的锚。一个不影响模型说了什么、只影响计费显示的字段,**不配让一整轮已经完整流完的回答
作废**。fail-closed 该守的是语义边界(未知 delta 字段、变了的 choice index、[DONE] 之后还来的
语义帧),不是计数器。

### 二、不做"两份相同才放行,不同才报错"

那等于把"哪一份对"的裁判权留给自己,而 kloop 没有裁判的依据。更实际的是:真要有网关在 finish
帧发一份部分统计,报错的代价是用户白跑一轮,收益只是一个数不准——不成比例。

### 三、不累加

观测到的两份是同一次统计的副本。累加会让 token 数直接翻倍,污染 `/cost` 与压缩预算——比丢掉
一份糟得多。

### 四、`parse_usage` 不动

它本来就只读认识的键(`prompt_tokens` / `completion_tokens` / `prompt_tokens_details.cached_tokens`),
对网关多塞的 `cost`、`cost_breakdown`、`router_detail`、`completion_tokens_details` 天然宽容。
需要放宽的只有"能出现几次",不是"能有哪些字段"。

## 验收

- `repeated_usage_keeps_the_last_report`:真实帧形(finish 帧一份、尾帧一份,两份数值不同),
  断言整条事件序列,末尾 usage 取尾帧那份。
- 既有 26 条 chat 契约测试全绿,尤其 `accumulates_tool_calls_and_usage_across_chunks`
  (只有一份 usage 的标准形状)不受影响。

## ✅ 完成

2026-09-18 完成,一次提交 `<sha>`。`cargo fmt` + `clippy -D warnings` + 全量 `cargo test` 全绿。
