# Plan 8 — 权限系统 ✅(abe3510)

> 一个会话完成。开工前先读 docs/plan/HANDOFF.md。参考:cc 的 canUseTool 回调形态、codex 的 approvals;按 kloop 体量做最小版。

**落地记录(两轮)**:第一轮按"最小版"做了 规则分层 + AGENT_ALLOW + 会话缓存 + Approver trait(abe3510);用户要求"按最优的来"后深调 cc/codex 权限系统(结论沉淀 `refs/README.md` 权限对比节)重做:cc 管线(deny → 安全检查[bypass 免疫] → ask 规则 → bypass → 只读 → acceptEdits → allow → 缓存 → 询问)、bash 判定移植 codex tree-sitter-bash word-only 遍历(新 `core/src/shell.rs`,替换掉一天内被抓出三个注入洞的手写字符串拆分)、只读分类器选项级审查、独立危险分类器、deny 匹配剥 wrapper、路径 glob 规则 + 敏感路径清单、`.kloop/config.toml` + 三个 env 的规则来源、y/a/p/n(a 两词前缀会话缓存,p 持久化建议规则回 config)、`--accept-edits`/`--yolo`(bypass 语义)。fmt/clippy/test 全绿(95 个),--mock + config 冒烟验证过。

**真实 API 验证(2026-07-09,expect 驱动 PTY,双轨)**:claude 系(claude-sonnet-5)与 openai-compat(gpt-5.4)各自跑通——询问出现且描述正确;y 放行(文件内容精确);n 后模型收到拒绝并按指令回 CANNOT-WRITE、文件未创建;只读 `ls` 不弹询问;`p` 落盘 `write_file(notes/**)` 进 config.toml 且新会话即时生效;`rm -rf` 带 `[destructive]` 标签强制询问;`AGENT_DENY='bash(rm *)'` 不弹询问直接拒、模型改道回 BLOCKED、目标目录完好。expect 脚本在会话 scratchpad(未入库);key 未落任何提交文件。

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
