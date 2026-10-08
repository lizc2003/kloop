# Plan 218 — 一个 delta 都不发的中继,以 `.done` 为权威

> 状态：✅ 已完成（提交 SHA 以本条所在提交为准）
>
> 依赖：Plan 96（函数参数按 JSON 值比对，容忍 pretty/compact）、Plan 165（模型写坏的 JSON 是模型的错，不是坏掉的线）。
> 参考：`refs/codex/codex-rs`（取 `output_item.done` 的完整 item 为权威，不做 delta 交叉校验）。

## 起因（真实复现，不是推理）

用户给的 prompt `审查：a49458f7^..51798d0e` 在 `benli-gpt` / `gpt-6.1-sol`（Responses 轨）上**稳定复现**（3/3）：

```
provider protocol error: openai-responses arguments done did not match accumulated delta
```

临时给这条错误带上两侧的有界取值之后，读出来的是：

- `deltas_seen=false`、累积 `deltas(len=0)=""`、`done(len=159..278)="{\"arguments\": …, \"name\": \"code-review\"}"`

即**这个 item 上一个 `function_call_arguments.delta` 都没到**，整个 arguments 只在 `.done` 里给一次。同一轮里前一个调用（skill）拿到了 delta、后一个（bash）没拿到——**逐调用间歇**，不是某个工具或某类参数的属性。

把"无 delta 时采纳 done"临时放开、并在"delta 迟到"的分支打点，再跑三轮：`[diag] done with no deltas seen` 每轮命中 3–4 次，`LATE delta` **一次都没有**，三轮全部 `exit=0` 跑完（skill、read_file、bash、glob、最终报告）。所以不是帧重排，是这条路由真的不发 delta。

## 根因

`responses.rs::on_arguments_done` 把累积 delta 与 `.done` 的 arguments 比对，不通过就 `protocol`。Plan 96 已把字节相等放宽成 JSON 值相等，但"一侧空、一侧非空"仍 fail-closed——Plan 96 当时明写这是未观测场景、保持保守，"如日后真出现，另开计划专门处理"。

而 `.done` 的 arguments 就是最终工具输入（`finish_function_call` 用它经 `parse_tool_input` 造 `tool_use`），delta 只喂流式显示：这里没有任何东西需要 fail-closed 去保护。

## 改动（窄口）

只动 `responses.rs::on_arguments_done` 一处分支：`deltas_seen == false` 时不再比对，直接把 `arguments` 置成 `.done` 的值；`deltas_seen == true` 时交叉校验原样保留。

`arguments_started` 的赋值、`finish_function_call` 的"两项都闭合"检查、`.done` 与 `output_item.done` 之间那次比对，一律不动。

## 边界语义

- **有 delta、值不同**：仍 fail-closed（既有 `function_arguments_differing_values_still_fail_closed` 锁住）。
- **有 delta、两侧都非法 JSON 但字节相同**：走原路（agree → `parse_tool_input` 失败 → Plan 165 的 invalid call），行为不变。
- **无 delta、`.done` 本身不是合法 JSON**：采纳后由 `tool_use_block` + Plan 165 降级成一次失败的工具调用，不是坏掉的线。
- **delta 在 `.done` 之后到达**：仍报 `arguments delta arrived after arguments done`。三轮实测一次没出现，按 Plan 96 的纪律保持保守不动。

## 关键文件

- `rust/crates/provider/src/responses.rs` — `on_arguments_done` 一个分支。
- `rust/crates/provider/tests/responses.rs` — 新增 `function_arguments_without_any_delta_take_done_as_the_value`（无 delta 的整轮 fixture：`output_item.added` → `function_call_arguments.done` → `output_item.done` → `completed`，断言 `ToolUse{ input }` + `Terminal(ToolUse)`）。
- `rust/DESIGN.md` — Provider seam 那一句改写（原文只写了 pretty/compact 一种放宽）。
- `docs/plan/HANDOFF.md` — 教训 215。

## 验证

- `make check`（fmt + clippy + test + parity）全绿。
- 真实回归：`benli-gpt` / `gpt-6.1-sol`，prompt `审查：a49458f7^..51798d0e` × 3 轮全部 `exit=0`；修复前同一 prompt 3/3 死在第一个工具调用上。