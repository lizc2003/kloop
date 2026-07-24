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
   **✅ 2026-07-17 定库:改用 `synoptic`(≈2.2.9)替 syntect**——纯 Rust、正则式,主依赖
   仅 `regex`/`char_index`/`if_chain`/`nohash-hasher`/`unicode-width`(无 C 编译),内置
   ~30 语言规则(Rust/Py/JS/TS/JSON/Bash/Go/C/CSS/YAML/TOML/SQL…)。API `run(&code)` 逐行
   `line(n,&line)` → `TokOpt::Some(text,kind)`/`None(text)`,**一 token 一 `Span::styled`** 与
   `markdown.rs` 的 Line/Span 1:1;token kind 映射到 styles.md 安全配色(避 black/white/blue/
   yellow 前景)、叠在现有 `CODE_BG=Indexed(236)` 上。精度是正则级(聊天代码块够用,非编辑器
   级)。放弃 tree-sitter 路线(syntastica/inkjet):虽复用已有 tree-sitter,但每语言一个 C 语法
   crate,编译负担反比 synoptic 重。**本次只定库,实现留到高亮小片(见切片 6 或独立片)。**
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
- **2026-07-24 更正(plan 43)**:满高 viewport 的 `insert_before` 特例会发单行
  DECSTBM(`CSI 1;1r`),而规范要求 top < bottom；iTerm2 下可能退化成整屏滚动，
  使物理屏幕与 Ratatui diff buffer 脱同步。现改为每批 overflow 插入后 clear
  当前 viewport 并由下一帧完整重画；原生 scrollback 保留，但「不重画 viewport」
  不再是可移植承诺。

**切片 0 会话后续的交互打磨(同会话用户逐条提,已提交,下会话别重做)**:
- **底部信息架构**(对齐 CC,提交 cfcd125/bf268d6/383acc3/12310cb/eac0977):布局 `transcript / 上规则 ─ / composer `>` / 下规则 ─ / footer`;**动态活动行**(`render::activity_line`)在**转录区最下方、composer 正上方**(running→`working…` / armed→`press Ctrl+C again to exit` / awaiting→`awaiting your approval` / idle→None)——**这就是切片 5 的 HUD 落点,结构已就位,只差 spinner/shimmer/动词过去式/耗时/token(需 FrameRequester + 计时,pure App 无时钟,得在 ui_loop 引入)**;**稳定 footer**(`render::footer_line`)在最底,`[mode]` 徽标 + 键位,仅 running/idle 两套 hint + 徽标在边界变、不逐事件 churn;去掉了运行态 status 里 churn 的 `last_note`(字段保留给 HUD)。
- **CC 键位**(提交 cd73648/32b1213/69a076d/e7fd1a5):**Esc** = running 中断 / idle 清输入行;**Ctrl+C** = 两拍退出(首拍 armed 提示、次拍退,其它键 disarm),在主输入+confirm 弹层+rewind picker **一致**(逻辑在 `on_key` 顶部、路由前);**Ctrl+D 屏蔽**(TUI 唯一退出路径 = Ctrl+C 两拍);弹层「取消/拒绝」交给 Esc。
- **退出光标**(提交 cb8a729):restore 时 `MoveTo(0, 底行)+\r\n`,光标落列 0 消除 zsh `%`。
- **`/exit` 命令**(提交 9c7c6f5,跨 core+三前端):`SlashResult.quit` + `commands/exit.rs`;TUI 发 `AgentEvent::Quit` 走同一 restore、plain break、server 惰性回提示。
- **切片 5/6 交接**:footer 两行 + 活动行 + 键位 hint **结构已落**;切片 5 只剩 shimmer/motion 动画 + FrameRequester 按需渲染 + 耗时/token HUD 内容;切片 6 的配色体系/会话头/diff 升级未动,footer 的品牌色也待「品牌强调色」定夺(见开工时定)。

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

### 切片 1 — markdown 渲染 + 代码高亮 ✅ 完成(2026-07-17)

助手消息 pulldown-cmark → 富 `Line`(标题 / 粗斜体 / 有序无序列表 / 引用块 / 行内 code /
表格 box-drawing);代码块高亮(先边框+dim 背景;高亮库后定为 `synoptic`,见关键决定 2)。
**流式安全边界缓冲**(借 claw:空行 / 围栏闭合才吐已稳定段,`ContentBlockStop` flush 余下)
避免半个 markdown 块被撕裂重排;表格未闭合整体扣住。`normalize_nested_fences` 修嵌套围栏。

- 验收:markdown 结构正确、代码块高亮、流式增量不撕裂、CJK 宽度不错位。

**完成记录(2026-07-17)**:
- 依赖:`pulldown-cmark 0.13`(`default-features=false`,只驱 pull parser,不引 html/escape/getopts)。**关键决定 2 拍板:syntect 后置**,代码块只做暗底(`Color::Indexed(236)`)+ 原文保真,语法高亮留独立小片。
- 新模块 `crates/tui/src/markdown.rs`:`Renderer` 走 pulldown 事件流,累积 inline styled chars → 按块 flush。支持标题(bold)、`**bold**`/`*italic*`/`~~strike~~`、行内 code(暗底)、有序/无序列表(marker + 悬挂缩进)、引用块(`│ ` dim gutter)、fenced 代码块(暗底矩形,`wrap` 硬折行 + padding)、hr、任务列表 `[x]`、链接/图片(下划线,丢 URL)、**box-drawing 表格**(`┌┬┐├┼┤└┴┘`,列宽按内容增长再按可用宽收窄、超宽 `…` 截断、按 `Alignment` 左/中/右对齐、表头 bold)。自带 CJK 宽度感知 word-wrap(`wrap_words`:空格断词、超长词硬断、`\n`(HardBreak)强制换行;CJK 无空格→逐字硬断)。**SoftBreak → 空格**(CommonMark 语义,段内单换行重排以支持按宽 reflow)。
- 流式:`assistant_stream_lines` 用移植自 claw 的 `find_stream_safe_boundary`(空行 / 围栏闭合定边界)切 stable 前缀 → markdown、forming 尾 → 裸文本(不 reflow);`normalize_nested_fences`(嵌套围栏升级到更长栅栏)防 pulldown 把内层 ``` 当闭合。
- 接线:`render::cell_lines` 的 `Cell::Assistant` 走 `markdown_lines`(封口渲染);`render::visible_transcript`(draw 专用)对**流式末 cell** 特判走 `assistant_stream_lines`——`App::streaming_assistant()`(`assistant_open && 末 cell 是 Assistant`)是 live/sealed 唯一判定点(流式 Assistant 恒是末 cell,commit_count 从不冻结末 cell,故 commit 路径见到的 Assistant 必已封口,可整体 parse)。`transcript_lines` 降级为 `#[cfg(test)]` 测试便利。
- 验收:fmt + clippy(`-D warnings`)+ 全 workspace test 绿(tui 65 测试,新增 markdown 结构/表格/右对齐/嵌套围栏/代码块暗底/安全边界/流式前缀+裸尾/CJK 等)。真 key **双轨** PTY+pyte VT100 驱动(CPR 由驱动模拟应答,解 inline 构造陷阱):anthropic + openai 默认 prompt 各 10/10(标题 bold / 粗体 attr / 行内 code / bullets `•` / box-drawing 表格对齐 + Alice/Bob 行 / 代码块 bg=索引236(pyte 见 `303030`));另 anthropic 单验多行 fenced 代码块渲染成三行暗底矩形、内容保真。
- 教训沉淀:HANDOFF 教训「markdown 流式安全边界与 inline commit 模型的解耦」。

### 切片 2 — 人类可读工具行 + 输出预览 ✅ 完成(2026-07-17)

工具行按类型格式化(借 claw `format_*` 信息架构):`● Bash($ ls -la)` / `Read(file.rs)` /
`Write(file, N lines)` / `Edit(file)` + 一行 diff / `Grep 🔎 pattern` / MCP `server__tool(...)`;
状态染色 bullet(`●` running 动画 / green ✓ / red ✗)。工具结果 gutter `└ ` 缩进预览
前 N 行(dim,**双限截断** 行数+字符,中间省略),超出提示「full result in session/offload」。
gutter 视觉语言统一 `● › │ └`,左槽宽度常量对齐。连续 read/grep/glob 可折成 Exploring 分组
(可选,后置)。

- 验收:各工具人类可读摘要 + 输出预览;拒绝/失败/中断状态正确。

**完成记录(2026-07-17)**:
- **数据流**:工具结果内容此前没传到 TUI(`tool_end` 只有 `ok`),工具行格式化又需结构化 input(此前只有截断到 120 的 JSON)。给 `Ui::tool_start` 加 `input: &Value`、`tool_end` 加 `output: &str`(现有实现——server/plain/headless/测试 mock——只加忽略参数,最小 ripple,只 TUI 用);`run_one` 传 `&input` + `content.as_text()` 有界到 4000 字符(传输界,UI 再截断显示)。`AgentEvent::ToolStart.summary→input`(全 JSON 不截断)、`ToolEnd` 加 `output`;`Cell::Tool` 加 `input`+`output: Option<String>`。
- **新模块 `crates/tui/src/toolrow.rs`**(纯函数,`(name, input-json, status, output) → Vec<Line>`,不碰终端可单测):`tool_label` 按工具名 → (verb, detail):Bash `$ cmd`(后台标 `(background)`)/ Read path / Write `path (N lines)`/ Edit path / Grep `pat in scope` / Glob / Fetch / Search / read_offloaded / bash_output / kill_bash / tool_search / skill / call_tool(拆内层工具名)/ 其它(MCP `server__tool` 等)保留原名 + 紧凑 JSON。状态 bullet `●`(running,黄)/`✓`(绿)/`✗`(红);verb bold、detail dim 且按宽截断。edit 走 `- old`/`+ new` 一行 diff(红/绿,取自 input);其它工具走 `preview_lines`——`└ `/`    ` gutter、双限截断(`PREVIEW_MAX_LINES=5`/`PREVIEW_MAX_CHARS=400`)、失败红/正常 dim、超行提示 `… full result in session`。**`clean()` 消毒**:read_file 的 `{n}\t{line}` 制表符在 ratatui 里宽度 0 会重叠错位(真终端也错,非仅 pyte),tab→空格、其它控制字符丢弃。`tool_preview` 给折叠的子 agent 行复用同一 label。
- 接线:`render::cell_lines` 的 `Cell::Tool` 一行委托 `toolrow::tool_cell_lines`;子 agent 折叠行的 `last_tool` 与 note 用 `tool_preview`;`cells_from_history` 按 tool_use↔tool_result 配对回填 status + output 预览(孤儿 = 失败无输出)。
- **验收**:fmt + clippy(`-D warnings`)+ 全 workspace test 绿(tui 75 测试,新增 bash/read/write/grep 标签、后台标记、edit 一行 diff、双限截断+more 提示、失败红、空输出无预览、MCP 保原名、制表符消毒、长 detail 截断)。真 key **双轨** PTY+pyte(`--permission-mode bypass` 免审批)：anthropic + openai 各 4/4(`✓ Bash $ echo…`+`└ hello/world` 预览 / `✓ Read Cargo.toml`+内容预览+`… full result in session` / `✓ Grep workspace in Cargo.toml`+匹配预览;无原始 JSON 行;制表符消毒后行号列对齐)。
- 教训沉淀:HANDOFF 教训 40(工具结果原文的传输/展示分层 + 制表符/控制字符消毒是渲染正确性)。

### 切片 3 — 多行 composer ✅ 完成(2026-07-17)

自绘多行输入 widget 替换单行 `input_view`:`Shift+Enter`/`Ctrl+J` 换行、多行光标移动、
上下键在首/末行切**输入历史**(否则移动光标)、大段粘贴(>阈值)→ 占位符 `[Pasted N chars]`
提交时展开、图片粘贴 → 附件占位(kloop 已有 image 输入)。左侧 `›` 提示符,空时 dim 占位串。

- 验收:多行编辑 / 粘贴大段 / 粘贴图片 / ↑↓ 调历史。

**完成记录(2026-07-17)**:
- **新模块 `crates/tui/src/composer.rs`**(纯函数,不碰终端可单测):`Composer` 持 `text`(可含 `\n` + 占位符)/`cursor`(char 索引)/`history`/`pastes`/`images`。编辑:insert_char/insert_newline/backspace/left/right/home/end(逻辑行感知)+ `goal_col`(上下移动保列)。**输入历史**:`up()`/`down()` 在首/末逻辑行切 history(否则移动光标),编辑即脱离 history 采纳为草稿,`down` 越过最新回到暂存草稿。**大段粘贴**(≥400 字符或≥5 行)→ `[Pasted #k: N chars]` 占位,submit 时 `replace` 展开;小粘贴逐字插入。**图片**:`attach_image(label, block)` 存附件,submit 随 `Submission{text, images}` 带走。`view(width)`:逐逻辑行按 content 宽(width-2 gutter)换行 + 光标可视位映射 + 窗口到 `MAX_ROWS=8`,空则 `› ` + dim 占位串;CJK 宽度感知。
- 接入:`App` 用 `composer: Composer` 替换 `input`/`cursor` + `submit_images`(ContentBlock 非 Eq 不能上 Command,走 App 字段被 loop 取);`on_key` 路由编辑键、Enter→`on_enter`(空则 None、`/`→Slash、否则 submit:展开文本 + 图片占位 User cell,running 则 Steer),Shift/Alt+Enter/Ctrl+J→换行,↑↓→composer。`render::draw` 改**动态高度** composer 区(附件 `📎` 行 + 换行输入 + 光标),`commit_overflow` 的 reserve 用 `composer_height`;删单行 `input_view`。`lib.rs`:setup/restore 开关 `EnableBracketedPaste`;`Event::Paste` → `load_image_paste`(单行图片路径存在且 magic 是 png/jpeg/gif/webp → `kloop_core::image::image_block_from_bytes` 附件,复用 core 无新 seam)否则文本粘贴;`Command::Submit` 取 `take_submit_images` 塞 `Turn.images`,worker 合并 `pending_images`(--image 首轮)建 `user_with_blocks`。
- **验收**:fmt + clippy(`-D warnings`)+ 全 workspace test 绿(tui 86 测试,新增 composer 12 项:换行+光标/goal_col/首末行历史/编辑采纳草稿/大段占位+展开/小粘贴/图片随提交/空提交 None/view 换行+光标/占位串/tall 窗口/CJK)。真 key **双轨** PTY+pyte:anthropic + openai 各 4/4(多行 `› first line`/`  second line` via Alt+Enter=`ESC CR`;500 字大段粘贴→`[Pasted #1: 500 chars]`;图片路径粘贴→`📎 shot.png` 附件;submit 真轮后 ↑ 调回 `say hi in one word`)。另 anthropic 端到端:粘贴真 64×64 PNG + 提问,模型答 "YES" 确认图片随 turn 送达(1×1 透明退化图 400 是 Bedrock `Could not process image`,与 `--image` 同路径同拒,非 bug)。
- 教训沉淀:HANDOFF 教训 41(bracketed paste + 终端换行键歧义 + 非 Eq 富数据走 App 字段而非 Command)。
- **OS 剪贴板直接取图(同会话追加,真 key 双轨端到端验收已过)**:参考 codex `clipboard_paste.rs`——触发键 **`Ctrl+V`/`Alt+V`**(终端把 Cmd+V 留给自己的文本粘贴,故用独立键,cc/codex 同款),不是空 paste 猜测。新 `crates/tui/src/clipboard.rs`(`arboard` 读 + `png` 编码):兼容两形态——**文件列表** `get().file_list()`(Finder 复制)读字节复用 `image_block_from_bytes`、**RGBA** `get_image()`(截图/浏览器复制)→ `png` 编码 → `image_block_from_bytes`。剪贴板读是副作用 → `on_key` 纯返回 `Command::PasteClipboardImage`,`ui_loop` 执行读取(成功 `attach_image`、失败 `Note`),保持 on_key 纯。依赖:`arboard 3`(macOS 后端只拉 `image[tiff]`,轻)+ `png 0.17`(RGBA→PNG)。验收:tui 88 测试(clipboard 2 项:RGBA→PNG→block 往返、坏缓冲拒绝;on_key 1 项:Ctrl/Alt+V→Command);真 key 双轨——osascript 把 64×64 PNG 塞剪贴板 + PTY 送 `\x16`(Ctrl+V)→ `📎 pasted image 64×64` 附件,提交后 anthropic+openai 模型均答 "YES"(图片经剪贴板→附件→turn→API 送达)。教训 42。
- **未做(后置)**:composer vim normal 模式、鼠标点选(已在「不做」)。
- **Windows/WSL 剪贴板状态(核过依赖+代码,darwin 已端到端验;非 darwin 未上真机)**:
  - **原生 Windows**:预期零改动可用——`clipboard.rs` 无任何 `cfg`/`target_os`/macOS 专属调用,arboard 的 Windows backend `get_image`(CF_DIB→RGBA)+`file_list`(CF_HDROP)公开 API 未被 cfg 掉,arboard 声明的 `cfg(windows)` `windows-sys` 依赖已进 Cargo.lock(lock 跨 target)。**只差在真 Windows/CI 跑一次销账**(本机 macOS 跑不了)。
  - **WSL**:是缺口——WSL 里 arboard 看到的是 WSL Linux 剪贴板非 Windows 的,Ctrl+V 抓不到图,当前**优雅降级**为 `[no image on the clipboard: …]` Note(不崩)。codex `clipboard_paste.rs` 有 PowerShell 兜底(`cfg(target_os="linux")` + WSL 探测 + shell 出去 `powershell.exe` dump 剪贴板图到临时文件 + Windows→WSL 路径转换),**待用户确认是否需要 WSL 再移植**(本机 macOS 验不了,需 WSL 环境真跑)。

### 切片 4 — 斜杠命令菜单 + @文件补全 ✅ 完成(2026-07-17)

输入 `/` 弹命令候选菜单(名 + 描述,`Tab`/`Enter` 补全,来源 `commands::` + skills);
`@` 弹文件选择器(后台异步文件搜索,尊重 .gitignore,复用 plan 14 的 ignore 族)。通用
**扁平菜单底座**(codex `ListSelectionView` 式:无边框盒,`user_message_style` 淡底 +
选中行 `accent`(cyan bold)整行上色,两列自适应,数字键快选,可滚动)。同一时刻至多一个 popup。

- 验收:`/` 菜单选/补全、`@` 文件选中插入、菜单键不漏给 composer。

**完成记录(2026-07-17)**:
- **触发检测(纯)`crates/tui/src/menu.rs`**:`detect_trigger(text, cursor, allow_slash)` 取「光标处回扫到空白」的当前 token——`/` 前缀且 token 在**输入首位**(整行开头才是命令)→ `Slash(query)`,`@` 前缀(**任意位置**,mid-line 文件提及)→ `File(query)`;`allow_slash=!running`(turn 运行时 `/` 是 steering 文本,菜单压制)。`slash_items` 按名字**前缀**(大小写不敏感)过滤目录序(built-ins 先),空匹配 → 不弹(未知 `/name` 仍照旧跑 + 报错)。`Popup{kind,query,items,cursor}`(move_up/down 钳制、selected)。
- **文件搜索(IO,core)`crates/core/src/fs_complete.rs`**:`complete_files(root, query, cap)` 复用 `ignore::WalkBuilder`(honor .gitignore、含隐藏文件、`filter_entry` 跳 `.git/.hg/.svn/.jj`),按相对路径打分排序——basename 前缀(0)> basename 子串(1)> 路径子串(2)> 散列子序列(3),再按路径长度(浅优先)+字典序;只出文件(目录仍下钻),`SCAN_CAP=4000` 访问上限防大树卡键、`cap=50` 出参。放 core 因 `ignore` 依赖已在此,TUI 不必新引。
- **App 接线 `app.rs`**:`popup: Option<menu::Popup>` + `commands`(启动播种);`on_key` 顶部(confirm/fork 之后)插 popup 捕获——`↑↓`/`Ctrl+P/N` 移光标、`Tab/Enter` **accept(补全不提交)**、`Esc` 关菜单(不清 composer、不 interrupt),**其它键放行**去编辑 composer 再走 `after_edit` 重算菜单;`Command::SearchFiles(String)` 让 loop 去搜(App 保持纯,不碰 IO,与 clipboard 同款);`set_file_results(query, paths)` 带**陈旧守卫**(composer 已变则忽略、空结果关菜单)。`accept_popup` 用 composer 新增 `replace_token`(回扫替换当前 token + 尾空格,前缀随 item.insert)。
- **渲染 `render.rs`**:`menu_lines`(纯,选中行整行 `REVERSED`——主题安全,同 fork picker;非选中行 label 常规 + detail dim,窗口到 `MENU_ROWS=8` 环绕光标)+ `draw_menu`(浮在 composer 上规则正上方、`Clear` 叠加,向上生长最相关行贴近输入);footer 开菜单时切「↑↓ choose · Tab/⏎ complete · Esc cancel」。`commit_overflow` 开菜单时跳过(同 confirm/fork,别在浮层下滚屏)。
- **lib.rs**:`slash_catalog(cfg)`(BUILTINS + skills/commands)+ cwd 传入;`Command::SearchFiles` 用 `spawn_blocking` 跑 `fs_complete`(await 内联,composer 未变,菜单本帧即显、无竞态);App 改在 `run()` 建好(带 catalog + resumed cells)传入 `ui_loop`(压参数,避 too_many_args)。
- **验收**:fmt + clippy(`-D warnings`)+ 全 workspace test 绿(tui 105 测试:menu 触发/过滤/钳制、composer replace_token、app slash 开/过滤/Down+Tab 补全/Esc 关/未匹配不弹/running 压制/@ 请求搜索+补全/陈旧守卫/键不漏、render menu_lines 高亮+窗口 + draw 浮层在 composer 上;core fs_complete 4 测试)。真 key **双轨** PTY+pyte(CPR 驱动模拟应答、TIOCSWINSZ 置窗)：anthropic + openai 各 5/5(`/co`→菜单列 `/cost`+`/compact`+描述 / ↑↓+Tab 补 `/compact` 且关菜单 / `@src`→列 `crates/*/src/*.rs` / Enter 补 `@crates/cli/src/ui.rs` / 箭头键不漏进 composer)。**菜单纯本地(无模型调用)**,双轨只为确认 TUI 在两 provider 配置下都构建+渲染。
- 未做(记为可能性):数字键快选(与打字查询的数字冲突,故略)、`@` 补目录下钻、模糊高亮片段、搜索防抖(现每键一搜,本地有界够快)、`@file` 在普通消息里自动内联内容(仅命令体 `expand_slash_injections` 展开,菜单只助打字)。
- 教训沉淀:HANDOFF 教训 43(补全菜单的纯/IO 分层 + PTY 需 TIOCSWINSZ 否则 inline viewport 塌成默认高)。

### 切片 5 — 动画状态行 + HUD + 按需渲染 ✅ 完成(2026-07-17)

spinner/shimmer 状态行(shimmer 光带 header + 动词 + `(1m05s • Esc to interrupt)` 耗时,
借 `shimmer.rs`+`motion.rs` ~260 行,尊重 reduced-motion);**FrameRequester 按需渲染**
(动画靠「渲染时给自己排下一帧」自驱,合并请求 + 120fps clamp,无变化 CPU 空闲——取代当前
每 loop draw);footer 两行(composer / mode + 系统状态,截图形态);footer 显示 context
剩余 %/token(复用 usage 记账)、model/mode。**thinking cell 在此片改为 CC 式动词 + 累计
耗时**(完成 `∗ Cogitated for Xs`、进行中动词现在式,与 spinner 动词同源),取代切片 0
沿用的 `∴` 最后一行预览。

- 验收:运行时有平滑动画 + 耗时;空闲无 CPU 空转;token/context 正确。

**完成记录(2026-07-17)**:
- **动画纯函数 `crates/tui/src/anim.rs`**(纯,单测):`spinner_glyph(phase,reduced)`(10 帧盲文,reduced→静态 `●`)、`shimmer_spans(text,phase,reduced)`(光带:整词 DIM,`BAND=4` 宽亮带按 phase 从左扫过再出画,相邻同亮度合并成 span;reduced→单 BOLD span)、`format_elapsed`(`8s`/`1m05s`/`2h03m`)、`reduced_motion()`(`KLOOP_NO_ANIM` 或 `TERM=dumb`)。`STEP_MS=100` 同时是相位步长和帧节奏。**没抄 codex 的 `shimmer.rs`+`motion.rs` ~260 行**(只抄手法),亮度式 shimmer 主题安全(styles.md 铁律,不押色相)。
- **计时在 loop、App 保持纯**:pure App 无时钟,`ui_loop` 持 `turn_started`/`thinking_started: Option<Instant>`——turn 钟随 `app.running` 起停(reconcile 在 loop 顶),thinking 钟在 events 分支按 `streaming_thinking()` 开/关转换起停,关闭时 `app.seal_thinking(secs)` 把耗时盖进 cell。每帧构 `render::Hud{elapsed,thinking,phase,reduced_motion}`(phase=`elapsed_ms/STEP_MS`)喂给 `draw`。
- **按需渲染 = tokio select 扮 FrameRequester**:loop 本就 select 驱动(idle 阻塞在 input/events 上、零 CPU),新增第三分支 `_ = sleep(tick_ms), if animating`——`animating = running && 无弹层`;idle 时该分支禁用(`if` 卫词),只在跑 turn 时按 `tick_ms`(reduced→1s,否则 100ms)唤醒重画推进 spinner/耗时。**不必自研 FrameRequester,select 卫词即达「无变化不重画、无 CPU 空转」**(修正 plan「取代每 loop draw」的假设:原 loop 已是事件驱动非空转,缺的只是动画的定时唤醒)。
- **活动行动画 `render.rs`**:`activity_line(app,hud)` 返回 styled `Line`——running→`{spinner} {shimmer(Working)} ({elapsed} · esc to interrupt)`;armed/awaiting 保持原静态文案;`has_activity_line`(无 Hud 版,给 `commit_overflow` 预留行用)。**thinking**:`Cell::Thinking(String)`→`{text,seconds:Option}`;`thinking_line(sealed,live,width)`——live(`visible_transcript` 特判流式末 cell,同 assistant 教训 39)→`∗ Thinking… (Xs)`、sealed→`∗ Thought for Xs`(无计时 resumed→`∗ Thought`),取代 `∴ 末行预览`。
- **footer 加系统状态**:`footer_line(app,width)` 返回 `Line`,左 `[mode]`+键位、右 `system_status`(`model · N% ctx`,窗口关→`~N tok`;model 空则省);右侧放不下(窄行)→只留左侧 hints。context 复用 usage 记账:新 `AgentEvent::Usage(u64)`,worker 每 turn 后发 `history.estimated_tokens()`;model/window 启动时 `App::with_context` 从 cfg 播种、initial used 从 history。
- **验收**:fmt + clippy(`-D warnings`)+ 全 workspace test 绿(tui 112 测试:anim 3、footer 徽标/gauge/窄行、活动行 spinner+耗时、thinking verb+耗时、Usage 事件、thinking 流式→seal)。真 key **双轨** PTY+pyte 各 6/6(spinner 盲文字符 / 动词 `Working` / `(Ns · esc to interrupt)` 耗时递增 / turn 后 footer `claude-sonnet-4-6 · 3% ctx`(openai `gpt-5.4-mini · 2% ctx`)context gauge 随 turn 增长 / idle footer 徽标+hints)。**空闲无 CPU 由 select 卫词架构保证**(PTY 测不了 CPU,但 `if animating` 卫词即证)。
- 未做(记为可能性):thinking 动词与 spinner 动词同源池(现固定 "Working"/"Thinking",没做 CC 式随机动词)、footer 双行独立布局(现复用 composer+footer 两行结构,系统状态挤右侧)、`last_note` 进 HUD(字段留着未用)、context gauge 精确到 token 数(现只显 %)。
- 教训沉淀:HANDOFF 教训 44(pure App 无时钟→计时在 loop + Hud 喂参;tokio select 卫词即 FrameRequester,不必自研;shimmer 用亮度不押色相主题安全)。

### 切片 6 — 会话头 + 配色体系 + diff 升级 ✅ 完成(2026-07-17)

开场 `SessionHeader` cell(圆角框 `╭─╮`:`>_ kloop`、model、cwd、branch、mode);`styles.md`
色规全面落地(cyan=输入/选中/状态,green=成功/新增,red=错误/删除,magenta=品牌,标题 bold,
次要 dim;避免 black/white/blue/yellow 前景);diff 渲染升级 GitHub 风格(行号 + gutter +/-/space
+ 语法高亮 + `+N -M` 统计),放宽 plan 21/25 的行数截断到 diff overlay 里看全(可选)。

**代码块 + diff 语法高亮用 `synoptic`**(2026-07-17 定库,关键决定 2):token kind → styles.md
配色叠在 `CODE_BG` 上;可与本片一起做,或拆成独立高亮小片(实现未开工,只定了库)。

- 验收:开场头美观、全局配色一致、diff 可读。

**开工时定拍板(2026-07-17,问用户)**:**品牌强调色 = magenta**(styles.md 明确留给品牌的槽,
主题安全,与关键决定 5「不用自定义色/不做终端色探测」一致;不取橙色——橙是自定义色、异色主题下
对比无保证)。**synoptic 语法高亮拆成独立小片(切片 7)**,本片 diff 升级不含高亮(避免高亮反复
改 diff、每块能独立验收)。

**完成记录(2026-07-17,提交 df6bcf9)**:
- **品牌语义切法**:magenta = kloop 自身在场(会话头框+标题、running spinner、footer mode 徽标);
  cyan = 用户/状态/选中(composer `›`、user `>`、running 工具/子 agent 标记 `●`/`…`、todo 进行中 `▶`、
  审批 y/a/p/n 动作条、菜单选中);green/red = 成功·新增 / 错误·删除;dim = 次要(todo pending `○`)。
  **消掉所有 yellow/DarkGray 前景**(status_mark/tool_mark 的 running、todo InProgress/Pending、confirm
  option bar),新增 `render.rs` `const BRAND = Color::Magenta`。
- **会话头 `Cell::SessionHeader{model,cwd,branch,mode}`**(app.rs 新变体)+ `render::session_header_lines`
  (纯:圆角框 `╭─╮│╰╯`、标题 `>_ kloop` bold brand、dim label 列 + default 值、按内容自适应宽 `MAX_W=72`、
  off-repo 省 branch 行)。**在 `lib.rs run()` 构造**(git/env 副作用留在这、渲染保持纯):`display_cwd`
  把 `$HOME` 缩成 `~`、`git_branch` best-effort `rev-parse --abbrev-ref HEAD`(非仓库/失败即省行)、mode 取
  `permissions.mode().label()`;`insert(0, header)` 作首 cell(resume 也领起回放),随转录溢出落 scrollback。
- **diff `+N -M` 统计**:`render::diff_stats_line` 数 preview 里 `+`/`-` 开头行(green/red),插在 confirm
  popup 描述与 diff 体之间;无 diff 行(如「overwriting existing file」提示)则不出统计行。**只在 TUI 渲染层
  加,不动 core `diff.rs` 线格式**(plain/server 共用的 preview 字节不变,避免 core/permissions 测试大改)。
- **验收**:fmt + clippy(`-D warnings`)+ 全 workspace test 绿(tui 117 测试,新增 session_header 结构/brand
  色/off-repo 省 branch、draw 端到端会话头、diff_stats 计数+green/red+无 diff 行返 None、todo 色板、footer 徽标
  brand;更新 confirm body 含统计行、option bar 改 cyan、tool_mark running 改 cyan)。真 key **双轨** PTY+pyte
  各 3 场景:①启动会话头(magenta 框/标题/徽标、cyan `›`、无 yellow)②bypass 真 turn(magenta spinner、`✓ Bash`
  green、`└ pineapple` 预览、无 yellow)③manual 编辑审批弹层(`+1 -1` green/red 统计 + 行号 diff + cyan 动作条,
  答 n 拒)。
- 品牌色追记(2026-07-24):用户不喜欢原 magenta 的粉红主调;只把 kloop chrome 的 `BRAND` 改为
  低饱和青蓝 `#6c9aa6`,会话头/working spinner/mode 徽标统一跟随。当天用户看过并排预览后又选定
  更醒目的鲜明青蓝 `#4fb3c8`(plan 42),品牌槽覆盖范围不变。cyan 仍专用于用户·状态·选中,
  代码块 keyword 的 magenta 仍是独立语法语义。回归测试锁定 RGB 值及其与 magenta/cyan 的区分。
- **验收**:品牌色单测 + `cargo fmt --all --check` + clippy(`-D warnings`)+ 全 workspace test +
  `cargo run -p kloop -- --mock` 全绿。提交见 plan 41。

### 切片 7 — 代码块语法高亮(synoptic) ✅ 完成(2026-07-17)

切片 6 拆出的独立高亮小片(关键决定 2 定库 `synoptic`)。markdown fenced 代码块按语言
`synoptic::from_extension` 逐行 tokenize,token kind → styles.md 安全配色叠在 `CODE_BG` 上。

**完成记录(2026-07-17,提交 3de88bf)**:
- 依赖:`synoptic 2.2.9`(纯 Rust,主依赖 `regex`/`char_index`/`if_chain`/`nohash-hasher`,无 C 编译)。
- `markdown.rs`:`Renderer` 加 `code_lang`(fenced info 串),`Tag::CodeBlock(kind)` 抓 `CodeBlockKind::Fenced(info)`。
  `flush_code` 改走 `highlight_code(body, lang)`——`lang_to_ext` 把 `rust`/`py`/`c++`/`bash,ignore`/`TypeScript` 等
  归一到 `from_extension` 的扩展名(未知/裸围栏→None 不高亮,退化为原暗底纯文本),`h.run(&lines)` 后逐行 `h.line(y,raw)`
  拿 `TokOpt::Some(text,kind)`/`None(text)`,一 token 一段 char 携 `base=bg(CODE_BG)` patch `token_style(kind)`。
  新 `hard_wrap_chars`(styled char 硬折,code 不按词 reflow)+ `pad_chars`(补 bg 空格成矩形),`coalesce` 合 span。
- **`token_style` 配色**(styles.md 安全,避 yellow/blue/black/white):keyword/boolean→magenta、string→green、
  comment→dim、digit/function/struct/namespace/tag/attribute/operator/type/key/header→cyan、其余→default。
  magenta 在代码块内是"关键字"(暗底矩形里与 UI chrome 语境隔离),精度是正则级(聊天够用,非编辑器级)。
- **流式无缝**:未闭合围栏在 `assistant_stream_lines` 的 forming 尾走裸文本,闭合后走 `markdown_lines` 才高亮——
  安全边界缓冲(切片 1)天然只对已闭合围栏高亮,无半块撕裂。diff 高亮**有意不做**:审批弹层里 +/- 整行 green/red
  是 diff 的首要信号,叠语法 fg 会打架;且需把语言穿进 core preview 线格式(不值当)。
- **验收**:fmt + clippy(`-D warnings`)+ 全 workspace test 绿(tui 121 测试,新增 rust 块 keyword magenta/string
  green/comment dim + 全 bg + 无 yellow、裸围栏/未知语言不高亮、`lang_to_ext` 归一+拒未知、`token_style` 安全色板)。
  真 key **双轨** PTY+pyte:让模型回 ```rust 块,anthropic + openai 均 5/5(代码落 CODE_BG(pyte `303030`)、
  `fn`/`let` magenta、`"hello"` green、无 yellow)。
- 教训沉淀:HANDOFF 教训 46(synoptic 逐行 tokenize→styled Chars 的接线;紧配色板下 magenta 双用靠语境隔离;
  diff 语法高亮为何不叠加在 +/- fg 方案上)。

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
- ~~关键决定 2:syntect 代码高亮引入 vs 后置~~(✅ 切片 1 拍板**后置**:代码块先做暗底,高亮留独立小片)。
  ~~高亮用哪个库~~(✅ 2026-07-17 定 **synoptic** 替 syntect,轻量纯 Rust;理由见关键决定 2)。
- ~~切片 2 工具行的具体样式细节(bullet 用 `●` 还是 `⏺`、gutter 符号)~~(✅ 定:bullet `●`(running,黄)/`✓`/`✗`,结果 gutter `└ `/`    `;对着真 key 调过,对齐、消毒制表符)。
- ~~**品牌强调色**定 magenta/cyan 还是**橙**~~(✅ 2026-07-17 切片 6 初定 magenta;✅ 2026-07-24
  按用户反馈先改为低饱和青蓝 `#6c9aa6`,再经并排预览改定鲜明青蓝 `#4fb3c8`)。语义切法不变:青蓝 = kloop 在场(会话头/spinner/mode
  徽标),cyan = 用户·状态·选中(`›`/`>`/工具标记/审批条/菜单选中);代码块 keyword 仍用 magenta。
  ~~gutter 提示符 `›`、mode 行~~ 已对真 key 双轨调过。
- ~~**切片 7(独立高亮小片)**:synoptic 代码块 + diff 语法高亮~~(✅ 2026-07-17 完成代码块高亮;
  diff 语法高亮有意不做——+/- 整行色是审批首要信号、叠语法 fg 会打架,见切片 7 完成记录)。
