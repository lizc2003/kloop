# Plan 44 — Web 工具契约在 core/tools 显式落位

## 背景

`web_fetch` / `web_search` 的网络实现位于独立 `kloop-web` crate，并由 CLI 适配成
`ToolSource`。分层正确，但 agent 面向模型的工具名、描述和 JSON schema 也全部藏在
`kloop-web/src/lib.rs`，导致 `core/src/tools/` 目录无法直接看出 Web 工具属于 kloop 的
工具面。

## 范围与决定

- 保持 core 无网络依赖；HTTP、SSRF、HTML 转换和搜索 backend 继续归 `kloop-web`。
- 新增 `core/src/tools/web.rs`，归属 agent-facing 契约：工具名、描述、输入 schema。
- `kloop-web` 只暴露 fetch/search 操作和已配置 backend 名称，不再依赖 protocol 或构造
  `ToolDef`。
- CLI `web.rs` 使用 core 契约生成 defs，并把名字分发到 `kloop-web` 的具体操作，继续实现
  `ToolSource`。
- 不改变工具可见条件、输入输出、权限、并发、deferred tools 或网络行为。

## 完成标准

- core 契约测试覆盖 fetch-only 与 search backend 已启用两种定义集。
- 现有 Web 网络测试、CLI 配置/降级测试全绿。
- README、plan 14 与 HANDOFF 同步。
- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `cargo run -p kloop -- --mock`
- 一次提交。

## 完成记录 ✅（2026-07-24）

- 新增 `core/src/tools/web.rs`，集中 `web_fetch` / `web_search` 的工具名、描述和输入
  schema；测试锁定 fetch-only 与启用 Tavily 后的定义集。
- `kloop-web` 不再依赖 protocol，也不再构造 `ToolDef`；只暴露 fetch/search 网络操作
  与已配置 backend 名称。CLI adapter 使用 core 契约并分发到这两个操作。
- 工具可见条件、输入输出、权限、并发、deferred tools、SSRF 与搜索 backend 行为不变。
- README、plan 14 与 HANDOFF 已同步。
- 验证：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo test --workspace`、`cargo run -p kloop -- --mock` 全绿。全量测试首次在外层
  OS sandbox 中仅两个 macOS seatbelt 自测因嵌套沙箱失败，按项目验证方式移除外层
  sandbox 后重跑全绿。
- 提交：本次（plan 44，见 git log）。
