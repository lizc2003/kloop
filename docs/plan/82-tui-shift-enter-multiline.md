# Plan 82 — TUI Shift+Enter 多行输入闭环

> 状态：✅ 已完成（2026-08-13；提交 SHA 以本文件所在提交为准）
>
> 基线：`0465b68`（Plan 81）
>
> 依赖：Plan 38、Plan 76、Plan 78

## Context

TUI 的 `Composer` 已有完整多行文档模型：`ComposerLayout` 统一硬换行、软折行、Unicode grapheme 光标、八行窗口和动态高度；`App::on_key` 也已把 `Shift+Enter`、`Alt+Enter` 与 `Ctrl+J` 路由到 `Composer::insert_newline()`，裸 `Enter` 负责提交或 steering。

缺口在真实终端输入层。传统 Unix 终端常把 `Shift+Enter` 和 `Enter` 都编码为 `\r`，而 kloop 未请求 Kitty/CSI-u progressive keyboard enhancement，故用户实际按 `Shift+Enter` 仍可能被 Crossterm 解析为普通 Enter。另一个状态机缺口是 completion popup 先捕获所有 Enter，菜单打开时 Shift+Enter 会接受候选而非换行。

本计划不重写 Composer、renderer 或输入线程；只闭合 keyboard protocol、按键优先级及分层回归。

## 产品契约

- Composer 可编辑时，`Shift+Enter` 插入 `\n`，不提交；`Alt+Enter` 和 `Ctrl+J` 保留为换行后备；裸 `Enter` 仍在 idle 提交，在 running turn 中 steering。
- slash/file completion popup 打开时，Shift/Alt+Enter 与 Ctrl+J 仍编辑 Composer；裸 Enter/Tab 仍接受候选。
- approval/question 与 rewind picker 是 modal surface，继续优先独占键盘，不让换行键修改隐藏 Composer。
- Unix 上 best-effort 请求 `DISAMBIGUATE_ESCAPE_CODES`。不支持该协议、或 multiplexer/SSH 未透传 CSI-u 时，应用无法从裸 `\r` 反推 Shift；不得把普通 Enter 猜成换行，Ctrl+J 仍是可靠后备。
- Windows 保持 Crossterm native console modifier 路径；本计划不增加 Windows ANSI keyboard protocol 或 ConPTY 测试声明。

## 实施

### 1. Terminal keyboard enhancement lifecycle

修改 `rust/crates/tui/src/lib.rs`：

- raw mode 与 bracketed paste 建立后、inline viewport CPR 和输入 poll thread 启动前，在 Unix 上写入 `PushKeyboardEnhancementFlags(DISAMBIGUATE_ESCAPE_CODES)`。
- 只启用 disambiguation，不启用 event types、alternate keys 或 all-keys，避免引入 release/repeat 或普通字符 keycode 变化。
- 不调用 `supports_keyboard_enhancement()`：Crossterm 的检测会读 stdin，在不支持的终端上可等待约 2 秒；协议允许未知终端忽略 push，直接 best-effort 发送更符合当前 CPR/poll 锁序。
- 使用 session-owned `KeyboardEnhancementGuard` 和 `TerminalModes`，只有 push 写成功才记录进程拥有的一层；正常退出、setup failure 与 panic 共用幂等 restore，通过 guard state 最多发送一次对应 Pop。panic hook 只发送 `AgentEvent::Quit`，由唯一 UI owner 执行完整 restore，不与 draw 并发关闭 terminal modes。
- Pop 先于 bracketed-paste disable；随后恢复 cursor、换行并关闭 raw mode。
- 保留 `poll(200ms)+read()` 输入线程，不改回 `EventStream`。

### 2. App key priority

修改 `rust/crates/tui/src/app.rs`：

- 抽出纯 `is_composer_newline_key`，沿用 `KeyModifiers::contains`。
- 顺序固定为：全局键 → modal interaction → rewind picker → composer newline → completion popup → 普通编辑/裸 Enter。
- 换行后继续调用既有 `after_edit()`，让 completion target 在新文档上重算或关闭。
- 不修改 `Composer::insert_newline()`、`ComposerLayout`、`text_layout.rs` 或 renderer。

### 3. 回归测试

- App unit：Shift+Enter、Alt+Enter、Ctrl+J 均只插入一个换行，不创建 User cell、不进入 running；随后裸 Enter 精确提交 `first\nsecond`。
- App completion：popup 打开时 Shift+Enter 插入换行并关闭已失效 target；既有裸 Enter 接受候选回归继续通过。
- Unix PTY lifecycle：启动 raw trace 含 CSI `>1u`，退出含 CSI `<1u`；Pop 在 bracketed-paste disable 之前，no-alternate-screen、cursor show、final CRLF 与非 emergency kill 不变。
- Unix PTY binary closure：向真实 binary 输入 `first` + CSI-u `ESC[13;2u` + `second` + bare CR；loopback provider 收到逐字节 `first\nsecond`。

## 文档

- README 说明 Unix keyboard enhancement、completion popup 中的换行语义，以及旧终端/multiplexer 的降级边界。
- HANDOFF 更新当前完成范围、TUI 契约与教训；PTY 只证明 emitted lifecycle bytes、Crossterm CSI-u parsing 和最终 payload，不冒充所有物理 terminal/tmux/SSH/Windows 已实测。

## 关键文件

- `rust/crates/tui/src/lib.rs`
- `rust/crates/tui/src/app.rs`
- `rust/crates/cli/tests/tui_pty.rs`
- `rust/README.md`
- `docs/plan/HANDOFF.md`

## 验证

本次在 macOS Darwin 25.4.0（non-TTY shell）执行：

```text
cargo fmt --all -- --check                         ✅
cargo test -p kloop-tui                            ✅ 166 passed
cargo test -p kloop --test tui_pty -- --nocapture  ✅ 8 passed（2 support tests + 6 PTY scenarios）
cargo clippy --workspace --all-targets --all-features -- -D warnings ✅
cargo test --workspace                             ✅ 全部 workspace tests passed；2 real-provider tests ignored
cargo run -p kloop -- --mock                      ✅
git diff --check                                   ✅
```

`cargo check -p kloop-tui --target x86_64-pc-windows-msvc` 未完成：本机无 MSVC/C headers，`ring` build 在 `assert.h` 缺失处停止；不是 Windows native 通过。未执行 Kitty、WezTerm、foot、Alacritty、Terminal.app、iTerm2、tmux、Zellij、SSH 或 Windows ConPTY 的 physical-terminal 人工场景，因此不作跨终端泛化承诺。

完成后在 main 生成一次 `feat(plan82)` 提交，不 push。

## 教训

1. 修饰 Enter 的产品契约不能只看 App 已有分支，还要确认终端是否会产生可区分的 wire。
2. 对可安全忽略的 progressive mode，直接 best-effort Push 比同步 capability probe 更适合已有 CPR/poll reader 架构；mode ownership 必须按“成功 Push 才 Pop”记录，并让全部 cleanup 路径共用幂等状态。
3. panic hook 不应从任意 worker 线程直接关闭共享 terminal；应只发停止信号，由唯一 UI owner teardown，避免与 draw/read 并发。
4. 按键优先级是契约的一部分：modal overlay 可压住 Composer，但 completion accept 不能吞掉 Composer newline。

## 最终提交

- 提交：以本文件所在提交为准（本计划收口时填写实际 SHA）。
- 不 push。
