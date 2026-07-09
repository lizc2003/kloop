# refs — 参考资料

kloop 设计时对比研究过的三个代码库(结论详见根目录 HANDOFF.md 第二节):

| 参考 | 位置 | 看什么 |
|---|---|---|
| **claw-code** | `./claw-code/`(本地拷贝,已删 target/) | ① `rust/crates/api/src/providers/openai_compat.rs` — tool_calls 流式翻译状态机,多模型适配的直接参考;② `rust/crates/mock-anthropic-service/` + `rusty-claude-cli/tests/output_format_contract.rs` — mock 契约测试纪律;③ `rust/crates/runtime/src/compact.rs:129-166` — 压缩边界回退(避免切开 tool_use/tool_result 对)。**注意:其压缩是假的(不调模型)、工具严格串行、约 15% 代码是表演——只抄上面三样,别抄别的** |
| **codex** | `refs/codex` | codex fork。分层循环:`codex-rs/core/src/session/turn.rs`;工具注册:`core/src/tools/spec_plan.rs`;并行锁:`tools/parallel.rs`;扩展范式:`ext/worktree`;集成测试:`core/suite` |
| **claude-code(逆向版)** | `~/work/claude-code` | 主循环:`src/query.ts`(七层压缩流水线在 queryLoop 每轮开头);工具并发分批:`src/services/tools/toolOrchestration.ts`(partitionToolCalls);子 agent 递归:`packages/builtin-tools/src/tools/AgentTool/runAgent.ts`;重试:`src/services/api/withRetry.ts` |
