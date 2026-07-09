# Plan 8 — 权限系统 ✅(abe3510)

> 一个会话完成。开工前先读 docs/plan/HANDOFF.md。参考:cc 的 canUseTool 回调形态、codex 的 approvals;按 kloop 体量做最小版。

**落地记录**:两个开工时定的点——询问缝选了独立 `Approver` trait(类型擦除 future,与 execute_tool 同款,零新依赖);allowlist 走 `AGENT_ALLOW` 环境变量(裸工具名 / `bash(前缀 *)`,尾 `*` 通配、无 `*` 精确,链式命令每段都须只读或命中)。额外收紧:AllowSession 对 bash 按每段首 token 缓存,`cargo build && rm x` 不会搭 `cargo` 的车。fmt/clippy/test 全绿(82 个),--mock 验证过;**真实 API 交互验证(询问出现、y 放行、n 拒绝后模型调整)待用户给 key 后补做**。

## 目标

工具执行前的用户批准机制。现状是 bash/write_file/edit_file 裸跑,这是拿 kloop 干真活前的最后一道坎。

## 设计要点

- **判定分层**:先规则后询问。
  1. 只读工具(read_file/read_offloaded)与只读 bash(复用 tools.rs 的 `bash_is_readonly`——并发安全分类和权限判定本来就是同一个问题的两面)→ 直接放行。
  2. allowlist 命中 → 放行(配置文件 `.kloop/config.toml` 或环境变量,形如 `allow = ["bash(cargo *)", "write_file"]`,通配格式开工时定,别过度设计)。
  3. 其余 → 询问用户。
- **询问的缝**:扩展 Ui trait 加 `async fn confirm(&self, description: &str) -> Decision`(Allow / AllowSession / Deny)?注意 Ui 目前是同步 trait、Send+Sync——改成 async trait 需要 async-trait 依赖或手写 poll;备选:confirm 走独立的 `Approver` trait,避免污染流式输出的 Ui。**开工时二选一,倾向独立 trait**。
- **AllowSession** 语义:本会话内同签名(工具名 + 规整后的命令首 token)不再问。
- **拒绝**的产物:is_error 的 tool_result("user denied"),模型可以换路走——不是 turn 终止。
- **子 agent**:继承父的批准缓存,询问照常走同一个 Approver(深度信息带进 description)。
- CLI:REPL 里 y/n/a(allow session)三键应答;`--yolo` 全放行(开发用)。

## 测试

规则分层判定(只读放行/allowlist/询问);Deny → is_error 结果且 turn 继续;AllowSession 缓存生效;mock Approver 记录询问次数。

## 完成标准

fmt/clippy/test 全绿;真实跑:让模型写文件,确认询问出现、y 放行、n 后模型收到拒绝并调整;README 更新。
