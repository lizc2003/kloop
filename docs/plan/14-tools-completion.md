# Plan 14 — 工具面补全

> ✅ 第一片(grep + glob,c795cea)、第三片(后台 bash,38ab3dc)、第二片(web_fetch/web_search,2fed3af + 3a86009,双轨验收全过)已完成。剩切片 4(图片,倾向归 plan 15 协议扩展一起做)。

> 体量偏大,开工时选片,可能不止一个会话。开工前先读 docs/plan/HANDOFF.md。参考:cc 的 Grep/Glob/WebFetch/后台 Bash 形态、codex 的对应工具;实现细节回源核对(教训 11)。注意 P0 决定 1 仍然有效:编辑保持 Edit 形态,不做 apply_patch/多文件 patch。

## 现状(补全的起点)

六个内置:bash、read_file、write_file、edit_file、read_offloaded、task。文件搜索靠 bash 转 rg/grep(能用:word-only 白名单已把它们归只读、可并发),但引号/转义易错、输出无结构、每次占一条 bash 调用。网络完全没有(core 无网络是刻意边界)。长命令只有前台 + 超时。

## 候选切片(开工时和用户定选哪几片、什么顺序)

1. ✅ **grep + glob 专用只读工具**(最高频,建议必选):倾向纯 Rust 库实现(ripgrep 的 `grep-searcher`/`ignore` crate 族,零外部二进制依赖,尊重 .gitignore),不倾向 spawn rg(存在性/版本不可控)——开工时定。天然 readonly:进并发批、权限只读自查直接放行。输出形态(文件:行号:内容、命中数上限、超限截断进 offload)对照 cc 定。要不要顺带 list_dir,开工时定。
2. ✅ **web_fetch / web_search**:架构决定优先于功能——core 无网络是硬边界(reqwest 独占在 provider),所以两条路:做成 cli 注册的内置 ToolSource(复用 MCP 的缝,core 零改动,倾向);或者用 Anthropic 服务端 web_search 工具(provider 层声明即可,但 OpenAI 轨没有对等物、且不解决 fetch)。开工时定,可能两者都要。fetch 的安全面:SSRF(内网地址拒绝)、大小上限、HTML→文本降噪。
3. ✅ **bash 后台任务**:`run_in_background` 参数 + 查询输出/终止的配套工具(cc 形态)。进程生命周期归属(退出时回收、interrupt 语义)、输出缓冲落 offload 目录。价值:dev server、长编译。
4. **图片输入**:read_file 读图片 → 协议要加 image 块(protocol ContentBlock + 双 provider 翻译 + rollout 前向兼容),和 plan 15 的 thinking 是同类协议扩展,开工时定归 15 一起做还是这里做。

## 完成记录(第二片:web_fetch + web_search,2026-07-10)

- **开工拍板**:走 cli 注册的 ToolSource 缝(core 零改动);web_search 也做,默认后端 Brave Search API(agent 生态最常用的中立选项;Bing API 已退役、Google CSE 限额难用、DDG 无官方 API),`SearchBackend` trait 保证可扩展(加 provider = 一个 trait 实现 + cli match 一条臂);Anthropic 服务端 web_search 不用(provider 层的另一条缝,OpenAI 轨无对等物),记为将来可选第二实现。
- **回源核对 cc**:WebFetch = url+prompt、turndown 转 Markdown、Haiku 加工、15 分钟缓存、跨 host 重定向不跟随(返回 REDIRECT DETECTED 让模型显式重发)、http→https 升级、URL≤2000、拒绝内嵌凭据;SSRF 防护很弱(只查 hostname 段数)。WebSearch = 适配器工厂(api/tavily/bing/brave/exa,这个逆向版默认自建 tavily 代理)。**抄了**:跨 host 重定向不跟随、http→https 升级、凭据拒绝、URL 长度上限。**没抄**:prompt+小模型加工(kloop 无小模型缝)、缓存、turndown(手写 HTML→text,零新依赖)、preapproved 域名白名单;SSRF 用 kloop 自己的 IP 级检查(环回/私网/link-local/CGNAT/元数据段全拒,域名解析后逐地址查,比 cc 强)。
- **结构**:新 crate `kloop-web`(reqwest 网络实现;fetch.rs/html.rs/search.rs),cli `web.rs` 胶合(`[web]` 配置解析 + ToolSource 适配,web 源注册在 MCP 之前)。原实现把模型可见 ToolDef 也放在 `kloop-web`;plan 44 后移到 `core/src/tools/web.rs`(工具名/描述/schema),CLI 将 core 契约绑定到 `kloop-web` 操作,core 仍不依赖网络。web_fetch 常开(--mock 除外);`BRAVE_API_KEY` 未设或 provider 未知时 web_search 不注册、启动警告降级。两工具 readonly 进并发批;权限门按外部工具处理(默认询问,`web_fetch` allow 规则可放行)。
- **测试**:+20(223 总):SSRF 拒绝表(loopback/私网/link-local/169.254 元数据/CGNAT/v4-mapped/localhost/file/ftp)、URL 卫生(升级/凭据/超长)、同 host 重定向跟随与循环上限、跨 host 重定向报告不跟随、HTML→text 契约(script/style/注释剥除、实体解码、块级换行、畸形输入)、大小写截断文案、非文本类型拒绝、Brave wiremock 契约(query/header/高亮剥除/429/空结果)、`[web]` 配置解析与降级路径。
- **验收(全部销账)**:web_fetch 双轨真 key 过(sonnet-5 抓 example.com 摘要;gpt-5.4-mini 给 http URL 自动升级 https 无异常);web_search 真实 Tavily 端点双轨过——sonnet-5 走 搜索→挑结果→web_fetch 验证内容 的完整闭环,gpt-5.4-mini 主动带 site: 限定与 max_results 查到 tokio 最新版本并给来源 URL,均首试正确。
- **拍板变更(同日,3a86009)**:默认搜索后端 Brave → **Tavily**——Brave 已取消无卡免费套餐,默认值应该开箱能用;Tavily 免费档 1000 次/月无卡,且是 agent 生态最常用后端(cc 逆向版默认适配器就是 tavily)。Brave 实现保留,`[web] search_provider = "brave"` + `BRAVE_API_KEY` 可切换。key 环境变量按 provider:`TAVILY_API_KEY` / `BRAVE_API_KEY`。
- **挂账**:搜索后端第三实现(searxng 或 Anthropic 服务端);fetch 缓存、`prompt` 参数小模型加工(等有便宜模型缝)。

## 备选池(未承诺)

- **write_stdin(交互式后台进程)**:codex unified_exec 的核心差异——往活着的后台进程写 stdin(REPL、ssh、交互式确认),空写 = 轮询。最小切法:在现有后台 bash 上加一个 `write_stdin(bash_id, chars)` 工具,连管道即可(需把后台 spawn 的 stdin 从 null 改为 piped),不抄 PTY/审批编排。调研结论见 refs/README.md"codex 后台/交互进程"节(2026-07-10)。
- 顺手小件(有痛感时抄):HeadTailBuffer(bash_output 头尾各留一半,保住开头报错)、后台进程表上限 + LRU 淘汰。

## 不做(维持现状)

apply_patch(P0 决定 1);notebook 工具(niche);TodoWrite 类计划工具(价值存疑,备选池观察)。

## 测试

grep/glob:契约测试(命中形态、gitignore 尊重、上限截断、并发批分类、权限只读放行);web:ToolSource 缝的 mock 测试 + SSRF 拒绝表;后台 bash:启动/查询/终止/退出回收、interrupt 不留孤儿进程;图片:协议往返 + 两 provider 翻译契约。

## 完成标准

fmt/clippy/test 全绿;真 key 手工验收按所选切片(至少:模型用 grep 工具完成一次真实代码检索任务,对比 bash-rg 无引号转义痛点);README、HANDOFF 更新;未选切片记挂账。

## 完成记录(第一片:grep + glob,2026-07-10)

- **开工拍板**:选片只做 1;纯 Rust 库实现(`grep-searcher`/`grep-regex`/`ignore`,rg 同源 crate);list_dir 判据 = "cc 有才做",回源核对发现 cc(逆向 TS 版)已移除 LS 工具(目录探索 = Glob+Grep+bash ls),所以不做。
- **形态对齐 cc**(两个参考库都回源核对过):grep 参数面 `pattern/path/glob/type/output_mode/-i/-n/-A/-B/-C/head_limit(默认 250,0=不限)/offset/multiline`,三种 output_mode 的文案、空结果文案("No matches found"/"No files found")、500 列单行截断、`--hidden` + 尊重 .gitignore + 显式排除 VCS 目录、20s 预算;glob 上限 100 + cc 原文截断提示。未抄:cc 的 `context` 参数(-C 的冗余别名)。
- **两处有意偏离 cc**:① glob 也尊重 .gitignore(cc 的 glob 默认 `--no-ignore`,Rust 仓库不滤 target/ 全是垃圾);② glob/grep-files 排序都是 mtime 新→旧再截断(cc 的 glob 是 rg `--sort=modified` 旧→新再取前 100,截掉的恰是最新文件;claw-code 也是新→旧)。
- **claw-code 调研结论**(反面教材,细节见会话):walkdir+regex 自研,不尊重 .gitignore、glob/grep 忽略策略不对称、multiline 语义失效、无超时——印证选 ripgrep crate 族。
- **实现**:`core/src/search.rs`(纯新增)+ tools.rs 注册/分发/并发白名单 + permissions.rs 只读自查白名单;`spawn_blocking` 包同步搜索。测试 +18(196 总):三种模式形态、gitignore/隐藏/.git/二进制、glob+type 过滤、-C/-A 上下文、multiline 开关、分页与截断文案、单行截断、错误面(坏正则/坏 glob/未知 type/不存在路径)、mtime 排序、100 上限、并发/权限白名单。
- **验收**:双轨真 key 过——sonnet-5 用 grep(content)+glob 并发完成真实检索;gpt-5.4-mini 用 count 模式 + `head_limit:0` + glob,首试全对,无 bash-rg 引号转义痛点。
- **挂账**:切片 2(web_fetch/web_search)、3(后台 bash)、4(图片输入,或归 plan 15);grep/glob 的路径级权限规则(`grep(<glob>)` 形)未做——deny 只有工具名粒度,`read_file(**/*.pem)` 式 deny 挡不住 grep 读同一文件,有痛感再补。

## 完成记录(第三片:后台 bash,2026-07-10)

- **回源核对 cc**:cc 已把后台 bash 并入统一 Task 框架(TaskOutput 已标 Deprecated、引导直接 Read 输出文件;完成推 `<task-notification>`;5GB 磁盘 watchdog;treeKill 进程组;后台模式 timeout 定时器被清除;权限判定与前台完全一致;TaskOutput readonly+并发安全、TaskStop 并发安全)。kloop 取骨架:落盘输出文件 + 返回路径、block/timeout 查询、进程组 kill、interrupt 不杀后台、权限同前台;不抄自动后台化、完成通知、stall 探测、Monitor 工具。
- **形态**:`bash` 加 `run_in_background`(后台忽略 timeout_ms,cc 同款);stdout/stderr 在 fd 层交织直写 `.kloop/offload/bg-N.out`(零 reader task、无管道死锁,cc file 模式同款);配套 `bash_output`(bash_id/block=true/timeout_ms=30000 上限 600000,回 running/completed/failed/killed + 尾部 30k 字节,更早内容引导 read_file 读文件)与 `kill_bash`(杀整个进程组,等 monitor 确认后返回)。
- **生命周期**:每 shell 一个 monitor task(select:退出/kill 令牌/5s watchdog),不挂 turn 的 cancel token——interrupt 天然不杀后台;`process_group(0)` 让终端信号也到不了它;1GiB 输出文件上限超限杀组;注册表 Drop 时对仍在跑的组补刀,`kill_on_drop` 兜底。注册表 `BackgroundShells` 挂 Config(Arc,子 agent 共享、server 每 thread 一个),bg 编号进程级全局(教训 2:共享 offload 目录防撞名)。
- **权限/并发**:后台 bash 权限判定与前台完全一致(判的是 command 字符串,与前后台无关);`bash_output`/`kill_bash` 进只读自查白名单(后者只能 signal agent 自己拉起的进程)+ 并发安全(对齐 cc)。
- **测试**:+7(203 总):启动即返、阻塞等待到 completed(stdout/stderr 交织)、exit code 上报、后台忽略 timeout + interrupt 后仍在跑、kill 五秒内确认 + 二次 kill 报 not running、block 超时文案、大输出只回尾部、未知 id 错误面;工具面契约(defs 顺序、50 工具告警、并发/权限白名单)同步更新。
- **验收**:双轨真 key——sonnet-5 走 启动→bash_output 阻塞等待→汇报 exit 0 与输出;gpt-5.4-mini 走 启动→block=false 窥探 running→kill_bash 终止,全部首试正确。无孤儿进程残留。
- **已知边角**:管道喂 stdin 时 REPL 缓冲会吞掉审批答案(HANDOFF 已记录的 CLI 阻塞读边角,验收改用 AGENT_ALLOW 绕过);会话内不主动通知后台完成(cc 的 task-notification 无对应通道),模型需自行 bash_output。
