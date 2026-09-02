# Plan 108 — `run_turn` 收成单一出口

## 背景

Plan 106 片 22 把循环内 12 处 `final_text: String::new()` 换成了
`produced_text.clone()`,但那只是**约定**:Rust 拦不住下一个人再写一次 `String::new()`。
当时判断"类型级保证要重构最核心函数,风险与收益不成比例"而没做;用户要求单开一片做掉,
并指定用片 22 的三条枚举测试当验收。

## 做法

`run_turn` 原本有 22 个 `return TurnOutcome { ... }`。改成:

- 新增 `struct Ending { reason, text: Option<String>, rounds, structured }`,循环内的出口
  只能 `break 'turn Ending { ... }`,**不能自己构造 `TurnOutcome`**。
- 循环写成 `let ending = 'turn: loop { ... };`,函数末尾**唯一一处**组装 `TurnOutcome`,
  在那里 `final_text: ending.text.unwrap_or(produced_text)`。
- 循环**之前**的 4 处保持直接 return:那时确实什么都没产出,空串是正确答案。

关键是 `text` 的语义:**`None` = 交回这一轮产出的全部**,是出口"什么都不说"时拿到的默认
值;只有确实有更精确答案的地方(完成时的最终答案、截断续跑拼好的交付物、取消时的部分
文本)才写 `Some`。想给空串**仍然可以**,但必须显式写 `Some(String::new())`——那是一句
可见的主张,不再是疏忽。

用带标签的 `break 'turn` 而非裸 `break`:循环内有 5 个嵌套 `for`,裸 `break` 会跳错层。

## 过程中的两个坑

**(a) 机械转换漏了字段简写。** 转换器用 `partition(":")` 拆字段,遇到 `rounds,` 和
`final_text,` 这种简写(没有冒号)就产出了 `rounds: ,` 和 `text: Some(),`,5 处语法错误。
编译器当场报出来,逐个改回 `rounds,` 与 `Some(final_text)`。**批量改写结构体字面量时,
简写字段是最容易漏的一类。**

**(b) `MaxRounds` 那处不再需要自带文本。** 它原本是片 11 唯一读累积器的地方
(`Some(produced_text)`),收成单一出口后 `None` 就是它想要的,改掉后语义更清楚:那个
出口并不比别人特殊。

## 验证 ✅

- 片 22 的三条枚举测试(终态失败 / 溢出无压缩 / 反应式压缩失败)+
  `max_rounds_returns_the_text_produced_before_the_cap`,四条全绿。
- 机械检查:循环内 `final_text: String::new()` 归零,剩余 4 处全部位于循环之前。
- `FMT_EXIT=0 CLIPPY_EXIT=0 TEST_EXIT=0`,33 个测试目标 0 失败。

## 非目标

- 不改 `TurnOutcome` 的公开形状,调用方不受影响。
- 不动循环前那 4 个出口。
- 不追求"编译期禁止空串":`Some(String::new())` 仍然写得出来,只是变成显式主张。真要禁,
  得让 `text` 变成一个不能承载空串的类型,收益不值那个复杂度。


## 片 2 ✅ — 子 agent 的轮次上限固定在代码里,不再由模型决定

Plan 106 片 12 曾把 `max_rounds` 从 `run_agent` schema 拿掉,实测更糟(子 agent 跑了 158
轮,请求数 70 → 342)于是回退。当时的错在于**只做了一半**:拿掉了模型的旋钮,却把上限设
成了无限。参考 cc(`forkSubagent.ts:65` 固定 `maxTurns: 200`,而 `AgentTool` 的 schema
只有 description / prompt / subagent_type / model / run_in_background,**没有轮次参数**),
正确形状是两半都要:

- **不给模型**:schema 与 `RunAgentInput` 都不再有 `max_rounds`;`deny_unknown_fields`
  让"以为自己设上了"的调用方当场收到错误,而不是被静默忽略。
- **代码里给一个大而有限的值**:`SUBAGENT_MAX_ROUNDS = 200`。大到真实工作够不着(现有的
  16 轮回归测试仍然通过),小到能拦住失控。

之所以现在敢强加这个闸,是因为片 22 与本 plan 片 1 已经让撞上限**带着结论返回**——上限
从"作废全部工作"降级为"少挖一层"。两条测试分别验这两半:
`run_agent_does_not_take_a_round_limit_from_the_model`(模型传了会报错)与
`a_capped_subagent_still_returns_its_findings`(上限确实生效,且 `final_text` 是
`"finding 0\n\nfinding 1"` 而不是空)。

三个数字放在一起就是这条决策的全部依据:模型自选 **12/10**(不够用,当时还全丢);无限
**158**(请求数三倍);固定 **200**(够用且有界)。
