# Plan 78 — TUI terminal dependency upgrade：Ratatui 0.30 / Crossterm 0.29 / vt100 0.16

> 状态：✅ 已完成（2026-08-12；plan78 提交，SHA 以本文件所在提交为准）
>
> 基线：`ca8d532`（Plan 77 完成提交；实际开工点）
>
> 依赖：Plan 38、Plan 43、Plan 74、Plan 75、Plan 76、Plan 77

## Context

Plan 76 已在基线 `1ab5d7e` 建立 terminal correctness 契约和分层回归网，但为避免在同一任务里同时改变行为与底层，保留了 Ratatui 0.29、Crossterm 0.28、`unicode-width` 0.2.0，并因当时的版本约束让 Unix 测试 parser 停在 vt100 0.15.2。现在可把生产 terminal pair 升到截至 2026-08-11 的兼容稳定组合，并让 vt100 回到同一条 `unicode-width` 0.2 版本线。

本计划是依赖/API 迁移，不重写 TUI 架构。目标组合固定为：

- Ratatui `0.30.2`；
- Crossterm `0.29.0`，保留现有 `event-stream` feature，并显式选择 Ratatui 的 `crossterm_0_29` integration；
- `unicode-width 0.2.2`；
- `unicode-segmentation 1.13.3`（lock 已是该版本，仅提高 manifest 最低版本）；
- Unix test-only vt100 `0.16.2`；
- Rust MSRV `1.88`，edition 继续为 2021。

`portable-pty 0.9`（lock 0.9.0）、`tempfile 3`（lock 3.27.0）和 `wiremock 0.6`（lock 0.6.5）已是当前稳定版本，保持声明与代码不变。Ratatui 0.30.2 的精确 crate metadata 要求 Rust 1.88；不能沿用 0.30.0/highlights 的 1.86 作为最终 patch 的 MSRV。

实施前有一个硬前置门：工作树必须没有不属于 Plan 78 的重叠修改。此前由 Plan 77 持有的 Plan 39/server、`rust/Cargo.lock`、`refs/README.md` 和独立 Plan 77 文件已经随 `ca8d532` 完成；开工前仍须重新检查 status/diff，若出现新的重叠修改，不得 stash、reset、覆盖或捎带提交这些内容，尤其 `Cargo.lock` 重叠时应先等其所有者完成或恢复干净。

## 产品契约

- 默认 CLI 继续使用 inline TUI，不进入 alternate screen；`--plain`、`--mock` 继续走 plain。
- overflow 事务继续是 `insert_before(all blocks) → clear viewport → drain exact committed prefix → redraw`；任一 terminal 写失败都不得提前 drain `App`。
- `Terminal::draw` 的内部 autoresize 建立 authoritative viewport；completed frame 后 capture physical size，并把 `Backend::size()` pin 到 confirmation、commit/drain 和强制 repaint 结束。geometry 已变化先 redraw，连续变化跳过当帧 commit，事务中到达的 resize 留给下一帧。
- 保持 Plan 76 的 Composer byte/grapheme/visual-row、exact-width EOL、1–2 列 cursor clamp、paste atom、completion target 和 typed attachment 契约；Unicode 版本变化不能倒退这些不变量。
- 输入线程继续使用 `poll(200ms) + read()`，不改成会与 CPR 抢 reader lock 的 `EventStream`；本计划也不顺手删除现有 feature。
- 双 Ctrl+C、bracketed-paste disable、cursor show 和 final CRLF 的退出恢复不变。
- Unix PTY 仍只证明 real binary、CPR、input/resize、ANSI lifecycle 和 current viewport；vt100 不证明 native scrollback。retained scrollback 继续由 TestBackend oracle 裁决，真实终端 scrollback/字形另作人工证据。
- `portable-pty`、vt100、tempfile 和 CLI fixture wiremock 继续是 Unix-only dev dependencies，不进入 production 或 Windows build graph。

## 实施方案

### 1. 建立 MSRV 与受控 dependency set

- 在 `rust/Cargo.toml` 的 `[workspace.package]` 增加 `rust-version = "1.88"`；所有 workspace member package 通过 `rust-version.workspace = true` 继承，`kloop-codemode` 保留现有显式 version/edition 但也继承 rust-version。不要增加 `rust-toolchain.toml`，也不要切换 edition。
- 更新 workspace dependencies：
  - `crossterm = { version = "0.29.0", features = ["event-stream"] }`；
  - `ratatui = { version = "0.30.2", features = ["crossterm_0_29"] }`；
  - `unicode-width = "0.2.2"`；
  - `unicode-segmentation = "1.13.3"`；
  - `vt100 = "0.16.2"`。
- 保留 Ratatui 默认 features；不要用 `default-features = false`，以免无关地关闭 macros、layout cache、underline-color 等默认能力。`rust/crates/tui/Cargo.toml` 继续在消费点启用 `scrolling-regions`，Cargo feature union 同时得到 `crossterm_0_29`。
- 保持 portable-pty/tempfile/wiremock 的 manifest 不变。用 targeted Cargo update 生成 lock diff，只接受目标 crate 和它们被迫变化的 transitive edges；不运行无边界的全 workspace `cargo update`。

### 2. 迁移 Ratatui 0.30 `Backend::Error`

集中修改 `rust/crates/tui/src/lib.rs`，保持 terminal transaction 算法不变：

- `PinnedBackend<B>` 的 `Backend` 实现增加 `type Error = B::Error`；`draw`、`append_lines`、cursor、clear、size/window-size、flush、scroll-region 等方法统一返回 `Result<_, Self::Error>`，继续完整委托。`append_lines` 在 0.30.2 仍是 provided trait method，不删除。
- `pin_current_size()` 返回底层 backend error；pin/unpin 的作用域、`window_size.columns_rows` 覆盖和 resize fence 不变。
- `insert_scrollback_blocks`、`commit_overflow`、`draw_frame` 等纯 terminal helper 改为传播 `B::Error`，避免用 `anyhow::Result` 强迫所有合法 backend error 额外满足 `Send + Sync`；到 concrete Crossterm 调用边界再由现有 `anyhow` 返回链转换。
- 测试 `StagedSizeBackend` 使用 `<TestBackend as Backend>::Error`（Ratatui 0.30 为 `Infallible`）并迁移全部签名；保留 `6 → 6 → 24` staged-size 事件顺序和断言。
- 对编译器指出的其他 0.30 API 差异只做最小适配。已确认 `Terminal::with_options`、`Viewport::Inline`、`Terminal::insert_before` 均仍存在，不换成 convenience `ratatui::run()`，不改 terminal setup/restore ownership。

完成该阶段即运行 `cargo test -p kloop-tui`，先把 Backend/API 问题与 Unicode/vt100 行为问题隔离。

### 3. 验证 Unicode、vt100 与 lock graph

- 复用 `text_layout.rs`、`composer.rs`、`render.rs`、`markdown.rs`、`toolrow.rs` 现有回归，逐项裁决 combining、ZWJ、skin-tone、flag/keycap、CJK/wide glyph、soft/hard row、exact-width EOL、超窄 cursor、wrap/truncate。若 `unicode-width 0.2.2` 的数据变化改变期望，先确认是上游有意语义，再最小更新期待值并补相应 regression；不得为“让测试绿”退回 char/byte 双轨或拆 grapheme。
- vt100 0.16.2 保留 `Parser::new/process/screen`，但删除 `Parser::set_size`；仅在实际编译差异处最小修改 `tui_pty_support` 为 `parser.screen_mut().set_size(rows, cols)`，不改变 CPR scanner、bounded raw capture、sealed env 或 child teardown。
- 运行 Unix PTY 7 tests，必须继续覆盖 full-lifecycle no-alt-screen、应用真实采用 shrink/grow geometry、ordered overflow ANSI、双 Ctrl+C restore 和 Unicode request round-trip。
- 用 `cargo tree` 核对：
  - Ratatui 只有 0.30.2，Crossterm 只有 0.29.0，且 Ratatui 走 `crossterm_0_29`；
  - direct `unicode-width` 是 0.2.2，vt100 是 0.16.2；
  - `unicode-width 0.1.x` 是否消失以 solver 结果为准。若仍由某条必要 transitive edge 引入，记录来源并保留；不得强制 patch 上游或降级其他 crate；
  - 不为了 lockfile 外观强行合并 `rustix`、`windows-sys`、`getrandom` 等不同 target/API 家族；
  - Windows target 的 kloop normal/dev graph 不包含 Unix-only PTY dependencies，production graph也不包含这些测试 crate。

### 4. CI、文档与完成记录

- 调整 `.github/workflows/ci.yml`：
  - stable 的 macOS/Linux/Windows matrix 保持；fmt 使用 `cargo fmt --all -- --check`，Clippy 使用 `cargo clippy --workspace --all-targets --all-features -- -D warnings`；保留 workspace test、mock 和 corpus-only verifier。Unix 的 `cargo test --workspace` 已包含 PTY suite，不重复跑同一 target；Windows 的 cfg skip 不是 ConPTY pass。
  - 新增 Linux Rust 1.88 MSRV job，执行 `cargo check --locked --workspace --all-targets --all-features`。stable matrix 继续负责当前工具链与原生三平台行为。
- 新建并完成 `docs/plan/78-tui-terminal-dependency-upgrade.md`，实际开工基线记 `ca8d532`，依赖 Plan 38、43、74、75、76、77；记录目标/最终版本、MSRV、Backend migration、lock graph 和证据边界。
- README 记录 workspace 最低 Rust 1.88 与已验证 terminal dependency set；现有 inline/no-alt-screen、Composer 和 PTY 描述只在实际行为变化时调整。
- `docs/capability-report.md` 将 Plan 78 记为 maintenance/correctness evidence，不升级任何 parity `same`/`compatible` 裁决；Windows workspace pass 不能写成 ConPTY 或 native-scrollback 证据。
- HANDOFF 记录实际 dependency graph、`Backend::Error` wrapper 迁移和教训：规划升级必须核对最终 patch 的 crate metadata，不能用同 minor 的首版/highlights MSRV代替。
- 完成记录必须分开写明本机实际运行平台、远端 CI 是否真的执行、physical-terminal 人工检查是否执行；未运行的 Linux/Windows/Terminal.app/iTerm2 不得写成通过。
- 全部验证通过后在 main 只暂存 Plan 78 文件清单，生成一次 `chore(plan78): upgrade tui terminal dependencies` 提交，不 push。禁止 `git add -A`；Plan 77、Plan 39/server 和 refs 的独立改动不得进入提交。

## 范围外

- Rust 2024 edition、Ratatui fork、alternate screen、unstable Ratatui feature、terminal 架构重写。
- 删除 Crossterm `event-stream` feature或把 poll/read 改为 EventStream。
- production PTY、`write_stdin`、REPL/ssh/vim、Windows ConPTY。
- native scrollback resize reflow、动态突破启动 inline height、重写 Composer/markdown/toolrow。
- 无关 workspace dependency 大扫除；升级或重构已最新的 portable-pty/tempfile/wiremock。
- 强制消灭 `unicode-width 0.1.x` 或其他合法重复 transitive packages。
- Plan 77 的 native event sequence/cursor/snapshot/sync、server protocol、Desktop adapter；PDF Plan 67；`refs/` fixture/matrix/capture。

## 关键文件

- `rust/Cargo.toml`、`rust/Cargo.lock`、`rust/crates/*/Cargo.toml`：版本、Ratatui feature、MSRV inheritance 和 Unix dev boundary。
- `rust/crates/tui/src/lib.rs`：`PinnedBackend`/`StagedSizeBackend` associated error、inline terminal、size-pin commit transaction。
- `rust/crates/tui/src/{text_layout.rs,composer.rs,render.rs,markdown.rs,toolrow.rs}`：Unicode 行为回归；原则上只在确认的数据语义变化时改测试/实现。
- `rust/crates/cli/tests/{tui_pty.rs,tui_pty_support/mod.rs}`：vt100 compatibility 与 real-binary terminal gates。
- `.github/workflows/ci.yml`、`rust/README.md`、`docs/capability-report.md`、`docs/plan/{78-tui-terminal-dependency-upgrade.md,HANDOFF.md}`：MSRV、CI、契约与完成记录。

## 完成记录

- 最终依赖为 Ratatui 0.30.2、Crossterm 0.29.0、`unicode-width` 0.2.2、`unicode-segmentation` 1.13.3、Unix test-only vt100 0.16.2；workspace MSRV 为 Rust 1.88，edition 保持 2021。
- `PinnedBackend`/`StagedSizeBackend` 与 generic terminal helpers 已迁移 associated `Backend::Error`；vt100 resize 使用 `screen_mut().set_size`，terminal transaction、resize fence、poll/read 与 restore ownership 不变。
- lockfile/feature/target graph：resolved graph 只有 Ratatui 0.30.2、Crossterm 0.29.0、`unicode-width` 0.2.2、`unicode-segmentation` 1.13.3 与 vt100 0.16.2；Ratatui feature union 含 `crossterm_0_29`/`scrolling-regions`，旧 `unicode-width 0.1.x` 已随 `unicode-truncate 2.0.1` 消失。production normal/build graph与 `x86_64-pc-windows-msvc` normal/build/dev graph均不含 portable-pty/vt100/tempfile/CLI fixture wiremock；resolved metadata 最高 `rust-version` 为 1.88.0（darling/Ratatui）。
- 本机自动验证：macOS Darwin 25.4.0 已通过 Rust 1.88.0 locked all-target/all-feature check、TUI 164 tests、Unix PTY 7 tests、workspace all-target/all-feature Clippy、workspace tests、keyless mock、corpus-only verifier 与 `git diff --check`。
- 远端 CI：未执行（本次不 push）；workflow 已配置 stable 三平台和 Linux Rust 1.88 locked check。
- physical Terminal.app/iTerm2：未执行；本次自动环境不把 vt100 current viewport 冒充 native scrollback/字形证据。
- 提交：本次 `plan78` 收口提交（SHA 以本文件所在提交为准）；不 push。

## 验证

从 `rust/` 运行：

```bash
cargo +1.88.0 check --locked --workspace --all-targets --all-features
cargo test -p kloop-tui
cargo test -p kloop --test tui_pty -- --nocapture   # Unix
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
python3 -B ../refs/claude-code-2.1.220/verify.py --corpus-only
git diff --check
```

额外核对：

- `cargo tree`/`cargo metadata --locked` 记录目标版本、Crossterm integration、重复 `unicode-width` 的实际来源和 resolved packages 的最高 `rust-version`（不得高于声明的 1.88）。
- Windows target tree 不含 `portable-pty`、vt100、tempfile、CLI PTY wiremock dev edge；release/normal graph不含测试 PTY 栈。
- 若有可用 CI，只按真实结果记录 macOS/Linux/Windows；不 push 时只写“已配置、未远端实跑”。
- 真实 Terminal.app/iTerm2（可选 tmux/Zellij）人工检查 inline native scrollback、resize、Unicode cursor 和退出恢复；执行环境非 TTY 时明确记“未执行”，不能以 vt100 替代。
