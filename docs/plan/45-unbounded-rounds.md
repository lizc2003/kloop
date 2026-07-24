# Plan 45 — 默认取消 sampling round 上限

## 背景

主 agent 的 `Config.max_rounds = 30` 与 `task` 子 agent 的默认/最大 15 轮经常在工作尚未完成时触发 `stopped`。用户要求调研 Codex 后按其更合理的做法处理，而不是只把常数稍微调大。

## 回源结论（2026-07-24）

- Codex/Codex 的 turn sampling 主循环是无计数 `loop`：`codex-rs/core/src/session/turn.rs:237-238`。
- 工具执行后有 pending input 就继续 sampling，模型不再请求工具才结束：同文件 `:438-468`；没有 sampling round cap。
- 子 agent 启动独立 child thread，复用同一 turn 机制：`core/src/tools/handlers/multi_agents_v2/spawn.rs:193-210`，也没有 per-agent round cap。
- 它限制的是不同风险维度：默认子 agent 深度 1、线程数 6（`core/src/config/mod.rs:237,298`），不是用执行轮数截断正常工作。

## 决定

- `Config.max_rounds` 改为 `Option<usize>`；`None` 表示不限轮数。
- interactive、plain、TUI、server、headless 默认均为 `None`，直到模型自然完成、用户中断或错误结束。
- headless `--max-rounds N` 保留为脚本显式 runaway guardrail。
- `task.max_rounds` 保留为调用方显式 guardrail；省略时不限轮数，不再默认/封顶 15。
- `context: fork` skill 同样不再隐式套 15 轮上限。
- `EndReason::MaxRounds` 与各前端/wire 状态保留，服务显式 guardrail；Ctrl-C、`stop_agent`、provider/context 错误语义不变。

## 测试

- 主循环无上限时跨过旧默认 30 轮后自然完成。
- task 省略上限时跨过旧默认 15 轮后自然完成。
- 显式 `max_rounds` 仍准确截断且 history 保持 tool_use/tool_result 合法。
- fmt、clippy、workspace test、mock 全绿。

## 完成记录 ✅（2026-07-24）

- `Config.max_rounds` 已改为可选 guardrail；CLI 构造默认 `None`，headless flag 显式写入 `Some(N)`。
- agent loop 改为无界 `loop` + 可选轮限检查；轮数统计和显式 `MaxRounds` 终态保留。
- task/fork skill 默认不限轮数；task schema 改为 minimum 1、明确“省略即不限”，移除旧 15 上限，运行时也拒绝 0/非整数旁路。
- README 与 HANDOFF 已同步。
- 验证：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`、`cargo run -p kloop -- --mock` 全绿。全量测试首次在外层 OS sandbox 中仅两个 macOS seatbelt 自测因嵌套沙箱失败，按项目验证方式移除外层 sandbox 后重跑全绿。
- 提交：本次（plan 45，见 git log）。
