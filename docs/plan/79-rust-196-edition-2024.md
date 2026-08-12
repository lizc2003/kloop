# Plan 79 — Rust 1.96.1 / Edition 2024 迁移

> 状态：✅ 已完成（2026-08-12；Plan 79 提交，SHA 以本文件所在提交为准）
>
> 基线：`799c55e`（Plan 78 完成提交）
>
> 依赖：Plan 78


## 实施记录（2026-08-12）

- workspace 实际为 10 个 package；根 manifest 不是 package。edition 2024、MSRV 1.96、codemode workspace 继承和 Rust 1.96.1 toolchain pin 已落地。
- Rust 1.96.1 host compatibility Clippy 已成功运行。真实命中按 compiler output 审计；环境读取改为 closure 注入，Windows HANDLE 转移保留 unsafe 契约并补 inner block，tail-expression 资源路径采用显式局部/循环/match。
- Windows cross-target 在本机因缺少可用 Windows std/sysroot 与 MSVC C 交叉编译环境失败；不能把该失败写成 Windows 源码兼容通过。Windows native CI/工作站仍是最终 HANDLE 生命周期证据。
- CI stable matrix 使用显式 `+stable`，MSRV job 使用 `+1.96.1` 并记录版本；README 已同步本地 pin、edition 与 CI policy。
- `Cargo.lock` 未发生变化；metadata 验收为 10 packages，`git diff --check`、host fmt/clippy/workspace tests 与 mock smoke 均通过。
- corpus-only verifier 的既有静态证据 `kloop-bash-tests` 固定覆盖到第 2504 行；rustfmt 一度让该文件缩至 2503 行，已在对应测试块保留说明行以维持只读 corpus locator，随后 verifier 通过。

## 迁移教训

`tail-expr-drop-order` 只表示 Rust 2021/2024 的相对析构点变化，不自动代表业务 bug。named local 会选择一个明确 drop 点，不宣称机械复刻所有 2021 顺序；必须按 stream/body、future、process、lease、guard、writer、validator 的实际生命周期决定，并在必要处使用显式 scope/drop 与 witness。Rust 2024 的 `std::env::set_var/remove_var` 也暴露了全局环境测试竞态，优先注入读取 closure 而不是继续包 unsafe。

<!-- Original implementation checklist follows for historical context. -->

---

*The remainder of this document is the original Plan 79 checklist and verification commands.*

---

## Original checklist



```bash
cargo +1.96.1 clippy --locked --workspace --all-targets --all-features \
  --message-format=short -- -W rust-2024-compatibility

cargo +1.96.1 clippy --locked --target x86_64-pc-windows-msvc \
  --workspace --all-targets --all-features --message-format=short \
  -- -W rust-2024-compatibility
```

- 保存完整输出，按 lint code/source location 去重并分类。
- 不运行 `cargo fix --edition`，不添加全局 allow，不凭 grep 数量机械改代码。
- 对每条 `tail-expr-drop-order` / `if-let-rescope` 诊断记录旧/新析构顺序及其资源影响；使用显式 `let`、block 或穷尽 `match` 保持正确生命周期。

已知静态候选（仍以 compiler diagnostics 为准）：

- `kloop/crates/cli/src/mcp.rs:1223,1236`：测试直接 `set_var/remove_var`，Rust 2024 要求 unsafe，且当前无串行保护。
- `kloop/crates/core/src/tools/fs/windows.rs:400-402`：`unsafe fn file_from_handle` 内调用 `File::from_raw_handle` 缺 inner unsafe block。
- keyword/static-mut/unsafe attrs/裸 extern 预审无典型命中。
- RPIT 低风险候选：`cli/src/provider_config.rs:510`、`core/src/skills.rs:254-257`、`tui/src/text_layout.rs:123`；只在诊断命中时精确 capture/lifetime。
- match candidate：`core/src/agent/tests.rs:263` 的 `EndReason::Error(ref error)`；只按编译器要求修改。

### 2. 先在 edition 2021 下修复兼容点

- `kloop/crates/cli/src/mcp.rs`：给 `http_headers_for` 注入环境读取 closure；生产传 `std::env::var(...).ok()`，测试传内存 map/closure。复用 `kloop/crates/cli/src/provider_config.rs::nonempty_env` 和测试 `env(...)` 的模式。删除测试中的 `set_var/remove_var`，避免全局 env 竞态；不要默认改成“unsafe + 全局锁”。
- `kloop/crates/core/src/tools/fs/windows.rs::file_from_handle`：仅给 `File::from_raw_handle` 增加最小 inner `unsafe` block，并紧邻说明有效 HANDLE 和唯一 ownership transfer 前提。
- 对真实命中的 tail/if-let/RPIT/match diagnostics 做最小语义修复；不做预防性全仓重写。

### 3. 按资源组审计生命周期并复用测试

只对真实 diagnostic 所在路径审计；现有回归不能证明释放顺序时才新增最小 witness。

- MCP：`crates/mcp/src/{lib.rs,http.rs,oauth.rs}` 的 child/reader/writer/pending、HTTP body/SSE、session reinit、OAuth refresh；复用 `crates/mcp/tests/client.rs` 的 EOF/concurrency 与现有 HTTP/OAuth tests。
- Provider：`crates/provider/src/stream.rs` 和三 adapter 的 producer abort/body/SSE；复用 `dropping_provider_stream_aborts_its_producer` 与 adapter contract tests。
- Process/background：`crates/core/src/tools/bash.rs`、`process_tree/*`、`background_executions.rs` 的 whole-tree terminate、root wait、dual-pipe drain、terminal exactly-once 与 Drop；复用 timeout/cancel/residual child/Unix+Windows shutdown/registry race tests。
- Server/locks：`crates/server/src/lib.rs` 的 EOF cancel→pending clear→writer drain，以及 `file_state`、inbox、permissions、task、worktree 等实际命中的 guard 尾表达式。

### 4. 切换 workspace 配置

- `kloop/Cargo.toml`：保持 `resolver = "2"`，将 `[workspace.package]` 改为：

```toml
edition = "2024"
rust-version = "1.96"
```

- `kloop/crates/codemode/Cargo.toml`：把显式 `edition = "2021"` 改成 `edition.workspace = true`。
- 新增 `kloop/rust-toolchain.toml`：

```toml
[toolchain]
channel = "1.96.1"
components = ["clippy", "rustfmt"]
```

- 不照搬 Codex 的 `rust-src`。toolchain pin 是本地开发默认；Cargo `rust-version` 才是兼容下限。
- 不运行 `cargo update`。edition/MSRV/toolchain 不应改变 dependency resolution；目标是 `kloop/Cargo.lock` 无 diff。

### 5. CI 与文档

- `.github/workflows/ci.yml`：保留 stable macOS/Linux/Windows 矩阵、Windows focused tests、workspace test、mock 和 corpus verifier。
- 因 `kloop/rust-toolchain.toml` 会成为目录默认，stable job 的所有 Rust/Cargo 命令显式使用 `+stable`，防止所谓 stable 矩阵实际被 pin 到 1.96.1。
- MSRV job 从 1.88.0 改为 1.96.1，先记录 `rustc +1.96.1 -Vv`、`cargo +1.96.1 -V`，再运行 locked all-target/all-feature check。
- 完成本文件；更新 `kloop/README.md` 的 Rust 1.96、edition 2024、toolchain pin 与 stable CI 说明。
- 更新 `docs/plan/HANDOFF.md`，记录 env 注入、inner unsafe、drop-order 按诊断审计的教训。
- `docs/capability-report.md` 只记 engineering maintenance，不改变 parity matrix/captures/pair/bridge 或平台能力结论。
- 不改历史 Plan78/Plan67 原文；本文件说明后继关系即可。

## 验证

从 `kloop/` 运行，dependency-resolution 命令使用 `--locked`：

```bash
# Toolchain / metadata
rustup show active-toolchain
rustc --version
cargo +1.96.1 metadata --locked --no-deps --format-version 1
cargo +1.96.1 check --locked --workspace --all-targets --all-features
cargo +stable check --locked --workspace --all-targets --all-features

# 最终 2024 compatibility，host + Windows target
cargo +1.96.1 clippy --locked --workspace --all-targets --all-features \
  -- -W rust-2024-compatibility -D warnings
cargo +1.96.1 clippy --locked --target x86_64-pc-windows-msvc \
  --workspace --all-targets --all-features \
  -- -W rust-2024-compatibility -D warnings

# Focused lifecycle
cargo +1.96.1 test --locked -p kloop-mcp
cargo +1.96.1 test --locked -p kloop-provider
cargo +1.96.1 test --locked -p kloop-core process_tree
cargo +1.96.1 test --locked -p kloop-core tools::bash::tests
cargo +1.96.1 test --locked -p kloop-server
cargo +1.96.1 test --locked -p kloop-tui
cargo +1.96.1 test --locked -p kloop --test tui_pty -- --nocapture  # Unix

# 全量门
cargo +1.96.1 fmt --all -- --check
cargo +1.96.1 clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo +1.96.1 test --locked --workspace
cargo +1.96.1 run --locked -p kloop -- --mock
python3 -B ../refs/claude-code-2.1.220/verify.py --corpus-only
```

还需验证：

- 实际 metadata 验收应为 **10 个** workspace package，全部 edition 2024 / rust-version 1.96；根不是 package。还需核对 `git diff -- kloop/Cargo.lock` 为空、仓库根 `git diff --check`，并如实区分本机 macOS、Windows cross-target、远端 CI/Windows native 是否真实执行。

```text
chore(plan79): migrate to Rust 1.96 and edition 2024

Co-Authored-By: Claude <noreply@anthropic.com>
```

不使用 `git add -A`，不 push。

## 关键文件

- 配置：`kloop/Cargo.toml`、`kloop/rust-toolchain.toml`、`kloop/crates/codemode/Cargo.toml`、`.github/workflows/ci.yml`
- 已知兼容点：`kloop/crates/cli/src/mcp.rs`、`kloop/crates/cli/src/provider_config.rs`、`kloop/crates/core/src/tools/fs/windows.rs`
- 生命周期路径：实际 lint 命中的 `kloop/crates/{mcp,provider,core,server}/src/**` 与现有 tests
- 文档：本文件、`docs/plan/HANDOFF.md`、`docs/capability-report.md`、`kloop/README.md`
