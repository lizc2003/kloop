# Plan 160 — 一屏之内的三个体验问题

> 来源:2026-09-17,用户贴了自己屏幕上的一行 bash 工具行,说「有 3 个体验问题」。
> 三条互不相干,都在 TUI 层,**一次会话全部做完**(✅ 见文末)。

用户贴的那一行原文:

```
✓ Bash Experiment: ClientCode survives the 451 rename  $ mkdir -p /tmp/check451b && cd /tmp/check451b && cat > go.mod <<'EOF'module check451bg…
  └ classified: kind=ContentReject reason="vidu INVALID_PARAM" code="invalid_request"
```

三条问题按它提的顺序编号。

---

## 一、bash 的具体命令该不该另起一行 ✅

### 现状

`toolrow.rs` 的 `tool_label` 把 **summary 和命令拼进同一个 detail**
(`"{summary}  $ {cmd}"`),`header_line` 再把整条 detail 截到终端宽度。于是:

- 描述越长,命令被截得越狠;而**描述长恰恰是命令长的时候**(模型写长摘要,因为这条命令复杂)。
- 上面那一行 80 列,命令只活下来 40 个字符;真正要看的 `cat > go.mod` 之后的内容全没了。
- 还有一个**顺带的 bug**:`clean()` 把控制字符**整个丢掉**,于是 heredoc 里的换行直接消失,
  `<<'EOF'` 和下一行的 `module` 被黏成 `<<'EOF'module`——读出来是个不存在的 token。

### 裁决

**另起一行,而且不分有没有 description。** 理由是"一个形状":命令的 `$` 永远在同一列,
一屏 shell 调用扫下来是一列;若按有无 description 分两种形状,用户还得先判断这行是哪种。
多付的一行,换回来的是命令拿到整行宽度。

`powershell` 同样处理(`PS> …`)。

### 做了什么

- `tool_label` 的 bash 分支只留 `description`(可以为空),powershell 分支 detail 为空。
- 新增 `tool_command(name, input) -> Option<String>`:只有 `bash`/`powershell` 返回
  `$ …` / `PS> …`,`background` 标记跟着命令走。**`command` 字段不存在时返回 `None`**——
  参数还在流式到达的调用不该多出一行空的 `$ `。
- `tool_cell_lines` 在 header 与结果预览之间插这一行,缩进 2 空格,让 `$` 和预览的 `└` 同列。
- `tool_preview`(折叠的子 agent 行,只有一行可用)把三段拼回一行,保持原样。
- `clean()` 从"丢掉控制字符"改成"**所有空白折成一个空格**,其余控制字符才丢"。

---

## 二、粘贴的文本要在滚动区里完整显示 ✅

### 现状

大段粘贴在 composer 里是一个 `[Pasted #1: N chars]` 原子标签(plan 76 的设计,没问题)。
问题在 `on_enter`:它拿 `composer.text()`(**压缩显示文本**)去 push `Cell::User`,
而真正发给模型的是 `composer.submit()` 展开后的全文。于是**发出去的和屏幕上留下的不是一回事**。

顺带发现:这两条路**早就分叉了**。恢复会话时的重放路径(`app.rs` 的 `Injected` 分支)是从
history 取文本 push `Cell::User`,那是**展开后的全文**。同一条用户消息,实时看是标签、
`--resume` 之后看是全文。

### 裁决

标签是 **composer 的** 呈现方式,不是 transcript 的。turn 一旦发出,滚动区就该显示真正发出去的东西
——也正好和重放路径对齐。

### 做了什么

`on_enter` 的两处 `Cell::User` 改用展开文本(`sub.text` / `submit_text()` 的返回值,trim 后):
正常提交一处、steering 一处。slash 分支不动——它回显的是命令本身,且 `is_command` 判定读的
就是这份压缩文本。

---

## 三、esc:有草稿时两下清草稿,空的时候才是 cancel ✅

### 现状

```rust
(KeyCode::Esc, _) => {
    if self.running { return Command::Interrupt; }
    self.composer.clear();
}
```

`running` 优先。于是turn 跑着的时候写下一条消息写到一半,手一按 esc(想停的是 turn),
草稿还在、turn 被打断——或者反过来,想清草稿结果打断了 turn。两件事共用一个键,而且顺序
是错的:**草稿是花过力气的,turn 再起一次就有**。

### 裁决(用户当场纠正了第一版)

第一版做成了「有草稿 → 一下 esc 清掉」。用户指出 cc 不是这样:
**有草稿时第一下 esc 只提示,第二下才清;空的时候 esc 直接 cancel。**
这条更对——清掉草稿本身也是个不可撤销的破坏性动作,和退出一样值一次确认;
而中断一个 turn 没什么可失去的,不该多要一下。

### 做了什么

- 新增 `esc_clear_armed`,形状逐条抄 `ctrl_c_exit_armed`:第一下 arm 并显示提示,
  第二下才清,任何别的键 disarm。两个 arm 都在 `on_key` 开头 `mem::take`,
  所以每个分支(包括提前 return 的那些)对自己不拥有的那个 arm 都自动是「别的键」,
  两个提示不可能同时亮。
- `is_blank()` 本来就把附件算进去,所以贴上的图片也算草稿,同样两下。
- composer 空了,esc **第一下**就 `Command::Interrupt`,不 arm。
- 提示三处同步(`render.rs`):arm 着的时候 activity 行整行换成
  `press esc again to clear input`(和 Ctrl+C 用同一个槽位,空闲时也显示);
  没 arm 时草稿在说 `esc esc to clear input`、空了说 `esc to interrupt`。
  **提示不能说一个按下去会干别的事的键。**
- 代价说清楚:turn 跑着又有草稿时,中断要按三下(两下清草稿、一下中断)。
  这是「esc 的含义由 composer 决定」这条规则的直接推论,不是遗漏。

---

## ✅ 验收(2026-09-17,两次提交:第二次是用户纠正第三条之后)

`cargo fmt` + `cargo clippy --all-targets --all-features`(零警告)+ `cargo test` 全绿
(34 个 test target)。新增/改写的测试:

| 测试 | 守住什么 |
|---|---|
| `toolrow::bash_row_leads_with_the_description_and_keeps_the_command_below` | 三种输入(有/无 description、background)的整对象行断言 |
| `toolrow::a_heredoc_command_keeps_full_width_and_does_not_glue_its_lines` | 用户贴的那条命令:命令拿到整行宽度,换行读成空格而不是黏住 |
| `toolrow::folded_preview_keeps_summary_and_command_on_one_line` | 折叠子 agent 行只有一行,命令不另起 |
| `render::long_tool_rows_truncate_to_one_line_each` | 两行各自不折行 |
| `render::transcript_renders_all_cell_kinds` | 参数还在流式的调用**没有**命令行 |
| `app::a_pasted_block_is_echoed_in_full_not_as_its_placeholder` | composer 里是标签、transcript 里是全文;steering 同样 |
| `app::esc_is_a_two_tap_clear_then_a_one_tap_interrupt` | 第一下只 arm、别的键 disarm、第二下才清;空 composer 一下就 Interrupt;附件同样两下 |
| `app::the_esc_and_ctrl_c_arms_disarm_each_other` | 两个 arm 互相 disarm,不可能同时亮,谁也不吞掉对方的第一下 |
| `tui_pty::a_shell_command_gets_a_row_of_its_own` | **新整屏基线** `bash_command_row_24x80`:真二进制跑一条 `seq`,header + 命令行 + 预览 |

README 同步五处:工具行形状、esc 语义(TUI 段 + 中断段)、粘贴标签的作用域、状态行文案。
