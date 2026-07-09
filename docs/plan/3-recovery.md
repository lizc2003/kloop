# Plan 3 — 恢复语义(截断续跑 + fallback 模型)✅ 已完成(2026-07-09)

> 历史记录。

## 任务

补齐 cc 恢复层剩余两项:输出截断续跑、fallback 模型(孤儿修补与 EndReason 枚举 MVP 已有)。

## 结果

- 截断续跑:stop_reason=max_tokens/length 且无 tool_use 时注入续跑提示,每 turn 限 3 次(stop_reason 的唯一合法用途——判"结束体面与否",不判续跑)。
- fallback:主模型 3 次重试耗尽后切 AGENT_FALLBACK_MODEL,每 turn 一次。
- Mock 扩展 Truncated/Error 脚本轮次。提交 `089cf63`。
- 真实验证:让模型数数到 2500,在 2284 被 8192 输出上限切断,续跑后从 2285 精确接上。
