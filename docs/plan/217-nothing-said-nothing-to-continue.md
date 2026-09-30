# Plan 217 — 一个字没说，就没有"接着写"

> 来源:2026-09-30。用户贴来 TUI:`∗ Thought for 6m37s` → `[response truncated by output limit; asking the model
> to continue (1/3)]` → `∗ Thought for 6m59s` → `(2/3)` → 又开始想。核对会话后用户:「开一个 plan」。起草时
> 用户 Esc、`/effort high`、发"继续",又贴来 `∗ Thought for 6m57s` → `(1/3)`。

## 一、现场

会话 `20260930-032304`,sky-bj / `deepseek-v4.1-flash` / effort `xhigh`,Responses rail。用户隔了 51 分钟发来一份
三条意见的审查回复:

| 轮 | 输入 | 缓存命中 | 输出 | 推理 | 正文 / 工具 |
|---|---|---|---|---|---|
| 首轮 | 261,807 | 0 | 32,768(打满) | 124,342 字符 | 无 / 无 |
| 续写 1 | 261,835 | 0 | 32,768(打满) | 125,021 字符 | 无 / 无 |
| 续写 2 | 用户 Esc,本轮 `aborted` | | | | |
| `/effort high` 后用户发"继续" | 261,863 | 261,760 | 32,768(打满) | 122,650 字符 | 无 / 无,又追了续写 |

- 这一轮是离群值：同一会话 135 轮，输出中位数 490,此前最大 12,641。
- **降到 high 没用。** kloop 的目录里这个模型不声明 effort 档位(`/effort` 显示 "declares none"),字段照发，
  网关认不认分档看不到;实测 high 与 xhigh 一样吃满。所以"降 effort"不能当作可靠的出路写进报错。
- **每一次都从同一个可见上下文出发。** 用户换 effort 后说"继续",输入仍不含前面约 25 万字符的推理,
  模型开头是 "The user gave three new findings. Let me verify each"——第三次从头来。推理末尾在纠结 "should I
  fix that too?":它在脑子里把整套修法设计完，而不是先调一次工具看看。同样的输入，几乎确定同样的结果。
- **两轮输入只差 28 token**,正好是那条 continue 提示的长度——上一轮 12 万字符的推理没有进到模型眼前。
  这条路由整个会话从没返回过 `encrypted_content`;kloop 把无签名推理当 `summary_text` 重放
  (`provider/src/responses.rs` 的 `to_input_items`),网关没把它计进输入。
- **续写那轮是从头想的**:开头是 "Let me start by verifying the three claims. I need to re-read … and run
  experiments"——它本想去读文件，结果又在推理里把上限耗光，一次工具没调。提示里 "break the remaining work into
  smaller pieces" 没起作用。

## 二、现状：这条恢复是给"正文写到一半"做的

`core/src/agent.rs` 的 `classify_outcome`,`AssistantOutcome::OutputLimit(_)` 分支:没到 `TRUNCATION_RECOVERY_LIMIT`(3)
就把本轮正文拼进 `truncated_prefix`、发 Note、记一条 `TRUNCATION_CONTINUE_MSG`("Continue exactly where you left
off; …")、`RoundStep::Retry`;到了上限以 `TurnError::ProviderOutcome(OutputLimit)` 结束，显示为 "response
remained truncated after 3 continuation attempts"。

进这个分支的轮次只有正文和/或推理：截断时已经完整落地的工具调用，adapter 报成 `ToolUse` 照常派发(plan 133)。
plan 133 抓包时三种形状里就有"推理烧满",当时和"正文写到一半"一起送进了这条恢复。对"只有推理"这一种，
它的每个前提都不成立:

- **没有可以接的地方。** 可见输出是空的,`truncated_prefix` 拼进去的是 `""`。
- **推理带不过去，或者带不带得过去 kloop 说了不算。** 这条路由上已实测没带过去;有签名的 rail 会重放，但
  模型收到一条新的用户消息后是接着想还是重想，由模型决定。
- **于是续写 = 同一请求、同一 effort 再跑一次**,只多一句话。本例每次 32k 输出 + 26 万输入(两次都没命中
  缓存，原因没查),最坏 4 轮约 27 分钟，最后以一句"仍被截断"和空文本结束——既没产出，报错也在暗示"有过
  回答，只是被截了"。

附带发现:`ANTHROPIC_MAX_OUTPUT_TOKENS` 的注释说"thinking budget 加在上面，推理永远吃不掉回答的空间",
这只对 `ThinkingMode::Budget` 成立;`Adaptive`(当前模型)的 `max_tokens` 就是 8,192,推理与回答共用，同样的
形状在 Messages rail 上更容易撞到。

## 三、裁决(两条候选，先量再定，见第六节)

共同部分:

- 判据:`OutputLimit` 且本轮可见正文为空(`text_content(blocks).trim().is_empty()`)。
- 有正文的截断照旧：最多 3 次，前缀拼接。
- 恢复进行到一半才出现的"只有推理"(前一轮有正文、这一轮没有)同样按下面处理,`truncated_prefix` 作为
  `final_text` 保留。
- **不自动降 effort**:effort 是用户的设定;而且第一节已经量到，降了也不一定管用。

**A. 当场结束。** 不追任何提示，报一个自己的错误：推理用完了整个输出上限、没有产出任何回答;建议把请求拆小
(比如一次只处理一条)或换模型。不写"降 effort"——第一节量过，不可靠。

**B. 换一句"先动手"的提示，只追一次，还是只有推理就按 A 结束。** 原提示的前提("接着写")不成立，但失败的
形状很具体：模型想先在脑子里把整件事解决完。候选措辞(model-facing,英文，开工时量):

> Your reasoning used up the entire output budget before you produced any output, and that reasoning is not
> available to you now. Do not work everything out up front: make the first concrete tool call now (for
> example, read the code you need), and reason about the rest after you see its result.

B 只有在量出来确实能让模型在上限内调出第一个工具时才值得做;模型会不会因为一句用户消息缩短推理，事先不知道。

## 四、形状

- `classify_outcome` 的 `OutputLimit` 分支先判本轮正文是否为空;空 → A:`RoundStep::Stop`,不发
  "asking the model to continue" 的 Note,不记 `TRUNCATION_CONTINUE_MSG`;B:第一次记那句"先动手"提示并
  `Retry`(Note 文案相应改掉，不再说 continue),第二次按 A。B 的次数与原来的 3 次分开计。
- 错误要有自己的名字：现有的 `TurnError::ProviderOutcome(OutputLimit)` 的显示文案("after 3 continuation
  attempts")在这里是错的。推荐 `TurnError` 加一个变体(暂名 `OutputSpentOnReasoning`),显示文案带上
  限数(`cfg.provider_route.api_family().max_output_tokens()`;Budget 档要加上 budget,开工时核)和建议。
  开工先查 `turn_terminal` 怎么把 `TurnError` 落进 rollout、旧会话回读与 server 的 `turn/completed` 会不会
  受新变体影响。
- `rust/DESIGN.md`:Adapters 那段 "only output limits enter bounded continuation" 补上"本轮没有可见正文的
  除外，当场结束";先读那段现在的说法再改，不追加。
- `ANTHROPIC_MAX_OUTPUT_TOKENS` 的注释改成实话(Budget 档加在上面，Adaptive 档共用)。数字本身不在这条里动。

| 测试 | 锁住什么 |
|---|---|
| `agent::tests::reasoning_only_truncation_ends_without_continuation` | (A)只有 thinking 的 `OutputLimit(MaxOutputTokens)`:新错误、`rounds == 1`、历史里没有 continue 提示、脚本里备好的第 2 轮**没被请求**、`final_text` 为空。(B)换成：只追一次"先动手"提示，第二次只有推理就结束,`rounds == 2` |
| `agent::tests::reasoning_only_truncation_mid_recovery_keeps_the_prefix` | 第 1 轮有正文被截 → 续写;第 2 轮只有推理 → 结束,`final_text` 是第 1 轮的正文，提示只有 1 条 |
| 现有 `truncated_response_recovers_with_continuation` / `truncation_recovery_is_bounded` / `model_context_output_limit_keeps_its_typed_kind` | 不改，有正文的截断行为不变 |
| 新错误的显示文案 | 整句断言 |

反证：去掉那一处判空，前两条新测试红。

## 五、不做

- **自动降 effort 重试**:见第三节。
- **调高 `OPENAI_MAX_OUTPUT_TOKENS`**:32,768 已是网关默认值;三次推理的末尾都还在发散("should I fix that
  too?"),看不出再给一倍就会收住，而一轮 7 分钟会变成 14 分钟。
- **把上一轮推理拼进 continue 提示里让它接着想**:12 万字符的输入，而且推理本来就不该当普通文本发回去。
- **`OutputLimitKind::ModelContextWindow` 的正确处理**(窗口满了,"接着写"同样没意义，该走压缩):另议。
- 两轮缓存都没命中的原因：另查，不在这条里。

## 六、开工时问(一次一个)

1. A 还是 B——推荐先量 B:照教训 199 的办法(临时 HOME、`--fork` 这个会话),在只有推理的那几轮之后分别发
   原 continue 提示与"先动手"提示，各跑几次，看在 32k 内调出第一个工具的比例。现场固定、缓存命中时一次约
   26 万缓存输入 + 最多 32k 输出。B 明显有效就做 B,否则做 A。
2. 规则是否不分 rail——推荐不分:"接着写"的前提是有可见输出，这在哪条 rail 上都不成立;推理带不带得回去
   取决于网关和模型，kloop 看不到。
3. 新错误的文案：带上限数，建议拆小请求或换模型，不提 effort——推荐这样。
