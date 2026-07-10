# Plan 14 — 工具面补全

> ✅ 第一片(grep + glob,c795cea)、第三片(后台 bash,38ab3dc)已完成;切片 2(web)/4(图片)挂账,后续会话继续。

> 体量偏大,开工时选片,可能不止一个会话。开工前先读 docs/plan/HANDOFF.md。参考:cc 的 Grep/Glob/WebFetch/后台 Bash 形态、codex 的对应工具;实现细节回源核对(教训 11)。注意 P0 决定 1 仍然有效:编辑保持 Edit 形态,不做 apply_patch/多文件 patch。

## 现状(补全的起点)

六个内置:bash、read_file、write_file、edit_file、read_offloaded、task。文件搜索靠 bash 转 rg/grep(能用:word-only 白名单已把它们归只读、可并发),但引号/转义易错、输出无结构、每次占一条 bash 调用。网络完全没有(core 无网络是刻意边界)。长命令只有前台 + 超时。

## 候选切片(开工时和用户定选哪几片、什么顺序)

1. ✅ **grep + glob 专用只读工具**(最高频,建议必选):倾向纯 Rust 库实现(ripgrep 的 `grep-searcher`/`ignore` crate 族,零外部二进制依赖,尊重 .gitignore),不倾向 spawn rg(存在性/版本不可控)——开工时定。天然 readonly:进并发批、权限只读自查直接放行。输出形态(文件:行号:内容、命中数上限、超限截断进 offload)对照 cc 定。要不要顺带 list_dir,开工时定。
2. **web_fetch / web_search**:架构决定优先于功能——core 无网络是硬边界(reqwest 独占在 provider),所以两条路:做成 cli 注册的内置 ToolSource(复用 MCP 的缝,core 零改动,倾向);或者用 Anthropic 服务端 web_search 工具(provider 层声明即可,但 OpenAI 轨没有对等物、且不解决 fetch)。开工时定,可能两者都要。fetch 的安全面:SSRF(内网地址拒绝)、大小上限、HTML→文本降噪。
3. ✅ **bash 后台任务**:`run_in_background` 参数 + 查询输出/终止的配套工具(cc 形态)。进程生命周期归属(退出时回收、interrupt 语义)、输出缓冲落 offload 目录。价值:dev server、长编译。
4. **图片输入**:read_file 读图片 → 协议要加 image 块(protocol ContentBlock + 双 provider 翻译 + rollout 前向兼容),和 plan 15 的 thinking 是同类协议扩展,开工时定归 15 一起做还是这里做。

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
