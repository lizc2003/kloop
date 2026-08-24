# Plan 96 — Responses 函数参数比对改用 JSON 值相等(容忍 pretty/compact 空白差)

> 状态：✅ 已完成（提交号见本次提交；真实回归见文末「验证记录」)
>
> 依赖：Plan 89/92 的严格解析纪律、Plan 95(带外事件放行后才在真实轮走到本检查)。参考:`refs/codex/codex-rs`(`codex-api/src/sse/responses.rs:336` 直接取 `output_item.done` 的完整 arguments 为权威,不做 delta 交叉校验)。
>
> 开发期决策(已定):
> - **窄口**:只把函数参数的**字节相等**放宽为**解析后 JSON 值相等**;两侧都非合法 JSON 时回退字节相等。值真不一致、或一侧空一侧非空,仍 fail-closed。
> - 只动**函数参数**两处比对;文本(`output_text.done`)、reasoning(`reasoning_summary_text.done`)的逐字节校验**不放宽**(那些应逐字回传,无 pretty/compact 问题)。
> - 不改 anthropic/Chat 解析;不改 wire/protocol;不加依赖。

## Context(真实验证地面真相)

Plan 95 让真实 Responses 轮越过 `codex.rate_limits` 后,`gpt-5.6-sol` 经 gateway 的真实工具轮**间歇**报 `provider protocol error: openai-responses arguments done did not match accumulated delta`(~4 次真实轮命中 2 次)。临时给错误信息加上有界取值抓到:

- `.done` 全量 arguments:`{"limit": 3, "offset": 1, "path": "README.md"}`(46 字节,冒号/逗号后有空格,pretty)
- 累积 `.delta`:`{"limit":3,"offset":1,"path":"README.md"}`(41 字节,compact)

两者**解析成同一个 JSON 对象**,仅空白/格式不同。代理把增量 delta 发成 compact,却把 `.done`(及随后的 `output_item.done`)发成 pretty。kloop 的字节级相等断言据此 fail-close。诊断改动已还原,工作树干净。

codex-rs 不做这种交叉校验:它直接把 `output_item.done` 的完整 item(含 arguments)当权威(`sse/responses.rs:336`),`function_call_arguments.delta` 只用于流式显示。这与 Plan 95 同类——严格解析器的"逐字节一致"假设被真实代理的线格式违反,只是根因不同(空白 vs 带外事件)。

## 根因(单一,两处触发)

`kloop/crates/provider/src/responses.rs` 对函数参数做了两处**字节相等**校验,真实代理的 pretty/compact 差异会先后触发:

1. `responses.rs:1078`(`response.function_call_arguments.done`):`event["arguments"]`(pretty) != 累积 delta(compact)→ `arguments done did not match accumulated delta`。真实轮先命中这处。
2. `responses.rs:689`(`finish_item` 处理 `response.output_item.done`):累积 delta != `item["arguments"]`(pretty)→ `final function arguments did not match streamed arguments`。即便只修第 1 处,这处会以同样空白差紧接着 fail-close。**且最终 tool input 正是用 `final_arguments`(= `item["arguments"]`)经 `parse_tool_input` 得到**(`responses.rs:696`),pretty/compact 对解析结果无影响。

## 已拍板设计(窄口)

在 `responses.rs` 加一个小助手:

```rust
/// Two Responses argument encodings agree when they parse to the same JSON
/// value. The proxy may stream compact deltas but send a pretty-printed
/// `.done`/`output_item.done` for the same object, so byte-equality is too
/// strict. If either side is not valid JSON (e.g. an empty "" for a no-arg
/// call), fall back to byte-equality so genuine divergence still fails closed.
fn arguments_agree(a: &str, b: &str) -> bool {
    match (
        serde_json::from_str::<Value>(a),
        serde_json::from_str::<Value>(b),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}
```

两处比对改用它:

1. `responses.rs:1078`:`if !arguments_agree(final_args, arguments) { return Err(protocol("arguments done did not match accumulated delta")) }`(错误文案不变)。
2. `responses.rs:689`:`if !arguments_agree(&arguments, final_arguments) { return Err(protocol("final function arguments did not match streamed arguments")) }`(错误文案不变)。

其余逻辑一律不动:`arguments_started`/`arguments_done` 关闭校验、call_id/name 一致校验、`parse_tool_input(final_arguments)` 生成 tool input 均保持。

### 边界语义(为何回退字节相等是对的)

- **空参调用**:两侧都是 `""`(非合法 JSON)→ 回退字节相等 `"" == ""` → agree。正确。
- **一侧空一侧非空**(如 delta 全无、只有 done 带对象):`""` 解析失败 → 回退字节相等 → 不相等 → **仍 fail-closed**。这是未观测场景,窄口下保持保守;如日后真出现"只发 done 不发 delta",另开计划专门处理(优先对齐 codex 取 done 为权威)。
- **值真不同**(如 `{"a":1}` vs `{"a":2}`):两侧都解析成功但值不等 → **仍 fail-closed**。
- **两侧都非法 JSON 且字节相同**(如现有测试 `"{oops"`/`"{oops"`):回退字节相等 → agree → 随后 `parse_tool_input` 对 `"{oops"` 解析失败 → Protocol,行为不变。

## 关键文件

- `kloop/crates/provider/src/responses.rs` — 唯一实质改动:加 `arguments_agree`;`:1078` 与 `:689` 两处比对改调用它。
- `kloop/crates/provider/tests/responses.rs` — 新增契约测试(harness 已有 `sse_body`/`mount_sse`/`responses`/`collect`)。
- `kloop/README.md` — Provider seam 一句补充(若合适):Responses 函数参数按 JSON 值比对,容忍代理的 pretty/compact 差异;开工看现状定要不要加。
- `docs/plan/HANDOFF.md` — 补一条教训(严格解析器对"结构化字段"应比语义值、对"逐字文本"才比字节;真实代理的 pretty/compact 差异;以 codex 取 done 为权威作对照)。

## 非目标

- 不放宽文本/reasoning 的逐字节校验(应逐字回传,无格式歧义)。
- 不改 anthropic/Chat 解析、wire/protocol,不加依赖/工具。
- 不实现"只发 done 不发 delta 时信任 done"(未观测;保持 fail-closed)。
- 不消费/展示限流遥测(Plan 95 已界定)。

## 测试 / 验证

新增于 `kloop/crates/provider/tests/responses.rs`:

1. `function_arguments_agree_across_pretty_and_compact`:整轮 fixture,delta 发 compact 片段拼成 `{"limit":3,"offset":1,"path":"README.md"}`,`function_call_arguments.done` 与 `output_item.done` 的 `arguments` 都发 pretty `{"limit": 3, "offset": 1, "path": "README.md"}`(即真实抓到的字节)。断言产出正确的 `AssistantBlock::ToolUse{ input == json!({"limit":3,"offset":1,"path":"README.md"}) }` 与 `Terminal(ToolUse)`,无错误。
2. `function_arguments_differing_values_still_fail_closed`:delta 拼成 `{"a":1}`,done 发 `{"a":2}`。断言 `ProviderFailureKind::Protocol`、不可重试——锁定"仅空白放宽、值差仍 fail-closed"。
3. 保留 `malformed_function_arguments_fail_closed`(两侧 `"{oops"`)绿:回退字节相等 agree → `parse_tool_input` 失败 → Protocol,行为不变。

- workspace `cargo fmt --all -- --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test`(1 秒、无网络无 key)全绿。
- 真实 key 回归:重复跑带工具调用的真实 Responses 轮(≥6 次,覆盖此前间歇命中率),确认不再报 `arguments ... did not match`,工具轮稳定收口。

## 完成标准

- 函数参数两处比对按 JSON 值相等;pretty/compact 差异通过,值差/非法 JSON 仍 fail-closed;新测试 + 现有测试全绿。
- 不动文本/reasoning 逐字节校验、anthropic/Chat、wire/protocol;无新增依赖/工具。
- `cargo fmt` + clippy(`-D warnings`) + `cargo test` 全绿;真实回归如实记录。
- README(如改)、HANDOFF 同步;本 plan 标 ✅ 与提交号;一次 commit(信息写清验证方式)。

## 验证记录（2026-08-24）

- `responses.rs` 加 `arguments_agree`(解析成 `Value` 比相等,任一侧非合法 JSON 回退字节相等);两处比对(`function_call_arguments.done`、`finish_function_call`)改调用它,错误文案不变,其余校验(started/done 关闭、call_id/name 一致、`parse_tool_input(final_arguments)`)未动。
- 新增契约测试:`function_arguments_agree_across_pretty_and_compact`(compact delta + pretty done/output_item.done,断言 `ToolUse{ input == {"limit":3,"offset":1,"path":"README.md"} }` + `Terminal(ToolUse)`,无错误)、`function_arguments_differing_values_still_fail_closed`(`{"a":1}` vs `{"a":2}` → Protocol、不可重试);保留 `malformed_function_arguments_fail_closed`(两侧 `"{oops"`)绿。
- `cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、workspace `cargo test`(provider 23 例含两新例全绿)均通过。
- 真实 key 回归:`gw_router`/`gpt-5.6-sol` 经 gateway,`--headless --permission-mode bypass` 连跑 8 个带 `read`/`bash` 工具调用的真实 Responses 轮,全部完成、无 `arguments ... did not match`(此前 ~4 轮命中 2)。
