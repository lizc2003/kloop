# Plan 7 — 会话持久化(rollout / resume)

> 一个会话完成。开工前先读 docs/plan/HANDOFF.md。参考:codex `codex-rs/rollout` crate 的思路(JSONL 逐条追加),但按 kloop 体量做最小版。

## 目标

会话历史落盘 + 重启恢复:REPL 退出再进,能接着上次的对话继续。

## 设计要点

- **格式**:JSONL,一行一个 `Message`(kloop-protocol 已 Serialize/Deserialize,直接用线格式)。文件 `.kloop/sessions/{session_id}.jsonl`,session_id 用时间戳(本项目无 rand 依赖,沿用这一约束)。
- **写入时机**:History::record 后追加一行(append-only 历史天然适配 append-only 文件);压缩(replace_all)是唯一重写点——策略:压缩后整文件重写,或写入一条压缩标记 + 后续行(倾向后者:保留完整审计,恢复时按标记重放;二选一开工前问用户)。
- **恢复语义**(HANDOFF.md 教训 5 相关):
  - usage 锚点不落盘,恢复后从零估算,首次采样自动重新锚定——不需要持久化。
  - offload 指针照常有效(off-NNNN.txt 还在磁盘);但进程级 offload 计数器恢复后从 1 重新数会撞旧文件——恢复时扫 offload 目录取 max+1 初始化计数器。
  - 恢复的历史必须过配对合法性检查(每个 tool_use 有 tool_result;不合法就地补 is_error 结果,复用孤儿修补逻辑)。
- **CLI**:`kloop --resume`(接最近一次)/ `--resume <id>`;`kloop --list-sessions`。默认行为不变(新会话)。

## 归属

新模块 `crates/core/src/rollout.rs`(读写 + 恢复校验),CLI 侧只做参数与选择。不开新 crate——体量不够。

## 测试

- 往返:record 若干 → 落盘 → 读回 → messages() 逐条相等。
- 压缩后恢复:压缩标记语义正确。
- 恢复时孤儿修补:构造缺 tool_result 的文件,恢复后历史合法。
- offload 计数器恢复不撞旧文件。
- 损坏行(半行 JSON)容错:跳过或截断到最后完整行,不 panic。

## 完成标准

fmt/clippy/test 全绿;真实跑一次:对话 → exit → --resume → 模型能引用上次内容(用 mock 或真 key 验证);README 更新。
