# Plan 38 — TUI 对齐 Claude Code 体验(inline 重构 + 视觉/交互升级)

> 备忘,未开工。开工前读 `docs/plan/HANDOFF.md` + 本文件。这是一个**多切片系列**
> (一片 = 一会话),按切片顺序推进;每片独立 fmt/clippy/test 全绿 + 真 key 验收 + 补 ✅。
> 缘起:用户要求「TUI 体验与 CC 一致」。现有 TUI(plan 9:alternate screen 全屏 +
> 自维护 cell 缓冲)在渲染质感与交互上离 CC 差距大——单行输入、纯文本无 markdown、
> 工具行显示原始 JSON、无 spinner/斜杠菜单/@补全、历史不进终端原生 scrollback。

## 目标

把 kloop TUI 从「全屏朴素版」升级到「与 Claude Code 体验一致」:

- **inline 渲染**:历史进终端原生 scrollback,可用终端原生滚动/复制/搜索(Cmd+F)
- **markdown 渲染 + 代码高亮**(助手消息)
- **人类可读的工具行 + 输出预览**(`● Bash($ ls -la)` + `└ 输出前 N 行`,不再 `bash {json}`)
- **多行 composer**(换行 / 粘贴大段 / 图片 / 上下键调历史)
- **斜杠命令菜单 + @文件补全**弹层
- **动画状态行**(spinner/shimmer + 动词 + 耗时 + token/context + Esc 提示)
- **配色体系 + gutter 视觉语言 + 开场会话头 + diff 升级**

**保留 kloop 优势,不动**:`events → app → render` 三层 + 纯函数可单测(无 TTY 用
`TestBackend`);`--plain` 裸 REPL;`--mock` 无交互验证;CJK 宽度感知 wrap/truncate;
panic hook 恢复终端。codex 的 11k 行 `chat_composer.rs` / 3k 行 `bottom_pane` 是
产品堆砌的反面教材——**只抄形态与手法,不抄体量**;对照上游 Codex(结构同、精简 2~3 倍)
或只取核心文件的结构。claw-code 是内联 REPL(非 ratatui),整体骨架帮不上,只借它
几块与渲染后端无关的纯逻辑(见「参考结论」)。

## 参考结论(调研已沉淀)

- **codex `codex-rs/tui`(主参考,ratatui)**:inline viewport + `insert_history`
  写进原生 scrollback;`HistoryCell` trait(每 cell `display_lines(width)`);流式
  `active_cell` + commit-tick 打字机;`styles.md` 色规;gutter `● › │ └`;`shimmer.rs`
  + `motion.rs`(~260 行 动画,尊重 reduced-motion);`FrameRequester` 按需渲染;
  `ListSelectionView` 扁平菜单;markdown=pulldown-cmark,高亮=syntect+two-face,
  diff=GitHub 风格分级配色。
- **claw-code(内联 REPL,借纯逻辑)**:流式 markdown **安全边界缓冲**
  (`MarkdownStreamState` + `find_stream_safe_boundary`,空行/围栏闭合才吐,避免撕裂);
  `normalize_nested_fences`(嵌套围栏修复);工具卡片/结果/diff 的**格式化分类**
  信息架构(把 ANSI 换 ratatui `Line`/`Span` 即可复用);输出**双限截断**(行数+字符)。
  它团队自留的 `refs/claw-code/rust/TUI-ENHANCEMENT-PLAN.md` 可当需求清单参考。

## CC 截图实测细节(2026-07-16 用户提供,校准用)

用户给的 CC 现役 TUI 截图,钉下可见事实(据此对齐,不臆测):

- **inline 确认**:对话直接在终端里(可见 iTerm tab 栏),历史进原生 scrollback——印证切片 0。
- **markdown 全渲染**:粗体、行内 code、**markdown 表格渲染成 box-drawing 边框表格**
  (`┌┬┐ ├┼┤ └┴┘`)——切片 1 的显眼特性,别只做粗斜体列表就收工。
- **thinking 折叠 = 动词 + 累计耗时**:完成态一行 `∗ Cogitated for 12m 24s`(gutter `∗`、
  dim、动词过去式);进行中应是动词现在式(与 spinner 同源)。**与 kloop 现状不同**——
  kloop 现在显示 thinking 最后一行预览(`∴ …`),切片 5 改为 CC 式动词+耗时。
- **composer 无边框**:`›` 提示符(品牌色)+ 多行文本,无盒子。
- **footer 两行**:上为 composer;下为 mode 指示 `⏵⏵ auto mode on (shift+tab to cycle)
  · ⏎ for agents`(左,品牌色)+ 右侧系统状态(可无)。
- **日常转录不画盒子**:只有表格有边框;其余靠前缀 + 缩进 + dim/色彩分层——符合关键决定 4。
- **品牌强调色**:CC 用**橙色**(`›`、mode、光标)。kloop 品牌色待定(见「开工时定」)。

## 已拍板的关键决定

1. **渲染模型 = inline viewport,零 fork**(推翻 plan 9 教训 9 的前提)。教训 9 当初弃
   inline,理由是「移植 codex 的 ~2000 行 vendored CustomTerminal 太重」——**那是误判**:
   用上游 ratatui 0.29 原生 API 即可,不必移植 codex 的终端层。核实依据(2026-07-16):
   - `Terminal::with_options(TerminalOptions { viewport: Viewport::Inline(h) })` 画活动区;
     `insert_before(height, draw_fn)` 把历史行写进光标上方、溢出进原生 scrollback。开
     `scrolling-regions` feature 时走 DECSTBM scroll region(`CSI top;bot r` + `CSI n S`),
     与 codex 手写 ANSI 同款,**不重画 viewport、无闪**。
   - codex 的 ratatui/crossterm fork **各只 +1 提交**(`set_viewport_area` 改公开 /
     `query_fg/bg_color`),对 kloop inline 路线都不必要;上游 0.29 的 `scrolling-regions`
     / `unstable-backend-writer` / `unstable-rendered-line-info` 已覆盖等价能力。
   - **唯一必须自研的核心**:`insert_before` 要固定 `height`,而历史块高度随宽度可变 →
     定稿前按当前宽度测折行行数(~50–100 行;开 `unstable-rendered-line-info` 用
     `Paragraph::line_count(width)` 可再省)。
   - **唯一集成陷阱**:`with_options(Inline)` 与 `resize` 内部发 CPR(`ESC[6n`)读光标位;
     **不能与异步 stdin event reader 并发抢 stdin**,否则应答被 event 循环吞掉 → 卡住/错位。
     对策(任选,无需 fork):Terminal 构造放在 spawn event reader 之前 / draw 与 event 读
     不并发抢 stdin / 一次性 startup probe 后自行摆 viewport。**切片 0 必须先解决**。
2. **依赖新增**:`pulldown-cmark`(markdown,轻)。`syntect`+`two-face`(代码高亮)较重
   (编译慢、体积大;kloop 一贯克制依赖,web crate 曾手写 HTML→text 避 turndown)——
   **开工时定**:引入 vs 先只做边框+dim 背景不高亮、syntect 后置为独立小片。
3. **resize 不 reflow scrollback**:已进原生 scrollback 的历史,宽度变了不重排(CC 与
   终端本身也如此)——接受,仅活动 viewport 每帧重画。codex 的 `draw_with_resize_reflow`
   是大工程,不做。
4. **overlay 策略**:confirm 审批 / fork picker / 斜杠菜单 / @补全 都做成**底部区模态**
   (在 inline viewport 内替换 composer,**不切 alt-screen**)。全屏 pager(如 Ctrl+T
   看全文转录)若要,再用 alt-screen(上游 inline 可切换,已核实)——后置/可不做。
5. **终端色探测不做**(需 crossterm `query_fg/bg` = fork)。用 `styles.md` 固定色规 +
   ANSI 默认色 `reset`,避免自定义色在异色主题下对比差(styles.md 铁律)。

## 切片路线图

> 顺序即依赖:切片 0 是地基,先定渲染模型再做视觉,否则视觉工作要在旧的「全量重画」
> 模型上做一遍、迁 inline 时返工。1/2 视觉增量最大。3~6 依次。

### 切片 0 — inline 渲染模型迁移(地基,必须先做,风险最高) ✅ 完成(2026-07-16)

把渲染从「`App` 持有全部 `cells` + 每帧 `transcript_lines` 全量重画」改为「cell 定稿即
`insert_before` 写进 scrollback,只有正在流式的 **active cell** + 底部区(状态行 + composer)
在 inline viewport」。

**完成记录(2026-07-16,提交 1f993ef)**:
- `crates/tui/Cargo.toml`:ratatui 开 `scrolling-regions` feature(flicker-free insert_before);删已不用的 `futures` 依赖。
- `setup_terminal`:去 `EnterAlternateScreen`,改 `Terminal::with_options(Viewport::Inline(满屏高))`;
  restore 去 alt-screen、show cursor + 换行落到 viewport 下。构造期 CPR 在输入线程启动前跑,无并发。
- **输入模型**:`spawn_input_thread` 专用 OS 线程 `poll(200ms)+read()` → tokio channel,取代 `EventStream`;
  `ui_loop` select(input channel, agent events),退出时置 stop 旗 + join。解了 CPR/EventStream 死锁陷阱
  (关键决定 1、教训 38)。
- **渲染模型**:上游满高 inline + 溢出提交(**修正关键决定**:上游 ratatui 0.29 不能运行时改 viewport 高度,
  小 live 区观感需 fork,故取满高 + `insert_before` 溢出落 scrollback,达成 scrollback 目标零 fork——见教训 38)。
  `render::cell_lines`(单 cell 渲染,viewport 与 insert_before 共用)、`commit_count`(定稿前缀 + 硬 cap
  兜背景 agent 卡死)、`App::drain_committed`(重基三索引映射);`draw` 底部对齐渲染未提交尾部。`App.cells` 语义
  变「未提交尾部」;退役 `scroll_up` 及滚动键(交给终端)。
- **弹层**:confirm / fork picker 仍 viewport 内居中模态,**零改**(满高 viewport 下 `f.area()` = 全屏,原样叠加)。
- **resume/clear/fork**:resumed cells 进未提交尾部、启动即溢出落 scrollback;`/clear`/rewind 不能擦 scrollback(inline
  固有,注释说明)。
- **验收**:fmt+clippy(`-D warnings`)+ 全 workspace test 绿(tui 49 测试,含 `commit_count`/`drain_committed`/
  端到端 `draw_confirm`)。真 key **双轨** PTY+pyte VT100 驱动:anthropic + openai 各 6/6(inline 渲染 / 流式 /
  工具行 ✓✗ / resize×2 不花屏[CPR 陷阱] / Ctrl+C 中断);另 anthropic 单验审批弹层(`approve?` 盒 + y 放行 + 工具跑 +
  结果回报)、60 消息 resume 溢出(尾部对位、composer 钉底、无花屏)。**留真终端人工核一条**:原生 scrollback 回滚
  (pyte 抓不到 DECSTBM scroll-region scrollback,模拟器保真度限制非 bug)。
- **教训沉淀**:HANDOFF 教训 38(CPR/poll 线程、上游不能动态改 inline 高度、PTY 测法);教训 12 修订(inline 是中等工程非「太重」)。

- `setup_terminal` 去 `EnterAlternateScreen`,改 `Viewport::Inline`;解决 CPR/stdin 并发
  (关键决定 1);`Cargo.toml` `ratatui` 开 `scrolling-regions`(+ 可选 `unstable-rendered-line-info`)。
- 引入 per-cell `display_lines(width) -> Vec<Line>` 渲染接口(替代大 `transcript_lines`
  match;为后续 markdown/工具行铺接口);定稿封装 `insert_before` + **变高折行测量**。
- active cell(当前流式 Assistant/Thinking)留 viewport,收尾(下一个 cell 打开 / TurnEnded)
  时 flush 进 scrollback。
- confirm / fork picker 改**底部模态**(viewport 内替换 composer);resume:历史 cells
  逐个 `insert_before` 回放。
- **行为等价**(暂不加新视觉,甚至可更朴素),换来:历史进原生 scrollback、鼠标滚轮/选择/
  搜索可用。TUI 自身的 scroll_up/PageUp 逻辑可退役(交给终端)。
- 验收:真 key,流式 / 工具行 / 审批 y-a-p-n / Ctrl+C 中断 / resize 不花屏 / resume 全过;
  终端原生滚轮+选择+搜索历史可用;`--plain`/`--mock` 不受影响。

### 切片 1 — markdown 渲染 + 代码高亮

助手消息 pulldown-cmark → 富 `Line`(标题 / 粗斜体 / 有序无序列表 / 引用块 / 行内 code /
表格 box-drawing);代码块 syntect+two-face 高亮(或先边框+dim 背景,见关键决定 2)。
**流式安全边界缓冲**(借 claw:空行 / 围栏闭合才吐已稳定段,`ContentBlockStop` flush 余下)
避免半个 markdown 块被撕裂重排;表格未闭合整体扣住。`normalize_nested_fences` 修嵌套围栏。

- 验收:markdown 结构正确、代码块高亮、流式增量不撕裂、CJK 宽度不错位。

### 切片 2 — 人类可读工具行 + 输出预览

工具行按类型格式化(借 claw `format_*` 信息架构):`● Bash($ ls -la)` / `Read(file.rs)` /
`Write(file, N lines)` / `Edit(file)` + 一行 diff / `Grep 🔎 pattern` / MCP `server__tool(...)`;
状态染色 bullet(`●` running 动画 / green ✓ / red ✗)。工具结果 gutter `└ ` 缩进预览
前 N 行(dim,**双限截断** 行数+字符,中间省略),超出提示「full result in session/offload」。
gutter 视觉语言统一 `● › │ └`,左槽宽度常量对齐。连续 read/grep/glob 可折成 Exploring 分组
(可选,后置)。

- 验收:各工具人类可读摘要 + 输出预览;拒绝/失败/中断状态正确。

### 切片 3 — 多行 composer

自绘多行输入 widget 替换单行 `input_view`:`Shift+Enter`/`Ctrl+J` 换行、多行光标移动、
上下键在首/末行切**输入历史**(否则移动光标)、大段粘贴(>阈值)→ 占位符 `[Pasted N chars]`
提交时展开、图片粘贴 → 附件占位(kloop 已有 image 输入)。左侧 `›` 提示符,空时 dim 占位串。

- 验收:多行编辑 / 粘贴大段 / 粘贴图片 / ↑↓ 调历史。

### 切片 4 — 斜杠命令菜单 + @文件补全

输入 `/` 弹命令候选菜单(名 + 描述,`Tab`/`Enter` 补全,来源 `commands::` + skills);
`@` 弹文件选择器(后台异步文件搜索,尊重 .gitignore,复用 plan 14 的 ignore 族)。通用
**扁平菜单底座**(codex `ListSelectionView` 式:无边框盒,`user_message_style` 淡底 +
选中行 `accent`(cyan bold)整行上色,两列自适应,数字键快选,可滚动)。同一时刻至多一个 popup。

- 验收:`/` 菜单选/补全、`@` 文件选中插入、菜单键不漏给 composer。

### 切片 5 — 动画状态行 + HUD + 按需渲染

spinner/shimmer 状态行(shimmer 光带 header + 动词 + `(1m05s • Esc to interrupt)` 耗时,
借 `shimmer.rs`+`motion.rs` ~260 行,尊重 reduced-motion);**FrameRequester 按需渲染**
(动画靠「渲染时给自己排下一帧」自驱,合并请求 + 120fps clamp,无变化 CPU 空闲——取代当前
每 loop draw);footer 两行(composer / mode + 系统状态,截图形态);footer 显示 context
剩余 %/token(复用 usage 记账)、model/mode。**thinking cell 在此片改为 CC 式动词 + 累计
耗时**(完成 `∗ Cogitated for Xs`、进行中动词现在式,与 spinner 动词同源),取代切片 0
沿用的 `∴` 最后一行预览。

- 验收:运行时有平滑动画 + 耗时;空闲无 CPU 空转;token/context 正确。

### 切片 6 — 会话头 + 配色体系 + diff 升级

开场 `SessionHeader` cell(圆角框 `╭─╮`:`>_ kloop`、model、cwd、branch、mode);`styles.md`
色规全面落地(cyan=输入/选中/状态,green=成功/新增,red=错误/删除,magenta=品牌,标题 bold,
次要 dim;避免 black/white/blue/yellow 前景);diff 渲染升级 GitHub 风格(行号 + gutter +/-/space
+ 语法高亮 + `+N -M` 统计),放宽 plan 21/25 的行数截断到 diff overlay 里看全(可选)。

- 验收:开场头美观、全局配色一致、diff 可读。

## 不做(裁剪,记为可能性)

终端色探测(需 fork crossterm)、resize scrollback reflow、OSC8 超链接、Zellij raw、
composer vim normal 模式、鼠标点击展开工具结果、bracketed-paste 之外的 PasteBurst 状态机、
每消息时间戳、全屏转录 pager(Ctrl+T,除非切片 2 的截断逼出需求)。

## 每片完成标准

fmt + clippy(`-D warnings`)+ test 全绿;纯函数单测 + `TestBackend` 端到端(无 TTY 断言
屏幕内容,plan 25 已有范式)+ **真 key 手工验收**(双轨,PTY 驱动;plan 9 教训:ratatui
差分渲染要按「屏幕重建」断言,不能字节流匹配);行为变更同步 README;plan 文件补 ✅ 与提交号;
新教训进 HANDOFF。**完成切片 0 后回填 HANDOFF 教训 9**:inline 经核实为中等工程(上游原生),
非当初以为的「太重」。

## 开工时定 / 问用户

- ~~切片 0 是否即刻开工~~(✅ 用户同意即刻开工,已完成)。
- 关键决定 2:syntect 代码高亮引入 vs 后置(依赖体积权衡)。
- 切片 2 工具行的具体样式细节(bullet 用 `●` 还是 `⏺`、gutter 符号)可开工时对着真 key 调。
- **品牌强调色**定 magenta/cyan(styles.md 建议)还是**橙**(靠拢 CC 截图);gutter 提示符
  `›`、mode 行 `⏵⏵`、thinking `∗` 等具体字形对着真 key 调。
