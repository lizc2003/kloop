# Plan 162 — 挑一个会话,不该先读一串 id

> 来源:2026-09-18,用户贴了一张 cc 的 `Resume session` 截图,只说了一句「体验更好一些」。
> 开工前问了一个点(做到哪一档),用户答**核心档**。当场做完(✅ 见文末)。

## 现状

`--resume` 不带 id 时(`cli/src/args.rs::pick_session`)是这样的:

```
saved sessions (most recent first):
  1. 20260911-061714  Output every integer from 1 to 5000 in order, separated by c…
  ...
resume which? [1-8, empty = 1] >
```

stdout 打一串编号、stdin 读一个数字。八条还能数,七十四条就是先读 id、再数行号。
而且**每一条都读了两遍整个文件**:`resumable_sessions` 用 `load_session` 过滤空会话,
`session_line` 再 `load_session` 取标题——`inspect_session` 读全文件、逐行解析 JSON、
跑一遍 pairing repair,只为拿第一句话。七十四个一兆的会话就是一百多兆的读+解析。

## 裁决

### 一、这次只做核心档

做:全屏列表、两行一张卡片、输入即搜、`↑↓` 选、Enter 进、Esc 取消。

**不做**cc 底部那排:`Ctrl+A` 跨项目、`Ctrl+B` 只看当前分支、`Ctrl+W` 全部 worktree、
`Space` 预览、`Ctrl+R` 重命名。其中「按分支过滤」还压着一个前置:**kloop 的会话文件
根本不记 git 分支**(`LineMeta` 只有 id/parent/subagent_of/ts),要先往 preamble 里加字段,
而已经在盘上的会话补不出这个值。留给以后。

### 二、第二行是`时间 · 大小`,不是`时间 · 消息数`

消息数是现成信息——但它**只有整文件解析之后才有**。picker 在第一次按键之前就要画完
每一行,这正是不能付的那笔钱。cc 在这一栏放文件大小,理由同一条:它在 `stat` 里。

于是副标题 = `19 hours ago · 1.2MB`,fork 的会话追一个 `· forked from …`。

### 三、`session_digest`:读到第一句用户的话就停

新的 `rollout::session_digest` 逐行读,拿到第一条 user 文本就 `break`,顺带带出
first-line 的 lineage、mtime、文件长度。坏行跳过(`parse_session` 对断尾也是这个态度),
一个损坏的会话仍要能列出来,而不是让整个列表打不开。

`--list-sessions` 不变,仍走全量 `load_session`:它是诊断视图,低频,而且消息数正是它该报的。

### 四、picker 住在 tui crate,只有二十行碰终端

`Picker` 折按键成状态,`Picker::lines` 折状态成行,两个都是纯函数,单测覆盖搜索、
边界、滚动、空结果;`pick_session` 只负责 raw mode + inline viewport + 一个 `read()` 循环。

入参是 `SessionEntry { id, title, age, bytes, badge }`——**age 是时长不是时间戳**,
渲染因此是纯函数,一张整屏基线才钉得住。

### 五、Esc 是退出,不是错误

取消应当安静退出(exit 0),不能走 `bail!` 打一行 `Error:`——用户没选,不是出了错。
`open_history` 因此返回 `Option`,`SessionState::open` 跟着返回 `Option`,一路到
`Ok(ExitCode::SUCCESS)`。

### 六、收尾:回到视口顶必须在 drop **之后**

ratatui 的 inline viewport 在 `Terminal` drop 时把光标停到视口**底部**(它假设最后一帧
要留在 scrollback 里)。picker 的最后一帧是 chrome,要抹掉,所以顺序是
`clear()` → `drop(terminal)` → `MoveTo(0, viewport_top)` + `Clear(FromCursorDown)`。
顺序反过来就是:屏幕清空了,然后 `[resumed session …]` 出现在第 24 行,上面二十三行空白。

### 七、视口按列表长度要,不按整屏要

三个会话不该让一块 50 行的屏幕变空。高度 = `chrome + 3×条数`,上限是终端高度。

### 八、非终端照旧

管道、测试夹具、拒绝 raw mode 的终端——`stdin`/`stdout` 不是 tty 就退回原来的编号列表,
`pick_index` 那条路和它的测试原样留着。

## 顺带

`first_user_snippet` 拆出一个 `max_chars`:stdout 那行仍是 60,picker 取 200 再按终端
宽度截——否则 80 列的屏幕上,标题在 60 字符处就断了,右边空着二十列。

## 做了什么

- `core/rollout.rs`:`SessionDigest` + `session_digest`;`origin_of` 从 `session_origin`
  里抽出来共用;`SessionOrigin` 补 `Clone`;`user_snippet(messages, max_chars)`。
- `tui/src/session_picker.rs`(新):`Picker`(状态/搜索/滚动)、`lines`(布局)、
  `relative_age`/`human_bytes`、`pick_session`(终端循环)。`tui` 新增 dev-dep `insta`。
- `cli/src/args.rs`:`Resumable` 带上 digest,`resumable_sessions` 不再 `load_session`;
  `pick_session` 分岔成 picker / stdout 回退;`open_history` 返回 `Option`。
- `cli/src/main.rs`:`SessionState::open` 返回 `Option`,取消即 `ExitCode::SUCCESS`。

## ✅ 验收(2026-09-18,一次提交;提交号以本条所在提交为准)

`cargo fmt` + `cargo clippy --workspace --all-targets --all-features -D warnings`(零警告)
+ `cargo test --workspace` 全绿。

| 测试 | 守住什么 |
|---|---|
| `session_picker::the_whole_screen_reads_as_a_list` | 整屏基线 `session_picker_24x80`:标题、搜索框、项目名、三张卡片、hint |
| `session_picker::typing_filters_and_the_count_follows` | 输入即搜;`(1 of 1)`;退格退回全表 |
| `session_picker::a_filtered_enter_opens_the_match_not_the_row_it_sits_on` | Enter 开的是**过滤后**那一条的原始下标 |
| `session_picker::enter_on_an_empty_result_opens_nothing` | 无匹配时 Enter 不落到"第一条" |
| `session_picker::the_cursor_stays_inside_a_list_that_shrank_under_it` | 列表在光标底下变短 |
| `session_picker::arrows_stop_at_both_ends` / `esc_and_ctrl_c_cancel` | 边界与取消 |
| `session_picker::a_long_list_scrolls_by_whole_entries_and_marks_the_overflow` | 整条滚动 + `↑`/`↓` 溢出标记 |
| `session_picker::a_short_list_does_not_ask_for_the_whole_screen` | 视口高度按条数 |
| `session_picker::ages_and_sizes_read_the_way_a_person_says_them` | `16 seconds ago`/`1 day ago`/`574.1KB`/`1MB` |
| `rollout::session_digest_stops_at_the_first_user_line` | 早停;末尾断行不影响;`bytes` = 文件长度 |
| `rollout::session_digest_marks_a_preamble_only_file_as_empty` | 空壳不进列表 |
| `rollout::session_digest_carries_lineage` | fork 的 origin 从第一行免费读出 |
| `args::an_empty_shell_never_wins_the_continue_pick` | 过滤改走 digest 后,旧契约不变 |

另外在 PTY 里跑了真二进制(`--mock --resume`,24×80,八个真实会话):列表、`prime` 搜索、
`↓`+Enter 进入会话、Esc 退出后屏幕干净且下一行接在视口顶——都核过。

README 同步:`--resume` 段补 picker 的键位、回退条件、视口与清屏行为、digest 的理由。
