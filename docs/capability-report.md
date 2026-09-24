# kloop 能力对比报告(vs claude-code / codex)

> 滚动更新,不设基线日期:**以各行自己的销账为准**(✅ + 日期 + 提交号)。
> 头部写死一个基线会过期而没人回头看——2026-08-15 那版就写着"到 plan 88",
> 正文却已经销账到 plan 156。精确 Claude Code 2.1.220 parity 的取证与验收见
> Plan 48–59 各自的计划文件。
> 用途:**补齐能力时对着本报告挑项**——每项差距标了出处、收敛强度、补齐路径与触发
> 条件;完成后在对应行销账(标日期 + 提交号)。项目状态细节在 `docs/plan/HANDOFF.md`,
> 参考库知识在 `refs/README.md`,本文件只管"差在哪、补不补、何时补"。

## 一、总评

- **规模**(2026-09-21 重量):kloop 10 crate 共 **13.5 万行** Rust,其中约 7.2 万行
  是测试(集成 1.2 万 + 单元 6.0 万)——**production 约 6.3 万行**;codex codex-rs
  ~118 万行、cc(TS 版)~71 万行。kloop 用约 5% 的代码量覆盖核心引擎面——对比按
  "同一能力的质量"看,不按面积。(2026-08-15 这里写的是"~3.1 万行",四十个 plan
  之后没人回头改过。)
- **形态完成度:约九成**。refs 标注的"必然解"收敛面全部落地,多处质量反超(见二)。
- **差距集中三类**:①长尾纵深(每能力主干齐、cc 的兜底层缺);②平台/生态宽度(沙箱
  单平台、MCP 单传输、前端打磨);③**实战里程**(唯一抄不来的,靠 dogfood 还)。

## 二、质量反超点(保持住,别在后续改动中回退)

| 反超点 | 参考的对应弱点 | 出处 |
|---|---|---|
| predictive 压缩负阈值守卫 | cc 小窗口时"永远压缩"盲点 | 教训 4;refs/README 压缩节 |
| web_fetch SSRF 逐地址查 | cc 只做段数检查 | HANDOFF web 工具条 |
| 子会话血缘到**行级**(`subagent_of`) | cc/codex 只到 session 级 | 教训 22 |
| code mode 用 QuickJS + 内存硬限 | codex V8 包袱且恰缺内存上限 | 教训 17 |
| sandbox denial 判定含 DNS 失败 | codex 关键词表自身的洞 | 教训 15 |
| grep/glob 路径级保护 | codex 完全没有(cc 有,已对齐) | plan 31 |
| 前台 Bash timeout/cancel 无遗留进程 | cc stubborn timeout 转后台、cancel abort 后 fixture 仍有 descendants 存活 | plan 50 |
| stale-safe 原子文件修改 | cc 接受 partial Write / unread-partial Edit 且可 stale-recover，父目录用 pathname recursive mkdir；kloop 完整 fresh Read + keyed lock + 审批前 ancestor capability + 批准后 handle-relative mkdir/sync/rename fail closed | plan 49、61 |
| 双轨 provider 对等 + 三线协议 | 两家各自单主轨 | plan 15 |
| MCP 工具名消毒比 cc 严(`-`→`_`) | cc 规则语法不兼容风险 | 教训 10 |

## 三、分域差距明细

状态:✅ 齐平/反超 🟡 主干齐、长尾缺 🔴 空缺(已立 plan) ⛔ 判不做(有依据)。
收敛强度:**双家** = cc 与 codex 独立都做(教训 14 强信号);单家 = 只一家做。

### 1. 生存层(压缩/恢复/token 记账)——🟡,对 codex 反超

Plan 81（2026-08-12）已销账当前 transcript 的 durable provider-reported token 分类：validated terminal sampling 与 accepted compaction 逐响应写 append-only rollout，resume/fork/clear 按 raw prefix/同 transcript 语义恢复，`/cost` 累计 input/output/cache-read/cache-creation 与 reported-response count。该能力不是金额、billable total、失败 attempt telemetry、parent+child 全局归因或 provider 账单对账；context estimate 仍是独立且可失效的预测状态。

Plan 85（2026-08-14）补齐 session persistence 的 correctness seam：只读 snapshot/list/read/spawn seed 不修盘，显式 resume/fork 才 truncate torn tail 或追加 pairing `repaired` marker；repair stats、terminal boundary remap、invalid UTF-8 tail、sequence exhaustion 和 client session-id traversal rejection 均有 core/server 回归。该维护切片不新增 public protocol/event journal、跨进程 recovery lock、fsync/exactly-once 或 provider/billing 能力。

Plan 91 的 Plan 87–90 先行验收只增加 conformance evidence，不新增能力；完整矩阵见 `docs/plan/91-agent-boundary-acceptance.md`。显式 provider route transition、lossy request view、route-aware usage 与 public route revision 仍按 `docs/plan/92-session-provider-switching.md` 挂账，Plan 91 因此未标完成。

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| 压缩后重注入最近读过的 ≤5 文件 | cc + chord | 未立。chord 的做法:每次请求临时注入 key files 头部(单文件 12KB / 总 48KB / ≤ 剩余预算 1/4,带 revision 与 changed 标记,过 read 权限,不写进持久化历史),见 `refs/README.md` 2026-09-23 节 | dogfood 出现"压缩后失忆"痛感 |
| 工具结果分档预算(200k/50k) | cc 单家 | **✅ Plan 154(2026-09-16)**:`ROUND_OFFLOAD_CAP_CHARS` = 4 × 单条 cap = 128000,一轮结果超预算按大小降序贪心 spill,落盘失败回退内联(cc 同款取舍) | 已完成 |
| microcompaction | cc 单家 | ✅ **plan 200(2026-09-24,`6762a6b`)做了请求级版本**:历史不动,只在发出的请求里把旧工具结果换成有形状的 stub,且**只在缓存已凉时落新 stub、已发的逐字节冻结**——用户定「不要破坏cache率」,所以没抄 chord 的摊销门。下为立项前的调研记录。**取值已经有第二家**:ZCode 保留最近 5 条 tool result、阈值 ratio 0.9 + buffer 2000 token、闲置 60 分钟也触发、预计省不到 256 token 就不做、可压缩工具白名单、清掉的结果换成固定占位串(2026-09-21 调研,出处见 `refs/README.md` 同日节)。ZCode 大量对标 cc,不算独立双家。**真正独立的第二家是 chord**(2026-09-23):不是按条数清,而是每次请求按 工具类型×批龄×字节 换结构化 stub,配 cache 摊销门(`saved×30 ≥ 9×tail` 才动已发送前缀)、仍有效的 read 不裁、有损先落盘、召回反馈,阈值与出处见 `refs/README.md` 同日节 | 超长工具输出场景痛感 |
| autocompact 警告带(剩 20k 提醒) | cc 单家 | 未立,小件 | 顺手做 |

### 2. 权限系统——✅ Project/Session/Workspace 归属已纠偏

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| permission modes 全集（含 plan） | 概念双家 | **✅ Plan 37（2026-07-16）** | 已完成 |
| Config 生命周期 + project-scoped durable permission | 内部架构/安全边界 | **✅ Plan 63（2026-08-06）**：global 只留 deny/ask；用户私有 ProjectStore 保存 allow；PermissionSession cache 按 WorkspaceId 分区；单次调用冻结 EffectiveWorkspace；native protocol 1.0 原位采用 scoped approvals | 已完成；独立 context 类型仍可继续细化，但不再是权限归属缺陷 |
| AI 分类器 auto 模式 / updatedInput 改写 | cc 单家 | 暂不做(plan 8 判) | 有小模型基建再议 |
| execpolicy(execve 级 Starlark 规则) | codex 单家 | 暂不做 | 沙箱已有,重;痛感驱动 |

### 3. OS 沙箱——🟡 macOS 完整,平台 1/3

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| Linux(bwrap+seccomp) | 双家 | **plan 19 余片**(设计已定:sibling `linux.rs`) | 一台能跑的 Linux 机器(CI 已于 2026-09-22 去掉) |
| Windows model-shell process tree（Job Object） | safety product boundary | **✅ Plan 62（2026-08-05）**：suspended assign-before-resume；所有 Bash/PowerShell 走 per-process initial-breakpoint debug admission；admission/terminate 同锁；bounded cleanup | corrective 原生 Windows 已复跑通过；filesystem/network sandbox 未销账 |
| Windows filesystem/network sandbox（restricted token/AppContainer） | cc 单家 | plan 19 更后；Job containment 不销此账 | 有 Windows sandbox 需求 |

**补齐来源(2026-09-15 定位)**:上面两行未销的账,现成实现都在 `refs/codex` 上游
(固定 `02a8f038b87ad34d4a1dc5058eda26972ed7aa6c`),不必另起炉灶——Linux 看
`codex-rs/linux-sandbox`(1.03 万行:landlock 管文件 + seccompiler 管系统调用 +
bwrap 管命名空间,另有 fd_mount 与 network-proxy),与 plan 19 余片已定的
`bwrap+seccomp` 设计同向;Windows filesystem/network 看 `codex-rs/windows-sandbox-rs`
(2.43 万行原生:CreateRestrictedToken + 私有 desktop 的 SetSecurityInfo + JobObject +
ConPTY,配 `windows-sandbox-service`),它走的是 restricted token 一路;macOS 对照在
`codex-rs/core/src/sandboxing`。开工前先读 `refs/README.md` 的 2026-09-15 调研节:那里记了
为什么 grok 的 `nono`(unix-only 薄封装、无 Windows)只作备选,以及 dsh 的 Windows ACL 后端
自标 partial 的边界(启动期保留 Everyone 访问、NTFS 硬链接可从另一路径暴露同一文件)。

### 4. 工具面——🟡 主干齐；固定条件下通过 Plan 59 受限行为兼容验收

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| PDF 读入 | 双家(cc Read 判 MIME;codex view_image 族) | **⛔ Plan 67（2026-09-17）停**：用户判目前不需要。plan 49:185 的两个技术前置条件复核后**都已成立**——精确 fixture 在（`read-special-contract-1` 的整份 PDF 那条）；canonical wire 也能表达（Anthropic 允许 document block 直接进 `tool_result.content`，OpenAI 两条 rail 各有 `file`/`input_file`）。别再从「wire 表达不了」这个过期前提重推，查清的事实在 `docs/plan/67-pdf-native-read.md` 文末「⛔ 停」一节 | 真实 PDF 需求（2026-09-17 问过，答案是没有） |
| 交互 stdin(write_stdin) | codex 单家,cc 明确不做 | **⛔ plan 30 判不做**(REPL 逃逸不过门)；Plan 76 的 TUI PTY 仅为测试 harness，不是 production process channel | 真痛感再重启,连 PTY 一起 |
| 自动后台化 / stall 探测 | cc 单家 | Plan 51 保留 intentional-diff：精确 gate/时钟未形成可运行 fixture，kloop 坚持显式后台与前台 no-survivor | 新证据或 dogfood 痛感 |
| model-visible Monitor（逐 stdout 行 / WebSocket frame） | cc 单家、server flag 默认关 | Plan 51 明确不伪造；`tengu_amber_sentinel` 真 profile 不可由本地 harness 权威开启，matrix 保留 `unknown` | 官方暴露该 profile 或出现逐事件 watch 需求 |
| 后台完成/失败/取消通知、下一 step 回灌、session 清理 | cc 单家 | **✅ Plan 51（2026-07-30）**：shell + agent/program 共享外部 lifecycle，不合并内部 registry | 已完成 |
| Windows Git Bash / foreground PowerShell / Job containment | kloop native safety surface | **✅ Plan 62（2026-08-05）+ corrective**：冻结可信 executable、显式 frozen catalog、PowerShellOpaque final-status/session gate、whole-tree cleanup | corrective 原生 Windows 已复跑通过；Windows filesystem/network sandbox 仍未实现 |
| HeadTailBuffer / 进程表 LRU | codex 单家 | 挂账"小卫生件" | 有痛感整段抄 |
| `read_file` 读图只管字节、不管像素 | 四家参考(cc/grok/codex/codewhale/dsh)都做客户端降采样,kloop 只有字节上限且是拒绝 | **✅ Plan 156(2026-09-16)**:`image.rs` 加像素预算(长边 2000)、3.75 MiB 传输字节目标、PNG→JPEG 阶梯与边长阶梯、解压炸弹守卫(64 Mpx / 256 MiB);降采样必告知模型。**5 MiB 读上限不动**(plan 61 的裁决) | 已完成 |
| `bash` 缺 display `description` | cc `Bash` 有、kloop 无 | **✅ Plan 156(2026-09-16)**:补上,沿用 `run_agent`/`run_program` 同一套 display-only 约束(200 Unicode 字符、非空白、单行、无控制字符),不进 shell、不改结果;TUI 行以它开头、命令原文跟在后面 | 已完成 |
| notebook cell 读取与编辑 | cc 单家 | **✅ Plan 57（2026-08-03）**：`read_file(.ipynb)` internal adapter + strict `notebook_edit`（CC：`NotebookEdit`）；完整 fresh cell-aware qualification、ordered 保真与原子提交 | 已完成 |
| 文件工具资源/EOL/父目录纠偏 | correctness + 跨平台安全 | **✅ Plan 61（2026-08-04）**：普通 Read/Edit 5 MiB、Notebook 10 MiB、preview 1 MiB；streaming fingerprint/equality；exact-first CRLF Edit；批准后 Unix FD / Windows HANDLE-relative recursive Write；Windows identity-bound cleanup、Unix 失败时保守保留新空目录 | 已完成；当时的 Windows 原生 CI 持续门禁已随 CI 于 2026-09-22 去掉 |
| model-visible LSP / language-server client | cc 条件分支 | **Plan 57 保留 unknown**：env gate 单独开启仍未注册，正常发现链还依赖 enabled plugin；kloop 不加推测性 client | 取得权威 hermetic enabled-plugin profile 与完整 stdio lifecycle 时 |
| CC WebFetch `prompt` + 二级模型/cache/Markdown pipeline | cc 单家 | **Plan 55 intentional-diff**：kloop 保留 strict `{url}` 有界纯抓取与更强逐跳安全边界 | 出现必须“按指令读页并总结”的产品需求再另立 adapter |
| WebSearch timeout/large/dynamic concurrency/CCR proxy、remote=true/cloud lifecycle | cc 条件分支 | **Plan 55 保留 unknown**：本地 fake provider 已闭合 success/empty/error；remote 仅证明 gate-false fallback | 取得合规 hermetic profile 或官方暴露入口 |

Plan 49 已销账 Read/Write/Edit/Glob/Grep 的主干正确性：session-only 完整读取资格、stale-safe
原子 mutation、UTF-8 安全的 7k Read/Search 输出预算、Grep context/`-o`/分页，以及搜索
ignore/VCS/敏感路径策略均有 exact CC fixture + kloop golden。补强证据门后，descriptor-locked
Pre/Post hook barrier 证明 Read/Glob/Grep 真并发，dependent Edit/Edit/Write/Write pair 证明 mutation
串行；generated pair contract 再以相同规范化调用输入与真实 `dispatch_tools` Rust call/event report 比较，
跨 profile cell 必须额外引用覆盖该维度的 exact-bundle bridge。Plan 49 当时的 matrix 保留 237 个跨其他
工具簇或不可运行分支的 `unknown`；8 个 `same` 只覆盖上述并发/串行分类与 Glob/Grep 无孤儿
lifecycle，不外推“全工具一致”。默认 verifier 校 exact binary，corpus-only 同语义门当时已进双平台 CI（CI 已于 2026-09-22 去掉，现只剩本机 `make parity`）。

Plan 61 在不改写 Plan 49 历史 parity 裁决的前提下补了资源与平台安全边界：普通
Read/Edit 在同一已打开对象上执行 5 MiB metadata 预检、limit+1 与读后版本复核，Notebook
保留 10 MiB，approval whole-file preview 为 1 MiB；Write 的显式 replacement 不受该 ceiling，
旧 target、temp 与 committed verification 改为 chunked SHA-256/equality。Edit executor/preview
共享 exact-first、CRLF logical-match/raw-preservation helper。新建 Write 在批准后才从 retained
nearest-existing-ancestor capability 逐段建目录；Unix 使用 `mkdirat/openat/renameat`，Windows
使用 stable volume/file ID、拒绝 reparse 的 `NtCreateFile(RootDirectory=...)` 与
`NtSetInformationFile(FileRenameInformationEx)` handle-relative rename。Windows 失败路径会释放 retained
child、相对 retained parent 重开 cleanup candidate、复核 identity，并只对匹配且为空的 handle 设置
disposition；POSIX 没有 portable atomic handle-bound `rmdir`，inode check→`unlinkat(name)` 会留下同 UID name-swap
窗口，因此 Unix 失败路径保守保留本次新建的空目录，不冒险删除替换对象。这条当初的 Windows 原生持续门禁已随 CI 于 2026-09-22 去掉，
剩下的只有本机实跑；cross-compile 仍不冒充运行验收。

Plan 50 已销账前台 Bash：schema/output/timeout-tree/cancel-tree determinism pairs 与 hook-based
concurrency singleton 将 exact corpus 扩至 82 captures/109 evidence，matrix 为 56 行/448 单元；
10 个 `same` 全由 4 个 executable contracts 覆盖。`bash-batching` 证明 read-only 调用同批并发、
opaque redirection 串行。kloop 新增双 pipe 有界 drain（每 fd 150k bytes、最终 30k chars）与
no-survivor 前台生命周期：timeout/cancel 同步 SIGKILL process group、reap direct child 并确认
residual group 消失。CC stubborn timeout/cancel fixture 在结果后仍留 descendants，因此 executor/
output/lifecycle 保留安全型 `intentional-diff`。

Plan 51 将 corpus 扩至 83 captures/116 static evidence：exact `bash-background` 与新增的
`bash-background-failure` 固定 `task_started → task_updated(completed|failed) → task_notification →
origin:task-notification` 再采样；bundle 静态证据固定 Monitor 的 `tengu_amber_sentinel`（默认 false）
+ Bash availability gate、command/WebSocket 二选一 schema、逐行/frame 通知、1h timeout/persistent、
TaskStop/session cleanup 和 URL/Bash 权限分流。clean CLI 未暴露 Monitor，harness 又不能权威开启
server flag，因此 `monitor@clean-cli` 八格继续 `unknown`，不是 `missing`。kloop 不新增同名工具；
它为 shell/agent/program 发无 turn owner 的统一 lifecycle event，后台 shell 终态只回灌状态和输出
文件指针（不塞命令输出），TUI/plain/native server 都在 idle 时按 inbox activity 自动 delivery；两套 registry 各自原子
裁决一次终态，session shutdown 先 cooperative cancel、deadline 后 abort/SIGKILL，再完成后台
registry teardown；session active worktree 无 remove intent 时保留。
自动后台化、stall 和逐事件 Monitor 仍是明确的产品边界。

Plan 62 已把 Plan 50/51 的 shell lifecycle 收进共享 `ProcessSpec` /
`ProcessTreeChild` / `ProcessTreeKiller`。Unix 保持独立 process group；原生 Windows 用 RAII
Job Object、lifetime-owned stdio handle allowlist、Windows ordinal-case UTF-16 environment 与 suspended
CreateProcessW，在 user code 前 assign root；所有 Bash/PowerShell spawn 再用 `DEBUG_PROCESS` 收紧
MSIX/silent-breakaway descendant。debugger 只消费每 PID 一次 first-chance initial breakpoint，其他异常
保持 unhandled；descendant admission 与 Job termination 共用 lifecycle mutex，terminate 永久关 admission
后再清两组 Job。cleanup 全阶段共用一个 absolute deadline，debugger join 只在线程已结束后执行，Drop
不 sleep/join。ShellPrograms 只在启动解析一次：Unix `sh`/WSL fallback 必须解析为 executable regular
file；Windows Bash 必须通过完整 Git for Windows 布局；PowerShell 7 同时发现版本化 MSI roots 与 Windows
package API 验证的官方 Microsoft MSIX family，并以 `pwsh.exe` file version 统一排序。只有 Win32 明确报告
missing version resource 时才用可信目录/package metadata；明确 non-v7 resource 或 access/query/signature/
format 错误均拒绝 fallback，5.1 是最后 fallback。PowerShell wrapper 在用户 block 的 `finally` 中首先
快照 final `$?` 与清零后的 `$LASTEXITCODE`，因此顶层 `return` 也不能跳过状态采集；再结合新
`$Error` 判最终状态，不传播较早 native command 的陈旧非零 code。每个 session 的 Config
另共享一个 PowerShell exclusive gate，覆盖 direct 与不同 foreground/background code-mode bridge；等待锁
时取消不 spawn，orchestration 外壳不持锁。catalog/defer/warning API 全部显式吃同一 frozen
`ShellPrograms` snapshot，不在 helper 内重新 discovery。Job 只销 process-tree containment，不销 Windows
filesystem/network sandbox，也不承诺对抗 protected process/自建 debugger；spawn gate 也不把
hooks/MCP/git 自动升级成 Job ownership。原生 Windows 10 x64 已实际跑绿 Plan 61 file-safety 62 tests、
Plan 62 初始实现的 process-spawn/shell discovery/process-tree/Bash/PowerShell/permissions focused gates、
全 workspace、fmt/clippy、mock 与 corpus-only；该次验收查实 PowerShell 7 MSIX `Start-Process`
descendant 可离开 root Job。`fe6a20d` 的后续验收报告记录默认并行/单线程 core 各 550 tests、core
all-target clippy、fmt、corpus-only 与 diff check 全绿，但“PowerShell focused 22 tests”没有保存 exact
selector/`--list` 分类，不能证明 2026-08-06 新增回归。`d350eb8` follow-up 后，`5599705` 把 gate
probe 的 active/entry 生命周期移入真实 executor scope，`064bfac` 又明确拒绝含 `[exit status N]` 或
`[killed by signal]` 的 probe 输出。当前 HEAD 已在上述 Windows 原生环境先用 `--list` 精确列出
PowerShell 26 tests，再实跑 26/26；process-tree 19/19、普通 Bash 17 passed / 1 provisioned ignored、
code-mode 26/26，官方 MSIX 四 lifecycle 的显式 `--exact --ignored` acceptance 1/1。首次并行 workspace
run 唯一 cleanup timeout 的 exact test 随即通过；停止并行构建负载后的完整复跑为 core 554 passed /
1 provisioned ignored，其余 crates 与 doc tests 全绿；fmt、workspace all-target clippy、mock 6 rounds、
corpus-only 与 diff check 也通过。普通 ignored 与显式 provisioned pass 分开记录；pinned Darwin
PowerShell `n/a` matrix 保持不变。

Plan 52 将 Agent、Task registry、Team mailbox/ListAgents 与 remote/cloud 拆开取证，当时 corpus 为
96 captures/137 static evidence，matrix 为 56 行/448 单元（67 compatible / 119 intentional-diff /
23 missing / 207 unknown / 22 n/a / 10 same）。exact CC Agent 要求 description+prompt、默认后台；kloop
当时保留原生 task 默认同步与参数面。Task lifecycle 已固定稳定 ID、owner/metadata、依赖、完成/删除，但产品
决定不实现独立 registry；todo/后台执行不冒充 Task*。SendMessage 只闭合 unknown-recipient，ListAgents/
team/remote 因无权威 hermetic true-profile 保持 unknown。新增两相 sampling gate + native report 锁定 Agent
真并发、后台终态与 inbox 边界；未接真实 cloud/team/mailbox。

**Plan 66（2026-08-07）命名收口**：不改变上述能力裁决，只把 kloop 原生面改为
`run_agent`、全局且 non-drain 的 `wait_for_activity`，以及资源专属
`stop_agent` / `stop_program` / `stop_workflow` / `stop_bash`。Bash 统一使用
`background` 参数并保留 `bash_output`；`task_*` 预留给后续结构化 Task graph。旧工具名与
`run_in_background` 不提供 alias，只给迁移错误；shell 文件型 registry 与结果回灌型
`BackgroundExecutions` 继续分离。四类 stop 对全部 12 个错误 ID 组合都 fail closed 并定向到正确工具，
`workflow-N` 与 durable `wf_*` 分域；`run_agent`/`run_program`/`wait_for_activity` 的执行器会严格拒绝
未知字段和错误类型，不依赖模型侧 JSON Schema 代替运行时校验。

Plan 53 将一般问答、Plan control、Workflow 与 CC `StructuredOutput` / kloop `structured_output` 分开落地：`ask_user_question`
使用独立 Questioner seam，plain/TUI/server 支持而 headless/断线 fail closed；`enter_plan_mode`
与 `exit_plan_mode` 固定 depth-0 工具数组并由共享 ModeState 精确保存/恢复前态。独立 `workflow`
始终后台，只暴露 agent 编排原语，不是 `run_program` alias；managed run store、phase/terminal identity、
stop/shutdown、resume hit/miss 和 edited script 均已接通。`structured_output` 仅在 Workflow schema child
中作为 synthetic tool 临时注入，本地 JSON Schema 复验后以原生 JSON Value 回传，不进入主 registry。
Named/nested workflow、token budget、remote execution 与 per-child provider effort 保持 intentional-diff。

**Plan 68（2026-08-07）运行时加固**：不改变 Agent/Program/Workflow 的产品分工，但把此前依赖
模型纪律的边界收进宿主。QuickJS raw `__*` bridge 在私有闭包捕获后删除，core 另以 Program 精确
catalog 在 hook/permission/dispatch 前二次拒绝；source tool 也不能用控制面同名逃逸。Program 恢复
原子保存并 byte-compare `source.js`/manifest，后台启动同时返回 transient `program-N` 与 durable
`run-*`。Plan 68 当时引入的 journal v2 以 root/helper/branch/item/stage 拓扑 ID + 完整结构化
prompt/options 匹配；现行由 Plan 88 升级为 journal v3，仅 v3 replay，v1/v2/future safe miss且无迁移/
双写；并发 callback 必须用显式 scope，pipeline 保持 item-local/no-stage-barrier。Program/Workflow
live agents 默认 16 路、总量 1000、单 helper 4096；排队可取消，journal hit 不占 live slot。超大后台
Agent/Program 成功结果复用 History offload，Workflow 继续 bounded summary + `result.json`。`phase`
只投影进度，不是 checkpoint/exactly-once。plain 与 native server 的 inbox activity idle delivery 文档也
已纠正。新增默认 ignored 的 native-server evaluator，以 tool lifecycle 配对、source/journal/result artifact、
唯一 terminal/delivery 验收真实 `claude-sonnet-4-6` 与 OpenAI Chat `gpt-5.5`，不以模型口头自评代替证据。

**Plan 69（2026-08-10）后台生命周期 UX**：不改变 Plan 66/68 的工具名、typed stop、恢复 ID、
终态仲裁或自动 delivery，只补展示元数据和跨 surface 投影。`run_agent` / `run_program` 新增 optional
`description`（200 Unicode 字符、非空白、无控制字符），缺省分别回退 prompt/source preview；验证在
worktree/run store/registry/spawn 前完成，且 Program description 不进入 `source.js`、manifest、byte-identical
resume 或 journal key。Workflow 继续只认 `meta.description`，顶层 `description/title` 保持
accepted-but-ignored。Agent/Shell 无 durable ID；Program Running/唯一 terminal 使用同一 `run-*`，Workflow
使用同一 `wf_*`；native `thread/background_task/updated` 继续无 `turn_id`、协议仍为 1.0，只 additive 使用
既有 optional `run_id`。Inbox framing 固定为 `[Agent agent-N]`、`[Program program-N] run run-*`、
`[Workflow workflow-N] run wf_*`，超大 Agent/Program 仍只 offload result body。

TUI 将后台更新从无关联 Note 改为 session-owned typed row：live tail 按 execution ID 原位 upsert；Running
row 被 hard cap 冻结进 native scrollback 后不回写，晚到 terminal 追加同 ID 关联 row；TurnEnded 只收口
foreground `Cell::Agent`。plain/headless 共用 core 单行 formatter，server 继续结构化 DTO。
`wait_for_activity` 明确为 ID-free、non-draining 的一次性 activity barrier：不调用也会在 step/final/idle
boundary 自动 delivery，timeout 不代表失败、不消费结果，并禁止短周期 status/output polling。仍未加入
Task registry、`TaskOutput`、通用 stop/status getter、registry snapshot/hydration 或资源管理面板。
默认 ignored 的 native-server evaluator 在真实 Anthropic `claude-sonnet-4-6` 与 OpenAI Chat
`gpt-5.4-mini` 各跑通：每家 1 个 background Agent、1 个 background Program、1 个 Workflow，均有
显式 description、typed execution ID、Running + 唯一 Completed terminal、Program/Workflow durable ID
一致、3 次 exactly-once automatic delivery；Program 20k 结果进入 offload，Workflow result 保持 artifact。
验收不调用 `wait_for_activity`，也不保存 key、endpoint、raw response 或 transcript。

Plan 54 已闭合 Skill、ToolSearch、dynamic MCP refresh/call 和 MCP resources：corpus 为
129 captures/164 static evidence，matrix 当时仍为 56 行/448 单元。kloop 保留原生
`skill {name,arguments}` inline/fork、稳定 provider array + `call_tool`、资源 URI 人工批准；stdio
catalog 支持 notification refresh 与 generation-bound unlock，HTTP 无长期 notification stream 时明确为
startup catalog 固定。resources/call/paginator/wire 均有接收期 byte/item/cursor 上界。

Plan 55 将 corpus 扩至 145 captures/180 static evidence；WebSearch clean/allow 分行后 matrix 为
57 行/456 单元（92 compatible / 149 intentional-diff / 34 missing / 149 unknown / 22 n/a /
10 same）。CC WebFetch strict `{url,prompt}` 和 domain-safety ordering 已固定，但全部 loopback transport
case 都是 zero request，executor 不越权升级。CC WebSearch allow profile 则真正到达本地 fake provider，
固定 `web_search_20250305` side query 的 success/empty/error。kloop 保留 strict `{url}` 纯抓取、逐跳
SSRF/DNS/credential/redirect/5 MiB/50k 防线，以及按 Tavily/Brave key 条件注册的 bounded text search。
remote 只证明 gate false 时 worktree/local fallback；remote=true/cloud lifecycle 继续 unknown。

Plan 56 将 corpus 扩至 190 captures / 188 static evidence，并按 clean、worktree headless、
worktree interactive 条件拆开 Enter/Exit 行；matrix 为 61 行/488 单元（112 compatible /
157 intentional-diff / 34 missing / 151 unknown / 24 n/a / 10 same）。kloop 已落地 strict
`{name?,path?}`、显式 `{action:"keep"|"remove",discard_changes?:bool}`、Managed/External 与
Session/Task 所有权、provenance 复核、dirty/ignored/commit fail-closed、effective cwd 全链和
shutdown retain。clean registration 继续是明确 intentional difference；no-active error、External
不可删除、ignored/provenance 阻断与保守 shutdown 也不伪装 exact same。native Rust report 已接入
full/corpus verifier 并有 mutation-negative 门；generated executable pair 仍为 4 个。

Plan 57 将 corpus 扩至 210 captures / 197 static evidence，matrix 为 62 行/496 单元
（125 compatible / 160 intentional-diff / 32 missing / 145 unknown / 24 n/a / 10 same）。
`.ipynb` 已确认是 Read internal adapter；kloop 用现有 `read_file` 输出有界 cell/text/image blocks，
并新增 snake_case strict `notebook_edit`（CC 精确目标名 `NotebookEdit`），支持 replace/insert/delete、fallback/generated ID、ordered
未知字段保真、code output reset 和无尾换行。编辑资格独立要求完整 fresh cell-aware Read，提交继续复用
parent FD/no-follow/keyed lock/temp sync/rename/parent sync；permission/preview/worktree FileState 全链闭合。
LSP 的 env-only profile 两次仍未注册工具，plugin-backed normal discovery 无权威 hermetic profile，故八维
保持 unknown 且没有生产 client。native report 与 normalization/schema/event/state/LSP negative gates 已接
full/corpus verifier；generated executable pair 仍为 4 个，不升级静态或 native-only 相似。

Plan 58 已完成原生调度面：depth-0 owner surface 的 `cron_create`、`cron_delete`、
`cron_list`、`schedule_wakeup` 与 `/loop` fixed/dynamic adapter；不提供 PascalCase alias，
CC `ScheduleWakeup.delaySeconds` 与 kloop `delay_seconds` 的差异是公开 native contract。
`cron_create` 的 recurring 默认 true、durable 默认 false，recurring job 最多存活七天，
最后一个已到期 tick 投递后删除。durable state 位于
`~/.kloop/scheduler/<project-key>/scheduled_tasks.json` 与 `.lock`；job 绑定创建它的
session/thread，其他 owner 不可 List/Delete/claim，late durable one-shot 在无交互
headless/server 保持 pending，等待同 owner 在可交互 frontend 恢复。TUI idle Wake、
plain stdin/Inbox select、server 单飞完整 turn lifecycle 均可投递；headless 主 turn 后先关
scheduler，session-only 消失而 durable 保留。调度器不安装或修改 crontab、launchd、
systemd timer 或登录项。Plan 58 完成时 corpus 为 218 captures / 211 static evidence，matrix 为
62 行 × 8 维 = 496 cells（125 compatible / 170 intentional-diff / 24 missing /
129 unknown / 24 n/a / 24 same），7 个 executable pair；其中新增
`scheduler-cron-schema`、`scheduler-cron-contract`、`scheduler-concurrency`。timed fire、
DST/clock jump、server-selected jitter、restart/re-arm 与 enabled dynamic-loop 成功路径仍为
`unknown`。

Plan 59 没有新增产品行为，而是对固定 SHA-256 的 Claude Code 2.1.220、darwin-arm64
本地 CLI、`team=false`、`remote=false` 的 14 个已执行 profile 做总体验收。最终 generated
snapshot 为 218 captures / 214 static evidence / 65 个两次采样 determinism group；matrix
为 62 行 × 8 维 = 496 cells（24 same / 141 compatible / 169 intentional-diff / 5 missing /
133 unknown / 24 n/a），7 个 executable pair 精确覆盖全部 24 个 `same`，108 个逐
cell/source-profile exact-bundle bridge 固定跨 profile 投影。3 个 kloop-only row / 24 cells
从 CC gap 与 parity-success 统计中排除；5 个 `missing` 都是已排除承诺的
`send-message@clean-cli` registration/schema/parser/executor/output，因此 blocking missing/unknown
均为 0。该结论仅为上述固定版本、平台、入口和已执行条件下的**受限行为兼容**，不表示
全工具对齐、逐字节一致或 drop-in replacement；team/remote true、PowerShell、SendUserFile、
Monitor、LSP、真实公网/MCP transport 仍不在兼容承诺内。最终证据门要求 bundle offset 的真实
byte anchor、executor/output/lifecycle 分维度 typed witness（复合行覆盖显式 required CC tool set）、三个 kloop-only logical-id 白名单与
bridge fixture/matrix refs 精确绑定。五条跨工具链 scenario 只按各自 `full`、`surface-gate`、
`seam-only` 或 `negative-boundary` scope 裁决；报告分开记录 CC `target_entrypoint=local-cli` 与
kloop `kloop_entrypoint=core-dispatch-test-harness`，不把 core test context 冒充 CLI startup wiring。
回灌按 framing 精确计数，ToolSource 共享 trace 实测 approval-before-dispatch 和 deny 后零调用，
worktree Git subprocess 不继承用户 Git config；负边界不冒充缺席 transport/process 的执行通过。

Plan 74 只对 kloop native extension 增补当前生成物：不改 218 份 pinned raw/normalized capture，static evidence 为 215 条，matrix 为 63 行/504 cells（24 same / 141 compatible / 177 intentional-diff / 5 missing / 133 unknown / 24 n/a）；新增 `task-clear@clean-cli` 八维 intentional-diff，故当前为 4 个 kloop-only row / 32 cells。7 个 pair 与 108 个 profile bridge 无实质变化。

Plan 76 同样只增加 **kloop-native correctness evidence**：cross-platform Composer/unit/TestBackend 与 Unix real-binary PTY 不修改 218 份 pinned captures、matrix、pair、bridge 或 Claude Code scope，也不升级任何 `same`/`compatible` 裁决。PTY 的 input/CPR/ANSI/visible-viewport 证据、TestBackend 的 synthetic scrollback oracle、真实终端的 native scrollback/字形行为是三个独立裁决面。

Plan 78 继续只增加 **maintenance/correctness evidence**：workspace MSRV 固定为 Rust 1.88，terminal dependency set 升到 Ratatui 0.30.2/Crossterm 0.29.0/`unicode-width` 0.2.2/Unix test-only vt100 0.16.2，并以 `Backend::Error` 迁移、Unicode/TestBackend/real-binary PTY 回归和 target dependency graph 证明现有契约未退化。它不修改 pinned captures、matrix、pair、bridge 或 Claude Code scope，也不升级任何 `same`/`compatible` 裁决；Windows graph 排除 Unix-only PTY edges 不是 ConPTY/native-scrollback 执行证据。

Plan 79 仍只增加 **engineering maintenance evidence**：workspace 迁移到 Rust 1.96 / edition 2024，本地 toolchain pin 为 1.96.1；compiler diagnostics 驱动的 compatibility 修复、host gates 与既有回归证明当前 macOS 路径未退化。它不修改 pinned captures、matrix、pair、bridge、Claude Code scope 或任何 parity 裁决；本机 Windows cross-target 因缺 std/sysroot 与 MSVC C toolchain 未完成，也不替代 Windows native lifecycle evidence。

Plan 59 同时纠正 Plan 53 的过度裁决：AskUserQuestion 为 C/C/C/C/D/C/D/D，EnterPlanMode
为 C/C/U/C/C/C/D/C，ExitPlanMode 为 C/D/D/C/D/N/C/C，StructuredOutput 为
D/D/U/U/D/D/U/U，Workflow 为 C/D/D/C/D/C/C/C；Ask parser 使用正交 empty/type/null/options/unknown-field 负例，report 只按显式 row/dimension mapping 投影。C/D/U/N 分别表示 `compatible`、
`intentional-diff`、`unknown`、`n/a`，没有新增 `same`。

### 5. 子 agent / 多 agent——✅ 本地执行与 Task graph 已落地，Team/remote 有意保留

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| task worktree 隔离（并行写不互踩） | 双家 | **✅ Plan 35 + Plan 56**：task-owned，clean 自动清理、changed/probe-failure 保留 | 已完成 |
| session Enter/Exit worktree | CC 单家 + kloop 原生 | **✅ Plan 56（2026-07-31）**：strict name/path、显式 keep/remove/discard、effective cwd 全链切换 | 已完成 |
| ownership/provenance-safe removal | 安全产品边界 | **✅ Plan 56**：仅当前 session Managed 可删，External/task/previous-session 与 provenance mismatch fail closed | 已完成 |
| Worktree 条件注册 | 表面策略分歧 | kloop 保留 depth-0 + `SurfaceCapabilities.worktree` gate，matrix 记 `intentional-diff` | 有意保留 |
| 独立 stable-ID Task graph + TUI projection | CC/CodeWhale 有独立状态层 | **✅ Plan 71–73**：session stable-ID root-owned graph；**✅ Plan 74（2026-08-11）**：五工具、atomic rollover/clear fence 与 TUI live projection | 已完成 |
| send_message / addressable mailbox | CC/Codex 均有，但 routing 契约不同 | **✅ Plan 70（2026-08-10）**：同 session `main ↔ agent-N` 与 sibling，typed local identity、严格有界 FIFO、safe-boundary delivery、独立 lifecycle；A2A 1.0 aligned 但不是 A2A endpoint/support | 已完成 |
| ListAgents / local live roster | CC exact bundle 有 descriptor/gate；远程/team profile 不权威 | **✅ Plan 70（2026-08-10）**：严格 `list_agents {}` 只列同 directory Open peers；不做 remote discovery、Agent Card 或团队持久态 | 已完成 |
| child/background execution provenance | CodeWhale receipt 思路 + kloop 分型生命周期 | **✅ Plan 88（2026-08-15）**：core-private 2 KiB typed receipt 冻结 execution parent、Agent-only mailbox、transient/durable、rollout、workspace 与 terminal owner；现有 registries 持 receipt/typed handle；Program/Workflow bounded `provenance.json` 与 journal v3 保存审计 evidence，v1/v2 不兼容 replay，public wire/Task/billing 不变 | 已完成 |
| Agent 并发上限 | exact 2.1.220 profile 测不出;cc 源码参考里 `AgentTool` 与 `Read` 共用同一个 10 | **✅ Plan 154(2026-09-16)**:批内统一 `MAX_CONCURRENT_TOOL_CALLS` = 10(cc / deepseek-harness 同值),`run_agent` 与其他工具同一个上限;后台 agent/program 每 session 8 不变 | 已完成 |
| agent 类型 per-type effort/max_turns | cc 单家 | plan 17 片 2 未做节 | 有真实 agent 类型库再说 |

### 6. code mode——✅ 无挂账(plan 24/27 全清)

⛔ 判不做已归档:token budget(cc null 占位 no-op,教训 24)、并发 pacing、V8、
codex 拉取式增量观察。

### 7. 会话持久化 / fork——✅ 形态齐

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| 大文件性能工程(chunked 读/50MB cap/文件缓存) | cc 单家 | 未立;**不改格式可后补**(教训 7) | 多 GB transcript 出现 |

### 8. provider 层——✅ 双轨特色,宽度取舍

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| responses 轨两条回放契约销账 | — | HANDOFF 挂账 | **拿到官方 OpenAI key** |
| OpenAI-compat reasoning 出站 | — | 备选池 | 副轨模型需要时 |
| bedrock/vertex 企业后端 | cc 单家 | 不立(定位外) | 定位变化 |
| model_info 多模型工具画像 | codex 单家 | 未立 | 第三轨模型形态冲突时 |

### 9. hooks——🟡 6/10 挂点

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| stdout 结构化 JSON 协议(decision/updatedInput/additionalContext) | cc 单家 | 备选池;裸文本可向后兼容升级 | 写复杂 hook 的痛感 |
| 更多挂点(cc 10 事件面)/ 并行执行 | cc 单家 | 备选池 | 同上 |
| ~~subagent 事件 agent_type matcher~~ | — | **✅ Plan 153 第三节(2026-09-16)**:类型从 live Agent 目录送到 `subagent_start`/`subagent_stop`,`matcher` 按类型精确筛;无类型的子 agent 报 `default`(codex/dsh 的形状),不再让配了 matcher 的 hook 对所有类型照常触发;配置侧同步放开(此前 `matcher` 在子 agent 事件上直接报错,根本配不出来) | 已完成 |

### 10. 上下文工程(指令文件/caching/deferred)——✅

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| 子目录懒加载(conditionalRules + paths glob) | cc 单家(codex/claw 均无) | plan 32 片 3 挂账,弱收敛 | 大 monorepo 痛感 |
| deferred tool capability receipt 绑定 | CodeWhale 局部实现 + kloop 原生安全边界 | **✅ Plan 87（2026-08-14）**：unlock receipt 绑定 source owner/generation、WorkspaceId/effective workspace、worktree epoch、permission/policy epoch 与 agent authority；Program callable manifest 同 provider request 冻结 owner/generation/readonly；跨 scope、refresh、owner hop、child/isolated worktree reuse fail closed；provider tool array 保持稳定 | 已完成 |
| claudeMdExcludes / enterprise 层 | cc 单家 | 不立 | 定位外 |

### 11. MCP——🟡 tools/resources 主干齐，双向能力缺

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| streamable HTTP 远程传输 | 双家 | **✅ Plan 34（2026-07-16）**：session/version、SSE response、重试与 reinitialize 已完成 | 已完成 |
| OAuth 授权码 + PKCE + discovery + refresh + 私有文件存储 | 双家 | **✅ Plan 34b（2026-07-16）** | 已完成 |
| OAuth keyring、跨进程 refresh lock、device/manual flow 等硬化 | 双家长尾 | 未立 | 真实托管 server 或凭据治理需求 |
| MCP resources list/read/directory | cc 单家 + 协议生态 | **✅ Plan 54（2026-07-31）**：独立 deferred surface、catalog/read/directory wire、有界分页/内容、错误与人工批准边界 | 已完成 |
| MCP resource templates / prompts / @-mention UX | cc 单家 | 未立；Plan 54 只闭合 resources | 生态需求 |
| HTTP server notification subscription | 协议生态 | 未立；Plan 54 对无 notification stream 明示 startup catalog 固定 | dynamic remote catalog 需求 |
| server-initiated request / sampling / elicitation / roots | 协议生态 | 未立；现有 Transport 只承诺 client-originated request/notify | 双向 MCP 需求 |
| 旧版 SSE 传输 | cc legacy,codex 不做 | ⛔ 不做 | — |
| MCP server 模式(kloop 自身作为 server) | cc 单家 | 未立 | 被集成需求 |

### 12. skills / slash / steering——✅ 核心生态位完整

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| 用户自定义 commands 目录 + `!cmd`/`@file` 注入 | cc 单家(已折进 skills) | **✅ Plan 36（2026-07-16）** | 已完成 |
| skills 余项(effort/bundled 懒解压/paths/远程 skills/开关字段) | cc 单家 | plan 28 挂账清单 | 生态兼容痛感 |
| plain 前端 steering enqueue | — | 平台事实(阻塞读),接受 | TUI/server 已覆盖 |

### 13. 前端(TUI / plain / server)——🟡 功能齐、打磨差距最大

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| intraline 词级 diff | cc 单家 | 未立，代码块语法高亮已由 Plan 38 完成 | dogfood 体感 |
| slash / `@file` / 历史补全 | 两家皆有 | **✅ Plan 38 切片 4** | 已完成 |
| `--image`、TUI 图片路径/剪贴板附件 | — | **✅ Plan 29 + Plan 38** | 已完成 |
| TUI 模式/档位状态栏显示 | 两家皆有 | **✅ Plan 37 + Plan 38 HUD** | 已完成 |
| Task graph TUI live projection | — | **✅ Plan 74（2026-08-11）**：revisioned full snapshot、composer 上方 non-Cell live chrome、Ctrl+T、blocked hint 与有界折叠；plain/server/headless 无 checklist/native wire | 已完成 |
| TUI grapheme / visual-row / paste-atom correctness | kloop native correctness surface | **✅ Plan 76（2026-08-11）**：strong byte ranges、whole-grapheme edit/Delete、canonical hard+soft row layout、exact-width EOL cursor、exact completion target、stable paste atom、completed-frame viewport geometry；cross-platform unit/TestBackend，real-binary PTY 仅 Unix | 已完成；本次 macOS 实跑，不外推 ConPTY/native scrollback parity |
| server 不转发 thinking delta | — | HANDOFF 记录在案 | client 需要时 |
| scheduled prompt 空闲投递 | kloop 原生；CC timed runtime 证据仍有 unknown | **✅ Plan 58（2026-08-03）**：TUI idle Wake、plain stdin/Inbox select、server single-flight 完整 turn；不伪造 `turnId:0` | 已完成 |
| `--resume` 挑会话:全屏 picker 而非编号列表 | cc 单家 | **✅ Plan 162(2026-09-18)**:全屏卡片 + 输入即搜 + `↑↓`/Enter/Esc;底部那排(跨项目/分支/worktree/预览/重命名)未做 | 已完成 |
| 选 provider / model / effort 的统一入口 | cc 有 `/model`;codex 有 profile 切换 | **✅ Plan 159(2026-09-17)**:picker 补第三级 effort,新 `/model` 入口,三个命令按起始层级分工 | 已完成 |
| vim mode / 主题 / statusline | cc 单家 | 不立 | 定位外 |

### 14. headless / 脚本化——✅

**Plan 33（2026-07-16）已完成**：位置参数/stdin、`--json` 事件流（复用 server wire）、无人审批默认拒绝、稳定退出码与 mock/headless 回归均已落地。

**Plan 58 补充**：headless 主 turn 完成后先关闭 scheduler，再关闭 background task/shell；
session-only scheduled job 消失，durable job 保留，等待同 owner 在可交互 frontend 恢复。

### 15. 测试 / CI / 工程质量——✅ 纪律同级

| 差距项 | 补齐路径 | 触发条件 |
|---|---|---|
| ~~远端 CI 首次实跑~~ | **⛔ 2026-09-22 停**（用户判「目前不需要 CI」）。推远端后一共跑过 9 次、9 次全红，且每次都红在同一处：matrix 要 `+stable`（当时已到 1.98.1）而仓库 pin 1.96.1，`-D warnings` 把这个版本差判成失败——同一批里 pin 1.96.1 的 MSRV job 始终绿。`.github/` 整个删除 | 不再有触发条件；要回来就是重开一个钉住 1.96.1 的 workflow |
| Windows Plan 62 原生 lifecycle gate | **✅ 2026-08-05 本地 Windows 10 x64 全门通过**；focused selectors 继续进 workflow | 已完成；持续门禁随 CI 于 2026-09-22 去掉，Windows 再验只能本机跑 |
| Plan 63 project permission / native protocol 1.0 | **✅ 2026-08-06**：kloop fmt/clippy/workspace/mock、real binary v1/v2-refusal smoke；Desktop 887 pass/52 skip/0 fail、TS/build、Tauri 229 tests；companion `54056dc5` | 已完成；private-store Windows target check/clippy已过，本次未做原生Windows runtime或真实Tauri GUI点击smoke |
| Plan 76 TUI correctness evidence boundary | **✅ 2026-08-11**：Composer/render/TestBackend 为 cross-platform suite，本次 macOS workspace 实跑；CI 已配置 macOS/Linux/Windows，`cfg(unix)` real-binary PTY 回答 `ESC[6n`、驱动 resize/input、解析有界 raw ANSI/current viewport | Windows 的 n/a/skip 不是 ConPTY pass；vt100 不证明 DECSTBM native scrollback，真实终端字形/scrollback 另作人工证据 |
| Plan 78 terminal dependency/MSRV maintenance | **✅ 2026-08-12**：Rust 1.88 workspace MSRV；Ratatui 0.30.2/Crossterm 0.29.0/Unicode/vt100 受控升级；associated backend error、target graph 与既有 TUI/PTY gates 验证原契约 | maintenance/correctness only；不改变 parity 裁决，Windows graph 排除 Unix PTY edges 不是 ConPTY pass |
| Plan 79 Rust 1.96 / edition 2024 maintenance | **✅ 2026-08-12**：10-package workspace 迁移到 edition 2024 / MSRV 1.96，本地 pin 1.96.1；host compatibility Clippy、workspace gates 与资源生命周期回归通过 | engineering maintenance only；不改变 parity 裁决；Darwin cross-target 未完成，不能冒充 Windows native pass |
| Linux 平台测试(连带沙箱 Linux 片) | Plan 19 余片 | 可用远端 Linux runner |
| 自审遗留:低危项与重复代码清理 | 教训 25 尾注挂账 | 顺手 |

### 16. 实战里程——**最大差距,唯一抄不来**

两家被海量用户长期锤过;kloop 的真 key 验收是每能力单场景闭环。长会话稳定性、大
repo 性能、并发边角、UI 体感只有用出来。**收敛路径 = dogfood**:
- **Task graph/TUI 已销账（Plan 71–74，2026-08-11）**：session-scoped `task_create/get/update/list/clear`、稳定 ID 与强约束 dependency graph、root-owned/result-only child 边界、atomic rollover 与 clear revision fence、composer 上方 TUI live panel 均已落地；当前原生契约没有 assignment/owner 字段，plain/server/headless 无 Task checklist/public wire，`todo_write` 已删除。继续用真实任务 dogfood；下一产品切片回到 PDF 原生分页读取；
- 之后每个 plan 的实现会话尽量在 kloop 里跑,痛点直接变本报告新行;
- 自审(教训 25 的 7 路并行精读)每完成 4–5 个 plan 复跑一轮,盯边界：UTF-8 byte offset / grapheme cluster / display column、combining/ZWJ/wide glyph、logical line / visual row、paste atom identity/recall/steer/exact-once、大小写、Drop/Weak、预算耗尽、截断累积。
- **销账:2026-07-20 对 plan 33–38(baseline b3e79d1)跑了一轮多 agent 对抗式自审
  (8 区域精读 → 每发现两路对抗验证)**,查实并已修 4 个 correctness bug + 1 个一致性
  项(提交 fe2224e/86cc139/d38beae/be1d511/c5e3766,均带回归测试):① bypass 下带
  重定向的 opaque bash 逃过 deny+危险命令层被自动执行(安全绕过,最重);② steer 时贴
  的图片被静默丢弃 + 误导占位行;③ 召回含大粘贴的历史条目重发时发出占位符字面量;
  ④ setup_terminal 失败泄漏 raw 模式 + git worktree;⑤ 未高亮代码块 tab 缩进丢失。

## 四、补齐路线图(按序挑,顺序可按意愿调)

- **T0 下一产品切片**：暂无。原队首 `read_file` PDF 原生分页读取 **⛔ 2026-09-17 停**（用户判「目前不需要支持 PDF」，**Plan 67**；拦住它的不是技术前置，是需求）。session-scoped Task graph/TUI 已由 **Plan 71–74（2026-08-11）** 完成：五工具、稳定 ID/DAG、无 owner、root-owned/result-only child 边界、revisioned snapshot、atomic rollover/clear fence 与 TUI-only live panel；旧 `todo_write` 已删除。
- **T0 架构与 correctness**：Plan 61–66 已完成；Plan 63 已把 durable permission 收到 ProjectId、session cache 收到 WorkspaceId，Plan 66 已把后台资源名与 ID 域分开。后续只按 dogfood 证据继续细化 Config façade，不把类型拆分本身当独立能力缺口。
- **T0 parity 余线**：Plan 59 已完成（2026-08-03）；后续内部重构不得外推或改写固定版本、平台和已执行条件下的受限行为兼容结论。
- **T1 有明确外部触发**：Linux 沙箱（需要一台 Linux 机器；CI 那条已于 2026-09-22 判停）；Responses 回放契约销账 + `/compact` 真 key 验收（拿到官方 key 时）。
- **T2 痛感驱动**：hooks JSON 协议、provider/model 价格与跨 transcript/child 全局金额归因、压缩后重注入、TUI 打磨件、HeadTailBuffer、send_message、MCP resource templates/prompts/双向 request、子目录懒加载、会话性能工程。
- **⛔ 已判不做(别再议,除非前提变)**:write_stdin(plan 30)、token budget(教训
  24)、并发 pacing、V8 引擎、旧版 SSE 传输、worktree 自动合回、bedrock/vertex、
  vim/主题。

## 五、维护纪律

- 每完成一个 plan:对应行销账(✅ + 日期 + 提交号),T0 队列前移。
- dogfood 出的新痛点:先进对应域的明细表(标"痛感已现"),再决定立不立 plan。
- 新回源发现改变收敛强度时(如教训 32 的否定断言翻转):当行更新并注日期。
- 本报告与 HANDOFF 分工:HANDOFF 记"已有什么、怎么实现的",本报告记"缺什么、
  何时补";能力落地后细节归 HANDOFF,本报告只留销账行。
- **2026-09-21 对齐 plan 157–175**:这一批多数是 dogfood 驱动的设计修正与轨道
  稳健性(config 字段审计、effort 单旋钮、凭证收口、chat/responses 三次线上报错、
  Makefile),**不对应本报告的任何差距行**——它们不是"参考项目有而 kloop 缺"。
  真正销掉的只有两条,都在第 13 节(Plan 162 的 resume picker、Plan 159 的三级选择器)。
