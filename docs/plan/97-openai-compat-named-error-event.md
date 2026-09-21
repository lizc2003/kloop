# Plan 97 — Chat rail 识别具名 `event: error` 帧,报出真实上游错误并按瞬时归为可重试

> 状态：✅ 已完成（2026-08-24；提交见本次提交）
>
> **实现与下文原设计的差异(开工后随用户拍板演进,以本节为准)**:
> - **归类模型翻转**:原设计是"显式瞬时白名单 + 默认致命"(对齐 `failure.rs` 的 HTTP 状态白名单)。开工时对照 Responses 与参考 codex-rs(`codex-api/src/sse/responses.rs` `classify_response_error`)发现两者都是**致命黑名单 + 默认可重试**。用户拍板"同意 codex-rs 风格",故最终实现改为:只有一小撮客户端/永久性标识(auth/permission/quota/policy/invalid-request/request-too-large/billing)判致命,其余(瞬时上游、过载、限流、乃至未知标识)一律默认**可重试**。这是 HTTP 白名单的**逆**:那边小集合放行重试,这边小集合拒绝重试。仍安全,因为 core 用 `after_semantic_output` 把关(见 `stream.rs`),可重试流错误只在未产出语义前重放。
> - **过载=可重试**(用户拍板):codex 把过载当致命,Anthropic 当可重试;kloop 取可重试,故 `overloaded_error`/`server_is_overloaded`/`slow_down` 不入致命集。
> - **范围扩到三 rail**(原为"仅 Chat,Responses 另开 plan",见下「待确认 #2」):用户问"Responses/messages 是不是也应该这样",拍板统一。抽出共享 `error_label`(code→type→unknown)+ `stream_error(rail,label)`+ `is_fatal_stream_error` 到 `lib.rs`,三 rail 同调。字段差异:Chat 读 `value["error"]`;Responses `response.failed` 读 `value["response"]["error"]`;Responses `error` 事件的 `type` 是信封字面量 `"error"`,故只读 `value["code"]`;Anthropic 读 `value["error"]`(type 制)。致命集是 OpenAI 族与 Anthropic 词表的并集,已核无字符串碰撞。
> - **仍恪守的窄口**:Chat 事件名守卫只多放行 `error`(连同 `None`/`message`),其余具名事件仍 `returned an unknown SSE event name` fail-closed;overflow 分支仍最先命中。
>
> ---
> 下文为原始设计记录(白名单版),保留作演进对照。
>
> 依赖：Plan 89/92 的严格解析纪律、Plan 95(带外遥测 vs 语义流的分层)。参考:`refs/codex/codex-rs`;kloop 自身 `responses.rs:1148` 对 `error` 语义事件的处理、`failure.rs` 的 `incomplete_protocol`(Protocol 类但可重试)、`stream.rs:89-95`(把 semantic 标记贴到 provider 错误上)。
>
> 开发期决策(已定):
> - **窄口**:只把 SSE 帧名 `error` 放行到**已有的** `data.error` 错误处理路径;其余具名事件仍 fail-closed(保留 `returned an unknown SSE event name`)。不采用"忽略所有具名事件"或"忽略未知事件"的宽口。
> - 错误**如实报出**:把 surfacing 的标识从只读 `error.code` 扩为 `code` → `type` → `"unknown"`,让 `upstream_error` 这类不带 `code` 的真实错误不被抹成 `unknown`。此增强同时作用于具名 `event: error` 与原有无名 `data:{"error":...}` 两条来源(统一)。
> - **可重试性**:流内上游错误按**显式瞬时白名单**分类——命中(如 `upstream_error`)→ `incomplete_protocol`(可重试);否则 → `protocol`(不可重试)。对齐 `failure.rs:124` 的 HTTP 状态白名单哲学(默认不可重试、只显式放行已知瞬时类)。重试是否真发生仍由 core 结合 `after_semantic_output` 把关(`stream.rs` 已贴标记),所以只在"未产出语义"时才会重放,安全。
> - 只动 `openai.rs`(Chat/openai-compat rail);不改 responses/anthropic/wire/protocol,不加依赖。overflow 分支(`is_overflow_message` → `context_overflow`)保持最前、不动。

## Context(真实验证地面真相)

对三 rail 做真实回归(网关,`--headless --permission-mode bypass`,每轮全新临时 HOME 走 env-only)时,Chat rail(`KLOOP_PROVIDER=openai`)间歇 fail-close,报:

```
provider protocol error: openai-compat returned an unknown SSE event name
```

在 `openai.rs:429` 加有界诊断(把帧名+截断 data 带进错误信息),第 13 轮复现,抓到确切帧:

```
event: error
data: {"error":{"message":"Upstream service temporarily unavailable","type":"upstream_error"}}
```

即:**网关 代理用具名 `event: error` 帧回传了一个真实的上游瞬时错误**(`type: upstream_error`,“暂时不可用”),不是 Plan 95 那种 `codex.*` 带外遥测。诊断改动已 `git checkout` 还原,工作树干净。

对照 Responses rail:同段真实回归里 run 4 撞到 `response.failed (server_error)`——那是走正规 `response.failed` 事件、如实报出的。Chat rail 因为错误裹在 `event: error` 里、且事件名守卫在错误处理**之前**,就退化成了误导性的 "unknown SSE event name"。

## 根因(单一,双重后果)

`rust/crates/provider/src/openai.rs:429` 的事件名守卫只放行 `None`/`message`,在读 `data` 之前就把任何具名事件判死:

```rust
if !matches!(frame.event.as_deref(), None | Some("message")) {
    return Err(protocol("returned an unknown SSE event name"));   // ← 具名 error 在此拦死
}
if frame.data.trim() == "[DONE]" { transport_done = true; continue; }
let value = crate::parse_sse_json("openai-compat", &frame.data)?;
if !value["error"].is_null() {                                     // ← 已有的错误处理,够不着
    if is_overflow_message(&value["error"].to_string()) { return Err(ProviderFailure::context_overflow()); }
    let code = value["error"]["code"].as_str().unwrap_or("unknown");
    return Err(protocol(format!("stream error ({code})")));         // ← 且此处恒不可重试
}
```

双重后果:

1. **真实错误信息被吞**:`:437` 起明明会解析 `data.error`、报出具体标识,但具名 `event: error` 在 `:429` 就被判成 "unknown SSE event name",走不到——用户看到误导性的“未知事件名”,而非真正的 “Upstream service temporarily unavailable / upstream_error”。
2. **可重试性误判**:即便够着 `:442`,它用 `protocol(...)`(不可重试);而 `upstream_error`/“暂时不可用”本该当**瞬时/可重试**。整轮直接挂,不重试。

## 已拍板设计(窄口)

`rust/crates/provider/src/openai.rs`,三点改动,均在 `stream()` 的帧循环内:

1. **放行 `error` 帧名**:守卫改为
   ```rust
   if !matches!(frame.event.as_deref(), None | Some("message") | Some("error")) {
       return Err(protocol("returned an unknown SSE event name"));
   }
   ```
   `[DONE]` 检查、`parse_sse_json`、`value["error"]` 处理均保持原位——`error` 帧自然落进 `:437` 的错误分支(其 data 就是 `{"error":{...}}`)。其余具名事件仍 fail-closed。

2. **如实 surfacing + 瞬时分类**:把 `:441-442` 两行替换为一个小助手的调用,由它统一处理具名与无名两条错误来源:
   ```rust
   /// Surface an OpenAI-compat stream error faithfully. Prefer `code`, then
   /// `type`, then "unknown" (proxies send transient errors as {type} with no
   /// code). Transient upstream conditions are retryable (Protocol kind but
   /// retry-admitted); everything else stays fatal. core still gates the actual
   /// retry on `after_semantic_output`, so this only replays before any output.
   fn stream_error(err: &Value) -> ProviderFailure {
       let label = err["code"].as_str()
           .or_else(|| err["type"].as_str())
           .unwrap_or("unknown");
       let message = format!("stream error ({label})");
       if is_transient_stream_error(label) {
           ProviderFailure::incomplete_protocol(message)   // Protocol 类、可重试
       } else {
           protocol(message)                               // 不可重试
       }
   }

   /// Explicit transient allowlist (mirrors failure.rs HTTP status allowlist:
   /// default fatal, only known-transient classes retry).
   fn is_transient_stream_error(label: &str) -> bool {
       matches!(label,
           "upstream_error" | "server_error" | "service_unavailable"
           | "overloaded" | "rate_limit_exceeded")
   }
   ```
   `:437-442` 改为:
   ```rust
   if !value["error"].is_null() {
       if is_overflow_message(&value["error"].to_string()) {
           return Err(ProviderFailure::context_overflow());
       }
       return Err(stream_error(&value["error"]));
   }
   ```
   overflow 分支保持最前、语义不变。

> 抓到的 `upstream_error` 命中白名单 → run-13 的失败将从 `Protocol/unknown/不可重试` 变为 `stream error (upstream_error)` 且**可重试**,信息如实、行为正确。

### 边界语义

- **具名 `error` 帧但 data 非 `{"error":...}`**(未观测):落进后续正常处理,若结构不符则原有校验 fail-closed。不为此臆造分支。
- **无名 `data:{"error":...}`**(原有路径):同样走 `stream_error`,与具名帧行为一致(统一后 code/type/瞬时分类都生效)。
- **overflow**:仍最先命中 `is_overflow_message` → `context_overflow`(不可重试),不受影响。
- **未知标识**(既无 `code` 也无已知 `type`):`unknown` 不在白名单 → 不可重试,保守。
- **其余具名事件**(非 `error`):仍 `returned an unknown SSE event name` fail-closed——窄口边界不破。

## 待确认(开工时问用户,一次一个)

1. **瞬时白名单取值**:上面列了 `upstream_error/server_error/service_unavailable/overloaded/rate_limit_exceeded`。是否够/是否收窄到只 `upstream_error`(已实锤)+ 明显 5xx 类?其余先保守不可重试,日后按真实抓包再加。
2. **跨 rail 一致性(非本计划目标,仅登记)**:Responses rail 的 `error`/`response.failed`(`responses.rs:1146/1153`)当前对同类瞬时上游错误也一律 `protocol`(不可重试)——run 4 的 `response.failed (server_error)` 即是。是否要把同一套 `is_transient_stream_error` 分类扩到 Responses rail?建议**另开 plan** 处理(需各自真实抓包佐证),plan 97 只修已复现的 Chat rail。

## 关键文件

- `rust/crates/provider/src/openai.rs` — 唯一实质改动:守卫放行 `error` 帧名;加 `stream_error`/`is_transient_stream_error`;`data.error` 处理改调 `stream_error`。
- `rust/crates/provider/tests/openai.rs` — 新增契约测试(harness 已有 `sse_body`/`mount_sse`/`collect`;`mount_sse` 收原始 body,可直接塞 `event: error\ndata: {...}\n\n`)。
- `rust/README.md` — Provider seam 若有 SSE 行为描述则补一句“Chat rail 识别具名 `event: error` 帧、如实报错、瞬时上游错误可重试”;开工看现状定要不要加。
- `docs/plan/HANDOFF.md` — 补一条教训(严格 SSE 解析对“错误帧”应先如实报错再谈严格;事件名守卫别挡在错误处理之前;瞬时 vs 致命的显式白名单;以 Responses `error` 事件与 `incomplete_protocol` 为对照)。

## 非目标

- 不放行 `error` 以外的任何具名事件;不改 `returned an unknown SSE event name` 对其余具名事件的 fail-closed。
- 不改 Responses/anthropic 解析;不改 wire/protocol;不加依赖/工具。
- 不实现 Responses rail 的瞬时重试对齐(见待确认 #2,另开计划)。
- 不消费/展示任何遥测(Plan 95 已界定)。

## 测试 / 验证

新增于 `rust/crates/provider/tests/openai.rs`:

1. `named_error_event_surfaces_upstream_error_and_is_retryable`:mount 原始 body 含正常文本 delta 后接 `event: error\ndata: {"error":{"message":"Upstream service temporarily unavailable","type":"upstream_error"}}\n\n`(即真实抓到的帧)。断言最终 `StreamResult` 为 `ProviderFailureKind::Protocol`、`is_retryable() == true`、`message()` 含 `upstream_error`(不再是 `unknown SSE event name`)。
2. `named_error_event_with_fatal_type_stays_fatal`:同上但 `type` 用一个不在白名单的值(如 `invalid_request_error`)。断言 `Protocol`、`is_retryable() == false`、message 含该 type。
3. `unnamed_error_body_uses_same_classification`:无名 `data:{"error":{"type":"upstream_error"}}`。断言与具名帧一致(Protocol、可重试、含 `upstream_error`)——锁定统一。
4. `unknown_non_error_named_event_still_fails_closed`:`event: something\ndata: {}`。断言 `Protocol`、`is_retryable() == false`、message 含 `unknown SSE event name`——窄口边界。
5. 保留 `http_overflow_maps_to_overflow_error` 绿(overflow 仍最先命中)。

- workspace `cargo fmt --all -- --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test`(1 秒、无网络无 key)全绿。
- 真实 key 回归:重复跑 Chat rail 真实工具轮(≥数十次以覆盖此前 ~1/13 的偶发命中),确认命中 `event: error` 时报出真实 `stream error (upstream_error)`、被当可重试(轮内自动重试或如实退避),不再出现 `unknown SSE event name`;并复跑 Responses/Anthropic 三 rail 冒烟不回归。

## 完成标准

- 具名 `event: error` 帧被如实报错(real code/type,不再 `unknown SSE event name`);瞬时上游错误可重试、致命错误仍 fail-closed;其余具名事件仍 fail-closed;新测试 + 现有测试全绿。
- 不动 Responses/anthropic/wire/protocol;无新增依赖/工具。
- `cargo fmt` + clippy(`-D warnings`) + `cargo test` 全绿;真实回归如实记录。
- README(如改)、HANDOFF 同步;本 plan 标 ✅ 与提交号;一次 commit(信息写清验证方式)。

## 验证记录(三 rail 真实回归,2026-08-24)

提交 `cd2e3e7`,工作树干净;本地 `cargo fmt --all -- --check` + `cargo test -p kloop-provider` 全绿(openai/responses/anthropic 契约 34+19+19、及 provider 24,共含 plan97 新例)。

真实 key 回归(网关 Chat/Responses + 真实 Anthropic 端点,`--headless --permission-mode bypass`;Chat/Anthropic 每轮全新临时 HOME 走 env-only,Responses 用 `~/.kloop/config.toml` 的 `gw_router`/`gpt-5.6-sol`):

- **Chat rail(决定性确证)**:22 轮里 21 轮干净收口,第 3 轮命中偶发具名 `event: error`(即诊断期抓到的 `type: upstream_error`)。plan 97 行为全兑现:错误如实报为 `openai-compat stream error (upstream_error)`(不再是误导的 `unknown SSE event name`)→ 判为可重试 → core 退避重试(attempt 1/3、2/3)→ **重试成功、整轮正常收口**。修复前同条件为硬挂 + 信息丢失。
- **Responses rail**:4/4 干净收口(`read_file` 工具轮 + 中文总结),共享助手重构无回归。
- **Anthropic rail**:完整收口一轮(端点抖:先 `open timeout: headers 45s 未到`、再 `http 503: upstream connect error/reset before headers`,均被正确判可重试并重试,第 3 次成功返回)。工具轮解析路径本会话另见执行(重试后 `read_file` 正常调用)。plan 97 对 anthropic.rs 的改动为流内 `error` 事件的共享分类,由 19 条契约测试覆盖。

范围说明:plan 97 的核心新路径(三 rail 统一的流内 `error` 事件分类、`incomplete_protocol` 默认可重试)在 **Chat rail 拿到真实命中并验证**;Responses 的 `error`/`response.failed`、Anthropic 的流内 `error` 事件均偶发,本轮未在真实流里撞上,由各 rail 绿色契约测试覆盖。所有观测到的错误(`upstream_error`/`server_error`/open timeout/503/429)分类与重试行为均符合设计,secrets/endpoints 未落任何文件。
