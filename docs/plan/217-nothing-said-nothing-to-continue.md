# Plan 217 — 推理要有地方放;一个字没说，就没有"接着写"

> 来源:2026-09-30。用户贴来 TUI:`∗ Thought for 6m37s` → `[response truncated by output limit; asking the model
> to continue (1/3)]` → `∗ Thought for 6m59s` → `(2/3)` → 又开始想。核对会话后用户:「开一个 plan」。起草时
> 用户 Esc、`/effort high`、发"继续",又贴来 `∗ Thought for 6m57s` → `(1/3)`。问「是不是要提高max_tokens啊」,
> 同意先量 64k;量完同意按结果改写本 plan(第一版提交里"不调高上限"那一条被实测推翻)。

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
- **两轮输入只差 28 token**,正好是那条 continue 提示的长度——上一轮 12 万字符的推理没有进到模型眼前。
  这条路由整个会话从没返回过 `encrypted_content`;kloop 把无签名推理当 `summary_text` 重放
  (`provider/src/responses.rs` 的 `to_input_items`),网关没把它计进输入。
- **每次都从头想。** 续写那轮开头是 "Let me start by verifying the three claims. I need to re-read … and run
  experiments";"继续"那轮开头是 "The user gave three new findings. Let me verify each"。提示里 "break the
  remaining work into smaller pieces" 没起作用。
- **降到 high 没用。** kloop 的目录里这个模型不声明 effort 档位(`/effort` 显示 "declares none"),字段照发，
  网关认不认分档看不到;实测 high 与 xhigh 一样吃满。

## 二、实测：同一条输入，上限 64k

方法(教训 199 的做法加两道闸):临时把 `OPENAI_MAX_OUTPUT_TOKENS` 改成 65,536 编一个二进制(源码当场还原，不提交;
先用本地假服务器确认请求体里确实是 65536);临时 HOME,复制配置与这个项目目录;`--fork 20260930-032304#518`
接原审查原文，与出事那一轮同一 provider / 模型 / effort。**为了不碰真实仓库**:配置里加一条不带 matcher 的
`pre_tool` hook,内联 `/bin/sh -c '…; exit 2'` 拦下一切工具(hook 起不来是 fail open,所以不依赖外部脚本;先用
假服务器返回一个 `touch` 调用，确认结果是 "blocked by hook"、文件没建),再加 `--max-rounds 1` 只采样一轮。
三次并行(方法与判据另记教训 210):

| 次 | 输入(缓存命中) | 输出 | 推理 | 用时 | 结果 |
|---|---|---|---|---|---|
| 1 | 260,523(2,048) | 18,322 | 70,620 字符 | 约 2.5 分钟 | 正文 + 2 次读代码的 bash |
| 2 | 260,523(49,152) | **34,455** | 129,974 字符 | 约 4.5 分钟 | 正文 + 1 次写复现程序的 bash |
| 3 | 260,461(0) | 20,399 | 75,026 字符 | 约 4.8 分钟 | 正文 + 1 次写复现程序的 bash |

- **是运气，不是必然。** 同一条输入的首轮一共四个样本：出事那一轮(≥32,768)、18k、34k、20k——一半超过 32k。
- **超过的那部分并不远。** 第 2 次在 34,455 收住，三次推理末尾都在收口("Let me write it" / "Let me read the
  relevant domain code")。第一版里"末尾还在发散"的判断，看的是被截断在半路的推理，不能拿来推断它停不停得下来。
- **网关接受 65,536**:第 2 次实际返回了 34,455。
- **提高上限几乎没有代价**:按实际输出计费，用不到的不花钱;预测式压缩只按 `min(上限, OUTPUT_GROWTH_CAP = 20k)`
  预留(`compact.rs` 的 `max_turn_growth`),上限多高都一样。唯一的代价是真停不下来的推理要多等一倍才被截断。

## 三、现状

**上限。** `protocol/src/lib.rs`:`OPENAI_MAX_OUTPUT_TOKENS = 32_768`(Responses 与 Chat 共用，注释说"等于网关
默认值");`ANTHROPIC_MAX_OUTPUT_TOKENS = 8_192`,注释说"thinking budget 加在上面，推理永远吃不掉回答的空间"——
这只对 `ThinkingMode::Budget`(配置里的 `thinking_budget`,如 haiku-4-5)成立;走 `efforts` 的模型
(`ThinkingMode::Adaptive`,如 opus-4-8、sonnet-4-6)`max_tokens` 就是 8,192,推理与回答共用，比 Responses 这边
更容易吃满。两个都是按 rail 写死的常量，不能按模型配。

**只有推理的截断。** `core/src/agent.rs` 的 `classify_outcome`,`AssistantOutcome::OutputLimit(_)` 分支：没到
`TRUNCATION_RECOVERY_LIMIT`(3)就把本轮正文拼进 `truncated_prefix`、发 Note、记 `TRUNCATION_CONTINUE_MSG`
("Continue exactly where you left off; …")、`Retry`;到了上限以 `TurnError::ProviderOutcome(OutputLimit)` 结束，
显示 "response remained truncated after 3 continuation attempts"。进这个分支的只有正文和/或推理(截断时已完整
落地的工具调用报成 `ToolUse` 照常派发，plan 133)。对"只有推理"这一种，它的前提都不成立：没有可见输出可接;
推理带不过去(本例实测)或带不带得过去由网关和模型决定;于是续写就是同一请求再抽一次签，只多一句话，最坏 4 轮
之后以空文本和一句"仍被截断"结束。

## 四、裁决

1. **主修复：提高上限。**
   - `OPENAI_MAX_OUTPUT_TOKENS` 32,768 → 65,536。开工先对配置里每个 Responses / Chat 模型发一个很小的请求，确认
     都接受(`deepseek-v4.1-flash` 已确认)。都接受就改常量;有拒的见第七节第 1 问。
   - `ANTHROPIC_MAX_OUTPUT_TOKENS`:Adaptive 档的 8,192 提高，数值按 opus-4-8 / sonnet-4-6 实测接受的定(第七节
     第 2 问);Budget 档"回答空间 + budget"的公式不变，但确认加起来的值模型接受。
   - 两个常量的注释改成实话。
2. **兜底：只有推理的截断不再追 "continue"。** 判据:`OutputLimit` 且本轮可见正文为空
   (`text_content(blocks).trim().is_empty()`)。上限提高后它只兜真停不下来的推理。两条候选(第七节第 3 问):
   - **A. 当场结束**,报一个自己的错误：推理用完了整个输出上限(带数值)、没有产出;建议拆小请求或换模型。
     不写"降 effort"——第一节量过，不可靠。
   - **B. 换一句"先动手"的提示只追一次**,还是只有推理就按 A:
     > Your reasoning used up the entire output budget before you produced any output, and that reasoning is not
     > available to you now. Do not work everything out up front: make the first concrete tool call now (for
     > example, read the code you need), and reason about the rest after you see its result.
   - 有正文的截断照旧(最多 3 次，前缀拼接);恢复到一半才出现的"只有推理"同样按兜底处理,`truncated_prefix`
     作为 `final_text` 保留。
3. **不自动降 effort**:effort 是用户的设定，而且量过降了不一定管用。

## 五、形状

- 常量与注释(第四节 1)。请求体测试若直接写了数值，跟着改;用常量名的不动。
- `classify_outcome` 的 `OutputLimit` 分支先判正文是否为空;A:`RoundStep::Stop`,不发 continue 的 Note、不记
  `TRUNCATION_CONTINUE_MSG`;B:第一次记"先动手"提示并 `Retry`(Note 文案改掉),第二次按 A,次数与原来的 3 次
  分开计。
- 错误要有自己的名字：`TurnError` 加一个变体(暂名 `OutputSpentOnReasoning`),显示文案带上限数
  (`cfg.provider_route.api_family().max_output_tokens()`;Budget 档加上 budget,开工时核)。开工先查
  `turn_terminal` 怎么把 `TurnError` 落进 rollout,旧会话回读与 server 的 `turn/completed` 会不会受新变体影响。
- `rust/DESIGN.md`:Adapters 那段 "only output limits enter bounded continuation" 补上"本轮没有可见正文的除外";
  grep 一遍文中写到 32,768 / 8,192 的地方。先读现在的说法再改，不追加。

| 测试 | 锁住什么 |
|---|---|
| `agent::tests::reasoning_only_truncation_ends_without_continuation` | (A)只有 thinking 的 `OutputLimit(MaxOutputTokens)`:新错误、`rounds == 1`、历史里没有 continue 提示、脚本里备好的第 2 轮**没被请求**、`final_text` 为空。(B)只追一次"先动手"提示，第二次只有推理就结束,`rounds == 2` |
| `agent::tests::reasoning_only_truncation_mid_recovery_keeps_the_prefix` | 第 1 轮有正文被截 → 续写;第 2 轮只有推理 → 结束,`final_text` 是第 1 轮的正文，提示只有 1 条 |
| 现有 `truncated_response_recovers_with_continuation` / `truncation_recovery_is_bounded` / `model_context_output_limit_keeps_its_typed_kind` | 不改，有正文的截断行为不变 |
| Anthropic 请求体(Adaptive 与 Budget 各一) | `max_tokens` 整对象断言:Adaptive 是新值，Budget 是回答空间 + budget |
| 新错误的显示文案 | 整句断言 |

反证：去掉那一处判空，前两条新测试红。

**真实验证**:改完用第二节同样的办法(fork #518、拦工具的 hook、`--max-rounds 1`)再跑三次，这回用正式构建。

## 六、不做

- **自动降 effort 重试**:见第四节 3。
- **把上一轮推理拼进 continue 提示里让它接着想**:12 万字符的输入，而且推理本来就不该当普通文本发回去。
- **按模型配置上限**:只在第七节第 1 问有模型拒 65,536 时才考虑。
- **`OutputLimitKind::ModelContextWindow` 的正确处理**(窗口满了,"接着写"同样没意义，该走压缩):另议。
- 两轮缓存都没命中的原因：另查。

## 七、开工时问(一次一个)

1. 若有 Responses / Chat 模型不接受 65,536:按模型可配(`[models."x"]` 下加一个键)还是取所有模型都接受的最大值
   ——到时看拒的是哪个再定。
2. Messages 的 Adaptive 上限取多少——推荐量 opus-4-8 / sonnet-4-6 接受的值，若都接受就与 Responses 一样取 65,536。
3. 兜底 A 还是 B——推荐 A:上限提高后，能走到这里的都是 64k 还没想完的推理，再追一句提示多半又是十几分钟;
   B 要量得先把上限临时调低才能复现，量的成本比它省下的高。
4. 兜底规则是否不分 rail——推荐不分:"接着写"的前提是有可见输出，这在哪条 rail 上都不成立。
5. 新错误的文案：带上限数，建议拆小请求或换模型，不提 effort——推荐这样。

## 八、✅ 完成

2026-09-30 当次会话做完，一次提交(SHA 以本条所在提交为准)。`make check` 全绿(1823 个测试,parity 语料校验通过)。

### 开工五问

1. **不用问。** 配置里能访问的 Responses 模型全都收 65,536:sky-bj 三个模型(`deepseek-v4.1-flash`、
   `deepseek-v4-flash-0731`、`glm-5.3-flash`)在 65,536 上 200,在 10,000,000 上 400——网关确实校验，所以这个
   200 有意义;`gpt-5.6-sol` 连 10,000,000 都是 200(看不出校验);`gpt-5.6-terra`/`luna` 用这把 key 一律 403(无权访问),
   与上限无关。配置里没有 Chat provider。
2. **plan 的推荐被实测推翻，用户另定了形状。** opus-4-8、sonnet-4-6 在 65,536 上各 5/5 通过;**haiku-4-5 的硬上限
   是 64,000**(64,001 就 400,有没有 thinking 都一样)。而这个常量不只管 Adaptive:Unset/Off(比如 haiku 选
   `none`)用的也是它,Budget 档是它加 budget——照 plan 取 65,536,haiku 每次都 400。用户:「可以在 provider
   配置里加个配置，开一个自定义的口子吗」,商量后「两层都做」:
   - `[providers.x] max_output_tokens`(网关给多少)与 `[models."x"] max_output_tokens`(模型收多少),两层都写
     取较小的，只写一层用那一层，都不写用 rail 默认值;**声明可以往上抬**(默认值是猜的，不是量的)——与
     `context_window` 唯一的不同。
   - rail 默认值:Messages **64,000**(三个模型都收),Responses/Chat **65,536**。
3. **A**:当场结束。
4. **不分 rail。**
5. 文案照推荐，数字取这一轮实际用的上限(用户:「不能写死」):
   `the model spent the entire {N}-token output limit on reasoning and produced no answer; split the request into
   smaller steps or switch models`。

### 与第五节的出入

- **上限由 route 解析，按 attempt 走。** `ProviderApiFamily::max_output_tokens` 改名
  `default_max_output_tokens`,只当兜底;`OutputCapRouting`(与 `ThinkingRouting` 并列挂在 `ResolvedRoute` 上，
  子 agent 换模型也不用回 catalog)按(模型, thinking)算出一个数，存进 `FrozenProviderAttempt::max_output_tokens`。
  请求体、压缩预留、新错误三处读的是**同一个数**。Budget 档的公式也挪进 route:`min(8,192 + budget, 上限)`
  (`ANTHROPIC_BUDGET_ANSWER_TOKENS`),provider 只照写，不再自己加。`stream_attempt` 多一个 `max_output_tokens`
  参数(加了 `too_many_arguments` 的 allow,与仓库里其它几处同样处理)。
- **Mock 钉在 8,192**,不跟 Messages 走:脚本化测试的增长预测一个不动。
- **Messages 的压缩预留从 23,192 变成 35,000。** 第二节"上限多高都一样"只对 OpenAI 两条 rail 成立
  (32k 与 64k 都大于 20k);Messages 从 8,192 抬到 64,000,`min(上限, 20k)` 从 8,192 变成 20,000。开工时向用户说明过。
- **兜底只认 `OutputLimitKind::MaxOutputTokens`。** `ModelContextWindow` 且没有正文照旧续写(第六节"另议"):
  那里说"推理用完了输出上限"是假话。
- **启动时多一道检查**(`ProviderCatalog::check_thinking_budgets`):thinking budget 必须小于它在每个 Messages
  provider 上拿到的上限，否则请求被 API 拒。原来 `parse_thinking_budget` 注释里"上界自己成立"在上限可配之后不再成立。
- `config/config-demo.toml` 的 `[models]` 说明补了这个键,haiku 写上 `max_output_tokens = 64000`——provider 往上抬
  时它不会被带着越过硬上限。

| 测试 | 锁住什么 |
|---|---|
| `agent::tests::reasoning_only_truncation_ends_without_continuation` | 只有 thinking 的 `OutputLimit(MaxOutputTokens)`:错误带上限(8,192)、`rounds == 1`、`final_text` 为空、脚本第 2 轮没被请求(请求记录只有一条，且上限就是 attempt 的数)、历史里没有 continue 提示、没有发 Note |
| `agent::tests::reasoning_only_truncation_mid_recovery_keeps_the_prefix` | 第 1 轮有正文被截 → 续写一次;第 2 轮只有推理 → 结束,`final_text` 是第 1 轮正文,`rounds == 2`,提示只有 1 条 |
| `provider_route::tests::attempts_ask_for_the_declared_output_cap` | rail 默认(64,000 / 65,536)、provider 抬到 128,000、模型声明 16,000、两层取小、Budget 为 8,192 + budget、Budget 再被声明的上限封顶、Budget 模型不选 effort 时用上限、子 agent 换模型拿到那个模型的数 |
| `provider_route::tests::a_thinking_budget_must_stay_below_every_cap_it_is_sent_under` | budget 等于某个 Messages provider 的上限 → 整句报错;OpenAI rail 的上限不参与 |
| `rollout::tests::output_spent_on_reasoning_round_trips_with_its_cap` | 新变体落盘的 JSON 整对象、读回相等、显示文案整句 |
| `provider_config::tests::the_output_cap_comes_from_the_profile_and_the_model` | 两层解析与取小、都不写用默认值、0 被拒 |
| `provider_config::tests::a_thinking_budget_over_the_output_cap_fails_at_startup` | 配置层的启动报错整句 |
| 改动的现有测试 | 请求体整对象里的 8,192 / 32,768 → 64,000 / 65,536;provider 层的 thinking 测试改成"照写传进来的数";`growth_follows_the_rail_cap` 改为 `growth_from_the_default_caps`(三条 rail 都到 20k 封顶,Mock 仍 8,192) |

反证：把判空改成 `false &&`,两条新的 agent 测试红，原有三条截断测试照旧绿。

## 九、真实验证(同日，正式构建)

第二节的办法，这回不改源码，用 `make check` 编出的二进制(默认上限就是 65,536):临时 HOME 复制配置、项目目录与
skills,配置加一条不带 matcher 的 `pre_tool` hook(内联 `/bin/sh -c '…; exit 2'`),`--fork 20260930-032304#518`
接原审查原文,`--max-rounds 1`。上真实网关前先用本地假服务器验过：请求体里 `max_output_tokens` 是 65536,返回的
`touch` 调用结果是 "blocked by hook",文件没建。

| 次 | 输入(缓存命中) | 输出 | 推理 | 用时 | 结果 |
|---|---|---|---|---|---|
| 1 | 260,434(0) | **36,543** | 132,185 字符 | 约 7 分钟 | 正文 + 2 次 bash(都被拦) |
| 2 | 260,496(6,144) | 29,716 | 112,466 字符 | 约 3.5 分钟 | 正文 + 2 次 bash(都被拦) |
| 3 | 260,434(260,352) | 32,231 | 119,634 字符 | 约 6 分钟 | 正文 + 2 次 bash(都被拦) |

- 三次都收住、写出正文、调了工具，结束状态都是 `max_rounds`(`--max-rounds 1` 的预期)。
- **第 1 次在旧上限下会重演事故**:36,543 > 32,768,会被砍成只有推理，然后进续写。第 3 次离旧上限只差 537。
  连同第二节，同一条输入的首轮现在有七个样本:≥32,768(事故)、18k、34k、20k、36.5k、29.7k、32.2k——三个超过
  32k,最大 36.5k,离 65,536 还远。
- 重放后原会话所在的仓库 `git status` 干净、HEAD 未变，模型要建的 `/tmp` 目录一个都没建;复制出来的配置(带 key)
  已删。
- 并行起三个 `--fork` 时有两个在同一秒撞了会话 id(`File exists`),错开 3 秒补跑。见 HANDOFF 217 条的"顺带发现"。
