# Plan 149 — 二十一次抽查,没有一次整屏

> 来源:2026-09-15,借鉴项目调研(`refs/README.md` 的 2026-09-15 节)收尾时,用户问
> "kloop 主要在 macOS 上用,该在哪些方面提高能力"。排序后第一条是 TUI 测试,用户拍板开工。
>
> **调研中我两次把结论下错**,都是按单个文件名判断、没往下看一层:先说"kloop 没有 bash
> 命令拆分"(在 `core/src/shell.rs`,不在 `permissions.rs`,已由 `9ae424d` 更正),再说
> "kloop TUI 测试没有虚拟终端解析"(在 `tui_pty_support/mod.rs`,不在 `tui_pty.rs`)。
> 本 plan 的现状描述基于读完那 632 行 harness,不要再按文件名推断。

## 一、已经有的,比想象中多

`rust/crates/cli/tests/tui_pty_support/mod.rs`(632 行)是一套真 PTY harness,不是字节断言:

- `portable_pty` 起**真二进制**,`wiremock` 起 mock provider(`ChatFixture`);
- `vt100::Parser`(`mod.rs:218`)解析输出,`FrameSnapshot`(`mod.rs:162`)给出
  `rows / cols / cursor / text / bracketed_paste / cpr_count / raw_len`;
- `CprScanner`(`mod.rs:193`)识别 `\x1b[6n` 并回 `\x1b[r;cR`,所以被测程序的光标查询有人应答;
- `resize()`(`mod.rs:405`)同时改 PTY ioctl 与 vt100 的 ANSI 尺寸;
- `wait_for()`(`mod.rs:464`)是 condvar + 25ms 轮询的谓词等待,失败时 `bail!` 带
  `debug_dump()`(`mod.rs:181`:带行号的整屏文本)。

`tui_pty.rs`(374 行)7 个测试 + `plain_pty.rs`(91 行)2 个,覆盖的是**终端协议契约**:

| 测试 | 管住什么 |
|---|---|
| `boot_answers_cpr_without_alternate_screen` | 启动不进 alternate screen,CPR 有应答 |
| `resize_keeps_cpr_and_current_viewport_in_sync` | 缩放后光标与视口一致 |
| `two_turn_overflow_commits_without_scroll_regions_then_repaints` | 溢出提交不用 scroll region |
| `double_ctrl_c_restores_terminal_modes_without_emergency_kill` | 退出恢复终端模式 |
| `bracketed_crlf_paste_submits_a_logical_multiline_message` | bracketed paste 语义 |
| `csi_u_shift_enter_submits_a_multiline_message` | CSI-u 修饰键 |
| `unicode_input_round_trips_through_real_binary_editing` | unicode 编辑往返 |
| `plain_pty` 两个 | idle/running 下的 Ctrl-C 退出路径 |

**这一层是好的,不要动它。**

## 二、缺的是哪一层

上面 9 个测试里的断言共 21 处,全是 `frame.count(needle)` / `frame.contains(needle)` 的
**点抽查**。`debug_dump()` 能产出整屏文本,但只出现在三条失败路径(`mod.rs:475/494/500`)里
当 debug 信息用——**从来没有一次,把一整屏当成断言对象**。

于是这些回退现在一个都抓不住:markdown 缩进或换行变了、toolrow 对齐差一列、
提交后多留一个空行、滚动后有残字、状态行位置漂了。只要那三五个被 `count()` 盯着的词还在,
测试就是绿的。

`tui` crate 内部有 185 个单元测试(`render.rs` 48、`app.rs` 52、`markdown.rs` 24、
`toolrow.rs` 14 ……),但它们测的是**渲染函数的输出**,不是真二进制跑完一轮后**终端上的最终一屏**。
两者之间的落差(组合、裁剪、光标、重绘时序)正是 `docs/capability-report.md` 第 13 节说的
"功能齐、打磨差距最大"所在。

AGENTS.md 明写"**测试整对象断言优先**"。这块是全仓最该整对象、却最点断言的地方。

## 三、做什么

1. **给 `FrameSnapshot` 一个稳定的可断言文本形式**(与 `debug_dump()` 分开:后者是给人看的
   诊断,前者要进基线)。只含 `rows×cols` 的屏幕文本 + 光标位置;**不含** `raw_len`、
   `cpr_count` 这类每次都会漂的字段。行尾空格 trim(`debug_dump` 已经这么做),尾部全空行折叠。
2. **规范化不稳定内容**,否则基线每跑必红。至少四类:mock provider 的端口、`TempDir` 的临时
   路径、任何时间/耗时显示、以及 spinner——`tui/src/anim.rs` 的 braille spinner 每
   `PHASE_MS` 换一个字形,但它认 `KLOOP_NO_ANIM` 和 `dumb` 终端会退化成静止 bullet
   (`anim.rs:6`、`spinner_glyph(_, reduced)`),**捕基线的测试必须设 `KLOOP_NO_ANIM`**。
3. **加一个"等屏幕静下来"的等待**。现有 `wait_for()` 等的是"出现某个串",快照要等的是
   "最后一次写入之后 N 毫秒无新字节"。在同一个 condvar 上加一个 `wait_for_quiescent(idle)`,
   不要用固定 sleep。
4. **给现有测试补整屏基线**。优先 `two_turn_overflow_commits…`(它本来就在测布局)和
   `resize_keeps_cpr…`(缩放前后各一张)。原有点断言**保留**——它们表达的是意图,基线表达的是全貌。
5. **新增纯布局场景**,覆盖单元测试够不到的组合:一条带列表和代码块的 markdown 回复、
   一行工具调用 + 结果、一次长输出截断。每个场景绑定固定 `rows×cols`。
6. **negative control**:故意改一处渲染(如 markdown 的缩进常量),基线必须红。红不了说明
   规范化把有效信息也抹掉了。这条纪律沿用 plan 147。

## 四、开工时问用户(先问,再动手)

**基线用 `insta`,还是自己写 fixture 文件对比?**

- 全 workspace 目前**没有 `insta`**(`rust/Cargo.toml` 与各 crate 的 dev-deps 均无)。
  引入它多一个 dev 依赖,但白得 `cargo insta review` 的交互式接受流程——TUI 基线会经常
  因为有意的改动而变,没有 review 流程的话人会倾向于直接覆盖,基线就废了。
- 自写 fixture(像 `refs/claude-code-2.1.220/` 那样存文本、比对、失败时 diff)不加依赖,
  和仓库已有的 parity fixture 风格一致,但接受新基线要手工搬文件。

**推荐 `insta`**,理由是第二条:基线的价值取决于更新它的摩擦是否合适——太大就没人更,
太小就没人看。但这是依赖决定,按仓库惯例该由用户拍。

## 五、坑

- **尺寸必须绑定**。同一段内容在 60 列和 100 列下换行完全不同,基线名要带尺寸。
- **unicode 宽度**。`unicode_input_round_trips…` 说明已经在测 CJK/emoji;vt100 的宽度计算
  与 kloop 渲染侧的必须一致,不一致时先查渲染侧,别改基线迁就。
- **`plain_pty` 的两个测试不要加基线**。它们测的是退出路径,屏幕在断言点上正在消失。
- **不要在基线里包含光标闪烁或终端模式序列**。`FrameSnapshot.text` 已经是解析后的屏幕,
  但新增的稳定文本形式不要顺手把原始序列也塞进去。
- 这套测试只在 `cfg(unix)` 下编译(`cli/Cargo.toml` 的 `[target.'cfg(unix)'.dev-dependencies]`),
  macOS 本机跑得到;别写进 Windows 也会编的路径。

## 六、非目标

- **不做帧耗时与性能基线**。grok 的 harness 有 L2b timing 层和 baseline 对比
  (`refs/README.md` 2026-09-15 节),那是"TUI 快不快",本 plan 只管"TUI 对不对"。
- **不动 `tui` crate 的渲染代码**。本 plan 只加测试能力;基线红了要修渲染,那是下一个 plan。
- **不引 grok 那套 scenarios/YAML 与 scroll matrix**(4 万行)。现在的规模不需要场景 DSL。
- **不动上面表里那 9 个测试的既有断言**,只做加法。

## 七、验收

1. `tui_pty.rs` 原有 7 个 + `plain_pty.rs` 2 个测试**全部仍绿**,且断言一条未删。
2. 至少 4 个场景有整屏基线:`two_turn_overflow_commits…`、`resize_keeps_cpr…`(缩放前后
   各一张)、一条带列表和代码块的 markdown 回复、一行工具调用 + 结果。每张基线绑定
   明确的 `rows×cols`。
3. **同一条命令连跑三次,基线零 diff**。跑不稳就是规范化没做干净(端口、临时路径、
   时间、spinner),回第三节第 2 条。
4. **negative control**:把 `tui/src/markdown.rs` 里一处缩进常量改掉,至少一张基线变红;
   改回来即绿。红不了说明规范化把有效信息也抹掉了,基线是废的。
5. 仓库完成标准照旧:`cargo fmt --all --check`、`cargo clippy --workspace --all-targets
   --all-features -D warnings`、`cargo test --workspace` 各自单独取退出码,全为 0。

## ✅ 已完成(2026-09-16;提交 SHA 以本条所在提交为准)

**第四节那个问题,用户拍了 `insta`**(理由问下来是"接受基线要看一眼 diff")。全 workspace
第一次引入它,只挂在 `crates/cli` 的 `cfg(unix)` dev-dependencies 上,默认 features
(彩色 diff 正是它的价值;serde/glob/redaction 三个 feature 都没要——一帧就是纯文本,
它的每跑必变项在 harness 里用已知值做字面替换,比 regex filter 准)。

### harness 加了什么(`crates/cli/tests/tui_pty_support/mod.rs`)

1. **`FrameSnapshot::stable_text()`**:`[{rows}x{cols} cursor=r,c]` 一行头 + 整屏文本。
   刻意不复用 `debug_dump()`——后者带 `cpr_count`/`raw_len`,那是给人看失败用的计数器,
   每跑必变。行尾空格 trim,尾部全空行折叠(屏幕高度固定,所以折掉的行数可反推)。
2. **四类规范化**,对应第三节第 2 条:
   - **端口与临时路径**:`redaction_table()` 字面替换,**按长度降序**——macOS 的
     `/var` 是 `/private/var` 的符号链接,两种写法都要收,先收短的会把长的切成半截;
   - **耗时**:`normalize_elapsed()` 只认两个锚点(`── Worked for ` 和
     ` · esc to interrupt)`),回复正文里出现 `30s` 不受影响。turn-end 那条横线
     **按它原本占的宽度重建**——它用 `─` 填满整行,`0s` 和 `10s` 差一个 `─`,
     光把数字换成 `<elapsed>` 还是两条不同的线;
   - **spinner**:harness 本来就设了 `KLOOP_NO_ANIM`,退化成静止 `●`,无需额外处理;
   - **cwd**:workspace 挪进 `$HOME` 下,banner 打 `~/workspace`。这是第五节"尺寸必须
     绑定"的延伸——banner 的框按最宽字段撑开,临时路径每次长度不同,框宽就跟着变。
     顺带 canonicalize 了 sandbox 根:子进程的 `current_dir()` 回来是解析过的
     `/private/var/…`,不 canonicalize 的话未解析的 `$HOME` 压根不是它的前缀,
     `display_cwd` 的 `~` 收缩不会发生。
3. **`wait_for_quiescent(idle)`**:`TerminalState` 加 `last_write`,等的是"最后一次
   写入之后 N 毫秒无新字节"。`wait_for` 抽出共同的 `wait_until`,语义一字未改。
   不是固定 sleep,仍走 reader 已经在 notify 的那个 condvar。
4. **`PtyOptions { args, files }` + `spawn_with_options`**:工具场景得有东西可读;
   `sse_tool_call()` 发 openai-compat 的 `tool_calls` delta。`spawn` / `spawn_with_args`
   签名不动。
5. 三个纯函数(`normalize_elapsed` / `is_elapsed` / `redaction_table`)各自带单测,
   其中一条锁的就是"同一条 turn-end 线,`0s` 与 `12m30s` 规范化后逐字相同、且宽度不变"。

### 六张基线(五个测试,均在 `tests/snapshots/`)

| 基线 | 尺寸 | 管住什么 |
|---|---|---|
| `resize_shrunk_16x60` / `resize_grown_24x100` | 16×60 / 24×100 | 缩放前后整屏,含 70 列输入的重新折行 |
| `two_turn_overflow_14x80` | 14×80 | 两次提交之后剩下的接缝 |
| `markdown_list_and_code_24x80` | 24×80 | 有序列表 + 嵌套项 + 代码块(新增场景) |
| `tool_call_and_result_24x80` | 24×80 | 一行工具调用 + 结果预览(新增场景) |
| `long_tool_result_truncated_24x80` | 24×80 | 超过 `PREVIEW_MAX_LINES` 的截断提示(新增场景) |

**原有 21 处点断言一条没删**,9 个既有测试全部仍绿。工具场景用 `read_file`——它是只读的,
按 `permissions` 的 `read_only_calls_skip_the_approver` 直接放行,不会卡在授权弹层上。

### 验证

1. 9 个既有测试 + 3 个新测试 + harness 的 5 个单测 = `--test tui_pty` 15 passed;
   `plain_pty` 2 个仍绿。
2. **同一条命令连跑三次,基线零 diff**(三次都是 12 passed、`*.snap.new` 数为 0)。
3. **negative control**:把 `tui/src/markdown.rs:44` 的 `CODE_INDENT` 从 `"  "` 改成
   `"    "`,`markdown_list_and_code_24x80` 一张变红、其余 11 个仍绿;改回即全绿。
   规范化没有把有效信息抹掉。
4. `cargo fmt --all --check`、`cargo clippy --workspace --all-targets --all-features
   -D warnings`、`cargo test --workspace` 各自单独跑并当场取退出码,全为 0。

### 一个开工时没预料到的点

第五节列了五个坑,真正绊人的是没列的那个:**规范化一个被宽度填充的行,只换里面的数字是不够的**。
turn-end 横线填满整行,elapsed 变长一个字符、`─` 就少一个——`<elapsed>` 替换完两行仍不相等。
凡是"某字段 + 填充到固定宽度"的行,规范化必须重建整行,不能只替换字段。已记为 HANDOFF 教训 142。
