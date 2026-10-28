# Plan 262 — TUI 正确性

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：待实现。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
> 先读代码确认，不成立就在本文件划掉并写明原因。渲染缓存与事件循环阻塞在 plan 263。

## 问题

1. **`--resume` 选择器出错时不恢复终端**(已复核)。`tui/src/session_picker.rs:380`、`:385`
   `terminal.draw(..)?`、`event::read()?` 直接返回;`disable_raw_mode` 只在 `with_options` 失败与
   循环正常结束后各调一次，也没有 Drop 守卫。最容易的触发：选择器开着时改窗口大小，ratatui 的
   inline autoresize 经裸 `CrosstermBackend` 发 CPR,终端不应答 2 秒超时 → 进程带着 raw mode
   退出(`cli/src/args.rs:509` 再 `?` 上抛)。主 TUI 为此专门做了 `PinnedBackend`(教训 268)。
2. **`@` 补全想跳过 `.git/.hg/.svn/.jj`,实际只跳过 `.jj`**(已复核)。`core/src/fs_complete.rs:30-32`、
   `99-101`:循环里四次 `filter_entry`,ignore 0.4.33 的实现是 `self.filter = Some(..)`,后一次覆盖
   前一次;walker 又 `hidden(false)`。git 仓库里空查询把 `.git/HEAD`、`.git/config` 排在最前，
   `.git/objects` 先占满 4000 的扫描上限。测试 `empty_query_lists_shallowest_first_and_skips_git_dir`
   里的 `.git` 是空目录，等于没测到。
3. **两套显示宽度算法混用**(已复核代码)。`tui/src/text_layout.rs:135` `display_width` 按字符串
   (`UnicodeWidthStr::width`);`tui/src/markdown.rs:702-707` `display_width`/`chars_width` 逐 char
   相加，`:831` `hard_wrap_chars`、`:1012` `truncate_chars` 也是。"❤️" 字符串宽 2、逐字符 1;
   家庭 emoji ZWJ 序列字符串宽 2、逐字符 8。markdown 段落与表格(`:628` 列宽、`:988` 单元格截断)
   因此列错位、提前换行;`:153`、`:168` 又走 `text_layout::wrap`,自己内部也不一致。
4. **粘贴的 Tab 等控制字符：宽度算 1,渲染被丢掉**(待复核)。`composer.rs:562`、`1076`、`698`:
   unicode-width 对控制字符给宽度 1,ratatui 的 `Span::styled_graphemes` 过滤掉含控制字符的字素;
   `canonicalize_paste_newlines`(`app.rs:47`)只处理 `\r`。小粘贴一段 Tab 缩进的代码，第二行显示
   成无缩进，光标每个 tab 偏右 1 列，软折行提前;提交后的 `Cell::User` 回显同样丢缩进。
5. **超高块 `as u16` 截断丢行**(待复核)。`tui/src/terminal/scrollback.rs:91`
   `let height = lines.len() as u16`:单个 cell 渲染超过 65535 行(`-c` 恢复含巨大输出的会话再在窄
   终端提交)时取模，其余行既不在屏幕上也不进 scrollback。
6. **`@` 触发不避开原子标签**(疑似)。`tui/src/menu.rs:64-75` `detect_trigger`:大粘贴首行以 `@`
   结尾时标签是 `[Pasted #1: … · cc @]`,光标在原子末尾 token 为 `@]`;仓库里有 `app/[id]/page.tsx`
   时菜单弹出，Tab 接受后 `replace_range` 的起点落在原子内部、被映射到原子起点，整段粘贴被替换。
7. **注释与行为不符**:`lib.rs` 的 `startup_allows_key` 文档注释(约 1437 行)与 `app.rs:1525` 说
   "Ctrl+D stays immediate",测试(`app.rs:4043`、`4348`)表明 Ctrl+D 已禁用;`startup_allows_key` 仍
   放行 Ctrl+D,在 `on_key` 里是空操作。
8. **panic hook 从不恢复**(观察)。`run()` 里 `std::panic::set_hook` 持有一份 `event_tx` 克隆，TUI
   返回后仍挂在进程上;目前只会对已关闭通道发送失败，无害。

## 开工先问用户

- 第 4 条 Tab 展开成几个空格?推荐 4(与多数编辑器默认一致);其余控制字符去掉(与 toolrow 的
  `clean()` 一致)。也可以改成让 `display_width` 把控制字符算 0 宽，但那样粘贴进去的 Tab 仍不可见。

## 做法

1. 选择器的 enable/disable raw mode 与清 viewport 包进 Drop 守卫;resize 走与主 TUI 相同的
   `PinnedBackend` 思路(或不在选择器里做 autoresize 的 CPR 查询)。
2. 合成一个 filter(`!matches!(name, ".git" | ".hg" | ".svn" | ".jj")`),或照 `search.rs` 的
   `build_walk` 用 `OverrideBuilder`;测试里给 `.git` 放真文件。
3. markdown 删掉自己的宽度函数，带样式的字符按字素簇用 `text_layout::display_width` 计宽;
   `truncate_chars` 复用 `text_layout::truncate`。
4. 在 `App::paste_text`/`Composer::paste` 入口按问答处理控制字符。
5. `insert_batches` 对超高块按 `u16::MAX`(或屏高的若干倍)切片。
6. Composer 暴露 `range_touches_atom`,`detect_trigger` 在 token 与原子相交时不触发。
7. 改注释，或从放行名单去掉 Ctrl+D。
8. `run()` 结束时恢复原 panic hook。

## 验收

- 单测(旧代码先红):选择器的 draw/read 出错路径恢复 raw mode(用可注入的 backend);`.git` 下
  有文件时补全不出现它们;含 emoji 的表格列宽与 `text_layout` 一致;Tab 缩进粘贴后的显示与光标列;
  7 万行 cell 全部进 scrollback(可缩小常量测);原子末尾的 `@]` 不弹菜单。
- PTY 快照如有变化逐一说明;外层执行沙箱之外 `make check`;HANDOFF 记进展与教训。
