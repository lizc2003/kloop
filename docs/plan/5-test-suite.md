# Plan 5 — 测试基建 ✅ 已完成(2026-07-09)

> 历史记录。

## 任务

正规项目的充分测试:16 → 54 个,补掉四个覆盖盲区。

## 结果

提交 `87d88f4`:

- protocol:线格式契约(JSON 精确形状、is_error 省略规则、serde 往返)。
- provider:wiremock HTTP 契约测试——SSE 事件序列进、StreamEvent 断言出;delta 累积、usage 捕获、溢出映射、畸形输入回退、流中途断死。只被真实 API 验证过一次的行为从此永久可重复。
- core:六工具全执行路径、派发顺序/孤儿修补/执行中取消、history 锚点数学、compact 失败不动历史、agent 重试/耗尽/MaxRounds/预取消/子 agent 回路。
- 纪律:整对象断言优先;适配器行为变更必须先改契约测试;1 秒跑完、无网络无 key 无 flake。
