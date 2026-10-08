# Plan 218 — 中继只发 `.done` 不发 delta 时，以 `.done` 为权威

> 状态：✅ 已完成（提交 SHA 以本条所在提交为准）
>
> 依赖：Plan 96（函数参数按 JSON 值比对，容忍 pretty/compact）、Plan 165（模型写坏的 JSON 是模型的错，不是坏掉的线）。
> 参考：`refs/codex/codex-rs`（取 `output_item.done` 的完整 item 为权威，不做 delta 交叉校验）。

## 起因（真实复现 + 抓帧，不是推理）

用户给的 prompt `审查：a49458f7^..51798d0e` 在 `benli-gpt` / `gpt-6.1-sol`（Responses 轨）上稳定复现（3/3）：

```
provider protocol error: openai-responses arguments done did not match accumulated delta
```

先给这条错误临时带上两侧的有界取值，读出 `deltas_seen=false`、累积 `deltas(len=0)=""`、`.done` 里是完整对象；再把"累积为空时采纳 done"放开、在"delta 迟到"的分支打点跑三轮，`LATE delta` **一次没响**、三轮全部 `exit=0` —— 排除了帧重排。

最后在同一台机器上**抓了上游的原始帧**（临时 trace，只记帧名与 function-call 家族的原始 `data`；2 轮 / 12 个 function call）：

| 调用 | delta 帧数 | `.done` 形态 |
| --- | --- | --- |
| skill | 43 | compact |
| bash / read_file / glob | 0 / 0 / 0 | pretty |
| bash / grep / read_file | 0 / 0 / 0 | pretty |
| skill | 42 | compact |
| bash / read_file / bash / glob | 0 / 0 / 0 / 0 | pretty |

两条路，泾渭分明：

- **该响应是流式生成的**（模型先流式写了一段正文）→ 逐帧转发小 delta（`"{\""`、`"arguments"`、`"\":"`、`"审"`… 每帧几个字节，`sequence_number` 连号），`.done` 是 **compact**（原样字节）。
- **该响应是一次性产出的**（模型不写正文、直接叫工具）→ **一帧 delta 都不发**，成品整颗放在 `.done` 里，且是上游**重新序列化**过的形式（冒号后有空格，`{"limit": 250, "offset": 1, …}`）；整段只有 `output_item.added` / `function_call_arguments.done` / `output_item.done` 三种帧，`sequence_number` 连续无缺口。

所以**这不是"偶发吞帧"，而是"发不发 delta 取决于这个响应有没有被流式生成"**——而"直接叫工具"恰恰是最常见的形态：12 个调用里 **10 个没有 delta**。旧代码在这个形态上必死，不是边缘抖动（这也就是为什么用户连着两次审查都死在第一个工具调用上）。

## 根因

`responses.rs::on_arguments_done` 把累积 delta 与 `.done` 的 arguments 比对，不通过就 `protocol`。Plan 96 已把字节相等放宽成 JSON 值相等，但把"一侧空、一侧非空"明确留成 fail-closed 的保守边界，并注明"如日后真出现，另开计划专门处理"——本次踩的正是这一条，如今它被观测到了，边界相应改写。

而 `.done` 的 arguments 就是最终工具输入（`finish_function_call` 用它经 `parse_tool_input` 造 `tool_use`），累积的 delta **没有别的消费者**（读代码核实：除那两处比对，没有第二处读它）。

## 改动（窄口；判据落在"有没有可比的东西"）

`responses.rs::on_arguments_done`：**累积为空**时不再比对，直接把 `arguments` 置成 `.done` 的值；累积非空时交叉校验原样保留。

判据取"累积为空"而不是"一帧 delta 都没来"：发不发帧是上游的生成模式属性（见上表），不是关于这个 item 的信号；把断言绑在"帧到没到"上，一个长度为 0 的 delta 帧就能把同一个失败原样复现出来。

`arguments_started` / `arguments_done`、`finish_function_call` 的"两项都闭合"检查、`.done` 与 `output_item.done` 之间那次比对，一律不动。

## 边界语义

- **累积非空、值不同**：仍 fail-closed（既有 `function_arguments_differing_values_still_fail_closed` 锁住）。
- **累积非空、两侧都非法 JSON 但字节相同**：走原路（agree → `parse_tool_input` 失败 → Plan 165 的 invalid call）。
- **累积为空、`.done` 本身不是合法 JSON**：采纳后由 `tool_use_block` + Plan 165 降级成一次失败的工具调用，不是坏掉的线。
- **累积非空但 `.done` 为空**：仍 fail-closed（这是真的失配）。
- **delta 在 `.done` 之后到达**：仍报 `arguments delta arrived after arguments done`（抓帧两轮一次没出现，保持保守）。

## 关键文件

- `rust/crates/provider/src/responses.rs` — `on_arguments_done` 一个分支。
- `rust/crates/provider/tests/responses.rs` — `function_arguments_without_any_delta_take_done_as_the_value`（一帧 delta 都没有）、`function_arguments_with_an_empty_delta_take_done_as_the_value`（delta 帧在、但为空）。
- `rust/DESIGN.md` — Provider seam 那一句改写。
- `docs/plan/HANDOFF.md` — 教训 215。

## 验证

- `make check`（fmt + clippy + test + parity）全绿。
- 真实回归：`benli-gpt` / `gpt-6.1-sol`，prompt `审查：a49458f7^..51798d0e` × 3 轮全部 `exit=0`；修复前同一 prompt 3/3 死在第一个工具调用上。