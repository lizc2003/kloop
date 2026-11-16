# Plan 263 — TUI 渲染缓存、事件循环与面板去重

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：✅ 2026-11-16 完成(plan 262 已完成)。行号是 2026-10-28 的读数，实际按函数定位。

## 问题

1. **每帧把整段流式回答重复解析 4–6 次**(已确认)。
   - `render::draw` → `visible_transcript` → `shown_cells` 算一遍;`lib.rs:945` `commit_overflow`
     再算一遍;提交时 `draw_and_hand_over`(`lib.rs:1046`)第三遍。
   - 每遍里 `markdown::assistant_stream`(`render.rs:715-727`)先由 `stream_cut` → `last_block_line`
     normalize + 全文 parse,再 `markdown_lines(prefix)` parse + synoptic 高亮(`markdown.rs:135`、
     `1065-1122`)。仍处 live 但不是最后一个的 Assistant cell 还会先算 `assistant_stream` 再丢掉
     `streamed`、另跑 `cell_lines`。
   - running 期间 tick 每 100ms 一次(`anim::STEP_MS`),整段流式总代价约 O(帧数 × 全文长度);
     每帧还算 4 次以上 `composer.view`(draw、`live_chrome_layout` → `composer_height`、
     `overflow_active_h` 两处)。
2. **事件循环里的同步或串行耗时操作**(已确认)。
   - `lib.rs:1251`/`1332` `Command::SearchFiles` 每次按键 `search_files(..).await` 一次全树遍历，
     没有防抖，不取消过期搜索;
   - `lib.rs:1326` Ctrl+V 的 `clipboard::clipboard_image()` 同步做 arboard 读取与 PNG 编码;
   - `lib.rs:1247` 拖入图片路径时 `load_image_paste` 同步 `std::fs::read`;
   - `lib.rs:1384` `absorb` → `resolve_cwd_branch` → `git_branch` 同步起 git 进程。
   这期间 spinner 停住、流式输出卡住。
3. **`is_grapheme_boundary` 每次从头扫**(疑似，长草稿才明显)。`text_layout.rs:77-86`
   `text.grapheme_indices(true).any(..)`;一次编辑里 `ComposerDocument::replace` 的 `assert_range`、
   每个原子的 `breaks_boundary`、`separate_image`、光标对齐、`detect_trigger`、`replace_range` 都调。
4. **重复代码**。
   - 四个面板的键盘导航：`app.rs:1934-1952`(confirm)、`1988-2024`(question)、`2104-2125`
     (provider)、`2160-2175`(fork);数字选行、`Up|k`/`Down|j` 加 clamp、PgUp/PgDn ±10 各写一遍，
     细节已开始分叉(question 上限是 `option_count + 1`)。
   - `app.rs:1494-1517` `drain_committed` 里六段相同的 `retain { *i = i.wrapping_sub(n); *i < len }`。
   - 状态行表头(`{mark} ` 着色 + `truncate(title, width-2)`):`render.rs:389`、`407`、`451` 与
     `toolrow.rs:69`。
   - worker 收尾：`lib.rs:495-503`、`523-531`、`608-616` 三处"发 Usage → 发 TurnEnded → 通道关闭就
     return";`run()` setup 失败的 teardown(`263-270`)与正常退出(`329-341`)逐行重复。
     (跨前端的统一在 plan 269,这里先收成 TUI 内部 helper。)

## 做法

1. 每帧只算一次 `shown_cells` 与 chrome,传给 commit;已封口 cell 的行按
   `(cell 下标, 修订号, width)` 缓存;非 last 的 live cell 只在需要 settled 时调 `assistant_stream`;
   `composer.view` 每帧算一次。
2. 搜索、剪贴板、读文件放进 `spawn_blocking`,结果经 channel 回送(`set_file_results` 已有"过期就
   丢弃"的校验，直接复用);`@` 搜索加防抖或取消上一次;分支查询异步回填。
3. 改用 `unicode_segmentation::GraphemeCursor::new(offset, len, true).is_boundary(text, 0)`。
4. 抽 `fn list_key(key, cursor, rows) -> ListKey { Moved, Pick(row), Page(i8), Other }`、
   `shift_indices(map, n, len)`(或把这些 map 收进一个结构体)、`status_header(..)`、
   `finish_turn(events, cfg, history, reason) -> bool`、`teardown(cfg, ui)`。

## 验收

- 去重提交行为零变更：现有 app/render 单测与 PTY 快照不变。
- 缓存提交：补"同一帧内 markdown 解析次数"的计数测试(测试钩子),流式 1 万行回答的解析次数随帧数
  线性而非与全文相乘;PTY 快照不变。
- 事件循环：用慢速搜索夹具断言 tick 与 agent 事件在搜索期间仍被处理。
- 外层执行沙箱之外 `make check`;DESIGN 的 TUI 渲染一节先读再改;HANDOFF 记进展与教训。

## 完成记录(2026-11-16)

- **渲染**：新增 `render::FramePlan`(尾部 cells + chrome + composer 视图),`draw` 与
  `commit_overflow` 读同一份;`render::ShownCache` 跨帧缓存不可变 cell;`FramePlan::transcript`
  只克隆可能进视口的那几行。提交后重画按新状态另算一份 plan(旧路径本来也要重画)。
- **缓存键**：(`cell` 内容, width, live),不是(下标, 修订号, width)——见 HANDOFF 教训 395。
  开放的最后 cell 每帧重算(文本在增长、thinking 时钟在走)。
- **事件循环**：`@` 搜索(60ms 防抖 + 序号作废)、Ctrl+V 剪贴板、拖入图片路径读取、cwd 变化后
  的 `git` 分支查询,全部 `spawn_blocking` + `UiTask` 回送,过期结果丢弃;`on_input`/`on_command`
  改回同步。图片路径读取失败仍把原文按普通粘贴处理(与原行为一致,只是晚一拍)。
- **字素边界**：`GraphemeCursor::new(offset, len, true).is_boundary(text, 0)`,整串当一个 chunk。
- **去重**：`list_key`/`ListKey`、`shift_indices`、`mark_span`/`status_header`、`finish_turn`、
  `teardown(cfg, ui, worker)`。setup 失败与正常退出共用 teardown,worker abort 仍夹在后台
  关闭与 worktree 收尾之间(顺序未变)。
- **验证**：kloop-tui 332 项 lib 测试(新增 7 项:`list_key` 表、字素边界逐字节等价、图片路径
  识别、搜索防抖/作废、`ShownCache` 每帧解析次数(10k 行回答)、真实 `draw_frame` 路径解析
  次数、plan transcript 与旧展平结果一致);`cargo test -p kloop --test tui_pty` 25 项(快照
  未变);`clippy -p kloop-tui --all-targets -D warnings`、`cargo fmt --check`、
  `cargo check --workspace`。未跑全量 `make check`、parity、真实 API。
- **教训**：HANDOFF 394(合并绘制路径时丢掉帧交接,PTY 才看得出来)、395(内容相等当缓存键)。
