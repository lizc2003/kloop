# Plan 7 — 会话持久化(rollout / resume)✅ 已完成(2026-07-09)

> 历史记录。参考:codex `codex-rs/rollout` crate 的思路(JSONL 逐条追加),按 kloop 体量做了最小版。

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

## 结果

提交 `47f165a`:

- 压缩落盘的"二选一"按用户指示看了参考项目定案:codex 是压缩标记行 + 继续追加,标记内嵌完整替换历史(`CompactedItem.replacement_history`),重放读到标记整体替换(`rollout/src/list.rs:1380`)——照此实现,文件保持 append-only 可审计,恢复不需要重跑压缩逻辑。
- `crates/core/src/rollout.rs`:行格式 `{"type":"message",...}` / `{"type":"compacted","replacement":[...]}`;load 截断到最后完整行;恢复后跑孤儿修补(复用 tools.rs 的 interrupted)。History 挂可选 Rollout 写透(record/replace_all),写失败降级纯内存不打断会话;`History::resume` 直接装入不重写文件,并 fetch_max 同步 offload 计数器。子 agent 历史不持久化。
- CLI:`--resume [id]` / `--list-sessions`;session id 用 UTC 时间戳 `YYYYMMDD-HHMMSS`(手写 civil_from_days,不引 chrono/rand),同秒冲突加序号;`--resume` 无 id 按 mtime 取最近。
- 测试 54 → 65:往返、压缩标记重放、孤儿修补(全缺 + 部分缺就地补齐)、坏尾行截断、计数器不撞旧文件、写透一致性、跨"重启"的完整 persist → resume 回合(mock)。
- 实跑验证:`--mock` → `--mock --resume` × 2 + `--list-sessions`;其中一次进程被 SIGPIPE 中途杀掉,恢复保住完整前缀并补齐配对——意外验证了崩溃恢复路径;三次运行 offload 文件 off-0001..0006 无覆盖。
