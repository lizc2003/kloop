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

### 片 6 — Anthropic 轨发 `x-claude-code-session-id` ✅（2026-08-31；提交 SHA 以本条所在提交为准）

片 2 只给 Responses 轨补了 body 里的 `prompt_cache_key`,理由写的是「Anthropic 没这个字段」。用户指出那个理由不完整:**字段没有,但真实部署里前面架着网关,网关要的是 header,而 kloop 一个都没发**——`anthropic.rs` 当时只发 `x-api-key` + `anthropic-version`。

用户给的参考是 Claude Code 的 gateway 协议(`code.claude.com/docs/zh-CN/llm-gateway-protocol`),它把这件事写死了:

| 请求头 | 文档定性 |
| --- | --- |
| `x-claude-code-session-id` | 会话唯一标识,「**使用它来聚合来自一个会话的所有请求,而无需解析请求体**」 |
| `x-claude-code-agent-id` | 子代理标识,仅子代理请求上存在 |
| `x-claude-code-parent-agent-id` | 父代理标识,仅嵌套时存在 |

分类是**「使用」不是「转发不变」**——只有 `anthropic-version`/`anthropic-beta`(以及 AWS 上的 `anthropic-workspace-id`)要求逐字转发,这三个会话头明写着给网关自己读,用途含**路由**。

**用户拍板「要兼容当前的网关」**:照抄这个名字,不另起 `x-kloop-*`。代价是网关会把 kloop 流量归属成 Claude Code——用户知情并接受。

落地:`anthropic::stream` 增 `session: Option<&str>`,取值仍是 `Config::cache_key()`(与 Responses 轨同源,采样/压缩/子 agent 共享)。两道过滤:空串不发(HTTP 接受空 header 值,过滤只能是我们自己做),**非 ASCII 不发**——`HeaderValue::from_str` 会把非 ASCII 当 obs-text **放行而不是拒绝**,而 obs-text 已废弃、代理处理不一致,正好砸在这个头唯一要经过的那一跳上;而会话 id 只要求是合法文件名(`checked_session_path` 只挡控制字符和路径分隔符),非 ASCII 的 `thread/start` id 完全合法。丢掉提示只损失缓存亲和,发一个网关噎住的头损失整个请求。

测试 `session_id_rides_the_gateway_header_without_touching_the_body`:五态表驱动(正常值发、空串不发、None 不发、非 ASCII 不发、控制字符不发),并断言 body 里**没有** `prompt_cache_key`——两条轨的载体不同,不能互相串味。

**未做**:`x-claude-code-agent-id` / `parent-agent-id`。kloop 的子 agent 在网关侧目前不可分辨(内部有 `child_session_id = {parent}-{agent}`,只用于转录文件名)。这属于成本归属而非缓存亲和,不在本次目标内。Chat 轨也未动(无实测)。

### 片 7 — 收紧会话 id 字符集 ✅（2026-08-31；提交 SHA 以本条所在提交为准；用户拍板「收紧,不用考虑兼容性」）

片 6 在 provider 层加了一道「非 ASCII 不发 header」的过滤,用户追问:**thread id 为什么要允许非 ASCII?**

查下来答案是——**没有理由,那是缺省不是决定**。`checked_session_path` 那组校验只回答路径穿越(非空、非 `.`/`..`、无 `/`、无 `\`、无控制字符、单路径组件、leaf 非符号链接),ASCII 与它正交,所以没人挡。

**先纠正调查中期一个说过头的结论。** 我一度断言这是活的正确性 bug,给出两个方向(`thread/start` 的 `create_new` claim 撞车、`thread/resume` 恢复没点名的会话)。在这台 macOS 上实测文件系统行为属实——NFC `é`(`\xc3\xa9`)与 NFD `é`(`e\xcc\x81`)字节不同,`create_new` 第二个报 `FileExistsError`,目录里只有一个文件。**但可达性不成立**:`thread_start`(`server/src/lib.rs:654`)用 `rollout::new_session_id` **服务端生成** id,客户端只能在 `resume`/`fork` 递 id 且必走 `checked_session_path`;而纯 ASCII 字符串没有非 ASCII 的 Unicode 规范等价形式,所以客户端递的非 ASCII id 永远匹配不到任何现有会话。文件系统那个观测是真的,把它接到 kloop 上的那条链是我编的。

**收紧的真实理由**:「会话 id 是安全 ASCII token」此前是**偶然事实而非被保证的性质**,下游想依赖只能各自重新推导——片 6 那道 header 过滤正是这么来的。收紧后它成为显式不变量,文件名/header/日志/wire 可直接依赖。成本为零:kloop 铸的每个 id 都在新集合内。

新规则 `session_id_is_safe`:非空、不以 `.` 开头、只含 `[A-Za-z0-9._-]`。前导点规则蕴含 `.`/`..`,字符集蕴含分隔符与控制字符,故旧的路径组件检查是冗余的,一并删去;符号链接 leaf 检查保留。片 6 那道 provider 过滤**也保留**——provider 不该信任调用方。

测试扩充既有的 `checked_session_paths_reject_traversal_and_symlink_leaves`:除原有穿越用例,新增非 ASCII、空格、前导点、控制字符、集合外标点五类拒绝;并**正向断言** kloop 自己铸的 id 全部通过(`new_session_id` 实时产物、`20260831-083106`、`-2` 撞名变体、`-agent-1` 子 agent 形态)——防止收紧收过头砸到自己。

### 片 8 — Chat 轨补 `prompt_cache_key` ✅（2026-08-31；提交 SHA 以本条所在提交为准）

片 2 把 Chat 轨列为非目标,理由是「无实测」。用户要求补上,于是**先测再定**(教训 84c:探针必须带一行「不发这个字段」的对照)。

真实 gateway `/chat/completions`,body 形状照抄 kloop 实际所发(`max_tokens`、`stream_options`、tools、`reasoning_effort`):

| 行 | 结果 |
| --- | --- |
| 带 `prompt_cache_key` | HTTP 200 |
| 对照:不带 | HTTP 200 |

**字段被接受**,且对照行同样 200,所以这个 200 有意义(不是"整条轨都能通"的假阳性)。

**中途一个错误结论,连同纠正一起记下来。** 我看到这次探针的 usage 里没有 `prompt_tokens_details`,就断言「这条轨不上报缓存,效果不可测,`/cost` 恒显示 cache-read 0」。用户指出 `cost_breakdown` 才是这个平台的观测点,复测推翻了我的结论——**那个探针只有 47 token、只发一次,根本形不成缓存**,我把「字段缺席」读成了「平台不上报」。

用 3,076 token 的稳定前缀连发三次:

| 次 | `prompt_tokens_details` | `prompt_text_cost` | `prompt_cached_cost` |
| --- | --- | --- | --- |
| 1 | 缺席 | 0.0012304 | — |
| 2 | 缺席 | 0.0012304 | — |
| 3 | `{cached_tokens: 2816}` | 0.000104 | 0.00011264 |

即 **chat 轨的缓存是真的会生效的**,命中时 prompt 成本降约 12 倍。`prompt_tokens_details` **只在命中时出现**,缺席意味着「没命中」而不是「不上报」——kloop 的 `openai.rs:253` 把缺失读作 0 而不是报错,行为正确;`/cost` 在命中时会显示真实数字。另外命中出现在第 3 次而非第 2 次,与 Responses 轨 0/6656/0 的抖动同型,是缓存亲和这条线的旁证。

按用户的界定,`cost_breakdown` / `prompt_cached_cost` 是 example.com 平台自己的字段而非 OpenAI 标准,所以 **kloop 不解析它**(`Usage` 是 provider 无关类型);它只作为诊断手段记在这里。顺带观测:chat 的 `router_detail.router_name` 是 `ccp`,responses 是 `polo`,两条轨走不同上游。

因此这一片**接受与收益都是实测的**。测试 `chat_carries_the_session_id_as_prompt_cache_key` 只断言 wire 形状(给 key 则发、空串不发、None 不发),命中率属于端点不属于我们。

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

## 片 9 ✅ — `read_offloaded` 的逃生口自己会被 offload

抓真实审查任务的请求体时发现的,不是推理出来的:

```
input[ 7] function_call_output  2093 字符  …[full output offloaded, id=off-0005, …]
input[11] function_call read_offloaded  {"id":"off-0005"}
input[12] function_call_output  2093 字符  …[full output offloaded, id=off-0006, …]
```

`bash` 的输出超过 8000 字符 → 被 offload,留下 head/tail 预览 + `off-0005`。模型照提示调
`read_offloaded(off-0005)`,取回的**全文在进 history 的路上又过了一次同一个阈值**,于是拿回
逐字节相同的预览,只换了个新 id。模型烧掉一个往返、什么也没得到,而且被引着再试一次。

而这恰恰是 `read_offloaded` **唯一该被调用的尺寸**——内容小于阈值时它根本不会存在。

改法:`read_offloaded` 按 `OFFLOAD_WINDOW_CHARS`(6000)开窗返回,还有剩余时结尾写明
`char_offset=<下一个偏移>`。窗口宽度必须**严格小于** `OFFLOAD_CAP_CHARS`(8000)并留出续行的
余量,否则同一个坑原样复现;这条约束写进了两个常量的文档注释,互相点名。参数取名
`char_offset` 而非 `offset`,因为 `read_file` 的 `offset` 是**行号**,同名不同单位是给模型
埋雷;续行提示直接把下一个数字印出来,模型抄即可,不必自己算。

测试 `read_offloaded_windows_a_large_output_without_spilling_again`:40,000 字符的载荷循环取窗,
断言(a) 每次返回都 `< OFFLOAD_CAP_CHARS`(直接编码那条不变量),(b) 拼回去与原文逐字节相等,
(c) 循环在 16 次内终止,(d) 越界 offset 报错而不是返回空窗——空窗对模型读起来就是"取完了"。

## 慢的真正原因(推翻本文件前面的判断)

用户指出对照组是 **codex 源头、手选 xhigh**,不是 codex 的 medium 记录。重测,同一个 commit、
同一个 gateway、都走本地抓包代理:

| | 墙钟 | 请求数 | provider 耗时 | 本地工具耗时 |
| --- | --- | --- | --- | --- |
| kloop medium | 353s | 16 | — | — |
| codex medium | 418s | 20 | — | — |
| kloop xhigh | 3312s | 45 | 1018s (31%) | **2294s (69%)** |
| codex xhigh | **317s** | 13 | 306s (97%) | 11s |

medium 下 kloop **更快**。xhigh 下差 10 倍,但差的不是模型也不是线路:kloop 那轮 0 个协议错误,
provider 只占 31%,其余全花在本地——它检出了一个 git worktree,跑了 63 次 cargo 编译和测试。

原因是**两边加载的项目指南不对称**:

- 仓库根有 `CLAUDE.md`,全仓没有 `AGENTS.md`。
- kloop 的 `context.rs` 把 `CLAUDE.md` 当兼容回退加载,于是「完成标准:cargo fmt + clippy +
  test 全绿」进了第一个请求(已核对 `instructions` 文本命中)。
- codex 只找 `AGENTS.md`(实测它执行 `rg --files -g 'AGENTS.md' ..`,零命中),整轮**没有任何
  项目指南**。

也就是说 kloop 把仓库自己的完成标准套到了"审查"这类只读任务上,把审查办成了验证。这不是引擎
缺陷,是上下文不对称加上该标准没有区分任务类型。要缩到 codex 的量级,给仓库加一个 `AGENTS.md`
(kloop 与 codex 都优先读它),并在完成标准里写明它约束的是**改动**而非审查。

顺带把两个前面的猜测记为**已证伪**,免得再走回头路:

- **不是请求头。** codex 每个请求带 `session-id` / `thread-id` / `originator` /
  `x-client-request-id` / `x-openai-internal-codex-responses-lite`,kloop 只有 auth。交错发、
  各 8 次的对照实验里两组的命中率**逐次完全相同**(95% 均值),头没有影响。
- **不是回放推理条目,也不是 `reasoning.context`。** codex 的 `input` 里一条 `reasoning` 都
  没有,kloop 有;剥掉 reasoning 条目、加上 `context: "all_turns"`,命中率都纹丝不动。
- **缓存本身没问题。** kloop 的 `input` 严格追加、共享部分逐字节相同,`instructions` / `tools`
  完全一致;同一个 body 连发命中 97%。之前看到的"卡在 7680"是因为 kloop 每轮只加 150–500
  token,很少跨过 1024 的块边界,不是缓存不延伸。

## 片 10 ✅ — 真实场景复测:仓库找错了,结论重来

用户指出实测仓库是 `被审仓库`,不是 kloop 仓库。这推翻了
片 9 那套「上下文不对称」的因果:**gateway 的 `CLAUDE.md` 内容就是 `@AGENTS.md`**,
两个工具读的是同一份 15,960 字节文件,codex 也照着里面第 160 行跑了 `go test`、
`make docs-check`。不对称在那里根本不存在。

同 commit(`2bb28b48`)、同 xhigh、同 gateway 实测:

| | 请求数 | 总提示量 | 缓存命中 | 单请求上下文 | 墙钟 | 产出 |
| --- | --- | --- | --- | --- | --- | --- |
| codex | 36 | 4.17M | 95% | 116k | 877s | 完整审查报告 |
| kloop 第 1 次 | 49 | — | — | — | 817s | 死于上游 `server_error` |
| kloop 第 2 次 | **70** | 5.58M | 82% | 80k | **1164s** | **交接摘要,不是审查** |

(注:kloop 的 `Usage` 存的是 `input_tokens = 总量 − 命中量`,第一次算命中率时拿
未命中部分当分母,得出 456% 这种不可能的数,已更正。)

**70 对 36,正好一倍的往返**,这就是用户说的"慢一倍";墙钟只差 33%,因为 kloop 单
请求上下文小、更快。

排除掉的假设(都做了实测,免得后人重走):审批不是原因(24 个 bash 全默认放行,0 次
被拒,连 `permissions.json` 都没生成);本地构建不是原因(整包跑和 `-run` 收窄都是 8
秒);上游 `server_error` 打死整轮不是**这个**症状的原因(用户明确说跑完了)。

### 慢的真正原因

链条终点是一行代码:

1. 主 agent 扇出两个后台子 agent,**模型自己填了 `max_rounds: 12` 和 `10`**
2. 审 11 文件 / 1437 行新增的提交,这点轮数不够,两个子 agent 正好用满配额
3. `agent.rs` 的 `MaxRounds` 提前返回写死 `final_text: String::new()` —— 22 个请求、
   35 万 input token 的工作**全部丢弃**
4. 父 agent 收到两句"未返回有效审查结论",只能自己重做(48 轮)
5. 拖长后触发压缩,最终输出是上下文交接摘要而不是审查报告

第 3 步是纯粹的 bug:下游 `subagent.rs:648` 明明写着
`format!("[sub-agent stopped at its round limit]\n{}", outcome.final_text)`——**管道是
通的,源头被写死成空串**。轮次上限的用意是"别失控地烧钱",不是"把已经烧掉的钱产出的
东西也扔掉"。

## 片 11 ✅ — 三处修改

**(a) `MaxRounds` 返回已产出的文本。** 回合循环外加 `produced_text`,每轮
`record_provider_assistant` 之后用 `append_produced` 追加该轮 assistant 文本(跳过只
发工具调用的轮次,轮次之间空行分隔),`MaxRounds` 分支返回它。

**(b) 可重试的流中断改为续跑,不再打死整轮。** `Sampled::Partial` 且
`error.is_retryable()` 时:部分输出已由 `record_provider_assistant` 入库,而
`replayable_partial` 早就滤掉了未签名 thinking 和全部 `ToolUse`,所以 history 里是一个
合法的 assistant 轮次,从它往下发是安全的。于是记一条 `STREAM_RESUME_MSG` 续跑提示、
`continue`,上限 `STREAM_RESUME_LIMIT = 3` 兜底 flapping 上游。

这里有个**测试逼出来的修正**:第一版只 `continue`,结果 headless 那条
`text_mode_preserves_transport_error_partial_and_exits_one` 红了——续跑之后模型只返回
剩余部分,`final_text` 就只剩后半段,前半段从最终答案里丢了。截断续跑那条路早就用
`truncated_prefix` 解决过同一个问题,所以续跑也押进同一个累加器。**如果当时把那条红
测试当成"契约过时"直接改掉,这个 bug 就会带着修复一起进仓库。**

**(c) `run_agent` 不再向模型暴露 `max_rounds`。** 模型没有任何依据去猜这个数——它不知
道每轮多贵,也不知道任务要几轮,这次猜了 12 和 10。要"别做太深"应该写在 prompt 里,而
不是设一个会把已完成工作作废的硬闸。schema、`RunAgentInput` 字段与解析一并删除,子
agent 固定无上限;人显式设的 `--max-rounds`(headless 跑飞兜底)保留。
`compact.rs` / `tools/mod.rs` 里那几处 `max_rounds: Some(5)` 是**测试脚手架**的 Config,
不是生产路径。

### 验证 ✅

新增五条测试:

- `max_rounds_returns_the_text_produced_before_the_cap` — 断言整串 `"finding one\n\nfinding two"`
- `retryable_stream_failure_after_output_continues_the_turn` — 断言 `Completed` 且
  `final_text == "first halfsecond half"`(两半都在)
- `fatal_stream_failure_after_output_still_ends_the_turn` — 不可重试仍然结束,但仍带回产出
- `text_mode_resumes_a_dropped_stream_and_keeps_both_halves` — headless 退出码 0、输出两半
- `text_mode_preserves_a_fatal_stream_partial_and_exits_one` — 致命错误仍退出 1 且保留部分输出
- `run_agent_no_longer_accepts_a_round_limit` — `deny_unknown_fields` 让"这个参数没了"
  可见,而不是静默忽略一个调用方以为设上了的上限

另记一条环境教训:并行跑的审查任务会在同一个 target 目录里调 cargo,把 provider 的构建
产物弄成过期状态,表现为 `stream_attempt` "takes 5 arguments but 6 supplied",而报错指向
的 `lib.rs:425` 是一行文档注释。`touch crates/provider/src/lib.rs` 强制重建即可——**当
编译器指的行号明显不是代码时,先怀疑构建缓存,别怀疑源码**。


## 片 12 ✅ — 修完之后的六轮实测,以及一次被实测推翻的自己的改动

片 11 只跑了单元测试就写完了,**没有实测**。用户问"慢的问题实测解决了吗",答案是没有,
补测之后结论如下(同 commit `2bb28b48`、同 xhigh、同 gateway):

| | 请求数 | 墙钟 | 产出 |
| --- | --- | --- | --- |
| codex 基线 | **36** | **877s** | 审查报告 |
| kloop 修前 | 70 | 1164s | 交接摘要,不是审查 |
| 片 11 三处改动全上 | **342** | 2001s | 审查报告 |
| 回退 (c) 之后 | **99** | 1911s | 审查报告 |

前四次实测有三次中途死掉,死因各不相同——一次是 `responsesapi.websocket_timing`
未知事件(见上一提交),两次是端点对大请求变慢触发 45 秒 open timeout(34 次重试后
耗尽)。**"网络不好"必须先排除再谈性能数**,否则量到的是链路不是实现。

### (c) 被实测推翻:`max_rounds` 回装

片 11 把 `max_rounds` 从 `run_agent` schema 拿掉,理由是"模型没有依据去猜这个数"。
这个理由本身没错,但结论错了:**拿掉之后一个子 agent 一口气跑了 158 轮,整轮请求数
从 70 涨到 342。** 那个上限实现方式很糟(撞上限把工作扔掉),但它是子 agent 唯一的
预算约束。

正确的组合是 **(a) 修好丢弃 + 保留上限**:回退后那轮仍然撞了 2 次 round limit,但
日志变成 `Agent(...) · Completed · stopped at round limit`,子 agent 带着 findings
回来,各只花 8 个请求,不再白烧。这正是 (a) 该修的东西,而它需要上限还在才有意义。

判据:**"这个旋钮被误用了"不等于"这个旋钮该删"。** 误用的后果由 (a) 兜住之后,
低估上限的代价从"丢掉全部"降到"少挖一层",旋钮就重新变成一个合理的预算表达。
删之前先问:它现在约束着什么,删掉之后谁来约束。

### 仍未解决:主 agent 的 83 轮

99 个请求里子 agent 只占 16 个,主 agent 自己跑了 83 轮。日志末段是主 agent 在
`git show ... | sed -n` 一段段重读子 agent 已经审过的文件——**它不采信子 agent 的
产出,自己又复核了一遍**。这和最初"子 agent 返回空所以父 agent 重做"是同一症状的
两个不同成因,修掉前者之后后者才露出来。下一步方向,未做。

### 一条待定的脆弱点

`STREAM_OPEN_TIMEOUT = 45s`(等响应头)vs codex 的 `stream_idle_timeout` 默认 300s,
而本会话实测 xhigh 的 TTFT 可达 69~238 秒。网络好时触发 0 次,网络差时一轮触发 34
次、每次 45 秒空等加整请求重发。不是常态成因,但是真实脆弱点。是否放宽未决。


## 片 13 ✅ — 两处按实测结论的改动

**(a) `STREAM_OPEN_TIMEOUT` 45s → 300s。** 这个超时等的是**响应头**,而在 gateway
这类代理上,头要等到模型开始产出才发,于是思考时间被折进了这个窗口:实测 TTFT 低
effort 约 3 秒,xhigh 是 69~238 秒。45 秒等于把"慢但健康的 xhigh 请求"判成断连——
一轮真实审查触发了 34 次,每次代价是空等加整个请求重发。codex 的同位旋钮
`stream_idle_timeout` 默认就是 300 秒。真正断掉的连接仍会被发现,只是晚一点;
30 分钟的 wall timeout 仍是外层兜底(契约测试新增了 open < wall 的断言)。

**(b) BASE_SYSTEM 补压缩后的连续性约束。** 用户提出"是不是 codex 的 system prompt
里有约束",比对属实:

- kloop 原文只有一句"上下文不受窗口限制,**别为了省地方少干活**"——在鼓励多做,
  而**压缩之后该怎么办一个字没写**。
- codex 有:"Do not restart from scratch… **Do not redo completely finished work**…
  treat a turn spanning compactions as **one logical chain of events**"。

实测症状精确对应:99 个请求里 83 个在主 agent,而压缩触发后主 agent 开始
`git show … | sed -n` 逐段重读子 agent 已审过的文件——它把摘要读成了"我还没做过"。
补的措辞除了照抄"当成一条链、别重做已完成的工作",还加了一条 kloop 特有的:
**不要重新推导子 agent 已经报过的结论;摘要里丢了哪个细节就去取那个细节,不要把
整轮调查重做一遍**。

并行那条不用改:kloop 已有"Independent tool calls in one turn run in parallel;
batch them",而且实测 kloop 每请求 3.6 个工具、codex 只有 1.1,批得更狠。

这两条的效果需要下一轮实测验证(基线:99 请求 / 1911s,codex 36 / 877s),**本片
未测**。


## 片 14 ✅ — 纯观测:236 轮到底在读什么

用户同意先不加改动、只观测。分析第 7 轮(主 agent 240 个请求)的会话文件,结果**同时
否掉了我此前的三个假设**:

- **不是压缩后失忆。** 读取按压缩边界(第 22/51/91/158 轮)分成五段,次数是
  15/16/13/8/8——均匀且递减;真是压缩导致的话每个边界后该有尖峰。
- **不是重复读同样的东西。** `<file>.go` 读了 60 次,其中 **56 个不同的
  (offset, limit)**,只有 3 个组合重复过。
- **不是 offload 阈值逼出小窗口。** 492 个工具结果里只有 **7 个(1%)** 被 offload。

也就是说片 13 那句压缩提示词的**归因是错的**,已撤回。

真实形态:434 次读取覆盖 **77 个不同目标,重复率 82%**,而结果长度中位 6400 字符、
最大 7919、**没有一个超过 8000**。这个分布指向一个常量——`READ_CONTENT_CHARS = 7_000`:
`read_file` 每次最多返回 7000 字符就截断并附"call read_file with offset=N to continue"。
**392 次 read_file 里 179 次(46%)撞上这条上限。** Go 代码 7000 字符约 180 行,一个
1000 行文件读一遍要 6 个往返。

## 片 15 ✅ — 把五个常量从"省上下文"改成"省往返"

两边的资源取向是反的:kloop 单次读取 7000 字符上限、8000 字符落盘、单请求上下文
80k;codex 单请求 116k、全程只有 40 个工具调用。**而两边缓存命中都是 95%——上下文
几乎免费,一个往返 20 秒。kloop 在攒便宜的资源、花贵的。**

| 常量 | 原值 | 新值 |
| --- | --- | --- |
| `READ_CONTENT_CHARS` | 7,000 | 30,000 |
| `SEARCH_CONTENT_CHARS` | 7,000 | 30,000 |
| `OFFLOAD_CAP_CHARS` | 8,000 | 32,000 |
| `OFFLOAD_WINDOW_CHARS` | 6,000 | 24,000 |
| `KEEP_RECENT_TOKENS` | 2,000 | 20,000 |

三条约束必须一起保持,单独抬任何一个都会更糟:读取上限要**低于**落盘阈值(否则大读取
立刻被换成预览),开窗要低于落盘阈值(否则逃生口自噬,见片 9),而 `KEEP_RECENT_TOKENS`
是压缩后逐字保留的量——2000 token 意味着压缩完手上几乎没有原文。

`context_window` 保持 200,000 不动:它是 provider 无关的默认值,而 codex 对
gpt-5.6-sol 用的是 258,400。想吃掉这 29% 的差距用 `KLOOP_CONTEXT_WINDOW=258000`,
不改默认。

### 测试夹具全部改为从常量推导

15 条测试因为常量变动而红,**它们护的性质都对,但尺寸是硬编码的**(`"x".repeat(9000)`
原本高于旧的 8000、现在低于新的 32000)。逐条改成从常量推导
(`OFFLOAD_CAP_CHARS + 1_000`、`KEEP_RECENT_TOKENS as usize` 等),这样任何取值都成立。

两条 glob 测试暴露了一个额外事实:抬到 30,000 之后,**字符上限对 glob 用短文件名已经
永远咬不到了**——`GLOB_LIMIT`(100 条)乘以文件系统 255 字符的单段上限仍然低于预算。
所以夹具改用两级 180 字符的嵌套目录把单条路径撑到 ~400 字符,让被测的确实是字符上限
而不是条目上限。**硬编码尺寸的危险不是测试会红,是它会在常量变动后静默地不再测该测的
东西。**

效果待实测(基线:99 请求 / 1911s;codex 36 / 877s)。


## 片 16 ✅ — 重复实验:方差大于我测出来的所有"改进"

用同一份代码、同一个 commit、同一句 prompt 把片 15 那轮重跑一次:

| | 第 8 轮 | 第 9 轮(重复) |
| --- | --- | --- |
| 请求 | 65 | **117** |
| 墙钟 | 1252s | **2087s** |
| 压缩次数 | 2 | **5** |
| 子 agent | 2 | 3 |

**1.8 倍。**这推翻了片 15 结尾那句"请求 −34%、墙钟 −34%"——65 和 117 是同一份代码,
而修前的 70、回退后的 99 都落在这个区间里。**之前每一条基于单次运行的对比,包括我
断定"拿掉 max_rounds 更糟"的那次(342 请求),都可能是在读噪声。**

### 分岔点查到了,而且是片 15 的直接后果

两轮的**前 6 个工具结果逐字节相同**(30068, 5389, 99, 22692, 11649, 10781),开局是
确定的。第 7 个开始分岔:

| | 第 8 轮 | 第 9 轮 |
| --- | --- | --- |
| 第 7 个工具结果 | 8,470 字符 | 17,204 字符 |
| 第 7 个请求的上下文 | 82k | **165k** |
| 首次压缩 | 第 16 轮 | **第 7 轮** |

压缩阈值 = 200,000 − (8,192 + 15,000) = 176,808。注意**第一个工具结果就是 30,068
字符,正好是新的 `READ_CONTENT_CHARS`**:一次读取现在能塞进约 7,500 token,六次就
把上下文推到压缩线。于是"第 7 轮越线还是第 16 轮越线"取决于模型早期的一点点差异,
而越线早晚决定了后面几十轮。

**片 15 做的事实际是:消掉了读取分页(46% → 0%,确定有效),但把瓶颈换成了上下文
饱和,并让整轮开销对早期微小差异极度敏感——把一个稳定的低效换成了一个不稳定的高效。**

### 顺带否掉的两条

- **「共用 `prompt_cache_key` 导致互相驱逐」**:直接探针(同一 key 配两个不同前缀 vs
  各用各的 key,各交错 4 轮)两组均值都是 58%,模式完全一样。否掉。
- **「抬常量导致命中率 93% → 53%」**:同一个探针顺带量到,端点对一个 4,000 token
  的**完全相同**的前缀连发,命中率是 16/99/99/16 来回跳,均值 58%。两轮相隔四小时的
  命中率差异不能归给代码。收回。

## 片 17 ✅ — 子 agent 拿到自己的缓存身份

`config.rs` 的 `subagent_from` 原本把父的 `session_id` 原样复制给子 agent,而
`cache_key()` 直接返回 `session_id`——于是主 agent 和每个子 agent 用**同一个**
`prompt_cache_key`,但三者前缀毫无共同之处。这个 key 的唯一用途是"共享前缀的请求路
由到一起",共用等于要求路由器把不相干的对话钉在同一台后端。

而子 agent 的**会话文件名**早就是 `{父}-{标签}`(走 `child_session_id`)——**文件名和
缓存键用了两套身份**。现在统一:`subagent_from` 直接把 `session_id` 设成
`{父}-{agent_id}`,`sub_history` / `child_session_note` 改用子 cfg 自己的
`session_id`(不再二次拼接),`child_session_id` 辅助函数随之删除。

注意:上面的探针已经证明这**不是**当前端点上命中率波动的原因,所以这条改动的理由是
设计正确性(各 agent 独立分流),不是性能。不拿它去解释任何墙钟数字。

## 片 18 ✅ — 抬窗口的实测,以及给 provider 加配置位

先用 `KLOOP_CONTEXT_WINDOW=258000` 实测(第 10 轮):

| | 请求 | 墙钟 | 压缩 | 缓存命中 |
| --- | --- | --- | --- | --- |
| codex 基线 | 36 | 877s | 0 | 95% |
| 第 8 轮(窗口 200k) | 65 | 1252s | 2 | 53% |
| 第 9 轮(第 8 轮重复) | 117 | 2087s | 5 | 36% |
| **第 10 轮(窗口 258k)** | **46** | **1037s** | **0** | **93%** |

因果链自洽:抬窗口 → 不越压缩线 → 前缀不被重写 → 缓存不失效 → 也不必压缩后重建理解
→ 轮数下来。这也解释了第 9 轮压 5 次时命中率只有 36%——**压缩本身就是缓存杀手**,而
第 8/9 轮那 1.8 倍的方差正是"早压还是晚压"的分岔被放大。**仍是 n=1**,但这次有一个
二元的机制指标佐证(压缩 0 次不是抖出来的),信心高于第 8 轮那次。

### 配置位

不把 258,400 写死进默认值:那是单个模型的窗口,窗口更小的模型会因此持续溢出。给
`[model_providers.*]` 加 `context_window` 键,让每个 provider 报自己的真实窗口:

    KLOOP_CONTEXT_WINDOW  >  provider 的 context_window  >  默认 200_000

环境变量仍然最高,这样 provider 值写错或缺失时不必改配置块就能纠正。为此
`RuntimeSettings.context_window` 从 `Option<u64>` 改成 `Option<Option<u64>>`
(外层 `None` = 用户没说)——原来的类型把"没设"和"显式设成 200_000"压成了同一个值,
provider 的窗口永远没机会生效。

`ResolvedProviderSettings` 只携带**所选** provider 的窗口:`/provider` 中途切换需要
重新推导整个压缩预算,那是另一件事。配置值必须是正整数,`0` 报错而不是当成"不限"
——`KLOOP_CONTEXT_WINDOW=0` 是关闭的意思,但配置文件里写 `0` 远更可能是笔误。

## 片 18-旧 — context_window 不改代码

按用户选择,用 `KLOOP_CONTEXT_WINDOW=258000` 试(codex 对 gpt-5.6-sol 用 258,400,
kloop 默认 200,000 保守了约 29%)。不把 258,400 写死进默认值:那是这一个模型的窗口,
窗口更小的模型会因此持续溢出;而 `context_window` 目前只能靠环境变量设,没有
per-provider 配置位。真要固化,应该先加配置位而不是改全局默认。


## 片 19 ✅ — 压缩提示词按 cc / codex 重写

三方对照(用户提议查参考项目):

| | kloop 原 | codex | cc |
| --- | --- | --- | --- |
| 长度 | 376 字符 | 2,626 | 17,062 |
| 结构 | 6 项一句话 | 9 节 | 9 节 + 完整输出样例 |
| 草稿区 | 无 | 无 | `<analysis>`,注入前剥掉 |
| 禁工具调用 | 无 | 无 | 有,且写明后果 |
| 防伪造用户消息 | 无 | 无 | 有 |
| 安全约束逐字保留 | 无 | 无 | 有 |
| 防任务漂移 | 无 | 无 | 要求直接引用 |
| 全量转录指针 | 无 | 有 | — |

采纳的不是最长的那份,是每条**有失效证据**的:

- **禁工具前言放最前并写明后果**(取自 cc)。cc 的注释给了数据:adaptive-thinking 模型
  上,尽管末尾已有较弱提示,模型仍会尝试调工具,4.6 上 2.79%、4.5 上 0.01%,一次就是
  整轮作废。kloop 的压缩同样是单轮 + 带全套工具(工具是缓存前缀的一部分,去掉要付一次
  全量重填),同一个坑。
- **防伪造用户消息**(取自 cc)。assistant 消息里长得像用户回合的文本是模型生成的,绝
  不可记成用户的请求/批准/确认。摘要一旦把它写成事实,原文已经没了,无从纠正。
- **安全与凭据约束逐字保留**(取自 cc)。"勿把 key 写进提交文件"这类规则,转述即失效。
- **`<analysis>` 草稿块 + `<summary>` 包裹**(取自 cc)。`canonicalize_summary` 剥掉草稿
  再解包;两个标签都是尽力而为——裸文本仍然可用,不为格式瑕疵丢弃真实工作。
- **转录指针**(取自 codex)。附在摘要末尾指向本会话 rollout。这条让"不要重新推导"变得
  可执行:片 13 那句失败的提示词写了"别重做",却没告诉它**去哪儿取**。
- **子 agent 结论一节**(两家都没有,kloop 特有)。片 16 实测:99 个请求里 83 个在主
  agent 重做子 agent 已审过的文件。

## 片 20 ✅ — 压缩请求自身溢出的自救

用户提的实践问题:上下文 + 压缩提示词超过模型上限,压缩这一步直接失败,怎么办。

原来是**死路**:`Sampled::Overflow` → 反应式压缩 → `compact_once` 把同一份过大的历史再发
一次 → 又溢出 → `EndReason::Error("reactive compaction failed")`,整轮结束。唯一缓冲是
保留尾巴那 20,000 token,超出量大于它就无解。而片 18 加的 per-provider `context_window`
让这个坑更容易踩:配大了,预测式压缩永不触发,直接落到这条路上。

四层实现:

1. **认出溢出**。`sample_summary` 把 `ProviderFailure` 包进 `anyhow`,`is_overflow` 顺链
   `downcast_ref` 取回 `is_context_overflow()`。必须分辨——网络断了去收缩折叠窗口毫无
   意义,为此专门写了负向测试。
2. **收缩**。`shrink_to_newest` 按 `estimate_message_tokens` 丢掉 request 里**最老**的一
   段,目标是被拒尺寸的一半。保新不保旧:尾巴已逐字保留最近的,摘要要与它接得上。切点
   复用尾巴边界那条规则,不劈开 `tool_use`/`tool_result` 对。
3. **底线**。收缩到只剩两条仍溢出就带上下文原样报错,不再空转。
4. **丢弃必须可见**。`DROPPED_PREFIX` 标记插在重建历史的最前面,`describe()` 让调用方在
   界面上说出丢了几条。丢了而不说,下一轮会把"这些事没发生过"当成事实——比丢本身更糟。

**外加一条预防**:一次拒绝是关于真实上限的地面真值。`History::note_overflow_at` 记下被拒
尺寸,`effective_window(configured)` 取二者较小值喂给预测阈值——**只降不升**(在 N 被拒
只证明 N 太大,不证明任何值安全)。这也让片 18 那个人填的配置位能自我纠正。
