# Plan 1 — MVP:五个架构赌注 ✅ 已完成(2026-07-09)

> 历史记录。调研结论见 `refs/README.md`;原始交接文档在 git 历史(commit `91dfa0b` 前后的 HANDOFF.md,后演化为根目录 CLAUDE.md)。

## 任务

Rust 单 crate 最小 agent,验证 5 个架构赌注:append-only 历史 + 录入时 offload、tool_use 有无判续跑、按入参动态并发、task 子 agent 递归复用 run_turn、provider 适配缝(Anthropic SSE / OpenAI-compat / Mock)。

## 结果

- 全部落地并双重验证:mock 端到端 + 真实 API(OpenAI-compat 代理 × gpt-5.4-mini、Anthropic 代理 × claude-sonnet-5),offload 闭环、子 agent、edit_file 精准编辑均首试即过。提交 `91dfa0b`。
- 顺带拍板 P0:Edit 工具形态、Claude 主 + OpenAI-compat 副双轨、codex 上游降为可跟随性。
- 教训入档:递归 async Send 推断盲区(签名级类型擦除 + tokio::spawn)、offload 全局计数、子 agent 静音。
