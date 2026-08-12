# Plan 80 — plain Ctrl+C 退出语义

> 状态：✅ 已完成（2026-08-12；提交 SHA 以本文件所在提交为准）
>
> 基线：`9063b9b`（Plan 79 完成提交）
>
> 依赖：Plan 38

## 背景

`kloop --plain` 的启动提示把 Ctrl+C 描述为只中断当前 turn，并把 Ctrl+D 当作显式退出快捷键。实际实现为每次 turn/命令临时注册一个 SIGINT watcher：运行中首个 Ctrl+C 只取消当前操作，idle 没有应用级退出分支；Tokio 首次安装进程级 handler 后又不会恢复默认 SIGINT 行为，因此提示和实际生命周期都不够可靠。

plain 是传统 line-based REPL，不需要复制 TUI 的 raw-key composer。按常规终端语义，空行 Ctrl+D/管道耗尽仍是 EOF 并结束输入，但启动提示无需广告 Ctrl+D；Ctrl+C 则成为 plain 的明确单拍退出方式。TUI 保持 Esc 中断、Ctrl+C 两拍退出、Ctrl+D inert，不改。

## 契约

1. `--plain` idle 时一次 Ctrl+C 干净退出。
2. turn、慢 slash command 或 scheduled delivery 运行中按一次 Ctrl+C：先取消当前 `CancellationToken`，等待既有 abort/history patch 路径收尾，再退出 REPL。
3. `exit`、`/exit` 与输入 EOF 继续退出；Ctrl+D 不被特殊屏蔽，也不写进启动提示。
4. plain 继续使用 canonical line input；不增加 raw mode、Esc、运行中 steering 或第二个 stdin reader。
5. headless 仍使用单次操作的 Ctrl+C cancellation，不随 plain REPL 改为“取消后退出”（它本来只有一次操作）。

## 实施

- `kloop/crates/cli/src/main.rs`
  - plain session 启动时创建一个覆盖整个 REPL 生命周期的 Tokio SIGINT stream；stdin 改由唯一 request-driven dedicated blocking thread 读行并送 channel，idle event loop 同时等待 stdin、Inbox 和 SIGINT，turn 内 interaction 不与预读线程争 stdin。
  - 抽取 `run_plain_operation`：同时 poll 当前业务 future 和 SIGINT；SIGINT 到达后 cancel token、等待业务 future 完成，并返回 exit intent。
  - turn、slash command、scheduled delivery 都通过该 helper，去掉各自短命 watcher；运行中 Ctrl+C 的终态文案明确 history 已修补并正在退出。
  - 启动提示改为只广告 `exit` 和 Ctrl+C，不提 Ctrl+D。
- `kloop/crates/cli/tests/plain_pty.rs` 与 `tui_pty_support/mod.rs`
  - 泛化现有 hermetic PTY harness 以传入 `--plain`。
  - 真 binary 覆盖 idle 单次 Ctrl+C 退出、运行中 Ctrl+C cancel→history patch→退出、banner 不再广告 Ctrl+D。
- `kloop/README.md`、`docs/plan/HANDOFF.md`
  - 同步 plain 与 TUI 的有意差异，以及 Ctrl+D 是 canonical EOF 而非应用快捷键的分层。

## 验证

从 `kloop/` 运行：

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace
cargo test --locked -p kloop --test plain_pty -- --nocapture
cargo test --locked -p kloop --test tui_pty -- --nocapture
cargo run --locked -p kloop -- --mock
python3 -B ../refs/claude-code-2.1.220/verify.py --corpus-only
git diff --check
```

未增加 dependency；`Cargo.lock` 保持无变化。一次提交，不 push。
