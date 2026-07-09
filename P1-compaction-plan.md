# P1 设计:codex fork 的 predictive / reactive 压缩

> 依据:2026-07-09 对 codex 与 claude-code(逆向版)两边压缩机制的全量调研。
> 所有 codex 路径相对 `refs/codex/`,cc 路径相对 `~/work/claude-code/`。

## 一、修正认知:codex 的家底比交接文档说的厚

交接文档说 codex "只有 pre/mid-turn 两处被动压缩"。gating 模型确实如此,但机制本身已相当完整:

| 已有机制 | 位置 | 说明 |
|---|---|---|
| pre-turn 门控 | `core/src/session/turn.rs:808-838` | 模型切换/降档补偿 + proactive trim + token 限额检查 |
| mid-turn 门控 | `turn.rs:356-380` | 每次响应后查 `token_limit_reached` 或模型主动请求换窗 |
| 本地模型摘要 | `core/src/compact.rs:216-392` | 真调模型;Codex 高保真 9 段 prompt(`fork_high_fidelity_compaction/prompt.md`)已对齐 cc 的 9 段式 |
| remote v2 | `compact_remote_v2.rs` | 服务端摘要,保留 user/developer/system ≤64k tokens |
| token-budget 换窗 | `compact_token_budget.rs` | 无摘要直接开新窗 |
| proactive trim | `fork_proactive_trim.rs` | 0.6× 低水位 + 缓存冷却后,旧的大块工具输出改写为 offload 指针,保护最近 16k tokens |
| 录入时截断/offload | `context_manager/history.rs:407-492` | 经 codex adapter,支持 Ccr 压缩与 Offload 策略 |
| 配对不变量 | `history.rs:396-405` normalize | 孤儿 tool output 清理、成对删除 |
| token 记账 | 服务端 usage 锚点 + ~4 字节/token 启发(`utils/string/src/truncate.rs:4`) | 与 cc 同构(cc 是 chars/4) |

**真正缺的两样**(与 cc 对照后确认):

1. **Predictive**:发请求前预判"本轮增长会不会撑爆窗口"。codex 的一切门控都基于已测量的当前用量;`get_estimated_token_count`(`session/mod.rs:1299-1305`)算了前瞻估计却只用于 trace 日志。`turn.rs:153-156` 有一条明确的 TODO 承认此缺口。
2. **Reactive**:主采样请求撞上 `ContextWindowExceeded` 后的"压缩→重试"恢复。现状只有压缩过程自身的溢出回退(`compact.rs:300` 丢最旧条目重试)和 `set_total_tokens_full` 记账钉死;主循环没有 compact-then-retry。

## 二、cc 的对应设计(移植蓝本)

### Predictive(cc `src/query.ts:848-888`)

```
currentTokens = tokenCountWithEstimation(messages) - snipTokensFreed
   // = 最后一条带真实 API usage 的 assistant 锚点 + 其后消息的 chars/4 估算
predictiveThreshold = effectiveContextWindow - estimateMaxTurnGrowth(model)
   // effectiveWindow = contextWindow - min(maxOutput, 20_000)
   // estimateMaxTurnGrowth = min(maxOutput, 20_000) + 15_000(工具结果增长预留)
if currentTokens > predictiveThreshold → 立即跑完整 autocompact,换上压缩后历史,同轮继续发请求
```

要点:阈值是**绝对余量**不是百分比;predictive 用裸有效窗口(不再叠加 autocompact buffer,避免双重预留,cc 注释明确此意);阈值压缩(已超线)与 predictive(现在没超、本轮会超)是互补的两层。

### Reactive(cc `src/query.ts:1349-1470` + `services/api/errors.ts`)

- 检测:错误消息匹配 `'prompt is too long'`(大小写不敏感,覆盖 400/413 两形态);`parsePromptTooLongTokenCounts` 用正则抽出 `actual > limit` 算出溢出缺口。
- 恢复:每 turn 单发(`hasAttempted` 守卫)→ 完整摘要压缩 → **继续循环重试**;失败才把错误浮出并终止。流式期间错误对 UI 暂扣,恢复成功用户无感。
- 摘要请求自身再溢出:`truncateHeadForPTLRetry` 按"整 API 轮"分组丢头部(绝不切开 tool_use/tool_result 对),丢多少由解析出的 token 缺口决定(兜底 20%),最多重试 3 次。
- 熔断:连续 3 次 autocompact 失败停止再试。

## 三、P1 实施方案

### P1-a:predictive 压缩(填 `turn.rs:153-156` 的 TODO)

**插入点**:`run_pre_sampling_compact`(`turn.rs:808`),在现有三步检查后追加第四步。

```
pending = estimate(待记录的 user input + context diff/世界状态重注入条目)
growth  = min(max_output_tokens, 20_000) + TOOL_RESULT_GROWTH_ESTIMATE(15_000,可配)
if auto_compact_scope_tokens + pending + growth > usable_context_window {
    run_auto_compact(..., CompactionReason::Predictive, CompactionPhase::PreTurn)
}
```

- 估算复用现成的 `estimate_response_item_model_visible_bytes` / `estimate_token_count_with_base_instructions`(`history.rs:598-657, 208-222`),零新依赖。
- 阈值基线用 `usable_context_window`(`turn_context.rs:208-215`,默认 95%),**不含** auto-compact buffer——照抄 cc 的不双重预留原则;现有 `auto_compact_token_limit`(90%)继续作为"已超线"层,两层互补。
- 新增 `CompactionReason::Predictive` 便于遥测区分。
- mid-turn 侧同理:`turn.rs:356-380` 的判断从 `token_limit_reached` 扩为 `token_limit_reached || predicted_overflow`(pending 此时为零,只加 growth 项)。

### P1-b:reactive 压缩(主循环 compact-then-retry)

**插入点**:`run_turn` 采样循环的错误分支,捕获 `ContextWindowExceeded`。

1. 每 turn 单发守卫(对齐 cc 的 `hasAttemptedReactiveCompact`)。
2. 触发 `run_auto_compact(..., CompactionReason::Overflow, CompactionPhase::MidTurn)`,成功后 `continue` 重试本轮;失败或已试过则按现行为浮出错误。
3. 若 provider 错误带 token 数(Anthropic 的 "N tokens > M maximum" 形态),解析缺口传给压缩层,供 `trim_function_call_history_to_fit_context_window` 精确定量;解析不出用现行为。
4. 熔断:复用/新增"连续压缩失败计数",上限 3(cc 同值)。

### P1-c:顺手补齐(便宜)

- 压缩内部的溢出回退(`compact.rs:300` 逐条丢最旧)升级为按"整轮"分组丢 + 缺口定量,参照 cc `truncateHeadForPTLRetry`;claw-code `runtime/src/compact.rs:129-166` 的边界回走逻辑可作 Rust 参考。
- `get_context_remaining` 工具的返回值加入 predicted growth 信息,让模型自己也能预判。

### 不做(明确排除)

- cc 的 snip / context-collapse / cached microcompact(cache_edits):依赖 cc 特有的消息 UUID 体系与 Anthropic cache-edit API,收益/成本比不划算,且 codex 的 proactive trim 已覆盖同一生态位。
- 更换 token 估算器:两边都是 ~4 字节/token 启发 + 服务端 usage 锚点,已同构,不动。

## 四、验证方式

- 单测:predictive 阈值数学(边界:pending 恰好压线/超线;growth 封顶);reactive 单发守卫与熔断;溢出缺口解析。
- 集成(`core/suite` 范式):mock provider 先回 `ContextWindowExceeded` 再回正常响应,断言历史被压缩且请求重试成功、事件序列含 ContextCompaction 生命周期。
- 真实流量:长会话喂大文件读输出,观察 predictive 在撞线**前**触发(遥测 reason=Predictive),全程无用户可见错误。

## 五、工作量预估

P1-a ~1-2 天(TODO 处代码已留好插槽,估算函数现成);P1-b ~1-2 天(错误路径改造 + 测试);P1-c ~1 天。合计约一周内,单人。
