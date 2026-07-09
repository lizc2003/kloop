# HANDOFF — 当前状态与会话交接

> 全局约束(定位、工作方式、风格、偏好)在根目录 CLAUDE.md(自动加载)。本文件是细节:读完即可开工,无需重新调研。参考库知识在 `refs/README.md`。

## 一、已拍板的架构决定(P0,2026-07-09)

1. 编辑工具用 Edit(old_string/new_string)形态,不迁就 apply_patch(实测 gpt-5.4-mini 都能首试用对)。
2. 目标模型双轨:Claude(sonnet-5)为主、OpenAI-compat 为副。
3. 对 codex 上游只保持"可跟随性",不追求可合并。

## 二、当前状态(plan 1–10、12 完成)

**结构**:Cargo workspace,七 crate,到 core 为止严格单链,其上两个平级前端 + 一个协议级旁支(详见 `kloop/README.md` Layout 节):
`kloop-protocol`(零依赖线格式)← `kloop-provider`(适配缝,独占 reqwest)← `kloop-core`(agent 本体,无网络)← {`kloop-tui`(ratatui 前端,独占终端), `kloop-server`(多会话 JSON-RPC 前端)} ← `kloop`(cli,解析参数后分发);`kloop-mcp`(MCP stdio 线协议客户端)只依赖 protocol,由 cli 胶合(core 不依赖 mcp)。

**能力**(全部真实 API 验证过,除注明):
- 5 个原始赌注:append-only 历史 + 录入时 offload(>8000 字符落盘 + 指针 + read_offloaded 回读)、tool_use 有无判续跑、按入参动态并发(连续安全调用并发批)、task 子 agent 递归复用 run_turn(深度限 1)、provider 适配缝(Anthropic SSE / OpenAI-compat / Mock)。
- 压缩双防线:predictive(采样前预判 当前+增长预留 是否爆窗,增长 = min(输出上限,20k)+15k,**窗口≤预留时跳过**)+ reactive(溢出错误 downcast OverflowError,每 turn 压缩一次重试)。压缩 = 模型写交接摘要 + 保留约 2k token 近期原文,边界绝不切开 tool_use/tool_result 对,失败不动历史。
- 恢复语义:流式重试 3 次(指数退避 + 纳秒抖动)、fallback 模型(AGENT_FALLBACK_MODEL,每 turn 切一次)、截断续跑(stop_reason=max_tokens/length 且无 tool_use 时注入续跑提示,限 3 次;这是 stop_reason 的唯一合法用途)、中断孤儿修补、EndReason 四态。
- token 记账:provider 回传 usage(Anthropic message_start/delta;OpenAI include_usage,usage 块在 finish_reason 后到)锚点 + 其后消息 chars/4 估算;压缩后锚点作废。
- 权限门(`core/src/permissions.rs` + `core/src/shell.rs`;双轨真实 API 全流程验证过,记录见 plan 8):cc 形态管线——deny 规则 → 安全检查(危险命令 `rm -rf`/`sudo`、敏感路径 `.git`/`.kloop`/`.ssh`/rc/`.env*`,**bypass 免疫**)→ ask 规则 → bypass → 只读自查 → acceptEdits(cwd 内文件写)→ allow 规则 → 会话缓存 → `Approver` trait 询问(类型擦除 future)。不变量:deny 永远先于 allow。bash 判定跑在 tree-sitter-bash word-only 白名单遍历上(移植 codex shell-command;子 shell/重定向/替换/赋值 → Opaque,永不自动放行/命中 allow/进缓存;`bash -c` 递归解包;只读分类器审查选项含 git 全局选项注入;deny/危险匹配前剥 sudo/env/timeout/xargs wrapper)。规则三形态:`tool` / `bash(tokens [*])`(按段,allow 全段须覆盖、deny 任一段命中)/ `write_file|edit_file|read_file(glob)`(globset,词法规范化路径 + cwd 相对双匹配)。规则来源 `.kloop/config.toml` `[permissions]` allow/deny/ask + `AGENT_ALLOW`/`AGENT_DENY`/`AGENT_ASK` 叠加。询问 y/a/p/n:a = 会话缓存(bash 两词前缀签名、文件按父目录);p = 追加建议规则(`bash(git commit *)` 形)进 config.toml(toml::Table 往返,保留无关段落,不保注释)。拒绝 = is_error tool_result + 改道引导,turn 继续。`Permissions` Arc 挂 Config,子 agent 继承,描述带 `[sub-agent]`/`[destructive]`/`[sensitive path]` 标签。CLI:`--accept-edits`、`--yolo`(= bypass,deny/安全检查仍生效)、`--mock` 才是完全无门。CLI 阻塞读边角:Ctrl+C 打断询问时孤儿读可能吞掉下一行输入(已接受)。调研结论沉淀在 `refs/README.md` 权限系统对比一节;沙箱/escalation/execpolicy 等待有沙箱基建再抄。
- TUI(`crates/tui`,默认入口;`--plain` 保留裸 REPL,`--mock` 仍走 plain 保持无交互验证命令可用):alternate screen 全屏 + 自维护 cell 缓冲 + 滚动偏移(codex 的 inline viewport 靠 ~35KB vendored CustomTerminal + 自写 scroll-region 机制,对最小可用太重,弃用;见教训 9)。`ChannelUi` 同时实现 `Ui`+`Approver`,事件经 mpsc 进 UI 循环,审批走 oneshot 回传(sender 丢弃 = Deny);agent 在独立 tokio task 持有 History;`App` 纯状态机(delta 聚合、工具行 …/✓/✗、confirm VecDeque 排队、按键→Command);渲染纯函数(CJK 宽度 wrap/truncate、工具行折叠、y/a/p/n 居中弹层);delta 攒批重绘;panic hook 恢复终端。core 配套:`Ui` 加 `tool_start`/`tool_end` 默认方法(默认退化为 note,plain 零改动)。真 key 验收已过(双轨,方法与结果见 plan 9 完成记录)。`--resume` 重放旧会话进转录区(`cells_from_history`:工具行按 tool_result 配对还原 ✓/✗,孤儿 ✗)。
- server 模式(`crates/server`,cli `--serve`;plan 12,真 key 验收已过):stdio 类 JSON-RPC(信封仿 codex app-server,无 "jsonrpc" 字段),多会话并行——`thread/start|resume|list`、`turn/start|interrupt`;每 thread 独立 task 持有 History(与 tui/plain 共用 `.kloop/sessions/`,会话可互换)+ 独立 Permissions(审批缓存不跨 thread,Config 由 cli 工厂闭包按 thread 构造);通知按 thread 标记(turn/started、text/delta、note、tool/started、tool/completed、turn/completed);审批 = server→client 请求(`srv-{n}` 独立 id 空间),回复 decision 四值,丢失/EOF = deny;`--mock --serve` 可无 key 全协议演示。坑:同秒双 `thread/start` id 相撞(rollout 懒创建),修法 = 选定 id 立即建空文件占位。
- MCP 客户端(plan 10;`crates/mcp` + `cli/src/mcp.rs` + core 的 `ToolSource` 缝;真 key 双轨闭环验收已过,见 plan 10 完成记录):stdio 换行分隔 JSON-RPC(**不是** LSP Content-Length;claw-code 的分帧是错的别抄)。握手 = `initialize`(protocolVersion 2025-06-18,capabilities `{}`)→ **必发** `notifications/initialized` → `tools/list`(跟 `nextCursor` 翻页,description 截 2048)。命名 `{server}__{tool}`,非 `[A-Za-z0-9_]` 一律折成 `_`(比 cc 严:`-` 也折,因为权限规则语法只收 alnum+`_`,"p" 持久化的规则必须能解析回来),超 64 字符跳过。`tools/call` 发**原始**工具名(前缀名只是客户端路由标签);结果 content 数组拍平成文本(二进制块降级为 `[image: …]` 标签),`isError:true` → is_error tool_result。ToolDef name/description 已改 String(protocol)。core 缝:`Config.tool_sources: Vec<Arc<dyn ToolSource>>`,`all_tool_defs` 合并(撞名跳过后者),`tool_merge_warnings` 给 cli 启动时打警告(撞名 + >30 工具),`is_concurrency_safe` 第三参查 source 的 readonly 标注(默认串行);权限层零改动——未知工具名天然落"询问",记忆粒度=整工具名。配置 `[mcp.servers.<name>]`:`command`(argv 数组,必填)/`env`(叠加在继承环境上)/`readonly`(原始工具名列表,只影响并发不影响权限)。生命周期:启动时连一次(--mock 不连),失败降级警告不阻塞,Config 共享进所有 server 线程与子 agent,退出时 kill_on_drop。子进程 stderr 接 null(TUI 独占终端)。超时:握手 30s(`npx -y` 每次启动都重查 registry,实测 13s+,10s 会误杀;两参考库也都是 30s)、tools/list 30s、tools/call 60s。deferred 工具 + tool_search 仍是未来工作。
- 会话持久化(`core/src/rollout.rs`):`.kloop/sessions/{id}.jsonl` 逐条写透(History 可挂 Rollout;写失败降级纯内存);每行带信封 id(`{stem}#{seq}`,无 rand)/ parent(上一行 id,跨恢复续链)/ ts,重放线性但链是未来 rewind/fork 的 schema 地基,未知字段读取忽略(前向兼容,测试锁死);压缩追加 compacted 标记内嵌完整替换历史(仿 codex rollout),文件保持 append-only;恢复(`resume_session`)= 重放 + 双向配对修补(正向补 interrupted、反向删孤儿 tool_result)+ 坏尾**物理**截断 + offload 计数器 fetch_max 同步;`load_session` 只读不动文件;usage 锚点不落盘,首次采样重锚定。CLI `--continue`(最近会话)/ `--resume`(无 id = 编号列表选择,UI 启动前 stdio 交互)/ `--resume <id>` / `--list-sessions`,session id 为 UTC 时间戳(手写 civil_from_days,无 chrono);子 agent 历史不持久化。

**测试**:143 个。mcp wire 契约(duplex 进程内 mock server:精确握手 JSON、翻页、schema 整对象透传、isError/RPC 错误映射、EOF 断挂账、拒绝 server 发起请求、并发按 id 路由)/ core 缝(合并撞名跳过、警告、外部工具分发与错误映射、readonly 并发分类)/ 权限 mcp 风格名默认询问+整名记忆 / cli `[mcp.servers]` 解析与命名消毒 / server wire 契约 + duplex 协议测试(流式/审批往返/并行不串/同秒 id/忙拒绝/interrupt/协议错误韧性/跨重启 resume)/ tui 事件契约(Ui→channel 序列、confirm 往返、丢 sender=Deny)/ App 状态折叠 / 纯渲染(CJK wrap、工具行折叠、输入光标窗口)/ protocol 线格式契约 / provider wiremock HTTP 契约(SSE 序列进、StreamEvent 断言出)/ tools 全执行路径 / shell 解析契约(word-only、引号拼接、不透明构造、`bash -c` 解包、选项审查、git 注入、wrapper 穿透)/ permissions 管线(deny 胜 allow 与 bypass、安全检查 bypass 免疫、敏感路径不可缓存、ask 规则胜 allow、acceptEdits 边界、glob 规则、两词缓存、AllowAlways 持久化、Opaque 不可缓存)/ history 锚点数学 + 写透 / compact 失败不动历史 / agent 恢复路径 / rollout 往返、标记重放、孤儿修补、跨重启 resume / cli 参数、时间戳、config 往返。纪律:适配器行为变更必须先改契约测试。CI(`.github/workflows/ci.yml`,仓库根)在 push/PR 上强制 fmt --check / clippy -D warnings / test,macOS+Linux;**仓库尚无远端,workflow 只做过本地等价验证,首次推远端后要看它实际跑绿一次**。

**运行**:真 key 用 `ANTHROPIC_API_KEY`+`ANTHROPIC_BASE_URL`(不含 /v1,适配器自己拼 /v1/messages)或 `OPENAI_API_KEY`+`OPENAI_BASE_URL`+`AGENT_MODEL`;可选 `AGENT_CONTEXT_WINDOW`(默认 200000,off 关压缩)、`AGENT_FALLBACK_MODEL`、`AGENT_PROVIDER`、`AGENT_ALLOW`/`AGENT_DENY`/`AGENT_ASK`(权限规则,叠加在 `.kloop/config.toml` 之上)。

## 三、沉淀的实现教训(新代码沿用)

1. 递归 async 的 Send 推断有共归纳盲区:递归边界必须是签名级类型擦除(`execute_tool` 返回 `Pin<Box<dyn Future + Send>>`)+ 子 agent `tokio::spawn` 成独立任务。
2. offload 编号用进程级全局 AtomicUsize——父子 agent 共用目录,per-实例计数会互相覆盖。
3. 子 agent(depth>0)的文本不流式输出到主 UI,产出走 tool result。
4. cc 的公式移植前先想小参数退化(predictive 负阈值教训)。
5. 压缩/重写类操作失败时必须保证原数据原封不动,测试锁死这一点。
6. 追加型文件的坏尾必须**物理**截断再续写:只做逻辑跳过的话,追加会和半行拼接成一行,之后的数据全部不可达(plan 7 潜伏 bug,7b 修复)。
7. 磁盘 schema 是基础模块里最难改的部分:演化字段(id/parent/ts)要在有存量文件之前落上;性能工程不改格式,可以后补。
8. 安全判定别手写 shell 拆分:字符串 replace/split 版一天内被找出三个注入洞(`$()`、换行、单 `&`),补丁式修补追不完;正解是 tree-sitter 白名单遍历 + "解析不了 = 不可分析 = 永不自动放行"。凡是"分类后放行"的逻辑,分类器必须是白名单而非黑名单。
9. 参考实现互相矛盾时以官方 SDK/spec 为准,不看谁的代码顺眼:MCP stdio 分帧 claw-code 用 LSP Content-Length(错),cc 包的官方 SDK 用换行分隔 JSON(对);跟真实生态互操作的协议细节,权威来源只有 spec 和官方 SDK。
10. 外部注入的名字要过本系统最窄的语法关:MCP 工具名进了权限规则语法(alnum+`_`),消毒时 `-` 也得折成 `_`,否则"允许并保存"写出的规则下次启动解析失败。
11. 抄参考实现的形态前先量它的真实移植成本,`[patch]` 段只是线索不是结论:codex tui 的 fork 增量其实极小(ratatui 仅 +`expose set_viewport_area`,crossterm 仅 +`query_fg/bg_color` 主题探测;scrolling-regions 在上游 0.29 本来就有),真正重的是 vendor 进仓库的 ~35KB CustomTerminal(ratatui Terminal 魔改副本)+ insert_history 自写 scroll-region 命令那套机制。对最小可用,这个体量本身就是不抄的理由;全屏 + 自维护缓冲的决策不变。

## 四、进度

下一个:**plan 11(hooks)**。plan 9/10/12 无挂账(均真 key 验收已过;plan 10 双轨,官方 filesystem server 全闭环)。真 key 在 `.kloop/env.local`(gitignored,勿写进任何提交文件)。
