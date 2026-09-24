# Plan 211 — 输入框该像 shell 一样能编辑

> 来源:2026-09-24 读 `refs/pi`(`earendil-works/pi@d5629e2`,MIT)后与用户逐条定的,
> 出处见 `refs/README.md`「Pi 全面复查(2026-09-24)」第 9 条。

## 一、为什么

composer 的按键分发在 `tui/src/app.rs:1428-1471`(`on_key` 里那个 `match (key.code, ctrl)`),
能编辑的只有:字符插入、Backspace/Delete、←/→、Home/End、↑/↓(视觉行 + 输入历史),
外加 `is_composer_newline_key`(`app.rs:59-64`)的 Shift/Alt+Enter、Ctrl+J 换行。
refs/README 的前提**成立**:没有 Ctrl+A/E/K/U/W、没有按词移动/删除、没有 yank、没有撤销。

读代码时还看到两处现状比"缺功能"更糟:

- **Alt+字母会插入字母本身。** `(KeyCode::Char(c), false)`(`app.rs:1459`)只看 ctrl,不看 alt。
  终端把 Option/Alt 当 Meta 时,Alt+B 发 `ESC b`,crossterm 解析成 `Char('b') + ALT`
  (crossterm 0.29 `event/sys/unix/parse.rs` 里 `ESC` 前缀那一支),于是输入框里多出一个 `b`。
  macOS Terminal.app 的 Option+← 默认就发 `ESC b`——**用户按"按词左移",得到一个字母 b**。
- **Ctrl+←/→ 只移一个字符。** `(KeyCode::Left, _)` 吞掉了所有修饰键。

另一个收益是白送的:iTerm2 的 "Natural Text Editing" 预设把 Cmd+←/→ 发成 Ctrl+A/E、
Cmd+Backspace 发成 Ctrl+U、Option+Backspace 发成 `ESC DEL`。这些键一旦接上,Mac 用户的肌肉记忆就直接能用。

pi 的做法:`packages/tui/src/keybindings.ts:70-146` 一张默认表,`components/editor.ts`
按表分发;kill ring(`kill-ring.ts`,连续 kill 合并、yank-pop 轮转)、撤销栈(`undo-stack.ts`,
整状态快照)、按词移动(`word-navigation.ts`,粘贴标记当一个原子段)。kloop 只取机制与默认键位,
**不做可配置键位**(没有强理由:键位冲突就那几个,下面逐条处理了)。

## 二、键位表

| 键 | 动作 | 备注 |
|---|---|---|
| Ctrl+A / Ctrl+E | 到硬行首 / 行尾 | 与 Home/End 同语义(`composer.rs:524,533`) |
| Ctrl+B / Ctrl+F | 左 / 右一个字素 | 与 ←/→ 同(pi 同) |
| Alt+← / Alt+→、Ctrl+← / Ctrl+→、Alt+B / Alt+F | 按词左 / 右移 | |
| Ctrl+W、Alt+Backspace、Ctrl+Backspace | 删前一个词,进 kill ring | Ctrl+Backspace 只在 CSI-u 终端可分辨 |
| Alt+D、Alt+Delete | 删后一个词,进 kill ring | |
| Ctrl+U | 删到硬行首,进 kill ring;已在行首则删掉前面的 `\n` | pi `editor.ts:1624` |
| Ctrl+K | 删到硬行尾,进 kill ring;已在行尾则删掉后面的 `\n` | pi `editor.ts:1659` |
| Ctrl+Y | yank:插入 kill ring 最新一条 | |
| Alt+Y | yank-pop:紧接 yank 之后,换成更早一条 | 非紧接 yank 时无效 |
| Ctrl+_ | 撤销 | 见下"编码" |

**不动的键**:Ctrl+C(两击退出,`app.rs:1374`,在分发最前)、Ctrl+D(禁用,测试
`app.rs:3579` 断言它什么也不做——不改成 readline 的删字符,免得"退出键"语义回来)、
Ctrl+R(空闲时 rewind,`app.rs:1440`;不做 readline 的反向搜索)、Ctrl+V/Alt+V(图片粘贴,
`app.rs:1418`,在 match 之前)、Ctrl+T(todo 面板,`app.rs:1381`)、Ctrl+J(换行)、
Shift+Tab、Esc。补全菜单打开时 ↑/↓/Ctrl+P/N/Tab/Enter/Esc 仍由 `on_popup_key`
(`app.rs:1506-1539`)先吃,其余新键落到 composer 编辑后照常走 `after_edit` 重同步菜单。

**编码**(crossterm 0.29 + `lib.rs:675` 推的 `DISAMBIGUATE_ESCAPE_CODES`):

- 旧终端 Ctrl+_ / Ctrl+- / Ctrl+7 都发 `0x1F`,crossterm 解析成 `Char('7') + CONTROL`;
  CSI-u 终端则上报 `Char('-')` 或 `Char('_')` + CONTROL(可能带 SHIFT)。撤销要**三种都认**。
- Alt+Backspace 旧终端是 `ESC DEL` → `Backspace + ALT`;CSI-u 下同。Ctrl+Backspace 在旧终端
  多是 `0x08` → `Char('h') + CONTROL`,**不绑 Ctrl+H**(有的终端 Backspace 键本身就发 `0x08`
  并被当成 Ctrl+H,绑了会把普通退格变成删词)。
- 未绑定的 Alt+字母:**不再插入字母**,吞掉(`Command::None`)。AltGr 在 Windows 上报
  CONTROL|ALT,本来就落在 `_ => None`,不受影响。

分发写法:把 `match (key.code, ctrl)` 改成先算一个 `ComposerAction`(穷尽枚举,纯函数
`composer_action(&KeyEvent) -> Option<ComposerAction>`),再一处调 composer。这样键位表能被一条
表驱动测试整张断言,不用一键一个用例。

## 三、形状

### 3.1 按词移动(粘贴原子当一个词)

字符分三类:空白、词字符(`char::is_alphanumeric() || '_'`)、其余标点。一次左移 = 先跳过空白,
再跳过一整段同类(词或标点);右移对称。**光标左侧紧挨一个 atom(`atom_ending_at`,
`composer.rs:69`)时整个 atom 算一个词**,跳到它的起点;右移用 `atom_starting_at` 对称处理。
所以按词移动的落点永远在 atom 边界上,光标不会进入 atom——与 `left/right`(`composer.rs:506-521`)
现有的不变量一致。

**跨行**:文档是一个带 `\n` 的串,`\n` 按空白处理,于是 Alt+B 在行首会跳到上一行最后一个词的
开头(pi 是停在上一行末尾,多按一次;kloop 不需要这层区别)。落点必须是字素边界——
按字素迭代,不按 char。

### 3.2 kill 与 yank(粘贴原子整体进出)

现有的 `ComposerDocument::replace`(`composer.rs:115`)遇到与 atom 相交的区间会**物化**它
(`materialize_intersections`,`composer.rs:145`:把 label 换成完整粘贴内容再编辑)。这对"剪掉
atom 一半"是对的,但 kill 不该这样——Ctrl+W 删掉一个 `[Pasted #3: 2000 chars]` 应该是删掉这个
atom,而不是把两千字摊开再删最后一个词。

- 新增 `ComposerDocument::cut(range) -> Fragment`:`Fragment { display, atoms }`(atom 区间
  重定基到片段内)。**前置条件是 range 两端都在 atom 边界上**(debug_assert);3.1 的词边界与
  硬行边界天然满足——atom 的 label 是单行,`\n` 不会落在 atom 里。
- 新增 `insert_fragment(at, &Fragment) -> ByteOffset`:原样插回 display,atom 平移后并入。
  插入点两侧若因此拼出跨边界的字素簇,按 `insert_atom`(`composer.rs:95-98`)现有的退路处理
  (该 atom 物化成正文)。
- **yank 回来的 atom 保留原 id 与 label。** 同一粘贴被 yank 两次会出现两个 `#3`,展开各自按区间
  走 `expand`(`composer.rs:57`),结果正确;不重新编号,因为 label 里的字数也要跟着重算,不值。
  `next_paste_id` 的高水位不动。
- kill ring:`Vec<Fragment>`,上限 32 条(超出丢最旧)。**连续 kill 合并**成一条:向后删的
  (Ctrl+W、Alt+Backspace、Ctrl+U)拼在前面,向前删的(Alt+D、Ctrl+K)拼在后面——pi
  `kill-ring.ts:19-27`。"连续"由 composer 里一个 `last_action: Option<LastAction>`
  (`Kill`/`Yank`/`TypeWord`)判定,任何其他按键(含光标移动)清掉它。
- yank-pop 要知道上一次 yank 插入的区间:yank 时记下 `TextRange`,Alt+Y 先删这段再插轮转后的
  那一条。pi 是按文本长度倒推(`editor.ts:2077`),kloop 直接记区间,不倒推。
- kill ring **跨提交保留**(emacs/pi 都这样),`clear()` 也不清。

### 3.3 撤销

- 快照 = `(ComposerDocument, cursor, Vec<Attachment>)` 整体 clone(pi `undo-stack.ts` 同思路)。
  **不含** `next_paste_id`(测试 `paste_id_high_water_survives_submit_and_clear` 的不变量:
  撤销一次粘贴后再粘贴,编号仍要递增)、不含输入历史、不含 kill ring。
- 上限 100 个快照,超出丢最旧——粘贴原子的 payload 可能很大,不能无界。
- 何时压快照(照 pi 的 fish 式合并,`editor.ts:1200-1213`):连续输入词字符合成一个撤销单位;
  每个空白字符单独一个;每次 Backspace/Delete、每次 kill、每次 yank/yank-pop、每次粘贴、
  每次补全接受(`replace_range`,`composer.rs:457`)、换行,各一个。进入输入历史浏览
  (`history_prev` 从 `hist == None` 进入时)压一个——撤销能回到浏览前的草稿。
- 撤销:弹出快照整体恢复,`hist = None`、`goal_column = None`、`last_action = None`。栈空时无操作。
- `submit`/`submit_text`(`take_text`,`composer.rs:627`)清空撤销栈。
- **`clear()` 是否可撤销**是开工问题(第四节第 3 问)。

所有这些状态都放在 `Composer` 上;`App` 只做键到动作的映射。

## 四、开工时必须问用户的点

1. **撤销绑哪个键?**
   - **只绑 Ctrl+_(连同它的 Ctrl+- / Ctrl+7 编码)(推荐)**:readline、emacs、pi 都是它;
     Ctrl+Z 在终端里是"挂起"的肌肉记忆,raw mode 下现在虽然只是被吞掉,但留着给将来的挂起。
   - 另加 Ctrl+Z:对不熟 readline 的人更好找,代价是以后想做挂起时要改键。

2. **"词"按什么切?**
   - **字符类(推荐)**:字母数字下划线一段、标点一段、空白跳过。`char::is_alphanumeric` 对汉字
     为真,所以**一整段中文(到标点或空格为止)算一个词**,Ctrl+W 删掉半句话。无新依赖。
   - UAX#29 词边界(`unicode-segmentation` 已是依赖,`split_word_bound_indices`):英文更细
     (`don't` 是一个词),但**每个汉字各成一个词**,中文里 Alt+B 退化成逐字移动。pi 用的
     `Intl.Segmenter` 带词典,能切中文词,Rust 这边没有等价物。

3. **Esc 两击清空能否撤销?**
   - **能(推荐)**:`clear()` 前压一个快照(含图片附件),两击清空误触时 Ctrl+_ 一下拿回。
     两击本来就是为了防丢草稿,多一层兜底不冲突。
   - 不能:清空就是清空,语义最简单。

## 五、测试

`composer.rs` 单测(整对象断言 `(text(), cursor())`,atom 用 `document.atoms` 的 label/range 断言):

- 按词左右移:`"foo  bar.baz"` 各位置的落点序列整体断言;跨 `\n`;CJK 段(按第 2 问的结论);
  带组合字符/ZWJ emoji 的词,落点都在字素边界上。
- atom 两侧:`"a [Pasted #1…] b"` 上 Alt+B/Alt+F 一次跨过整个 atom,不进入。
- Ctrl+W 删 atom:删掉后 `atoms` 为空、**payload 没有被物化进 display**;Ctrl+Y 回来后 atom
  的 id/label/content 与原来相同,`submit().text` 展开一次。
- 连续 kill 合并:`Ctrl+W Ctrl+W` 后 Ctrl+Y 得到两个词原顺序;`Ctrl+K` 后接 `Ctrl+U` 的拼接方向;
  中间插一次光标移动则不合并(ring 里两条)。
- Ctrl+U/Ctrl+K 在行首/行尾删掉 `\n`;在空文档上无操作且不压快照。
- yank-pop:两条 kill 后 `Ctrl+Y Alt+Y` 换成较早一条;非紧接 yank 的 Alt+Y 无操作。
- 撤销:输入 `"hello world"` 后撤销的步进序列(fish 式合并)整体断言;撤销粘贴后再粘贴,
  label 编号仍递增;撤销越过 `history_prev` 回到草稿且 `hist == None`;submit 后撤销无操作;
  超过 100 个快照丢最旧;(按第 3 问)Esc 清空后撤销恢复文本与图片附件。

`app.rs` 单测(沿用 `key()`/`ctrl()` 助手,`app.rs:2424-2428`):

- **键位表驱动**:一张 `(KeyEvent, Option<ComposerAction>)` 表整体断言 `composer_action`,
  覆盖 Ctrl+7 / Ctrl+- / Ctrl+Shift+_ 三种撤销编码、`Backspace+ALT`、`Left+CONTROL`、`Char('b')+ALT`。
- `Char('b')+ALT` **不再插入 `b`**;未绑定的 `Char('q')+ALT` 也不插入。
- 不动的键回归:Ctrl+R 仍是 `RequestForkPoints`、Ctrl+V/Alt+V 仍是 `PasteClipboardImage`、
  Ctrl+D 仍是 `None`、Ctrl+C 两击仍退出。
- 补全菜单打开时 Ctrl+W 删词后菜单按新文本重过滤(`after_edit` 被调到)。

## 六、完成时要一起做的

- `rust/DESIGN.md`:先读"Keys follow Claude Code"一段(约 1223 行起)与 composer 一段
  (约 1241 行起,"Left/Right, Backspace, and forward Delete…Home/End retain hard logical-line
  semantics"),把键位表并入 composer 一段而不是另起一节;粘贴原子一段(约 1261 行起)
  "adjacent Backspace/Delete removes it whole" 改写为"相邻删除、按词移动、kill 都把它当一个整体,
  kill/yank 保留 atom 不物化"。写明 Alt+字母不再插入字母。
- HANDOFF.md 若有新教训则记(候选:crossterm 对 `0x1F` 的解析、Alt 前缀落进 `Char` 分支)。
- 本文件补 ✅、日期、提交号与开工问答结论。
