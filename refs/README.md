# refs — 参考资料与调研结论

kloop 设计时对比研究过三个代码库。本文件是关于"别人代码"的全部知识:导读 + 调研结论 + 可移植设计参考。(项目自身的状态与教训见根目录 HANDOFF.md。)

## 导读

| 参考 | 位置 | 看什么 |
|---|---|---|
| **codex** | `refs/codex` | codex 生产级 fork。分层循环:`codex-rs/core/src/session/turn.rs`;工具注册:`core/src/tools/spec_plan.rs`;并行锁:`tools/parallel.rs`;压缩全家桶:`compact*.rs`、`fork_proactive_trim.rs`;会话落盘:`rollout/`;扩展范式:`ext/worktree`;集成测试:`core/tests/suite`(mock SSE + wiremock 范式) |
| **claude-code(逆向 TS 版)** | `~/work/claude-code` | 主循环:`src/query.ts`(七层压缩流水线在 queryLoop 每轮开头);压缩:`src/services/compact/*`;工具并发分批:`toolOrchestration.ts`(partitionToolCalls);子 agent 递归:`AgentTool/runAgent.ts`;重试:`withRetry.ts`;溢出检测:`services/api/errors.ts` |
| **claw-code** | `./claw-code/`(本地拷贝,已删 target/) | **不可作底座**(见结论 3)。仅三样值得抄:① `rust/crates/mock-anthropic-service` + `rusty-claude-cli/tests/output_format_contract.rs` 的 mock 契约测试纪律;② `rust/crates/api/src/providers/openai_compat.rs` 的 tool_calls 流式翻译状态机;③ `rust/crates/runtime/src/compact.rs:129-166` 的压缩边界回退(不切开 tool_use/tool_result 对) |

## 调研结论(三轮调研的浓缩)

1. **codex**:地基最硬——分层循环(任务→主循环→provider 故障转移→请求重试→流消费,各一层)、append-only 历史硬规则、多模型工具画像(model_info 按模型切工具形态)、unified exec 持久 shell 会话。弱在:上下文耐力(门控全部基于已测量用量,无 predictive;此缺口 2026-07 已在其 fork 上试补过一轮,见下"预演记录")、恢复语义少、工具默认不并行、shell 万能导致权限粒度粗。
2. **claude-code**:赢在生存层——七层上下文防线(含 predictive/reactive 压缩)、丰富恢复语义(输出截断升级重试、fallback 模型、孤儿 tool_result 修补、Terminal 原因枚举)、专用工具(Read/Edit/Grep/Glob)+ `isConcurrencySafe(input)` 按入参动态并发(只读批并发上限 10)、子 agent 递归复用同一 query() 循环。
3. **claw-code**(agent 自治维护的 Rust 克隆,精读过 11.6 万行):约 60% 真实 / 25% 孤儿 / 15% 表演;压缩是假的(不调模型,关键词模板套 `<summary>` 戏服,触发数学错误)、工具严格串行、Worker/Cron 是内存模拟。
4. **两边独立收敛的"必然解"**(直接照抄不必发明):tool_use 有无判续跑(别信 stop_reason)、deferred 工具 + tool_search、超长输出落盘 + 回读工具、MCP `server__tool` 命名。
5. 多模型编辑工具实测数据点(2026-07-09):同一"修 bug 并验证"任务,gpt-5.4-mini 和 claude-sonnet-5 都能首试用对 Edit(old_string/new_string)形态并理解 offload 指针——apply_patch 不构成选型约束。

## cc 压缩层设计参考(kloop 压缩已按此实现;扩展时对照)

**Predictive**(cc `src/query.ts:848-888`):
```
currentTokens = 最后一条带真实 API usage 的 assistant 锚点 + 其后消息的 chars/4 估算
predictiveThreshold = effectiveContextWindow - estimateMaxTurnGrowth
   // effectiveWindow = contextWindow - min(maxOutput, 20_000)
   // estimateMaxTurnGrowth = min(maxOutput, 20_000) + 15_000(工具结果增长预留)
超线 → 立即完整压缩,同轮继续发请求
```
要点:阈值是绝对余量不是百分比;predictive 用裸有效窗口,不叠加 autocompact buffer(避免双重预留,cc 注释明确);与阈值压缩("已超线")互补。**cc 盲点(kloop 已修):窗口 ≤ 增长预留时阈值非正,变成"永远压缩"——必须守卫。**

**Reactive**(cc `src/query.ts:1349-1470` + `services/api/errors.ts`):检测匹配 `'prompt is too long'`(大小写不敏感);正则抽 `actual > limit` 算溢出缺口;每 turn 单发守卫 → 完整压缩 → 继续循环重试,失败才浮出错误(流式期间错误对 UI 暂扣);摘要请求自身溢出时按"整 API 轮"分组丢头部(缺口定量,兜底 20%,最多 3 次);连续 3 次压缩失败熔断。

**其余 cc 常数**(移植时对照):autocompact buffer 按窗口 50k/30k/13k;手动 compact 预留 3k;警告带 20k;单消息工具结果预算 200k 字符、单工具默认 50k;摘要 prompt 九段式(kloop 的 COMPACT_INSTRUCTION 是其精简版);全量压缩后重注入最近读过的 ≤5 个文件现状。

## 预演记录(codex fork,2026-07-09)

kloop 的压缩设计曾先在 codex fork 上完整实现过一轮(分支 `codex/worktree/predictive_reactive_compaction`,提交 57c746ef7,Buildbot 绿,未合入 main):predictive 插在 `run_pre_sampling_compact`、reactive 插在采样错误分支、Feature 双旗标、compact_fork_tests.rs 四个集成测试。价值:验证了设计、抓出小窗口负阈值盲点。教训:kloop 才是项目,参考库不用于开发。
