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

## 同日复审修正(2026-11-16)

> 第一轮的粘贴方案(快照 + 旧字节位)在同日第二轮复审后被**预占**方案取代，见下节；这一节保留
> 当时的推理，`popup_generation` 与缓存槽位化仍为最终实现。

- **异步粘贴的归属**：`PasteSnapshot`(draft generation + 当时的文本 + 光标字节位)随任务走。
  落地时 generation 不符(草稿已提交/清空)就丢弃并留一条 note；记录的位置仍是文本里的接缝
  就插在那里(光标挪过也不影响)，接缝没了才退回光标并说明。`Composer::attach_image_at` 只在
  字素边界且不落在 atom 内时插入，否则把 source 交还。粘贴在途时 Enter 被暂存
  (`enter_waits_for_paste` 只拦真正会提交的 Enter，弹窗/面板的 Enter 不拦)，最后一个粘贴落地
  后回放，图片因此随这条消息发出而不是下一条；读取任务用 `spawn_blocking` 的 JoinError 兜底，
  计数不会因 panic 卡住 Enter。
- **关闭补全作废在途搜索**：`App::close_popup` 关闭菜单时 bump `popup_generation`，搜索任务带上
  它，`search_lands` 同时要求序号最新且 generation 未变——Esc 不改 token，只靠目标校验会让结果
  把菜单重新打开。
- **缓存查找改成按槽位**：`ShownCache.entries` 与 `App::cells` 下标对齐(每帧重建、越界槽丢弃)，
  每帧 O(cells) 而不是每 cell 扫一遍缓存；提交移动下标后由内容比较兜底重画。
- **验证**：kloop-tui 338 项(新增 6 项：粘贴位置/代际、菜单关闭作废搜索、Enter 等待条件、
  搜索落地条件、缓存槽位跟随提交)；`cargo test -p kloop --test tui_pty` 25 项仍全过；
  clippy、fmt。

## 第二轮复审(2026-11-16)

- **P1：延迟的 Enter 会替后来的审批框回答 Yes**。回放原始按键会把 Enter 送进面板路由。改为
  只记草稿代际，落地时走 `App::submit_draft()`(不经过按键路由)，且要求代际未变、没有弹窗/
  面板/选择器占据键盘；否则直接丢弃，草稿留在输入框里。
- **P2：清空草稿不撤销延迟提交**。延迟提交绑定代际，两段 Esc 清空会 bump 代际，旧提交作废。
- **P2：粘贴位置只是未校验的旧字节位，且两张图会撞位**。改成**预占**：粘贴一发起就在光标处
  插入 `AtomPayload::PendingImage`(带 token)的 `[Image #N]` 标签，标签从此是草稿的一部分，
  编辑按已有 atom 规则带着它移动，第二张自然排在后面；读取完成时按 token 填充，失败则把标签
  换成原文粘贴文本(位置不变)，标签已被删/草稿已发送则 refuse。`Composer::attach_image_at`
  随之删除。
- **验证**：kloop-tui 344 项(新增 5 项：预约标签随编辑移动、按粘贴顺序而非完成顺序、废弃预约
  的文本落位、删除后拒绝填充、按住提交在换草稿/面板出现时作废)；PTY 24/25——唯一失败项是
  `a_shell_command_gets_a_row_of_its_own`，它跑真实 `bash` 工具调用，而当前测试进程自身在
  kloop 的 seatbelt 里(`sandbox-exec: Operation not permitted`)，同一环境在上一提交(第一个
  修复前)也以同样方式失败，需要在沙箱外终端跑(plan 283 的既有约束)；clippy、fmt。

## 第三轮复审(2026-11-16)

- **P1：无图时删预占位留下越界光标**。`abandon_image` 现在把光标映射过被删标签(标签在光标
  前就减长度，在后就落到标签起点；正文中光标不可能在标签内)，并把测试跑一次
  `ComposerLayout::new` 证明不再越界。
- **P2：历史召回偷走异步粘贴与提交意图**。文档代际下沉到 `Composer`：`take_text`、`clear`、
  `restore`、`history_prev/next/load` 每次换文档都 bump；`App::draft_generation` 直接返回它，
  因此召回后旧的延迟提交自然作废。`resolve_image`/`abandon_image` 同时查找当前文档与召回时
  暂存的 `draft`，图片落在粘贴时的输入上，按 ↓ 回来即可见；失败文本同理。
- **P2：填充图片会关掉期间新开的补全菜单**。填充只换 atom 的 payload，文本和光标都没动，所以
  `App::resolve_image` 不再 `close_popup()`；菜单保留，也继续占着 Enter(延迟提交因此按“界面
  已被占”丢弃，而不是绕过检查提交)。
- **验证**：kloop-tui 348 项(新增 4 项：废弃预占后光标仍在文本内并可继续布局、预占跨历史召回
  存活、召回是新的文档代际、填充不关闭期间的菜单；按住提交的用例补了“弹窗占键盘”一格)；
  clippy、fmt；PTY 24/25(唯一失败项仍是需要沙箱外终端的真实 bash 用例)。

## 第四轮复审(2026-11-16)

- **P2：召回期间的失败粘贴把文本写进了可见文档**。`abandon_image` 返回的字节位属于“持有 token 的
  文档”，调用方却在当前 `document` 里插入。改成 `Composer::paste_text_at_reservation` 在**所属
  文档内**一次完成删除标签 + 插入原文，并用 `ReservationPaste::{Visible, Stashed, Gone}` 告诉
  `App` 是否改变了可见文本（只有 Visible 才重新同步补全菜单）。
- **P2：删标签后光标可落在字素内部**。只按长度回退只保证不越界；两侧字符可能合成一个字素
  （两个 regional indicator、基字 + 组合符）。`remap_cursor` 现在回退后再
  `snap_grapheme_boundary`，插入原文后同样校准；测试用 🇨[Image #1]🇳 把光标停在标签末尾复现。
- **验证**：kloop-tui 350 项（新增/改写 3 项：废弃预占后原文落位与光标、召回暂存草稿里的原文
  回退、删除标签后光标必在字素边界并可布局）；clippy、fmt；PTY 24/25（同上，真实 bash 用例）。
