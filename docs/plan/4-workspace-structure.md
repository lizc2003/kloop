# Plan 4 — 工程化结构重组 ✅ 已完成(2026-07-09)

> 历史记录。

## 任务

MVP 平铺单 crate → 正规 Cargo workspace,对照三个参考库的边界收敛点。

## 结果

四 crate 严格单向依赖链(提交 `ce0385f` + `29fb5bf`):

`kloop-protocol`(零依赖线格式叶子)← `kloop-provider`(适配缝,独占 reqwest)← `kloop-core`(agent 本体,无网络)← `kloop`(cli)。

刻意不拆:tools 留 core(task 工具与 run_turn 类型互嵌);features/analytics/utils 等参考库有的 crate 因 kloop 尚无对应内容不建空壳。git mv 全程保留历史,行为零变化。
