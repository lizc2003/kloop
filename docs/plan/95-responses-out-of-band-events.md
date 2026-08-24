# Plan 95 — Responses 放行厂商带外事件(codex.rate_limits),窄口不破 fail-closed

> 状态：✅ 已完成（2026-08-24；提交 SHA 以本条所在提交为准）
>
> 依赖：Plan 89/92 的 provider terminal/reasoning 与 route 严格纪律(本计划不动其语义,只在 SSE 解析层给"带外遥测"开一道确证的缝)。参考:`refs/codex/codex-rs`(codex 对未知 SSE 事件的处理 + rate-limit 来源)。
>
> 开发期决策(已定,来自用户拍板"窄口"):
> - **窄口**:只放行已知厂商带外前缀 `codex.`,其余未知事件仍 fail-closed(保留 `returned an unknown semantic event`)。不采用"忽略所有未知事件"的宽口方案。
> - 照 anthropic 侧 `ping` 的现成先例落地——**两处拒绝点一致处理**:①"terminal 后又来语义事件"的守卫;②match 兜底 `_`。让 `codex.*` 在两处都被当带外 no-op(与 `anthropic.rs:266`+`431` 的 `ping` 同形)。
> - **anthropic.rs 不改**:评估结论是它早已正确处理自己的带外事件(`ping` 静默 no-op + terminal 后守卫豁免),并对其余未知事件 fail-closed。它就是本计划要照抄的模板,无需改动。
> - 带外事件**静默 no-op**(与 `ping` 同款),不加日志。理由:我们放行的是**已识别**的具体前缀,不是"未知兜底",没有 codex 那种 `trace!("unhandled…")` 的必要;保持最小改动。

## Context

真实 gateway/Codex 代理在 Responses SSE 流里会私自塞一个**私有带外事件** `codex.rate_limits`:它不进官方语义流水线(`response.created → output_item.* → response.completed`),只捎带限流配额遥测(参考 codex `codex-api/src/rate_limits.rs:123` 的 `RateLimitEvent`:`plan_type`/`rate_limits.{primary,secondary}.{used_percent,window_minutes,reset_at}`/`credits`/`metered_limit_name`),与助手回答的字节无关。事件名的 `codex.` 前缀 = 厂商命名空间,不在 OpenAI 公开 Responses 事件族内。

codex 自己不靠这个 SSE 事件取限流:它从 HTTP header 解析(`parse_all_rate_limits(&headers)`),SSE 里这个同名事件对它冗余,故在 `codex-api/src/sse/responses.rs:454` `_ => trace!("unhandled…")` 静默丢弃。而 kloop 的 Responses 解析器是严格 fail-closed(Plan 89/92 纪律),任何没登记的事件名一律当致命协议错误——于是这条纯遥测事件把整轮 turn 打断,真实跑报 `provider protocol error: openai-responses returned an unknown semantic event`。

**证据**:临时把 `responses.rs` 的 `_` 兜底改成忽略未知事件后,真实工具轮正常收口;诊断改动已还原,工作树干净。

对照:anthropic 侧对自己的带外 keep-alive `ping` 早有正确处理——`anthropic.rs:266` 的 terminal 后守卫写 `if completion.is_some() && event != "ping"`,`anthropic.rs:431` 有 `"ping" => {}` 的 no-op 分支。Responses 只是缺这一层。

## 根因(单一)

`kloop/crates/provider/src/responses.rs` 的 SSE 事件循环对 `codex.rate_limits` 无对应分支,落进 `responses.rs:1130` 的 `_ => return Err(protocol("returned an unknown semantic event"))`,致命化整轮。

## 已拍板设计(窄口)

在 `responses.rs` 加一个小分类器:

```rust
/// gateway/Codex 代理往 Responses 流里塞的厂商带外事件(如
/// `codex.rate_limits`)只携带限流遥测、不属官方语义族——识别后跳过,
/// 其余未知事件仍 fail-closed(见 match 兜底)。对齐 anthropic 的 `ping` 处理。
fn is_out_of_band(event: &str) -> bool {
    event.starts_with("codex.")
}
```

两处一致消费它(照 anthropic `ping` 先例):

1. **terminal 后守卫**(现 `responses.rs:796`,`if completion.is_some()`)。为让带外事件在 terminal 之后也能被豁免(与 anthropic `event != "ping"` 同形),把事件解析提到守卫之前,守卫改为
   `if completion.is_some() && !is_out_of_band(event) { return Err(protocol("semantic event arrived after response terminal")) }`。
   即把现有 `let value = parse_sse_json(...)?; let event = event_type(&frame, &value)?;`(现 799–800)上移到该守卫之前;`[DONE]` 检查(现 790)保持原位(它只看 `frame.data`,不需解析)。

2. **match 兜底**(现 `responses.rs:1130`)前加一条守卫臂:
   ```rust
   _ if is_out_of_band(event) => {}
   _ => return Err(protocol("returned an unknown semantic event")),
   ```

这样 `codex.*` 在"流中(completion 前)"与"terminal 后"两种到达位置都被当带外 no-op、不产生任何 `StreamEvent`、不推进状态;其余任何未登记事件仍原样 fail-closed。

> 到达位置说明:真实观测里 `codex.rate_limits` 出现在 `response.completed` 之前(诊断时 match 兜底忽略即足以收口)。守卫豁免(第 1 点)是为对齐 anthropic `ping` 的健壮性(容忍 terminal 后到达),代价仅是把两行解析上移。

### 与现有守卫/测试的相容性

- 现有 `terminal_is_low_latency_but_later_complete_semantic_frames_fail_closed`(`tests/responses.rs:895`)发**重复 `response.completed`** 于 terminal 后并期望 Protocol 错误:重复 completed 非 `codex.*` → `!is_out_of_band` 为真 → 仍报错,不变。
- 解析上移后,"terminal 后来一个畸形帧"从"terminal 后语义事件"错误变为"解析错误"——两者同为 Protocol/不可重试,且无测试覆盖该具体路径,无回归(`tests/responses.rs:928` 的 `\xff` 是**唯一帧**、无前置 completion,走原解析错误路径,不受影响)。

## 关键文件

- `kloop/crates/provider/src/responses.rs` — 唯一实质改动:加 `is_out_of_band`;上移两行解析并改 terminal 后守卫;match 加带外守卫臂。
- `kloop/crates/provider/tests/responses.rs` — 新增 wiremock 契约测试(harness 已有 `sse_body`/`mount_sse`/`responses`/`collect`)。
- `kloop/README.md` — 若有"provider/SSE 行为"描述则补一句"Responses 放行 `codex.*` 厂商带外遥测事件";无则不加(避免为内部细节新增段落,开工时看 README 现状定)。
- `docs/plan/HANDOFF.md` — 补一条教训(带外遥测 vs 语义流的分层、窄口放行前缀、两处拒绝点须一致、以另一 rail 的既有先例为模板);编号取所在教训列表的下一个。

## 非目标

- 不改 anthropic 解析(已正确,是模板)。不改 OpenAI Chat 解析。
- 不放行 `codex.` 以外的任何前缀;不采用"忽略所有未知事件"宽口;不改 `returned an unknown semantic event` 对其余未知事件的 fail-closed 语义。
- 不消费 `codex.rate_limits` 的遥测内容(不落 usage/限流展示)。如需展示限流,另开计划、并优先走 HTTP header(对齐 codex),不从 SSE body 取。
- 不动 Plan 89/92 的 terminal/reasoning/route 语义;不改 wire/protocol、不加依赖、不加工具。

## 测试 / 验证

新增于 `kloop/crates/provider/tests/responses.rs`(参照 `streams_reasoning_text_and_function_call` 的整轮 fixture):

1. `out_of_band_codex_event_is_ignored_mid_stream`:`created → in_progress → codex.rate_limits(带真实字段的 payload) → output_item(text) → completed`。断言产出正常的 `TextDelta/BlockDone/Terminal(EndTurn)` 序列,`codex.rate_limits` 不产生任何 `StreamEvent`,无错误。
2. `out_of_band_codex_event_after_terminal_is_ignored`:`created → completed → codex.rate_limits`(流随后结束,无 `[DONE]`)。断言恰好一个 `Terminal`、无错误——验证 terminal 后守卫豁免(与 anthropic `ping` 同形)。
3. `unknown_non_codex_event_still_fails_closed`:`created → some.unknown.event`。断言 `ProviderFailureKind::Protocol`——锁定窄口边界:只有 `codex.*` 被放行,其余仍 fail-closed。

- workspace `cargo fmt --all -- --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test`(1 秒、无网络无 key)全绿。
- 可选:真实 key 回归一轮(问用户要代理/key,勿写入任何提交文件),确认真实 Responses 工具轮不再报 `unknown semantic event`。

## 完成标准

- `codex.*` 带外事件在 completion 前后两种位置都被静默跳过;其余未知事件仍 fail-closed;三条新测试 + 现有测试全绿。
- 不新增依赖/工具/wire 变更;不动 anthropic/Chat/Plan89-92 语义。
- `cargo fmt` + clippy(`-D warnings`) + `cargo test` 全绿;真实回归如实记录(跑或未跑)。
- README(如改)、HANDOFF 同步;本 plan 标 ✅ 与提交号;一次 commit(信息写清验证方式)。

## 完成记录（2026-08-24）

按已拍板窄口方案落地,`crates/provider/src/responses.rs` 唯一实质改动:

- 在 `event_type` 旁新增 `is_out_of_band(event) = event.starts_with("codex.")`(带注释说明厂商带外遥测 vs 官方语义族)。
- 把 `parse_sse_json` + `event_type` 两行上移到 terminal 后守卫之前;守卫改为 `if completion.is_some() && !is_out_of_band(event)`;`[DONE]` 检查(只看 `frame.data`)保持原位。
- match 兜底前加 `_ if is_out_of_band(event) => {}`,其余未知事件仍 `returned an unknown semantic event` fail-closed。

`crates/provider/tests/responses.rs` 新增三条契约测试:`out_of_band_codex_event_is_ignored_mid_stream`(带真实 `plan_type`/`rate_limits`/`credits`/`metered_limit_name` payload,断言仅 TextDelta/BlockDone/Terminal(EndTurn) 三个事件)、`out_of_band_codex_event_after_terminal_is_ignored`(terminal 后到达仍恰好一个 Terminal)、`unknown_non_codex_event_still_fails_closed`(非 `codex.` 未知事件仍 Protocol 且不可重试)。

未改 anthropic/Chat/Plan89-92 语义,无新增依赖/工具/wire 变更。README 第 5 条 Provider seam 补一句 `codex.*` 带外遥测放行说明;HANDOFF 顶部教训列表补 Plan 95 教训。

**验证**:`cargo fmt --all -- --check` + `cargo clippy --all-targets -- -D warnings` 全绿;`cargo test` 全工作区通过(`responses.rs` 21 passed 含 3 条新测试、`kloop_core` 739、`kloop_tui` 172 等,退出码 0)。未跑真实 key 回归(诊断阶段已确认真实工具轮因此事件报错、忽略后收口;本轮无网络无 key)。
