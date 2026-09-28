# Plan 213 — 起草 206–212 时顺手看到的四件小事

> 来源:2026-09-24 起草 plan 206–212(`refs/pi` 复查)时,几个起草 agent 读代码顺带发现的问题,
> 当时记在 HANDOFF「〇‴」一节;用户看完说「四个小问题,写个 plan 修啊」。四件互不相干,
> 放一条 plan 是因为每件都只有几行,**但分四个提交**(一件一个,各带测试),出问题好回退。

## ✅ 已完成(2026-09-28)

四个提交,顺序 4 → 3 → 2 → 1,每个提交前 `make check` 全绿:

| 件 | 提交 | 标题 |
| --- | --- | --- |
| 4 | `f4c3283` | `fix(plan213): correct the COMPACT_SYSTEM comment about tools and caching` |
| 3 | `f1d797a` | `fix(plan213): stop Alt+letter from typing the letter` |
| 2 | `49a230c` | `fix(plan213): send slash commands expanded, not their paste placeholders` |
| 1 | `4139d8e` | `fix(plan213): park a known slash command in the composer mid-turn` |

**开工问答结论**:第 1 件按推荐——已知命令留在输入框 + 一行提示,轮次结束后再按 Enter
(用户「行,按推荐来」)。第 2 件按推荐——只记 HANDOFF,不立 plan,等 212 的评测量出摘要成本再说。

**实现与 plan 的出入**:
- 第 1 件的提示不是一条 cell,而是渲染层的一行 chrome(`render::held_command_line`),与活动行同槽、
  经共享的 `live_chrome_layout` 预留行数,所以"画出来"和"冻结高度预算"两处不会各算各的。状态
  **推导**而非武装(`App::held_command()` 读 composer + 目录),用户把 `/` 改掉就自然消失,没有可以
  变馊的 flag。
- 第 1 件的第 3、4 条测试(运行中 skill 名、空闲 `Command::Slash`)注意:空闲时输入 `/compact` 会打开
  斜杠补全菜单,Enter 是"接受补全"而非提交,所以空闲路由那一条用空目录的 `App::new` 断言。
- 第 3 件:`Char('v') + ALT` 仍是 `PasteClipboardImage`(它在按键 match 之前),`CONTROL|ALT` 仍落到
  `_ => None`,都在新测试里钉住了。

## 一、四件事

### 1. 运行中输入的 `/命令` 被当成插话发给模型

`on_enter`(`tui/src/app.rs:1576`)只在空闲时把斜杠行当命令(`:1590`,`!self.running &&
is_command(..)`);运行中所有文字一律走 `Command::Steer`(`:1597-1610`),注释写明"While a turn
runs, a '/'-line is steering"(`:1589`)。于是运行中敲 `/compact`,模型收到一句字面上的
"/compact",既不压缩,还可能让模型困惑。运行中斜杠菜单也关着(DESIGN.md「a `/` line is then
steering text」),用户没有任何提示。

**坑:不能按 `is_command` 拦。** `is_command`(`core/src/commands/mod.rs:189`)只看"`/` 后面紧跟
非空白字符",`/usr/bin 这个路径不对` 也成立——那是正常插话,不能拦。**只拦名字是已知命令或 skill
的行**:`App.commands`(`app.rs:549`,就是斜杠菜单的目录,内置命令 + skills/user commands)里
查第一个词。未知名字照旧当插话发出去。

处理方式是开工问题(第三节第 1 问)。推荐**留在输入框里、给一行提示**
(`/compact runs when the turn ends — press Enter again then`,措辞开工时定):草稿不丢,用户
轮次结束后再按一次 Enter 即可。不推荐"排到本轮结束后自动执行":`/provider`、`/clear`、`/compact`
这类改会话状态的命令在用户没盯着的时刻自己跑,比晚一点跑更让人意外;要排队还得处理排了几条、
被 Esc 打断时怎么办,那是 follow-up 队列的规模,用户已判不做。

server 端不动:`turn/steer`(`server/src/lib.rs:517`)是客户端显式调用的方法,客户端自己知道
发的是插话;斜杠命令走 `turn/start` 的输入。

### 2. 斜杠命令后面粘贴的内容没有展开

`on_enter` 用 `self.composer.text()`(显示文本,粘贴块是 `[Pasted #N: …]` 占位)做命令检测,
**也把它原样当命令行发出去**(`app.rs:1585,1591,1595`);`submit_text()` 返回的展开文本被
`let _ =` 丢掉(`:1591`)。于是 `/compact <粘贴的一大段>` 交给摘要器的是占位标签;skill 参数
(`/name <粘贴>`,经 `result.run_turn` 变成一条 user 消息,`tui/src/lib.rs:518`)同病——
**模型收到的是 `[Pasted #1: 2345 chars]`,不是内容**。

修法:检测仍用显示文本(占位标签不以 `/` 开头,不会误判),**发出去的用 `submit_text()` 的展开
文本**,回显(`Cell::User`)仍用显示文本——斜杠命令不记 user 消息,回显只是"已受理"的信号,
一大段粘贴刷进 scrollback 没有意义。注意 `submit_text` 已 trim 过判空但没 trim 内容,和现在
`display` 的 `.trim()` 对齐。

另一个发 `Command::Slash` 的地方(`app.rs:1969`,provider 选择器拼出来的命令)没有粘贴,不动。

### 3. Alt+字母会插入字母本身

按键 match(`app.rs:1435` 起)是 `match (key.code, ctrl)`,插字符那一臂 `(KeyCode::Char(c), false)`
(`:1473`)只排除了 Ctrl,没看 Alt(`alt` 在 `:1428` 已经算好,只给 Ctrl/Alt+V 用了)。终端把
Option/Alt 当 Meta 时,Alt+B 发 `ESC b`,crossterm 解析成 `Char('b') + ALT`,于是**macOS
Terminal.app 上 Option+← 打出一个 `b`**(它默认就发 `ESC b`)。

修法:插字符只在**既无 Ctrl 也无 Alt**时发生;Alt+字符(且无 Ctrl)吞掉(`Command::None`)。
Ctrl|Alt 同时按下(Windows 上的 AltGr)现在就落在 `_ => None`,不改变——plan 211 草稿里核过。
**本条只做"不再插入",不做按词移动**:Alt+B/F、Alt+←/→ 的词移动是 plan 211 的事;本条先落地,
211 那张键位表里"未绑定的 Alt+字母吞掉"那一行就已经成立(211 开工时按本条的实现核一遍即可)。

Ctrl+←/→ 现在忽略修饰键移一个字符(`(KeyCode::Left, _)`),那是缺功能不是 bug,留给 211。

### 4. `COMPACT_SYSTEM` 上方的注释说反了

`core/src/compact.rs:59-64` 说摘要请求"ships the session's full tool set (the tools are part of
the cached prefix; dropping them would cost a full re-prefill)"。实际 `sample_summary`
(`compact.rs:724`)传的是 `&[]`,测试也断言 `seen[0].tools.is_empty()`(`compact.rs:1711`);
system prompt 也是 `COMPACT_SYSTEM` 而不是会话的。这句注释是 plan 146(`f2ab181`)照 cc 的形状
写的,cc 确实带全套工具以复用缓存前缀,kloop 从来没有。

修法:改写注释为实情——摘要请求用自己的 system、不带工具,所以**和主会话不共享缓存前缀,输入
按全价计**;"no-tools 规则放在最前"仍保留理由(模型仍可能在文本里试图调工具、把唯一一次机会
花掉,这是 cc 在 adaptive-thinking 模型上踩到的),但不再引用"工具在缓存前缀里"。

**只改注释,不改行为。** "摘要要不要改成带会话 system + 工具以复用缓存"是一个真问题(plan 210
草稿也注意到摘要输入全价),但它牵动 plan 200 的缓存纪律、摘要质量和 `COMPACT_SYSTEM` 的 load-bearing
条款测试,不是小修——**在 HANDOFF 记一笔,另议**。

## 二、顺序与提交

四个提交,顺序无所谓,建议 4 → 3 → 2 → 1(从零风险到要问用户的)。每个提交单独 `make check`。
第 1、2 件都改 `on_enter`,**先做 2 再做 1**,免得 1 的拦截分支再去碰一遍取文本的那几行。

与其他 plan 的关系:
- **plan 211** 改的也是 `app.rs` 按键分发(要把 `match (key.code, ctrl)` 改成 `composer_action`
  纯函数)。本条第 3 件只动一个 match 臂,211 开工时顺着改即可;若 211 先做,第 3 件就随 211 完成,
  本条划掉。
- **plan 209**(`/compact <focus>`)依赖第 2 件:没有它,粘贴进 focus 的内容到不了摘要器。209 草稿
  第三节已记这件事;**本条先于 209 做**,或 209 开工时顺手做掉并在这里划掉。

## 三、开工时必须问用户的点(一次问一个)

1. **运行中输入已知命令,怎么处理?**
   - **推荐:留在输入框 + 一行提示**,轮次结束后再按 Enter。
   - 排到本轮结束后自动执行(见第一节第 1 件为什么不推荐)。
   - 当场执行不改会话状态的那几个(`/help`、`/cost` 之类)、其余照推荐处理——要逐个命令归类,
     以后加命令漏归类就错,除非用穷尽 match 强制归类。
2. **第 4 件的"摘要复用缓存"要不要立 plan?** 推荐先只记 HANDOFF,等 212 的评测能量出摘要成本再说。

## 四、测试

- **第 1 件**(`app.rs` 单测,整对象断言 `Command` 与 composer 状态):运行中 `/compact` + Enter →
  `Command::None`、输入框原样保留、有一条提示 cell/状态;运行中 `/usr/bin 不对` + Enter →
  `Command::Steer("/usr/bin 不对")`;运行中 skill 名 → 同 `/compact`;空闲时 `/compact` 照旧
  `Command::Slash`。
- **第 2 件**:粘贴一段多行文本后前面补 `/compact ` → `Command::Slash` 带展开后的全文、回显 cell
  是占位形态;同样的形状测 skill 参数。纯文本 `/help` 行为不变(现有 `app.rs:2956` 那条用例)。
- **第 3 件**:`Char('b') + ALT` → `Command::None`、输入框为空;`Char('b')` 无修饰 → 插入;
  `Char('v') + ALT` 仍是 `PasteClipboardImage`(它在 match 之前,确认没被新分支截走);
  `Char('q') + CONTROL|ALT` 仍不插入。
- **第 4 件**:只改注释,无新测试;确认 `compaction_prompt_keeps_its_load_bearing_clauses` 仍过。

## 五、完成时要一起做的

- `rust/DESIGN.md`:先读「a `/` line is then steering text」那段(按键/斜杠菜单一节,约 1288-1296 行)
  现在还成不成立——第 1 件做完它就不成立了,**改写**成"已知命令留在输入框、未知 `/词` 仍是插话"。
  第 2 件若 DESIGN 描述过"命令行用显示文本"也一并改。第 3、4 件无 DESIGN 变更。
- HANDOFF.md「〇‴」一节的"四件事"逐条划掉并指向本 plan;第 4 件的"摘要复用缓存"作为待议记一行。
- 本文件补 ✅、四个提交号与开工问答结论。
