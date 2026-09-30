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
