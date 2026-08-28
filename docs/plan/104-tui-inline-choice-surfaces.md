# Plan 104 — 交互界面统一改造:居中浮窗 → 贴着输入框的内联选择器

> 状态:✅ 已完成(2026-08-28;提交见本次 git log,plan104)
>
> 依赖:Plan 38 切片 0/4/5(inline scrollback、补全菜单、活动行)、Plan 99/100/103(提交高度预算与闪屏修复)。

## Context(dogfood 报告)

用户对着 kloop 的审批界面截图和 CC 的选择界面截图:「kloop 的让用户选择界面体验太差」,
并拍板「都改,只要与用户交互的,都要改体验」。

现状(`render.rs`)四个捕获键盘的界面全是**屏幕正中的带框浮窗 + `Clear` 抠掉底下内容**:

| 界面 | 入口 | 现状 |
| --- | --- | --- |
| 审批 | `draw_confirm` | 居中浮窗,只有单键 `y/a/p/n`,没有光标 |
| 提问 | `draw_question` | 居中浮窗,有光标但形态割裂 |
| provider/model | `draw_provider_picker` | 居中浮窗 |
| rewind | `draw_fork_picker` | 居中浮窗 |

对照 CC 的实测差距:

1. **位置**:浮窗盖住 transcript 中段;CC 贴在 composer 上方,历史自然上滚,不遮挡。
2. **交互**:审批只有单键,没有可导航的编号列表;CC 是 `1./2./3.` + `↑↓` + `Enter`。
3. **信息**:`describe()` 把 `[sub-agent] [hazard] [no sandbox] bash: cmd` 拼成一行扁平字符串,
   命令被挤在标签后面;CC 分层——标题、命令独占一行、原因一行。
4. **拒绝**:`n` 只是拒绝;CC 末项是「拒绝并告诉我该怎么改」,拒绝后光标就在输入框。
5. **提示**:浮窗内一句 + footer 一句,措辞还不一样(`↑↓ scroll` vs `↑↓ choose`)。
6. **宽度**:clamp 到 76 列,长命令被压折;内联可用全宽。

## 已拍板设计

### 一、统一的内联面板 `choice.rs`

新模块 `crates/tui/src/choice.rs`,一个渲染模型 + 一个纯函数,四个界面共用:

```
▌ Bash command                     ← header:BRAND 粗体,左竖条(单行、不依赖 box 对齐)
go test ./upstream -count=1        ← body:命令 / 说明 / diff preview,可滚动
sandbox denied — run without sandbox?

Do you want to proceed?            ← prompt
                                   ← 空行
> 1. Yes                           ← 选中:`> ` + BRAND
  2. Yes, and don't ask again in this workspace
  3. No, and tell kloop what to do differently (esc)
                                   ← 空行
Enter select · ↑↓ move · 1-9 pick · Esc cancel   ← hint:DIM,唯一一处提示
```

- 选中行 `> ` 前缀 + BRAND 着色(不用整行 REVERSED 色块——现有 picker 那种大反色块在
  内联形态下会跟 transcript 抢视线)。
- 选项可带 dim 的第二行 detail(question 的 `option.description` 就走这里)。
- multi-select 用 `[x]/[ ]`。
- 前 9 项显示编号,数字键直选。

### 二、布局:面板是 live chrome,不是 overlay

面板行接到 transcript 尾部(活动行/任务面板之后、composer 之前),并进
`live_chrome_layout.reserved_rows()`——否则 plan 99/103 那套 scrollback 提交会把高度算错,
闪屏/重影会回来。删掉四处 `Clear` + 居中 `Rect` 计算。

高度预算两级:header/prompt/选项/hint 是 pinned(永远可见,选项超高时围绕光标窗口化),
body 拿剩下的行并可滚动,溢出显示 `↑ / ↓ more`。

### 三、键盘统一

| 键 | 行为 |
| --- | --- |
| `↑↓` / `k` `j` | 移动光标(审批的 `j/k` 不再滚 body) |
| `1`–`9` | 直选并立即执行 |
| `Enter` | 确认光标项 |
| `Esc` | 等同末项(deny / cancel) |
| `PageUp/PageDown` | 滚 body |
| `y` `a` `p` `n` | 审批的单键快捷,保留(熟练用户) |

`confirm_scroll` 改名 `panel_scroll`(四个面板共用一个滚动偏移)。

### 四、审批信息分层

`ConfirmRequest` 加两个可选字段,`description` 原样保留(server wire 和 `cli/ui.rs` 继续用它,
不破坏现有协议与测试):

- `title: Option<String>` — 面板 header,如 `Bash command` / `Edit file` / `Exit plan mode`
- `notice: Option<String>` — 为什么在问,如 `sandbox denied — run without sandbox?`

TUI 有结构化字段就分层渲染,没有就退回整条 `description`。

### 五、footer 去重

面板自带 hint 后,footer 在面板打开时不再重复一遍提示,回到稳定的 mode badge + 系统状态。

## 落地与实测偏差(实现时改的两处设计)

1. **被批准的对象必须钉住,不能和 diff 一起滚**。第一版只分 `body`(可滚)一层,40×16 的
   终端上实测:预算按 hint → 选项 → header → prompt 分配完,`body` 拿到 0 行——「要批准
   `big.txt` 的什么」整条消失,只剩「Do you want to proceed? / 1. Yes」。改成两层:
   `subject`(命令/路径 + 黄色 notice,pinned)与 `body`(diff/plan,可滚)。
2. **列表不能把 header 和 subject 挤掉**。12 项的 picker 在 9 行预算里把 header/prompt
   全吃了,只剩一排没有标题的答案。改成分配给选项前先扣下两行(header 1 + subject 首行 1),
   没用掉的行再退回 body。
3. **notice 只靠黄色不够**:某些终端主题下黄色不跳,加 `⚠ ` 标记;`⚠`(U+26A0)是 Neutral
   宽度,但给它 emoji presentation 的终端会画成两列,所以按 2 列预算并让续行缩进对齐——
   两种渲染下都不会溢出成多余的一行。

最终退化顺序:body 先让(并可滚)→ 空行分隔全去 → prompt → 选项围绕光标窗口化;
hint、header、subject 首行、光标所在那一项是最后倒下的。

## 验证

- `choice.rs` 单测 9 条:满高全块顺序(整对象断言)、挤压时先丢 body、body 滚动 + 过滚回夹、
  长列表围绕光标窗口化且 header/subject 保住、编号只给前 9 项、multi-select 勾选、
  editor 光标行列、1 行终端只剩 hint、`window_lines` 边界。
- `render.rs`:审批面板拆 subject/notice/编号答案 + 无结构化字段时回退整条 description、
  diff `+N -M` 进 body、question 面板整对象断言(两个兜底项)、fork picker 同款、
  端到端 TestBackend 断「hint 落在 composer 上方那一行、无 `╭` 边框、transcript 还在、
  过滚自纠」。
- `app.rs`:光标移动 vs PgUp/PgDn 滚动分家、换面板时 cursor 与 scroll 都归零、
  数字键/Enter/Esc 三条路径各自答对 scope、越界数字不答。
- `cargo fmt` + `clippy --all-targets` + `test --workspace` 全绿。
