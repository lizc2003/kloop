# Plan 2 — 压缩双防线(predictive / reactive)✅ 已完成(2026-07-09)

> 历史记录。cc 设计参考与 codex 预演记录见 `refs/README.md`;原始设计文档在 git 历史(P1-compaction-plan.md)。

## 任务

给 kloop 上 cc 式的上下文生存层:发请求前预判溢出(predictive)+ 溢出后压缩重试(reactive)。

## 结果

- token 记账(provider usage 锚点 + chars/4 增量)、predictive 阈值(增长 = min(输出上限,20k)+15k,含小窗口守卫)、reactive(OverflowError downcast,每 turn 单发)、压缩本体(模型写交接摘要 + 2k token 原文尾巴,边界不切 tool 对,失败不动历史)。提交 `2c3020c`。
- 真实验证:AGENT_CONTEXT_WINDOW=25000 下 predictive 真实会话触发两次,agent 无感继续。
- 附注:此设计先在 codex fork 上预演过一轮(分支留存,未合入),小窗口负阈值盲点即预演所抓。
