# Plan 83 — TUI 粘贴文本保留多行

> 状态：✅ 已完成（2026-08-13；提交 SHA 以本文件所在提交为准）
>
> 基线：`c18e499`（Plan 82）
>
> 依赖：Plan 38、Plan 76、Plan 82

## 背景

在 TUI 中复制包含回车的两行文本时，部分终端把行尾作为 `CR` 或 `CRLF` 放进 bracketed paste payload。Composer 内部布局的逻辑换行是 LF；CR 被当成内容字符，终端渲染会覆盖前一行，出现类似 `abceff  ` 的结果。

## 契约

- bracketed paste 在唯一 TUI ingress 将 `CRLF` 与孤立 `CR` 规范化为 LF。
- LF、其他字符、前导/尾随空格和尾随换行保留；不做 trim。
- 规范化后的文本由 Composer 与 question editor 共用，paste atom、history、draft、steering 和 provider payload 都精确保留该 canonical 文本。
- 普通未包裹的 raw key burst 不在本计划范围；裸 CR 仍是 Enter/提交，Ctrl+J/Shift+Enter 才是键盘换行路径。
- 大 paste 的 `[Pasted #N: M chars]` 仍只是显示 projection；提交时展开规范化后的完整 payload。

## 实施

- `rust/crates/tui/src/app.rs` 新增 `canonicalize_paste_newlines`，由 `App::paste_text` 在 Composer/question 分流前统一调用。
- App unit 锁定 LF、CRLF、CR、空格和尾随 LF；提交断言使用 canonical LF。
- Composer 现有 paste atom 展开不变，通过 App ingress 回归验证 canonical payload。
- Unix PTY 增加 bracketed `ESC[200~abc\r\ndefESC[201~` + Enter 场景，断言 mock provider 收到 `abc\ndef`；不把裸 CR burst 混入该测试。

## 文档与验证

README、Plan 38 多行 composer 记录和 HANDOFF 必须说明：exact payload 指 canonicalized payload，provider/core 不负责终端 EOL 修复；Plan 38 的非-bracketed PasteBurst 不做结论保持不变。

从 `rust/` 执行：

```bash
cargo fmt --all -- --check
cargo test -p kloop-tui
cargo test -p kloop --test tui_pty -- --nocapture
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
git diff --check
```

完成记录（2026-08-13，Darwin arm64，macOS 26.4）

- `cargo fmt --all -- --check`：通过。
- `cargo test -p kloop-tui`：167 passed，0 failed。
- `cargo test -p kloop --test tui_pty -- --nocapture`：9 passed，0 failed；包含 bracketed CRLF paste 回归。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：通过。
- `cargo test --workspace`：1187 passed，0 failed（含 doc-tests）。
- `cargo run -p kloop -- --mock`：通过，mock run completed。
- `git diff --check`：通过。
- 未执行：物理 Terminal.app/iTerm2/tmux/Zellij 手工场景、Windows/WSL、真实 provider/远端 CI 和 Desktop 端到端；本次 Unix PTY 证据不外推到这些环境。

## 关键文件

- `rust/crates/tui/src/app.rs`
- `rust/crates/cli/tests/tui_pty.rs`
- `rust/README.md`
- `docs/plan/38-tui-cc-parity.md`
- `docs/plan/HANDOFF.md`

## 范围外

不改 Composer layout、provider/core 文本构造、普通 raw key burst 聚合或终端 multiplexer 配置。

## 最终提交

- 提交：`fix(plan83): preserve pasted multiline text`。
- SHA：以本文件所在提交为准。
- 不 push。
