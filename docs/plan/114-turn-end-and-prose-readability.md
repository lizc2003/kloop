# Plan 114 — 一轮结束时用户看到的东西：结论要显眼，面板要退场

> 来源：2026-09-03 dogfood。用户把 kloop 与 codex 的收尾截图并排放出来，结论是
> 「体验比 codex 差多了」，追问后收敛到一句话：**「主要是结论不显眼」**，外加
> 「布局和颜色也可以优化」「task 仍然没有隐掉」。

## 三个事实

截图里 kloop 输出的是一段代码审查结论，三条 bullet，每条都是「论述……判定」。

1. **高亮打在了噪声上。** `Event::Code` 只给 code span 铺 `Indexed(236)` 灰底、
   不设前景，前景交给终端。结果 `30s`、`45s`、`status=0`、`encoding/json` 每一个
   都被框成灰块，而三条 bullet 真正的判定——「不是本次提交造成的回归」「不足以
   认定实现错误」「没有足够合同证据」——和周围的字一模一样。眼睛先被灰块牵走。
2. **代码块是个突兀的矩形。** `flush_code` 把每行 pad 到最长行宽再铺同一个灰底，
   在正文里砸下一块参差的深色板。codex 不铺底。
3. **task 面板赖着不走。** plan 74 的退场条件是「turn 结束且**全部** task 完成」
   （`054ece7`）。可模型交完答案很少回头勾自己的框，于是未完成的图留在输入框上方，
   在没人干活的时候读起来像还在干活。

还有一条对比：codex 收在 `— Worked for 12m 00s —`，kloop 的 `Working (Xs)` 活动行
直接消失，屏幕上不留任何「到此为止」的痕迹。

## 改动

### 一、强调分级：强调色给结论，代码只留一个前景

- `CODE_BG` 删除，换 `CODE_FG = Cyan`：inline code 只有前景，没有底色。
- `Tag::Strong` → `BOLD + BRAND`；`heading_style` 的 H1/H2 也吃 `BRAND`，更深的层级
  保持纯 BOLD。**加粗从此是一个模型能用的信号**：它是段落里最亮的东西。
- `flush_code` 不再 pad 成矩形，改用两列缩进（`CODE_INDENT`）把块托出来；
  `highlight_code` 的 base 从 `bg(CODE_BG)` 变 `Style::default()`。
- `token_style` 拿掉 `digit` 和 `operator` 的青色。预览里一条 `go test ./... -count=1`
  被点亮了 `.`、`/`、`-`、`=1` 五处——语法高亮反而把 shell 块变成了彩纸屑。

### 二、结构：层级、留白、URL

- 列表符号按深度分化 `• / - / ·`（`BULLETS`），顶层 marker 用 `BRAND`，更深的 `DIM`。
  原来每层都是 `•`，模型写的嵌套在屏幕上被压平了。
- 块间留白按 loose/tight 走源码：`OpenItem { start, loose }` 记住每个打开的 item
  起点与是否 loose（item 的块被 Paragraph 包裹即为 loose）。loose list 的 item 之间、
  以及 item 内第二个块之前留空行；tight 的嵌套列表一行不加。
- `Tag::Link` 记住 `dest_url`，收尾时把 ` (url)` 以 DIM 追加——终端点不了下划线，
  URL 丢掉就等于地址没了。autolink（文本本身就是 URL）不重复打印。
- wrap 插入的分隔空格在两侧样式相同时继承该样式，粗体短语不再被空格切成三个 span。

### 三、提示词：判定前置并加粗

`context.rs` 的 Communication style 加一条：结论开头、`**bold**`、然后才是理由；
并说明「终端把粗体渲染成强调色，所以这个记号要花在结论上，不要花在标签上」。
渲染层只能保证「标出来的东西真的显眼」，**标不标出来是模型的事**——只改渲染救不了
截图里那种通篇不加粗的输出。

### 四、turn 收尾

- `task_panel_retired = true` 无条件设置：面板跟着**开它的那一轮**走，做没做完都退。
  下一次 task 更新会把它带回来（`TaskGraphUpdated` 仍清 retired）。
- 新 `Cell::TurnEnd(u64)` + `App::seal_turn`：事件循环在 `app.running` 落下的那一刻
  把 turn 的秒数盖进 transcript（时钟在循环里，`App` 无钟——沿用 `seal_thinking` 的
  形状），渲染成 DIM 全宽 `── Worked for 12m05s ───…`。它同时是两轮之间的接缝。

## 验证

`cargo fmt` + `cargo clippy --workspace --all-targets -D warnings` + `cargo test --workspace`
全绿。markdown 新增 4 条测试（嵌套符号、loose/tight 留白、item 内块留白、link URL），
`render` 新增 turn-end 规则一条，`app` 的面板退场测试改为无条件断言。

另外用一个临时的 `#[ignore]` dump 测试把渲染结果转成 ANSI 落盘、肉眼过了一遍真实
配色（提交前删除）——这类纯视觉改动，测试断言只能保证结构，颜色得看。
