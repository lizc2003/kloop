# Plan 106 — gateway xhigh 实测:放行 `keepalive` 带外事件、补 `prompt_cache_key`

> 状态：进行中（2026-08-31）
>
> 起因：用户反馈「kloop 用 gpt-5.6-sol xhigh 审查代码非常慢,codex 同样 gpt-5.6-sol xhigh 快一倍」。
> 依赖：Plan 95(Responses 带外事件窄口放行,本计划沿用其纪律与形状)。
>
> 开发期决策(已定,来自用户「按待办顺序一条一条做」)：
> - 三片按序:①`keepalive` 放行 + 错误信息带事件名 → ②`prompt_cache_key` → ③前缀瘦身(第三片单独再议,不在本计划内落地)。
> - `keepalive` 走 Plan 95 同一条窄口:加进 `is_out_of_band` 的**已识别**名单,不改成"忽略所有未知事件"。

## Context — 诊断实测

用真实 gateway(`~/.kloop/config.toml` 的 `gw_router` / `gpt-5.6-sol`)逐条测出来的:

**1. 「codex 也是 xhigh」这个前提不成立。** 扫本机 `~/.codex/sessions` + `archived_sessions` 全部 110 个 rollout 的 `reasoning_effort`:

```
118 次 "medium"   1 次 "low"   0 次 "xhigh"
```

`~/.codex/config.toml` 无 effort 设定,走默认 medium;kloop 配置写死 `effort = "xhigh"`。同端点同模型实测首字延迟:

| effort | ttft_ms | 单请求 reasoning tokens |
| --- | --- | --- |
| low | 2,822 | 0 |
| xhigh | 69,205 ~ 238,276 | 9,840 ~ 12,040 |

codex medium 会话每请求 reasoning 中位 0、最大 516。**慢的主因是 effort 档位差,不是 kloop 的实现。**

**2. `max_output_tokens` 这条排除掉了。** kloop 发 `max_output_tokens: 8192`(codex 的 `ResponsesApiRequest` 无此字段),一度怀疑 xhigh 推理超限触发 `agent.rs:665` 的截断续跑。实测把该字段原样发出去:回 `status: completed`、`output_tokens: 12489`(> 8192)、`incomplete_details: null` —— **gateway 忽略这个字段**,截断续跑从未触发。本计划不动它。

**3. 真 bug:`keepalive`。** kloop 真跑一轮 xhigh 直接死于
`provider protocol error: openai-responses returned an unknown semantic event`。抓 SSE 原始帧:

```
event: keepalive
data: {"sequence_number":2,"type":"keepalive"}
```

gateway 在**模型思考期间**填 keepalive,思考越久填得越多——这正是 xhigh 的常态:

| effort | keepalive 数 |
| --- | --- |
| low | 0 |
| xhigh(kloop 原样请求,ttft 69s) | 2 |
| xhigh(纯问答,ttft 238s) | 14 |

它落进 `responses.rs:1160` 的 `_ => Err(protocol("returned an unknown semantic event"))`,而 Protocol 错误 `retryable=false`(`failure.rs:98-102`)→ 整轮报废。**low/medium 不触发,xhigh 必触发。**

**4. 缺 `prompt_cache_key`。** codex 每请求都发 session id(`client.rs:997`),kloop 不发。同一 7,697-token 前缀背靠背发三次:

```
第 1 次 cached=0    第 2 次 cached=6656    第 3 次 cached=0
```

命中全看运气(缓存亲和路由缺失);codex 会话记录里命中率中位 68%。

## 分片

### 片 1 — 放行 `keepalive`,并让未知事件报错说出事件名 ✅（2026-08-31；提交 SHA 以本条所在提交为准）

`is_out_of_band` 加进 `keepalive`(Plan 95 同一窄口,`codex.` 是厂商命名空间前缀、`keepalive` 是裸名,两者都属"已识别的带外名单")。
兜底错误复用现成的 `bounded_reason` 把事件名带进消息——这次定位靠抓包才拿到事件名,报错本身不说是谁,是可诊断性缺口。

**完成记录**:`responses.rs` 两处改动(`is_out_of_band` 加 `keepalive`;match 兜底错误带有界事件名)。`tests/responses.rs` 新增 4 条:`keepalive_event_is_ignored_mid_stream`(真实 wire 形状 `event: keepalive` + `{"sequence_number":2,"type":"keepalive"}`,断言只产出 TextDelta/BlockDone/Terminal)、`keepalive_after_terminal_is_ignored`(terminal 后守卫同样豁免)、`unknown_non_codex_event_still_fails_closed`(补逐字断言错误含事件名)、`unknown_event_name_is_bounded_in_the_error`(500 字符名只回显 80 + `…`)。README 第 5 条 Provider seam 同步;HANDOFF 补教训 89。

**验证**:`cargo fmt --all -- --check` 通过;`cargo clippy --all-targets -- -D warnings` 退出码 0;`cargo test --workspace` 全绿(退出码 0,`responses` 27 passed 含 4 条新测试,`kloop_core` 754)。**真 key 回归**:重建二进制后对 `gw_router`/`gpt-5.6-sol`/`xhigh` 跑 `--headless` 一轮——`unknown semantic event` 不再出现,错误推进到下一道墙 `reasoning parts overlapped`(见下方"片 1 回归暴露的后续缺陷")。

### 片 1 回归暴露的后续缺陷 — reasoning summary part 生命周期不匹配

放行 keepalive 后,真实 xhigh 轮改报 `openai-responses reasoning parts overlapped`(`responses.rs:931`)。抓包看 gateway 对**每个** reasoning item 的实际形状是:

```
part.added sidx=0 → text.delta sidx=0
part.added sidx=1 → text.delta sidx=1
part.added sidx=2 → text.delta sidx=2
text.done  sidx=2 → part.done sidx=2      ← 只有最后一个 index 收口
item.done  summary_len=3
```

即**每个 part 都 added+delta,但只有最后一个 part 有 `.done`**;前面的 part 永不显式关闭。两次独立抓包、共 24 个 reasoning item 全部是这个形状,不是偶发。

kloop 假设的是严格嵌套(`added(i) → delta(i) → text.done(i) → part.done(i)` 之后才允许 `added(i+1)`),并在 `reasoning_summary_part.added` 处用"有未关闭的 part 就报错"来强制它。两种形状都自洽,但不兼容。

处置见片 3(用户已拍板"接受")。

### 片 2 — `prompt_cache_key` ✅（2026-08-31；提交 SHA 以本条所在提交为准）

Responses 请求体加 `prompt_cache_key`,取会话内稳定值。

**完成记录**:按 Plan 102 教训 84 的形状——会话级旋钮走**请求期参数**、不烘进 `Provider` 构造——给 `stream_attempt` 增 `cache_key: Option<&str>` 参数(紧跟 `effort`)。取值来自新的 `Config::cache_key()`:`session_id` 非空则用它,空则 `None`(`--mock`/测试不发这个字段;空串会把所有未绑定会话赶进同一个桶)。**采样与压缩共用同一个 key**——压缩是同一段对话的又一次请求,单独一个 key 只会把它送到一台没有前缀的后端(与 codex 的 `reuses_prompt_cache_key` 同款)。**子 agent 随 Config 继承父会话的 id**,这是有意的:它的前缀与父会话共享开头字节,同机放置对两者都有利(codex 的 `api_key_subagent_uses_session_id_as_prompt_cache_key` 同款)。

**只上 Responses 轨**。Chat 轨同名字段虽然 OpenAI 也支持,但本次没有针对该轨的实测数据,按教训 84b「不知道就别假装知道」不动它;Anthropic 轨走的是显式 `cache_control` 断点,与此无关。

`MockRequest` 加 `cache_key` 字段(与既有 `effort` 同款),让 core 侧能断言接线。

**验证**:`cargo fmt --check`、`cargo clippy --all-targets -D warnings`、`cargo test --workspace` 全绿。新增 4 条测试:provider 层 `prompt_cache_key_is_sent_when_bound_and_omitted_otherwise`(三态表驱动:给 key 则发、空串不发、None 不发)、core 层 `turn_samples_with_the_session_id_as_the_cache_key`、`unbound_session_sends_no_cache_key`、`compaction_reuses_the_session_cache_key`。

### 片 3 — reasoning summary part 生命周期放宽 ✅（2026-08-31；提交 SHA 以本条所在提交为准；用户已拍板"接受"）

片 1 回归暴露的那道墙。gateway 对每个 reasoning item 的真实形状是「每个 part 都 added+delta,只有最后一个 part 有 `.done`」,kloop 假设严格嵌套并在三处强制它。按用户拍板放宽,**放宽的只是冗余复核,真正的保证一个没动**:

- `reasoning_summary_part.added`:去掉"有未关闭 part 就报错"的守卫。
- `add_content_part` 的 `ItemKind::Reasoning` 分支:同样去掉。两处是同一个生命周期问题,只放宽一半会留下另一半随时再炸。**Message 分支保持严格**——没有观测到消息内容 part 重叠,窄口不外扩。
- `verify_reasoning_parts`:不再要求 `field_done`/`part_closed`。**保留**索引稠密有序、part 数量与最终数组一致、每个 part 的累积文本逐字等于最终数组对应项;错误名相应改为 `reasoning part indices were not dense`。

代价如实记:某个 part 的文本若只在流中出现而与最终数组不符,发现点从"part 关闭时"推迟到"item 结束时"——仍然 fail closed,只是晚一步。`part_closed`/`field_done` 本身没删,`.done` 到达时的乱序/重复关闭检查照旧。

新增两条测试:`reasoning_summary_parts_may_stay_open_until_the_item_closes`(复刻真实形状:index 0 只 added+delta 不收口、index 1 收口,断言 ThinkingDelta×2 + BlockDone(拼接文本 + `enc-blob`)+ Terminal 恰好四个事件)、`unclosed_reasoning_part_still_fails_when_final_text_diverges`(永不收口的 part 最终文本与流式累积不符时仍 fail closed,逐字断言 `final reasoning text did not match streamed text`)——后者是关键,它证明动的是冗余那层。

### 片 4 — 前缀瘦身 ✅（2026-08-31；提交 SHA 以本条所在提交为准）

固定前缀 7,697 input token(instructions 7,977 B + 30 个工具 32,365 B)。逐项拆下来,唯一**零能力损失**的肥肉是 `run_program` 的 TypeScript manifest:它给每个可编程内置工具生成 `/** 完整描述 */` + 签名,而那份描述**在同一个请求的 `tools` 数组里已经原样存在一遍**。实测构成:描述 7,940 B 中,注释占 3,101 B(39%),真正的签名只有 1,159 B(15%)。

改法:新增 `summary_line`(在 `one_line` 之上按 `MANIFEST_SUMMARY_CHARS = 120` 字符截断,截断处加 `…`),用于内置与 inline source 两处 typed 声明。**deferred 工具那处继续用 `one_line` 不截断**——它们没有 catalog 条目,那行是模型唯一的信息来源。

实测效果(本地捕获服务收 kloop 真实请求体,改前 vs 改后):

| | 改前 | 改后 |
| --- | --- | --- |
| `run_program` | 9,019 B | 7,135 B |
| tools 总计 | 32,365 B | 30,481 B |
| 整个请求体 | 40,816 B | 38,971 B |

省 1,845 B ≈ 470 token ≈ 前缀的 6%。同一次捕获顺带确认片 2 的 `prompt_cache_key` 已在线上(值为会话 id)。

**到此为止,后面的都不是零成本的**,结论记在这里免得下次重走:

- 继续砍工具描述散文 = 拿模型行为冒险,`grep`(2,110 B)、`run_agent`(2,465 B)、`bash`(1,542 B)那些字都是 plan 49/66/98 一条条调出来的。
- 把冷门内置(scheduler 2,365 B + worktree 1,420 B + task_\* 3,158 B + send_message/list_agents 1,539 B ≈ 8.5 KB ≈ 2,100 token)挪到既有 `tool_search` 后面 = 能力仍可达,但每次用到都多一个完整往返(xhigh 下是几分钟),且模型可能压根发现不了。**在片 2 已经让前缀大概率命中缓存之后,这笔买卖不划算**,故不做。

### 片 5 — 未做:effort 档位

慢的主因(xhigh vs codex 实际在跑的 medium)是**用户配置**不是代码问题,`~/.kloop/config.toml` 改 `effort = "medium"` 即可,代码不动。

固定前缀 7,697 token(30 个工具的 JSON 占 32KB,`run_program` 一个 9KB)。要不要砍、砍哪些,单独再议。

## 非目标

- 不动 `max_output_tokens`(实测被端点忽略,且它同时喂预测性压缩的增长估算)。
- 不改 effort 默认值——那是用户配置,不是代码问题。
- 不放行 `codex.` / `keepalive` 以外的任何事件;不改其余未知事件的 fail-closed 语义。
- 不动 anthropic / Chat 轨解析。

## 完成标准

每片:`cargo fmt` + `cargo clippy --all-targets -- -D warnings` + `cargo test` 全绿,新增行为带测试,一次 commit 写清验证方式;本文件补 ✅ 与提交号;教训进 HANDOFF.md。
