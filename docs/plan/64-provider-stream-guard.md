# Plan 64 — Provider Stream Guard

> 状态：✅ 已完成（2026-08-04）
>
> 来源：Plan 60 A1
>
> 开工基线：`533fe49`
>
> 施工关系：固定 provider 内部 defaults，不扩 flat `Config`，避免与未开工的 Plan 63 生命周期重构交叉。

## 背景

三条 provider wire 已有稳定 translation，但现有 `Provider::stream()` 只返回裸 mpsc receiver：open/read 没有统一 timeout 与容量边界，HTTP status/`Retry-After` 在 `anyhow` 字符串中丢失，consumer 取消时 producer 可能继续读网络，adapter 与 channel close 共同解释终态，core 又只以 depth-0 可见 delta 阻断重试。

本计划落实 Plan 60 首推的 A1，只做 provider open/read guards、retry safety invariant、三 wire terminal conformance 与验收。不加入 provider catalog、event replay、subagent manager 或 Web runtime。

## 已确认的产品决定

1. guard 使用 provider 内固定默认值，不加 Config/env：open 45s、chunk idle 900s、wall-clock 1800s、累计响应 10MiB、单个未闭合 SSE frame 1MiB。
2. Anthropic 只有 `message_stop` 完成；Responses 只有 `response.completed` / `response.incomplete` 完成；Chat Completions 以 `finish_reason` 完成，`[DONE]` 只结束传输。
3. 任何 text/reasoning delta 或完整 text/thinking/redacted-thinking/tool-use block 都封死透明 retry；主 agent 与 subagent 一致。
4. 空 tool input 可规范成 `{}`；非空非法 JSON 三条 wire 全部 fail closed，不产生 `ToolUse`。
5. 总计 3 attempts。仅 transport、open/idle/wall timeout、HTTP 408/429/5xx、缺 terminal 的 EOF 可在尚无语义内容时 retry；`Retry-After` 支持秒/HTTP-date并封顶 60s。malformed frame/tool JSON、content cap、其他 4xx 立即失败且不 fallback。
6. provider/core 内部 typed failure；公开 `EndReason`、rollout terminal 与 native protocol 1.0 保持 `Error(String)`，不抢跑 Plan 63 protocol 2.0。

## 必须保持的不变量

- 每个 provider attempt 恰有一个 `Done` 或 typed error，error 后不再产生事件。
- 缺 wire completion marker 的 EOF 不能伪造成成功；Chat 已有 `finish_reason` 的 clean EOF 除外。
- retry/fallback 只能发生在该 attempt 尚无语义内容时。
- partial/非法 tool JSON 不能进 history、不能 dispatch。
- 取消必须停止 producer；compaction 同样受 transport guard 保护且失败不改 history。
- HTTP 错误正文与凭据继续有界、脱敏，不向 UI/server 泄漏 key。

## 实施切片

1. provider typed failure、单终态 sink/completion、RAII `ProviderStream`。
2. 共享 open/read/size guards、SSE frame cap、HTTP/Retry-After taxonomy。
3. 三 adapter completion 与严格 JSON conformance。
4. `ProviderStream` semantic watermark、core typed retry/fallback/partial 映射。
5. provider/core/server 确定性回归，README/HANDOFF 与真实 API smoke。

## 验收

所有分钟级 guard 用 paused time/纯函数测试，不真实等待。

## 完成记录 ✅

- provider 新增 `ProviderFailure` / `ProviderFailureKind`、`StreamSink` / `StreamCompletion` 与 RAII `ProviderStream`；adapter 只能发非终态事件，外层统一发唯一 `Done`/error，consumer drop 立即 abort producer。裸 channel close 在 seam 内物化为 typed incomplete failure，semantic watermark 同样由 seam 单调维护。
- 共享 transport guard 固定 45s open、15m idle、30m wall、10MiB response、1MiB SSE frame；已知超限 `Content-Length` 立即拒绝，SSE 增量扫描不随未闭合 frame 退化成 O(n²)。HTTP 错误体 64KiB/4096 chars 双重有界并保留 key 脱敏。
- HTTP 408/429/5xx、transport/open/idle/wall 与 incomplete EOF 仅在 watermark 未封口时做总计 3 attempts；`Retry-After` 秒/HTTP-date均支持且 60s 封顶。non-retryable 立即 `Error`，不 retry、不 fallback。
- Anthropic、Chat Completions、Responses 的 completion marker 与非法 tool JSON 已按本计划收紧；`parse_sse_json` / `parse_tool_input` 共享三 rail 的 fail-closed 规则。
- core/provider/server 定向测试通过：provider 57 tests；`agent::tests` 40 tests；server partial terminal 1 test。新增覆盖 paused open/idle/wall、response/frame cap、Retry-After、三 rail malformed/EOF/tool JSON、tool/subagent retry seal、producer abort、single terminal 与 rollout 恢复。
- 全量通过：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`、`cargo run -p kloop -- --mock`、`python3 -B refs/claude-code-2.1.220/verify.py --corpus-only`、`git diff --check`。
- 真实 API：OpenAI Responses 正常完成两轮，headless JSON 有 reasoning/text、`thread/tokenUsage/updated` 与唯一 `turn/completed`；OpenAI Chat 同样完成并回传 usage/terminal。Anthropic 配置与 key 可用，但目标 proxy 直连 70s 内 `http=000`、0 bytes，两个模型均由新 open guard 稳定报 typed timeout；这是当时上游不可达证据，不把它冒充 Anthropic 正常流验收。
- `/simplify` 四路清理已收敛：共享 JSON parser、seam-owned premature-close/semantic watermark、持久 timer、SSE 增量扫描、Content-Length 快拒与错误体 cap；无行为扩项。
- 完成提交：以本条所在提交为准。
