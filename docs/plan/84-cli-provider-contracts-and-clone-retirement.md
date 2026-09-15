# Plan 84 — 提炼 claw-code 契约并退休本地参考克隆

> 状态：✅ 已完成（2026-08-13；提交 SHA 以本文件所在提交为准）
>
> 基线：`2c0304d`
>
> 依赖：Plan 65、Plan 83；参考快照 `claw-code@b71afddae100ced324457337925a694686b8fef2`

## 背景

`refs/claw-code` 是被 `.gitignore` 排除的独立只读克隆，不能作为 kloop 的架构底座。本计划只吸收四项局部价值：真实 CLI 输出契约、provider 请求捕获、OpenAI-compatible `tool_calls: null` 兼容边界，以及 compaction 不切开 `tool_use`/`tool_result` 配对的回归测试纪律。完成实现、测试和文档后，最后精确删除本地克隆；它未被主仓库跟踪，物理删除不形成 Git 删除 diff。

## 契约与范围

### 本次吸收

- `--mock --headless` 文本 stdout 只输出最终答案，进度/工具说明在 stderr。
- `--mock --headless --json` stdout 每行都是合法 NDJSON，沿现有 native event projection，stderr 不混入事件。
- `--help` 与 `--list-sessions` 在隔离坏配置/无 key 环境中走本地 fast path。
- OpenAI Chat delta 的 `tool_calls` 缺失或 JSON null 都是 no-op；非 null 非 array 和其余非法结构继续 protocol error。
- compaction 边界测试必须证明完整的 assistant ToolUse / user ToolResult pair 被保留。

### 非目标

不复制 claw runtime、全局 registry、内存 Task/Worker registry、同步收集式 loop、snapshot persistence、粗粒度权限、ToolSearch 字符串打分或未接通 ACP/remote/team/Fleet。此次不新增 local OpenAI placeholder auth、MCP failure phase、doctor 或第二套 ToolSpec 生命周期；不创建新的 mock crate/scenario harness；不批量改写历史计划中的事实引用。

## 实施

1. 在 `crates/cli/tests/headless_contract.rs` 增加真实 `CARGO_BIN_EXE_kloop` subprocess 的 text/NDJSON/fast-path 测试，复用隔离 HOME/cwd 和现有 `--mock` seam。
2. 复用 `crates/provider/tests/anthropic.rs` 的 wiremock 体系和既有 request capture，确认真实 Anthropic request body/header 契约；不引入 claw mock 服务。
3. 在 `crates/provider/src/openai.rs` 的 `apply_choice_payload` 将 `Value::Null` 与缺失字段合并处理，在 `crates/provider/tests/openai.rs` 覆盖 null 成功和 object/string/number 失败。
4. 只在 `crates/core/src/compact.rs` 强化 oversized、多 ToolUse/ToolResult 和 compaction replacement 的边界测试，不改生产算法。
5. 更新根 `CLAUDE.md`、`refs/README.md`、`rust/README.md` 与 `docs/plan/HANDOFF.md`，将 claw 改为固定 commit 的历史调研来源；保留 parity fixture 的 historical provenance。

## 验证结果

以下均通过：

- `cargo test -p kloop --test headless_contract`：3 passed。
- `cargo test -p kloop --test mock_hermetic`：1 passed。
- `cargo test -p kloop-provider --test anthropic`：18 passed。
- `cargo test -p kloop-provider --test openai`：14 passed。
- `cargo test -p kloop-core compact::tests`：13 passed。
- `cargo fmt --all -- --check`：通过。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：通过。
- `cargo test --workspace`：全 workspace 通过；2 个真实凭据 evaluator 按既有规则 ignored。
- `cargo run -p kloop -- --mock`、`--mock --headless`、`--mock --headless --json`：通过；JSON smoke 解析 46 行 NDJSON。
- `git diff --check`：通过。

## 删除结果

删除前检查：目标为精确 `<repo>/refs/claw-code`；HEAD 为 `b71afddae100ced324457337925a694686b8fef2`；分支 `main` 跟踪 `origin/main`；`status --short` 干净；ahead/behind 为 `0/0`；无 local-only branch commit。随后只执行 `rm -rf -- <repo>/refs/claw-code`，未使用 `git clean`。

删除后：目标路径不存在；`git ls-files -- refs/claw-code` 为 0；`refs/codewhale` 和 `refs/claude-code-2.1.220` 保留；主仓库只包含本计划的代码、测试和文档变更。

## 完成记录

本计划未改变 kloop 的 provider rail、认证、MCP 生命周期、ToolSource ownership 或 compaction 生产算法；只增加兼容/契约证据。一次提交收口，不 push。
