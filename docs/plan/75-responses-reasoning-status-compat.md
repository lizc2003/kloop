# Plan 75 — Responses 三原语执行兼容

> 状态：✅ 已完成（2026-08-11；plan75 提交，SHA 以本文件所在提交为准）
>
> 基线：`ad2f1a2`（Plan 74 设计提交）
>
> 依赖：Plan 64、Plan 65、Plan 68、Plan 69

## Context

真实 OpenAI Responses 复测显示，当前后端在 `response.output_item.added` 与 `response.output_item.done` 的 `reasoning` item 上均省略 `status`，但同一响应中的 `function_call` 仍分别携带 `in_progress` / `completed`。Plan 65 的状态机要求 item identity、part closure 和 final accumulator 完整闭合，并未把 reasoning item 的 `status` 规定为必填；当前 fixture 全部人工携带该字段，使实现把代理的合法 shape 误判为 non-retryable protocol error。

修复 parser 后，Responses 三原语 evaluator 又暴露了第二个独立契约缺口：模型会为 optional string 参数生成空字符串；`run_agent.description/agent_type`、`run_program.description/resume_from_run_id` 和 Workflow 的 resume-only identity schema 没有表达生产 parser 的非空约束。模型输出因此通过 provider schema，却在副作用前被 runtime 正确拒绝。这里应收紧模型可见 schema，而不是把显式空 identity 偷偷归一为“省略”。

上述错误都发生在目标原语完成前，因此 Agent、Program 和 Workflow 的 Responses acceptance 不能只靠 provider smoke 销账。现有三原语真实 evaluator 又只接受 Anthropic/OpenAI Chat，且隔离子进程没有转发 `KLOOP_EFFORT`，不能权威复现 reasoning shape。

## 契约

- `reasoning` 的 `response.output_item.added` 可省略 `status`；若字段存在，必须是字符串 `in_progress`。
- `reasoning` 的 `response.output_item.done` 可省略 `status`；若字段存在，必须是字符串 `completed`。
- `message` 与 `function_call` 的 added/final `status` 继续必填并严格校验；不放宽 response terminal status、item identity、part closure、final accumulator 或 tool identity。
- 对 reasoning `status` 而言，显式 `null`、非字符串和错误状态都不是“省略”，继续按 protocol error fail closed。
- optional control 若 runtime 只接受非空 identity/metadata，模型可见 schema 同样声明 `minLength` 或稳定 ID `pattern`；需要表达“省略”的 rail 可发送 schema 明示的 `null`，但显式空字符串仍不归一为默认值。
- `run_agent.agent_type` 只在当前 Config 确有自定义 Agent type 时出现，并使用 `null | 配置名称 enum`；无配置时删除该 property，不能诱导模型伪造 `general-purpose` alias。`max_rounds`、Program/Workflow resume identity 同样用 nullable option 区分省略与伪造占位值。
- 三原语真实 evaluator 接受显式 `KLOOP_PROVIDER=openai-responses`，并把 `KLOOP_EFFORT` 转发到 temp-HOME app-server；不自动从 key 推断 Responses rail，也不复制用户全局 config。
- evaluator 的 exactly-once Agent case 显式要求省略具有默认语义的 optional 字段；生产 parser 对显式未知/空 identity 继续严格拒绝。

## 实施

1. 在 `kloop/crates/provider/tests/responses.rs` 先加入真实 reasoning 缺 status 的成功 fixture，并加入 message/function_call 缺 status与 reasoning 错误 status 的负向矩阵。
2. 在 `kloop/crates/provider/src/responses.rs` 按 item type 区分 status policy；仅 reasoning 使用“缺失可接受、存在则严格”的校验。
3. 对齐 `run_agent`、`run_program`、`workflow` 的 optional control schema 与既有 strict parser：可保留的 display/path 明示非空，durable Program ID 明示 `run-*` pattern，可省略的 identity/round cap 使用 nullable schema；运行时按当前 Config 删除无可用类型的 `run_agent.agent_type`，或以 `null | 配置名称` enum 约束；不再向模型广告未实现的 Workflow `name`，补 exact schema tests，不把空字符串归一为省略。
4. 在 `kloop/crates/cli/tests/real_agent_program_workflow.rs` 放开三原语 evaluator 的 Responses rail gate、更新 ignore 说明并转发 `KLOOP_EFFORT`；mailbox evaluator 不在本计划扩 rail。
5. README 记录 Responses reasoning item 的 provider 兼容边界、strict optional schema 与真实 evaluator 入口；HANDOFF 记录根因、修复和新教训。

## 验证

```bash
cd kloop
cargo test -p kloop-provider --test responses -- --nocapture
cargo test -p kloop provider_config::tests -- --nocapture
cargo test -p kloop-core tools::subagent::tests -- --nocapture
cargo test -p kloop-core tools::codemode::tests -- --nocapture
cargo test -p kloop-core tools::workflow::tests -- --nocapture
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
git diff --check
```

使用用户私有 global Responses profile 在进程内转换为 evaluator 所需的 compatibility env，真实运行：

```bash
KLOOP_PROVIDER=openai-responses \
OPENAI_API_KEY=<private> OPENAI_BASE_URL=<private> OPENAI_MODEL=<model> \
KLOOP_EFFORT=<effort> \
cargo test -p kloop --test real_agent_program_workflow \
  real_agent_program_workflow_contract -- --exact --ignored --nocapture
```

不得输出或提交 key、Authorization、private endpoint、raw authenticated response 或完整 transcript。

## 完成记录（2026-08-11）

- 真实 capture 固定了 reasoning added/done 均无 status、同轮 function_call 仍有 in_progress/completed 的 wire shape；adapter 只对 reasoning 接受字段缺失，显式 null/错误值与 message/function_call 缺失仍由负向 fixture 证明 fail closed。
- Responses evaluator 继续暴露 optional key 物化：空 description/agent_type、随后伪 `general-purpose` 与 `run-placeholder` 都曾被既有 strict runtime 正确拒绝。最终模型 schema 以 non-empty/pattern + nullable omission 对齐 parser；无自定义类型时不广告 agent_type，有类型时只广告 null 或精确配置名称；未实现的 Workflow name 不再广告。
- 三原语 ignored native evaluator 已用用户私有 global Responses profile 转成进程内 compatibility env 实跑 `gpt-5.6-sol`：1 次 foreground Agent、2 次同源 Program（1 次 child spawn + journal resume）、1 次 background Agent、1 次 background Program、1 次 Workflow、3 个唯一后台 terminal 与 3 次 exactly-once automatic delivery 全部通过；未输出 key、endpoint、Authorization 或 raw response。
- Responses/provider-config/Agent/Program/Workflow focused tests、`cargo fmt --all --check`、workspace all-target Clippy `-D warnings`、`cargo test --workspace`（core 634 tests）、mock 与 `git diff --check` 全绿。focused Agent 并发计时测试曾在并行负载下一次超时，exact rerun及最终 focused/workspace 均通过。
- 提交：本次 `plan75` 收口提交（SHA 以本文件所在提交为准）；不 push。

## 完成标准

- 捕获到的 reasoning 缺 status shape 完整产出 Thinking/ToolUse/terminal，不再报 `missing or invalid output item status`。
- message/function_call 缺 status 以及 reasoning 显式非法 status 仍 fail closed。
- Responses 模型对 optional key 的物化只能产生 schema 明示的 `null`/合法值；空 identity、伪 `general-purpose` 与占位 run ID 不会越过 schema/runtime 边界。
- Agent、Program、Workflow 的 Responses native evaluator 通过，工具生命周期、journal/artifact、后台终态与 exactly-once delivery 断言不降级。
- focused tests、fmt、workspace all-target Clippy、workspace tests、mock 与 diff check 全绿。
- README、HANDOFF 和本计划完成记录同步；kloop main 一次 `plan75` 提交，不 push。
