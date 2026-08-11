# Plan 76 — TUI terminal correctness：PTY/ANSI 回归与 Composer grapheme/visual-row

> 状态：✅ 已完成（2026-08-11；plan76 提交，SHA 以本文件所在提交为准）
>
> 基线：`a04b2e1`（Plan 74 实现提交；Plan 75 已完成）
>
> 依赖：Plan 30、Plan 38、Plan 43、Plan 48、Plan 74、Plan 75

## Context

kloop 当前 TUI 已形成稳定的 inline 架构：`setup_terminal()` 使用 Ratatui 0.29 `Viewport::Inline`，不开 alternate screen；输入线程在 CPR 完成后启动；每帧按 `autoresize → commit_overflow → draw` 运行；finalized Cell 经 `Terminal::insert_before` 进入 native scrollback，并在 `clear` 成功后才从 `App` drain；Task/activity 作为 non-Cell live chrome 与 commit 共用高度预算。现有 App、renderer 和 TestBackend 测试能精确裁决状态、布局及 synthetic scrollback，但没有持续启动真实 `kloop` binary、经 PTY 输入、回答 CPR 并解析 ANSI 的自动回归层。

Composer 仍以 `String + char-index cursor + char goal column` 建模：Left/Right/Backspace 会拆 combining/ZWJ/flag/skin-tone grapheme，Up/Down 只理解硬换行，`view()` 与 navigation 各算一套行；completion range 也使用 char index。大粘贴则以可见 label 充当 identity，提交时全局 `String::replace`，存在字面碰撞、复制/局部编辑后错误展开的问题。Plan 76 把“真实终端验收层”和“Composer 文本正确性层”作为同一个任务闭环：先建立 dev-only PTY/ANSI harness，再统一 byte/grapheme/layout/atom 不变量，并用 unit、TestBackend、真实 PTY 各自裁决适合的证据。

本计划不因调研结果升级 Ratatui/Crossterm，也不引入生产 PTY。Codex/xAI/CodeWhale 只提供算法和测试参考；kloop 保留现有 inline/native-scrollback 产品架构。

## 产品契约

- 默认 CLI 仍进入 inline TUI；`--mock`、`--plain` 仍走 plain，不进 alternate screen。
- overflow 事务保持 `insert_before(all blocks) → clear viewport → drain committed cells → redraw`；任一 terminal 写失败都不得提前 drain App。
- event loop 让 `Terminal::draw` 的内部 `autoresize` 建立 authoritative viewport；completed frame 后 capture physical size，并把 `Backend::size()` pin 到该值贯穿 autoresize confirmation、不可逆 commit 与 repaint。geometry 已变化则先 redraw、连续变化则本 frame 跳过 commit；事务中到达的 resize 留给下一 frame。按键使用最终 frame 的 width/height，不再混用独立的 `crossterm::terminal::size()`。
- Composer cursor、completion target、layout source range、paste atom range 统一为强类型 UTF-8 byte offset/range；cursor 永远位于 UTF-8 与 extended-grapheme boundary，不保留 char/byte 双轨 API。
- Left/Right、Backspace、新增的 forward Delete 均跨完整 grapheme；Home/End 保持当前 hard logical-line 语义。
- Up/Down 按当前宽度的 visual rows（硬换行 + 软折行）移动，使用 display-column goal；跨 CJK/emoji/短行 clamp 后仍保留 goal。只有不存在上一/下一 visual row 时才进入 history。
- `Composer::view()` 与 vertical navigation 必须消费同一个 canonical `ComposerLayout`；终端宽度作为按键处理的显式输入，不缓存进 document state。
- 大粘贴以稳定 `PasteId + TextRange + payload` 表达。可见 placeholder 只是投影；提交按有序 segments 精确展开一次，用户手输相同 label 永远是普通文字。光标不进入 atom；邻接 Backspace/Delete 删除整个 atom，任何切穿 atom 的 replace 先 materialize 再编辑。
- history/draft 保存完整 Composer document snapshot；paste ID high-water 在 Composer 生命周期内不复用。
- 图片继续使用 typed `ContentBlock::Image`，不变成文本 atom；把当前 `images + labels` 平行数组收成单个 `Attachment` 结构，但不增加 selection/单项删除等新 UX。
- 真实 PTY 测试只证明 binary 启动、CPR、输入/resize、ANSI lifecycle、当前 viewport 和退出恢复。native scrollback retention 继续由现有 TestBackend oracle 裁决；不得用 vt100 history 外推真实终端 scrollback。

## 实施方案

### 1. 建立 Unix-only 的真实 binary PTY/ANSI harness

在 `kloop/crates/cli/tests/tui_pty.rs` 及同目录 support module 增加 test-only harness；从 `refs/codewhale` 借鉴 `PtySession`、`Frame`、raw keys 和 deadline wait 的形状，但针对 kloop inline viewport 补 CPR responder：

- 通过 `env!("CARGO_BIN_EXE_kloop")` 启动默认 TUI，不传 `--mock`/`--plain`。
- 在 workspace/CLI 的 Unix dev-dependencies 中加入 `portable-pty 0.9`、`vt100 0.15.2`、`tempfile` 和现有 `wiremock`；它们不得进入 production dependency 或 Windows build graph。Ratatui 0.29 锁定 `unicode-width 0.2.0`，不能与要求 `unicode-width ^0.2.1` 的 vt100 0.16 统一，故保留 API 等价的 0.15.2。
- child 使用空环境重建白名单：临时 HOME/XDG/cwd、固定 PATH、`TERM=xterm-256color`、`COLORTERM=truecolor`、`KLOOP_NO_ANIM=1`、`KLOOP_PROVIDER=openai-compat`、synthetic key/model、loopback `OPENAI_BASE_URL` 和 `NO_PROXY`。不得继承真实 provider key、proxy、配置或项目 cwd。
- 复用 provider tests 的 OpenAI Chat SSE shape，以 queue-backed `wiremock::Respond` 按请求序号返回 deterministic turn；只保存去敏后的 JSON body 与计数，不打印 Authorization。
- PTY reader 持续 drain raw bytes并增量喂给 vt100；writer 由 responder 与测试输入共享串行锁。CPR scanner 必须跨 chunk 识别每个 `ESC[6n`，按 parser 当前一基 cursor 回复 `ESC[row;colR` 并计数；对每个 split point、多 query/同 chunk 单测。
- harness 提供 resize、raw ANSI facts、当前 frame/cursor、条件式 `wait_for`、CI-scaled deadline、显式 shutdown 和 Drop emergency kill/reap/join。失败 dump 只含 synthetic frame 与有界 ANSI 摘要，raw capture 超预算应失败而非静默截断。
- vt100 parser 的 scrollback capacity 不作为 assertion surface；现有 `insert_scrollback_blocks` TestBackend 测试继续是 retained-history 的精确 oracle。

首批真实 PTY case：

1. **boot/CPR/no-alt-screen**：header、composer/footer 可见；至少一次 CPR；开启 bracketed paste；raw bytes 无 47/1047/1049 alternate-screen enter/leave。
2. **resize/CPR**：先 shrink、再 grow 到不超过启动高度；每次尺寸变化后 CPR 继续响应，frame 与 PTY size 一致，cursor/chrome 不越界、不重复，也不触发 2 秒 hang。
3. **two-turn overflow**：本地 fixture 返回带稳定 sentinel 的长第一轮和短第二轮；raw trace 只断言 scroll-region/scroll-up、clear、随后 repaint 的有序事实，最终 viewport 有一套 live chrome 和第二轮 tail；不声称 vt100 证明 native history retained。
4. **graceful restore**：双 Ctrl+C 后 exit 0；第一拍只 arm；退出 raw bytes 含 bracketed-paste disable、cursor show、final CRLF，且 cleanup 未走强杀。
5. **Unicode input closure**：经 PTY 输入 combining/CJK/ZWJ grapheme 并用方向键、Backspace/Delete 编辑；fixture 收到的 user content 逐字节等于预期 UTF-8。

### 2. 收敛 grapheme-aware text primitives

新增 `kloop/crates/tui/src/text_layout.rs`，直接依赖当前 lock 已有的 `unicode-segmentation 1.13`，集中提供：

- `ByteOffset`、`TextRange` 及 boundary invariant；
- previous/next/snap grapheme boundary；
- grapheme byte ranges、`UnicodeWidthStr` display width；
- byte offset ↔ display column 映射；
- grapheme-safe plain truncate/wrap。

先让 Composer、completion 和 `render.rs` 的 plain `display_width`/`truncate`/`wrap` 共用这些 helper，避免 Task/footer/composer 对同一 grapheme 采取不同宽度；`toolrow.rs` 的 plain width helper 一并收敛。Markdown 的 styled-span parser 不在本计划重写，但新增回归确保本计划改动不破坏其现有 ANSI/style contract；未来若要跨 style span 做完整 grapheme grouping，另立独立计划。

helper 测试覆盖 combining mark、ZWJ family/technologist、skin tone、flag、variation selector/keycap、CJK、极窄宽度、exact-width EOL，且 truncate/wrap 不留下 orphan codepoint。

### 3. 一次性迁移 Composer document、cursor、completion 与 paste atom

在一个编译闭环内完成，禁止留下兼容 char-index API：

- 在 `composer.rs` 引入 `ComposerDocument { display, atoms }`、单调 `PasteId`、`PasteAtom { id, range, label, content }`、`Attachment { label, block }`；所有 insert/delete/replace/materialize/expand/range-rebase 只经 document methods。
- cursor 改为 `ByteOffset`；删除旧 `char_count`、`byte_index`、char-index `line_starts/row_col/move_to_row`。Left/Right/Backspace/Delete 按 grapheme；插入使相邻 codepoint 合并成新 grapheme 时，cursor 规范化到完整 cluster 边界。
- atom ranges 每次 mutation 后保持 sorted、non-overlapping、boundary-safe，且 `display[range] == label`；submit 按 ranges 分段拼接，不再使用全局 `String::replace`。history/draft 保存完整 document；clear/submit 不倒退 PasteId high-water。
- 把 `images + labels` 合并为 attachment vector，保留 image-only submit、steer 保留图片、fresh turn 提交 typed blocks 等现有语义。
- `menu::detect_trigger` 返回带 `kind/query/TextRange/cursor` 的 completion target；Popup 与 `Command::SearchFiles` 保存该 target，accept 直接替换精确 byte range。file results 只在 target 完整相同时落地，防同 query 不同位置误投；replacement 与 atom 相交时走 document materialize 规则。
- grep/static assertion 锁定 Composer/menu 不再以裸 `usize` 暗示 char cursor/range；`chars().count()` 只允许用于 paste 显示计数等非坐标用途。

### 4. 建立 canonical `ComposerLayout` 并贯通 viewport width

在 `composer.rs`（或相邻私有模块）增加全量 layout：每个 `VisualRow` 保存 source `TextRange`、`Soft/Hard/End` break、display width，并提供 cursor point 和 display column 到最近合法 byte boundary 的映射。

- `Composer::view(width)` 仅把 canonical layout 窗口到现有 8 行并添加 prompt/continuation gutter；不自行重新 wrap。
- Up/Down 接收当次 width，在相邻 visual row 按 goal display column 定位；只有 visual 边界才触发 history。horizontal edit、Home/End、history load 和普通 mutation 清 goal。
- 明确定义 exact-width cursor 与宽 grapheme 在极窄终端的投影，确保 Ratatui cursor 永不落到 frame 外；layout source ranges 加 hard newline 后能无损重建 compact display。
- `App::on_key` 显式接收 composer width；更新所有 App tests。`ui_loop` 不做独立 pre-draw size probe：先让 `Terminal::draw` 完成内部 `autoresize`，再 capture + pin backend size；confirmation、`commit_overflow` 和强制 redraw 在同一 size fence 内，变化则先 redraw，连续变化则跳过当帧 commit。最终 Rect 传给 key handling；`commit_overflow` 不再另读 physical crossterm size。
- 保留 Ratatui 0.29 当前 inline-height 边界；本计划不增加超出启动高度的动态 viewport 扩高或历史 reflow。

### 5. 回归、文档与完成记录

测试分层：

- **Composer/unit**：每类 grapheme 整 cluster 移动/删除与 boundary invariant；visual hard+soft row、CJK/emoji display goal、短行 clamp 恢复、resize width 切换、history edge；paste label 碰撞、多 atom 顺序、删除/materialize、recall/draft/resubmit、高水位；attachment/steer 语义。
- **Menu/App**：Unicode 前缀和 token 中间 completion、相同 query 不同 range 拒绝迟到结果、Delete 路由、按当次 width 导航。
- **Render/TestBackend**：layout/view cursor 一致、exact cursor screen coordinate、窄屏 grapheme 不越界、Task/activity/composer reserve 不分叉；保留 native synthetic scrollback oracle。
- **CLI PTY**：上述五个真实 binary 场景；只在 Unix 运行。

同步：

- README 记录 grapheme/visual-row/paste atom 行为和 Unix PTY 测试入口，明确 `--mock` 仍是 plain、dev PTY 不是 production interactive process。
- `docs/capability-report.md` 只把 Composer correctness 记为产品能力，把 PTY 记为测试证据；Windows 只具备 pure/TestBackend 覆盖，不宣称 ConPTY。
- HANDOFF 记录四条教训：byte/char offset 不可双轨；view/navigation 必须共用 layout；width 是 event-loop 输入；PTY raw ANSI、vt100 viewport、TestBackend scrollback 各自有独立裁决边界。
- 不修改 `refs/` pinned fixtures、matrix、normalized capture；不记录/提交 temp HOME、synthetic session、raw dump、Authorization、真实 key 或私有 endpoint。

## 范围外

- Ratatui/Crossterm 升级、Ratatui fork、`unstable-rendered-line-info`、alternate screen。
- production `portable-pty`、Bash `tty`/`interactive`、`write_stdin`、持久 REPL/ssh/vim、Windows ConPTY。
- public server wire、ToolDef、权限/进程模型、native Task panel 语义。
- native scrollback resize reflow 或清空后全历史重放。
- Composer selection、mouse editing、undo/redo、Vim mode、word movement、图片文本化或 attachment 新交互。
- 用 vt100/pyte 的 current screen 结果宣称真实终端 native scrollback 完全兼容。

## 关键文件

- `kloop/crates/tui/src/{text_layout.rs,composer.rs,menu.rs,app.rs,render.rs,lib.rs}`：统一 offset/layout、visual navigation 及 terminal geometry。
- `kloop/crates/cli/tests/{tui_pty.rs,tui_pty_support/*}`：真实 binary PTY、CPR、ANSI frame 和本地 SSE fixture。
- `kloop/{Cargo.toml,crates/tui/Cargo.toml,crates/cli/Cargo.toml}`：Unicode 与 Unix-only dev 依赖。
- `kloop/README.md`、`docs/plan/{76-tui-terminal-correctness.md,HANDOFF.md}`、`docs/capability-report.md`：当前契约和完成记录。

## 验证

从 `kloop/` 运行：

```bash
cargo test -p kloop-tui
cargo test -p kloop --test tui_pty -- --nocapture   # Unix
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
python3 -B ../refs/claude-code-2.1.220/verify.py --corpus-only
git diff --check
```

- macOS/Linux 的 workspace test 必须包含 PTY suite；Windows 不编译 Unix PTY 依赖，但必须通过 Composer/App/renderer、workspace、mock 与 corpus 门。
- 额外做一次真实宿主终端手工检查（Terminal.app/iTerm2，可选 tmux/Zellij）：native scrollback、resize、Unicode cursor、退出恢复；该人工结果与 PTY/vt100 证据分开记录。
- 完成后在 main 生成一次 `feat(plan76): harden tui terminal correctness` 提交，不 push；Plan 标 ✅ 并记录平台、测试数和过滤事实，提交 SHA 采用“以本文件所在提交为准”。

## 完成记录（2026-08-11）

- Composer document、cursor、completion target、layout source 和 paste atom 已统一为强类型 UTF-8 byte offset/range；Left/Right/Backspace/Delete 只跨完整 extended grapheme。hard+soft visual rows、display-column goal、八行 view 与 cursor projection 共用 canonical `ComposerLayout`；exact-width EOL 显式投影到下一 visual row，1–2 列 frame 仍在 renderer clamp 到有效 cursor coordinate。
- 大粘贴已改为单调 `PasteId + TextRange + payload` atom，literal label 不再参与 identity；history/draft/steer 精确保留 document snapshot，切穿 atom 先 materialize，atom 两端可继续插入独立 paste。图片平行数组已收敛为 typed `Attachment`，image-only/fresh-turn/steer 语义保持不变；bracketed-paste 的 `@` target 会把 `SearchFiles` 命令交还 event loop，不丢 completion。
- event loop 由已完成 draw 的 Ratatui `Rect` 驱动 overflow reserve，并在 commit 前 capture physical size、把 backend `size()` pin 到该值贯穿 confirmation、`insert_before → clear → drain` 与强制 redraw；已变化就先 redraw，连续变化则跳过当帧 commit，事务中到达的 resize 由下一 frame 接收。对抗审查补出的 `6 → 6 → 24` 三阶段 TOCTOU 已有 staged-size Backend 回归；inline viewport、native scrollback 与 `--plain`/`--mock` plain 路径保持不变。
- Unix-only real-binary harness 使用 sealed HOME/XDG/cwd/env、loopback OpenAI-compatible SSE、持续 CPR responder 和 vt100 current viewport。7 个 PTY tests 覆盖 full-lifecycle no alternate screen、以 composer cursor column 证实应用采用 shrink/grow geometry、ordered overflow ANSI、双 Ctrl+C restore、逐 cluster combining/CJK/ZWJ editing，以及 scanner split/multiple-query；Windows target dependency tree 不含 `portable-pty`、`vt100`、`tempfile` 或 `wiremock` dev edge。
- 本次自动验收运行于 macOS Darwin 25.4.0：TUI 164 tests、PTY 7 tests、core 638 tests 与 `cargo test --workspace` 全绿；`cargo fmt --all -- --check`、workspace all-target/all-feature Clippy `-D warnings`、`cargo run -p kloop -- --mock`、corpus-only verifier 和 `git diff --check` 均通过。
- 当前执行 shell 为 non-TTY，未伪报 Terminal.app/iTerm2 的 physical-terminal 人工检查；vt100 只裁决 current viewport，native retained scrollback 仍由 TestBackend oracle 裁决，也不宣称 Linux/Windows ConPTY 已实跑。
- 提交：本次 `plan76` 收口提交（SHA 以本文件所在提交为准）；不 push。
