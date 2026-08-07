# refs — 参考资料与调研结论

kloop 设计时对比研究过四个代码库。本文件是关于"别人代码"的全部知识:导读 + 调研结论 + 可移植设计参考。项目自身的状态与教训见 docs/plan/HANDOFF.md。

## 导读

| 参考 | 位置 | 看什么 |
|---|---|---|
| **codex** | `refs/codex` | codex 生产级 fork。分层循环:`codex-rs/core/src/session/turn.rs`;工具注册:`core/src/tools/spec_plan.rs`;并行锁:`tools/parallel.rs`;压缩全家桶:`compact*.rs`、`fork_proactive_trim.rs`;会话落盘:`rollout/`;扩展范式:`ext/worktree`;集成测试:`core/tests/suite`(mock SSE + wiremock 范式) |
| **claude-code(逆向 TS 版)** | `~/work/claude-code` | 主循环:`src/query.ts`(七层压缩流水线在 queryLoop 每轮开头);压缩:`src/services/compact/*`;工具并发分批:`toolOrchestration.ts`(partitionToolCalls);子 agent 递归:`AgentTool/runAgent.ts`;重试:`withRetry.ts`;溢出检测:`services/api/errors.ts` |
| **claw-code** | `./claw-code/`(本地克隆,已删 target/) | **不可作底座**(见结论 3)。仅三样值得抄:① `rust/crates/mock-anthropic-service` + `rusty-claude-cli/tests/output_format_contract.rs` 的 mock 契约测试纪律;② `rust/crates/api/src/providers/openai_compat.rs` 的 tool_calls 流式翻译状态机;③ `rust/crates/runtime/src/compact.rs:129-166` 的压缩边界回退(不切开 tool_use/tool_result 对) |
| **CodeWhale** | `./codewhale/`(本地克隆,固定 `b494236312ef3ac36489c83706a0b11ab73935a1`) | 本地 agent 平台的控制面。重点看 provider stream guard、runtime event `seq`/replay、tool preparation/resource claim、subagent lifecycle、context no-follow、MCP/Skills catalog budget 与 loopback Web bootstrap；不照搬巨型 TUI runtime、多套协议/MCP 面或未接通的 Fleet/remote scaffold |

`./claw-code/` 与 `./codewhale/` 都由根 `.gitignore` 排除，只作为本机只读参考，不随 kloop 提交；不得在其中开发或推送。CodeWhale 的完整源码审计、成熟度边界和 A–D 候选清单见 `docs/plan/60-codewhale-source-review.md`。

## CodeWhale 固定源码审计(2026-07-27)

本地参考库固定为：

- remote：`https://github.com/Hmbown/CodeWhale.git`；
- path：`refs/codewhale`；
- commit：`b494236312ef3ac36489c83706a0b11ab73935a1`（2026-07-26）；
- 用法：只读回源；若要更新，必须先记录新 commit 并审查差异，不能让滚动 HEAD 悄悄改变既有结论。

源码确认的生产主路径是 `refs/codewhale/crates/tui` 内的 Engine → `RuntimeThreadManager` →
`/v1/threads/*` HTTP/SSE → durable JSONL events；`/v1/stream`、stdio app-server 和 legacy chat
completions 主要是兼容包装。CodeWhale 约 36 个 provider identity 最终归入 Chat Completions、
Responses、Anthropic Messages 三类 wire，真正值得 kloop 吸收的是 guard/replay/边界治理，不是 provider
名称数量。

近期候选优先级：

1. provider header/chunk-idle/wall/content guard 与 partial-output retry safety；
2. instruction/import 的 symlink、非普通文件和 `O_NOFOLLOW` 防护；
3. append-only rollout 的 turn-terminal/compaction/fork boundary `sync_data`；
4. canonical protocol additive event `seq`，再逐步接 durable replay。

明确不抄：单 JSON snapshot 替换 rollout、巨型 `turn_loop.rs`/subagent 单文件、多套
Runtime/legacy/app-server 或 MCP 所有权、非幂等 stale-session 自动重放，以及同时铺 Web/IDE/Fleet/
remote/mobile。CodeWhale 的 subagent 是同进程 Tokio task；共享 token budget 不是严格预留总账，stale
cleanup 是机会式触发，checkpoint receipt 也不等于 Interrupted worker 已可原地恢复。Fleet 成熟的是
外进程/SSH、ledger 和 generation fencing，Docker 与多项 budget/scheduling 字段未完整接入生产链。

完整证据位置、kloop 对照入口、测试/CI 边界与下一会话拍板顺序见 Plan 60；本节只作导读，不代替该文档。

## Claude Code 2.1.220 工具对齐基线(2026-07-27)

Plan 48 将工具对齐目标钉死在本机精确二进制,不再拿滚动产品文档或旧逆向源码补实现:

- `~/.local/bin/claude` 指向
  `~/.local/share/claude/versions/2.1.220`;
- `claude --version` 为 `2.1.220 (Claude Code)`,文件大小 `<redacted>` bytes,
  SHA-256 为 `<redacted>`;
- Mach-O 内 bundle 元数据(byte `<redacted>` 附近)记录构建时间
  `2026-07-24T22:17:45Z` 与 commit
  `<redacted>`;
- bundle 中统一工具适配器有 66 个 `$i({` 构造点,公共接口覆盖
  name/aliases/schema/enabled/concurrency/read-only/open-world/permission/call/render/result mapping;
  最终工具数组受 feature、平台、入口、权限档、plan/worktree/team/remote、MCP/defer/depth
  条件过滤,不是静态全量表。

已定位的 2.1.220 静态锚点:别名归一化 byte `<redacted>`;默认工具能力位
`<redacted>`;Glob `<redacted>`;ToolSearch `<redacted>`;ExitPlanMode `<redacted>`;
WebFetch `<redacted>`;Agent `<redacted>`;Bash `<redacted>`;Notebook/Edit stale-read 稳定串
`<redacted>`。这些只是静态入口,没有走完 schema→parser→executor→permission/concurrency→
output/lifecycle 或黑盒 fixture 的维度一律仍是 `unknown`。

本轮也重新固定了三个架构参考的快照:

- `~/work/claude-code` commit `<redacted>`;
- `refs/codex` commit
  `bb21ed4b8d8f74567cd6fecb3c7d4fba795bc6e3`;
- `refs/claw-code` commit `4ea31c1bc91c4e9bcbd67d51c550c01e127e6d0d`。

回源交叉核对的收敛点:工具计划/条件注册与执行分发分层;编辑前保存并校验文件读取状态;
并发能力按调用事实判定而非把所有工具一刀切;worktree 是带创建、持久化、清理决策的会话资源。
反面教材:claw 的裸 PID 后台 shell 没有输出/回灌生命周期,进程内 Task registry 不是真 agent
调度,字符串包含打分的 ToolSearch 也不能当目标语义。codex 的持久 exec/write_stdin 可借架构,
但其静态 parallel flag 不能替代 cc/kloop 的按入参动态并发。

**裁决边界**:上述三个 Claude Code 对齐架构参考库只解释独立收敛、分歧和移植成本;公开文档只帮助设计 probe。当前
工具的注册条件、schema、空值/默认、权限层序、截断、后台通知和状态机最终只由精确 2.1.220
bundle + 隔离黑盒 fixture 裁决。固定 target 是 darwin-arm64；Windows 源码参考与 kloop 原生
Windows 测试都不能把 `powershell@clean-cli` 的 `n/a` 八维升级成 `same`/`compatible`，除非另有
目标 Windows binary identity、fixture 和 exact locator。

Plan 48 已提交以下可重放基线:

- `refs/claude-code-2.1.220/manifest.json`:6 个完整条件 profile、38 组 raw/normalized
  capture，包含 clean determinism pair 和独立的 ExitPlanMode PTY approve/reject/cancel；
- `static-evidence.jsonl`:46 条带 target SHA、bundle offset 或固定 commit locator 的静态证据；
- `tool-matrix.json`:43 个“逻辑能力 + profile + 可见变体”行，逐项记录 registration→schema→
  parser→executor→permission→concurrency→output→lifecycle；
- `collect.py`/`verify.py`:采集前 exact identity guard，隔离 HOME/config/cwd、本地 provider/Web/MCP，
  受限 normalization、hash/determinism、证据引用、文件集合、环境/网络和敏感信息 fail-closed 校验。

Plan 49 已在该基线上闭环文件/搜索簇并补齐 executable parity 门。该阶段 `static-evidence.jsonl` 有
95 条证据：Plan 48 的 46 条仍是历史提交数，新增 kloop Read/Write/Edit/file-state/Glob/Grep、
permission、per-call Pre/Post hook 与 Rust parity report code/golden anchors 进入 fail-closed 必备集合。
当时采集集为 73 组 capture：40 组 schema v1，33 组 schema v2 scripted workspace，包含 18 组
normalized bytes 完全一致的 determinism pair；同轮并发仍为明确 singleton，不伪装 deterministic pair。
schema v2 保留 provider/tool/hook/timeline 顺序，只归一化已声明的临时根、ID、时间、PID、workspace
mtime，以及由 fixture 明示的 interactive PTY 收尾状态。

新增证据固定了 Read 文本/媒体/二进制分支、Write/Edit prior-read 与 stale 路径、Grep
parser/output、hidden/VCS/gitignore allowed-path 观察、路径种类和同轮读写顺序。CC 的
interactive pair 证明批准后 Write 会重新检查 stale；Edit 可在 old string 对当前内容仍唯一时
stale-recover 并保留无关外部修改。No/Esc 已改为自然 continuation：第一次采样不携被拒调用
result，下一 settlement request 带唯一 `is_error` result 后正常 final；旧的 harness Ctrl-C/SIGTERM
“无最终结果”观察已订正。

kloop 则保留更严格的安全边界：Read 在 permission 前 canonicalize + no-follow/nonblocking 打开 regular-file descriptor，仍用原始 alias 匹配规则，Unix 多 hard-link regular file 因 pathname 无法分类同 inode 的其他名字而拒绝，只有最终成功、完整展示的结果建立 session-only observation；
existing Write/Edit 要求完整且 fresh，任意版本漂移都 fail closed；pre-hook 后只允许 leaf 缺失、
直接父目录必须存在，先 canonicalize + no-follow 打开 parent directory handle，让 permission deny/
sensitive/acceptEdits 与审批 preview 判断同一 effective target；批准后复核 parent dev/inode，随后 target
read、同目录 temp、最终复核、rename、cleanup 与 parent sync 全 descriptor-relative，不重新遍历父路径，
因此 parent alias 不能隐藏或在等待中改指 `.git` 等目标。提交保留权限、sync 并拒绝 symlink/非普通
leaf。Read 文本和 Grep/Glob 模型输出使用 UTF-8 安全的 7k 字符上限；PDF 与非 UTF-8 文本明确拒绝。
Grep 每个 walker candidate 先以 canonical parent FD + no-follow leaf 绑定 inode，permission 同看 original + resolved path，再由 `search_reader` 搜 descriptor，消除 filter→pathname reopen 竞态；多 hard-link candidate 作为不可安全分类的 inode alias 隐藏并计数；Glob/Grep 继续尊重 `.gitignore`、跳 VCS，并在读取前过滤 deny/sensitive 路径。CC 会为新文件隐式
创建缺失父目录，kloop 在 Plan 49 当时也未复制这一行为；它与 CC partial-read、Edit stale recovery、广义搜索可见性
均记为 `intentional-diff`，没有为字面一致降级。descriptor boundary 阻断审批等待中的常规 alias retarget，但不是 hostile same-UID 文件系统事务；最终 identity check→`renameat` 的微小 namespace window 仍明确保留。

Plan 61 后续纠正了资源与父目录产品行为，但不改写上述 Plan 49 历史裁决：普通 Read/Edit raw 输入现为 5 MiB、Notebook 保留 10 MiB、preview 为 1 MiB，版本/committed-content 验证改为 streaming；Edit raw exact 零命中时才走 CRLF logical-match/raw-preservation helper。新建 Write 现可在批准后从 retained nearest-existing-ancestor capability 逐段创建缺失父目录，Unix 用 `*at`，Windows 用 stable file ID、reparse 拒绝与 handle-relative native API。Windows 可经 retained HANDLE 清理 identity-matching empty directories；POSIX 没有 portable atomic handle-bound `rmdir`，Unix 失败路径保守遗留本次新建空目录，避免 check→pathname-rmdir 误删 name-swap 替换对象。因此 matrix note 已更新，但原 fixture 未覆盖嵌套目录，cell status 不升级。

Plan 49 完成时的 440 个矩阵单元为 70 个 `compatible`、83 个 `intentional-diff`、20 个 `missing`、
237 个 `unknown`、22 个 `n/a`、8 个 `same`。`paired-parity.json` 由 matrix generator 同步生成，
3 个 contract 精确覆盖全部 8 个 `same`：Read/Glob/Grep 并发，Write/Edit 串行，以及 Glob/Grep
无孤儿 lifecycle。verifier 固定运行一个真实 `dispatch_tools` Rust report test，再比较 CC/kloop 的
规范化调用输入、start/finish、result、偏序和 workspace projection；fixture profile 与 matrix cell
不同时还要求该 cell 引用由精确 bundle 支撑、覆盖当前维度的 profile bridge。旧的
`kloop_golden: true` 声明门已移除。
`grep@clean-cli` 因精确 search-tools gate 不注册，downstream parser/executor/output/lifecycle 有负注册
证据并要求 allow-profile coverage 后标 `n/a`，不是静默删除未知。WebFetch 本地 URL 仍在 2.1.220
domain-safety 层先被拒、stub 收到 0 请求，所以重定向/认证/大响应 executor 继续为 `unknown`；
Plan 49 没有外推其他工具簇。

Plan 50 随后闭环前台 Bash。采集集当时为 82 组 capture，新增 schema、output、timeout tree、
cancel tree 四组 determinism pair 与一个 concurrency singleton；`static-evidence.jsonl` 为 109 条，
其中 exact bundle locator 固定输入/输出 schema、input-dependent `isConcurrencySafe`、permission+
sandbox override、spawn/stdio、timeout cap、process-tree kill、result mapper、30k inline cap 与 persisted
output truncation。schema fixture 覆盖 command 缺失/null/空/错类型，timeout null/字符串/负数/0/
600000/越界，boolean/description 错类型和 unknown field；output fixture 覆盖 stdout/stderr、无换行、
空结果、non-UTF-8、非零/signal 与大输出文件化 preview。

精确 fixture 也固定了不能抹平的生命周期差异：CC 对 TERM-ignoring tree 的短 timeout 会转为
background task 并返回成功，运行中 SIGINT 则返回 user-rejected/aborted-tools；两条路径在工具结果后
child/grandchild 都仍存活，collector 只为 hermetic 收尾额外 `SIGKILL`。kloop 不复制该行为：前台
stdout/stderr pipe 并行 drain 到 EOF且有界（每 fd 150k bytes、最终 30k 字符），每次 spawn 建独立
process group；timeout/cancel 同步 SIGKILL 全组、wait/reap direct child，再确认 residual group 消失才
完成调用。正常 shell leader 先退出但仍留同组 descendant 时也清组。审批/hook 期间取消不 spawn，
spawn 后 dispatch 会等待上述 executor cleanup，不能用外层 cancel 先 drop future。

Plan 50 完成时 matrix 为 56 行/448 单元：70 `compatible`、90 `intentional-diff`、20 `missing`、236 `unknown`、
22 `n/a`、10 `same`。第 4 个 generated contract `bash-batching` 覆盖两个 Bash concurrency
`same` cell：CC 用 Pre/Post hook barrier 证明 `pwd`/`ls -d .` 同批重叠、两个 redirection 调用串行；
kloop 固定 Rust report 跑同输入真实 `dispatch_tools` 并比较 call/event/result/workspace。Bash 的
permission、native output envelope 和 no-survivor lifecycle 保持 `intentional-diff`。

Plan 51 完成时 corpus 为 83 captures/116 static evidence。`bash-background` 与
`bash-background-failure` 的 raw/normalized 对固定：显式后台立即返 id + output file，随后独立发
`task_started → task_updated(completed|failed) → task_notification`，再以
`origin.kind=task-notification` 启动一轮采样；输出正文不塞进通知。bundle locators 同时裁决了
model-visible `Monitor`：`tengu_amber_sentinel` server flag 默认 false，且还要求 Bash 可用；输入是
command/WebSocket 二选一 + description + bounded timeout/persistent，command 每个 stdout 行、WS 每个
text frame 都成为通知，TaskStop/timeout/session exit 收尾，URL 与 command 分别走 egress/SSRF 和 Bash
权限。clean CLI 没暴露它，local harness 也不能权威开启 server flag，因此 `monitor@clean-cli` 八格
仍是有理由的 `unknown`，绝不改写成 `missing`。

kloop 没新增同名 Monitor：那是逐事件 watch，而已证明的 background Bash 是一次性终态通知，不能
混为一谈。Plan 51 在现有两套 registry 上补一个共享的只读 lifecycle event：shell、agent、program
都发 session-scoped running→单一 terminal；server 用无 `turnId` 的
`thread/backgroundTask/updated`，TUI/plain 显示 note。shell 终态把 status/summary/output-file pointer
回灌 launching agent inbox（命令输出仍只在文件），所以运行中在下一 step、TUI 空闲靠 autowake、
plain/server 在下一 turn 交付。agent/program 的结果回灌语义不变。两套 registry 不合并，但都用原子
终态裁决和 generation signal；session shutdown 先关注册、协作取消，deadline 后 abort/SIGKILL
process group，并在 active worktree teardown 前等待，防 orphan、重复通知和 server sender 挂账。
自动后台化、stall policy 与逐事件 Monitor 保留为明确的产品边界。

Plan 62 的 Windows shell 结论只使用固定逆向源码 `<redacted>`
作架构参考：原生 Windows 的 Bash 仍是经 Git for Windows 布局验证的独立 `bash.exe -lc`，
PowerShell 是另一个 foreground-only 工具，WSL 按 Linux 分流。kloop 将所有 model shell 收敛到
跨平台 process-tree façade；Windows 采用 suspended `CreateProcessW`、value-lifetime 自持的 stdio
handle list、Windows ordinal-case UTF-16 environment、assign-before-resume 的专属 Job Object，
assign/resume 失败不裸 spawn、不设置 breakaway；唯一 blocking waiter 可在取消后继续 poll，不随后台
watchdog tick 累积。临时 inheritable stdio 窗口与 hooks/MCP/Git 等生产 child creation 共享 workspace
`kloop-process-spawn` gate，但这些其他 child 仍未迁入 Job ownership。正常 leader 先退、timeout/cancel、
后台 kill/watchdog/session Drop 都显式清完整 Job，再有界收 pipe。PowerShell discovery 除 MSI roots 外，
只接受 Windows package API 枚举到的官方 `Microsoft.PowerShell[_-LTS]_8wekyb3d8bbwe` MSIX roots，
并以实际 `pwsh.exe` file version 与固定 `PowerShell\7` MSI root 统一排序，不信任任意 PATH alias。该 Job 只是 process-tree containment，不是 restricted token/AppContainer 或
filesystem/network sandbox；PowerShell 原脚本经 UTF-16LE EncodedCommand 执行但原文留在
hook/permission/UI/history，权限按 opaque 每次审批，不套 Bash AST。这是 kloop 的安全产品契约，不改
pinned 2.1.220 darwin matrix 中 `powershell@clean-cli` 的 `platform=darwin-arm64`、`kloop_name=null` 与八维
`n/a`。

2026-08-05 原生 Windows 验收补充：官方 MSIX PowerShell 7 的 `Start-Process` 可产生不留在
root Job 的 descendant，因此 PowerShell 专用 spawn 加 `DEBUG_PROCESS | CREATE_SUSPENDED` gate。
root 仍先 assign 后 resume；每个 descendant create event 在继续前检查 membership，不在 root Job 者先
assign 到第二个 kill-on-close Job。open/check/assign/continue 失败终止 event process 与两组 Job，
timeout/cancel/normal exit/Drop 只操作这个固定 owner 集，不用 PID 扫描、裸 spawn、无控制 breakaway 或
direct-child fallback。PowerShell 7/5.1 descendant no-survivor、pipe EOF、waiter reuse、handle growth 与
nested-host-Job 均已在 Windows 10 x64 原生通过；这仍不产生 Claude Code Windows parity evidence。

Plan 52 完成时 corpus 为 **96 captures / 137 static evidence**，matrix 仍为 56 行/448 单元，状态为
67 `compatible` / 119 `intentional-diff` / 23 `missing` / 207 `unknown` / 22 `n/a` / 10 `same`。
新增 Agent fixture 固定 `description + prompt` 必填、默认后台、显式 `run_in_background:false` 前台、
model override，以及 remote gate 不满足时回退 local async；bundle locator 则补齐 async output、默认分支与
team/remote gate。Plan 52 当时固定 kloop 原生 `task` 默认同步；Plan 66 后公开名迁为
`run_agent`，默认值、参数、depth=1 与并发策略不变，故仍只把 local executor
能力记为 compatible，wire/schema/parser/output/lifecycle 均为 intentional diff。

六个 Task* 现在各有独立 runtime case；额外的 scripted lifecycle 固定两条稳定 ID、owner/metadata、
`blocks/blockedBy` 双向投影、pending→in_progress→completed→deleted 与删除后查询。TaskOutput/TaskStop
只裁决 missing-ID 分支，不能外推 live lifecycle。kloop 不把 `todo_write`、后台执行或 shell
组装成同名 registry：todo 仍是无 ID 全表替换；`BackgroundExecutions` 只拥有
agent/program/workflow 回灌生命周期，`BackgroundShells` 继续拥有文件型 shell。
Plan 66 将控制面收口为 ID-free、non-drain 的 `wait_for_activity`，以及
`stop_agent` / `stop_program` / `stop_workflow` / `stop_bash` 四个资源专属 stop；交叉 ID fail closed。

SendMessage clean fixture 固定 string-message observable-input backfill 与 unknown recipient；kloop 无
model-visible mailbox，已执行维度标 missing。ListAgents 只在 bundle/alias 中坐实，`team=false` clean
profile 明确不注册；真实 team/remote/ListAgents 因 account/environment/server gate 无法 hermetic 开启，
继续 unknown，未连接真实 cloud、团队、凭据或用户 mailbox。

kloop 新增公开 mock-only sampling `Gate`（started/release 两相同步），Plan 52 Rust report 真实运行
`dispatch_tools`/`run_turn`，无 sleep/文件轮询地证明两个同步 run_agent 同时到达 sampling，并锁定后台完成回灌、
stop-vs-completion 一次终态、final sampling inbox 兜底、todo replacement、wait_for_activity non-drain、unknown-stop
error settlement 与 CC 同名 surface 缺席。`verify.py` 固定 selector 运行该 report，并以缺场景、事件重排、伪 adapter
tamper 测试 fail closed；没有新增 pair contract 或人为制造 `same`。Plan 66 的普通 Rust golden 另行表驱动
全部 12 个跨资源 stop 负组合、`workflow-N`/`wf_*` 边界和 strict background/wait parser；这些不冒充
Plan 52 report 自己覆盖的场景。

Plan 66 是 kloop 原生协议的命名收口，不是 Claude Code adapter：`task → run_agent`、
`wait → wait_for_activity`、`kill_bash → stop_bash`，并新增 typed `stop_program` / `stop_workflow`；
Bash 的 `run_in_background` 同步迁为 `background`，与 Agent/Program 统一。`bash_output` 保留，
因为它查询状态、可等待并读取输出尾部；`bash_background` 会误导成第二个启动入口。旧工具名与旧字段
不双栈，只返定向迁移错误，`task_*` 留给后续结构化 Task graph。

Plan 53 完成时 corpus 为 **110 captures / 151 static evidence**，matrix 仍为 56 行/448 单元，
状态为 65 `compatible` / 132 `intentional-diff` / 36 `missing` / 183 `unknown` / 22 `n/a` /
10 `same`。AskUserQuestion 新增 single/multi/Other/two-question/preview/notes/cancel/invalid/headless
九组 PTY 与 headless fixture；EnterPlanMode 新增 enter→exit、重复 enter、headless 三组；Workflow 新增
minimal async launch/phase/completion 与 invalid-script-before-task 两组。bundle locator 同时固定 Workflow
pure-literal meta、runtime capability、journal/resume 与 CC `StructuredOutput` synthetic adapter/AJV/nudge 链。

kloop 不把 Approver、ReportFindings 或 `run_program` 改名计作 parity：新增独立 Questioner、Enter/Exit
ModeState、always-background `workflow` profile 和 Workflow-child-only `structured_output`。kloop-owned 名称统一为 snake_case，
Exit 继续 inline plan preview；named/nested workflow、budget、remote 与 per-child provider effort 诚实标为
intentional-diff。RunStore 使用受控 run component、descriptor-bound/no-follow artifact IO，Workflow
completion/phase/terminal event 保持 task/run/store identity；resume journal 保留 JSON Value 并兼容 legacy
string entry。full exact-binary verifier 与 corpus-only 门均通过。

Plan 54 完成时 corpus 为 **129 captures / 164 static evidence**，matrix 仍为 56 行/448 单元，
状态为 89 `compatible` / 143 `intentional-diff` / 34 `missing` / 150 `unknown` / 22 `n/a` /
10 `same`。新增 19 个 capture：Skill 成功双样本与三种错误、ToolSearch required-term/ranking/parser
双样本、select/`tools/list_changed` refresh 后真实 `tools/call` 双样本，以及独立 resources-capable
local MCP 的 list/read/missing/directory。采集明确固定 `ENABLE_TOOL_SEARCH=true`；proxy 默认关闭
ToolSearch 的条件没有被抹平。

六个 `child_plan:54` 行保守裁决：Skill 与 ToolSearch 的核心能力 compatible，但 CC 的
`Skill {skill,args}` companion user block、`tool_reference` + 后续 tools 重组，与 kloop 的
`skill {name,arguments}` inline/fork、稳定 provider array + `call_tool` 分别记 intentional diff。
Dynamic MCP 的权限/并发仍无权威 fixture，继续 unknown。Resource list 八维 compatible；read 与
directory 因 kloop 对模型选择 URI 保留外部工具批准而在 permission 标 intentional diff，图像/blob
输出和目录 feature lifecycle 另保留已证明差异。没有新增 `same` 或 executable pair。

kloop 实现同时把安全边界收紧：stdio list_changed 原子替换 catalog、lag/失败有界重试；unlock 绑定
definition generation from one atomic source snapshot，同名 schema 更新后必须重新 ToolSearch；MCP call 与 refresh 以 async gate 线性化（已开始调用先完成，随后才发布 catalog；发布后的旧代调用拒绝）；
stdio/HTTP 在 JSON parse 前限单消息 wire bytes，tools/resources paginator 与 read/tools-call content 另有 page/cursor/item/byte/base64/image 限制。Resource read/directory
先刷新 catalog、只接受当前 advertised URI，动态 URI 的 AllowSession/AllowAlways 不记忆；list 的单/全失败为错误，部分失败保留成功项并点名 server。
Streamable HTTP 仍无长期 server-notification stream，因此连接状态明确报告 startup catalog 固定，未把
request 内短 SSE response 伪装成动态订阅。full exact-binary verifier 与 workspace Rust 门均通过。

Plan 55 完成时 corpus 为 **145 captures / 180 static evidence**。WebSearch 按 profile 拆分后，matrix 为
57 行/456 单元，状态为 92 `compatible` / 149 `intentional-diff` / 34 `missing` /
149 `unknown` / 22 `n/a` / 10 `same`。新增 16 个 capture：WebFetch 7 个 strict
`url+prompt` parser/URL safety case，WebSearch 9 个 permission/parser/side-query/output case；既有
8 个 WebFetch transport case 与 Agent remote-gate fallback fixture 继续复用。generated pair 仍为 4 个，
没有把静态链或负证据包装成新 `same`。

WebFetch 的 exact 2.1.220 loopback case 全在 domain-safety preflight 前失败，LocalWeb 请求严格为 0；
所以 redirect/auth/status/large/decode 和 transport concurrency 继续 `unknown`。kloop 不为制造成功 fixture
放宽安全边界，保留 strict `{url}` 纯抓取、逐跳 DNS/SSRF、embedded credentials、same-site redirect、
5 MiB download 与 50k model-text 限制，和 CC 的 mandatory `prompt` + secondary model/cache/markdown
pipeline 明确记为 `intentional-diff`。

WebSearch clean profile 固定 unconditional registration、strict basic parser 与默认 permission denial；
allow profile 真正把 `web_search_20250305` side query 发到本地 `ANTHROPIC_BASE_URL` fake provider，
固定 conflict filters、success/empty/server-tool error 与模型输出映射。matrix 分为
`web-search@clean-cli` 和 `web-search-execution@allow-cli`，不靠跨 profile 隐式吸收运行证据。
kloop 保留按 Tavily/Brave key 条件注册和 bounded plain-text output；timeout、large、动态目标并发、
HTTP provider/CCR proxy 与 lifecycle 仍 `unknown`。remote 只证明 `remote=false` 时请求 remote isolation
会回退 worktree/local async Agent；true cloud/team 和 remote lifecycle 没有被当前 hermetic profile 裁决。

Plan 56 完成时 corpus 为 **190 captures / 188 static evidence**；其中 worktree 新增 45 组，
raw/normalized 各一份。matrix 以真实条件向量拆出 clean registration、
`worktree-scripted-allow-cli` 与 `worktree-scripted-manual-pty` 行，共 61 行/488 单元：
112 `compatible` / 157 `intentional-diff` / 34 `missing` / 151 `unknown` / 24 `n/a` /
10 `same`。clean 行只记录 kloop capability-gated registration 的 intentional difference，禁止再把
worktree=true executor fixture 暗挂到 worktree=false 行。generated executable pair 仍为 4 个；没有把
native report 或静态结构相似升级成无 comparator 的 `same`。

45 组 exact fixture 固定 strict optional name/path、默认名、registered path、重复/switch Enter、显式
keep/remove/discard、tracked/untracked/ignored/commit、interactive approve/reject/cancel、same-round/
concurrent calls、no-active 与 shutdown。每例只操作独立临时 Git repo，并保存 registry、branch/base/HEAD、
status 与 cleanup 审计。kloop 侧 `kloop-plan56-native-report` 真跑 Rust dispatcher，验证 strict schema/parser、
managed/external ownership、provenance fail-closed、effective cwd 的文件/搜索/Bash/system 重锚、permission
与动态 AllowAlways 共享；Python verifier 对缺场景、schema 篡改、cwd 事件重排、external 删除、主树泄漏和
stale permission gate 做 negative mutation。
no-active error、ignored/provenance removal blocker、External
不可删、session shutdown retain 及条件注册均明确保留为产品/安全差异。

Plan 57 完成时 corpus 为 **210 captures / 197 static evidence**；新增 10 组 deterministic pair，
覆盖 rich notebook read、replace/insert/delete、fallback ID、errors、stale、同轮 seriality 与
`ENABLE_LSP_TOOL=1` 负注册。matrix 新增真实的 `notebook-read-adapter@clean-cli` 并把 LSP
切到 `lsp@lsp-env-cli` 条件向量，共 62 行/496 单元：125 `compatible` / 160
`intentional-diff` / 32 `missing` / 145 `unknown` / 24 `n/a` / 10 `same`。generated
executable pair 仍为 4 个，没有把 native report、静态结构或负注册包装成新的 `same`。

精确 fixture 证明 `.ipynb` 读取是 `Read` 的 internal adapter：按 cell 顺序呈现
markdown/code/raw，缺 ID 投影为 `cell-N`，code-output image 成原生 image block，文本/图片/
尾随文本顺序保留。CC `NotebookEdit` 的 strict schema、默认 replace、insert/delete、code output
reset、8-hex insertion ID、fallback ID 不回写、one-space serializer、无尾换行和 untouched
字段顺序/未知字段保真均已固定；随机 normalization 只允许 fixture 证明的单个 generated ID 与
派生 hash。kloop 复用现有 `read_file` 和 descriptor-relative atomic mutation，并以原生
snake_case `notebook_edit` 暴露编辑；不加独立
`NotebookRead`；另加 notebook-qualified FileState、cell-aware approval preview、
`notebook_path` permission key 和 worktree state isolation，普通 raw Read/Write/Edit 不得冒充资格。

LSP 的 bundle locator 固定 `ENABLE_LSP_TOOL`、plugin `.lsp.json` loader 与 manifest
`lspServers`。但隔离 profile 设置 `ENABLE_LSP_TOOL=1` 后，两次 capture 仍只提供 24 个工具且无
LSP；当前无法权威 hermetic 构造 enabled-plugin profile，也就不能通过正常发现链启动本地 stdio
stub 并重放 cleanup。因此 `lsp@lsp-env-cli` 八维继续为有条件向量与失败阶段的 `unknown`，kloop
没有新增推测性生产 LSP client。verifier 同时拒绝未过门的 `lsp.rs`、伪 cleanup、扩大随机
normalization、CC Notebook schema 放宽、native report 场景/事件/资格/preview 漂移。

Plan 58 完成时 corpus 为 **218 captures / 211 static evidence**；matrix 为 62 行/496 单元：
125 `compatible` / 170 `intentional-diff` / 24 `missing` / 129 `unknown` / 24 `n/a` /
24 `same`，generated executable pair 增至 7 个，其中新增
`scheduler-cron-schema`、`scheduler-cron-contract`、`scheduler-concurrency`。kloop 提供
深度 0、owner-scoped 的 `cron_create`、`cron_delete`、`cron_list`、`schedule_wakeup` 和
`/loop` adapter，明确以 snake_case 暴露 `delay_seconds`，不加 CC PascalCase alias。
recurring job 最多七天；durable store 固定在
`~/.kloop/scheduler/<project-key>/scheduled_tasks.{json,lock}`，job 仅创建 session/thread
owner 可见、可 claim 和恢复。TUI/plain/server 分别以 idle Wake、stdin/Inbox select 和
single-flight delivery turn 投递；headless 关闭 scheduler 后保留 durable state。调度为进程内
能力，不创建或修改 crontab、launchd、systemd timer 或登录项。精确 timed fire、DST/clock
jump、effective recurring jitter、restart/re-arm 和 enabled dynamic-loop runtime 仍保持
`unknown`，不由 native behavior 反推 exact same。

Plan 59 最终验收已将 corpus 固定为 **218 captures / 65 个 determinism group（每组恰有两次 capture） / 214 static evidence**；
matrix 为 62 行/496 单元：24 `same` / 141 `compatible` / 169 `intentional-diff` /
5 `missing` / 133 `unknown` / 24 `n/a`。7 个 generated executable pair contract 精确覆盖全部
24 个 `same`；新增 `profile-bridges.json`，以 108 个逐 cell/source-profile bridge 固定两侧完整
condition vector、condition diff、与 matrix evidence 精确相等的 fixture 集合及 normalized hash、exact-binary evidence 与
`same-binary-dimension-only-v1` projection。Static evidence 的 `covers` 现由 generator/verifier 全局
校验；每条 bundle 证据还必须显式使用 exact locator 或另带 exact base64 byte anchor。executor/output/lifecycle
的非 unknown/n-a cell 分别要求固定 normalized 协议路径上的 typed tool use/result/content/terminal witness；复合行还必须覆盖显式 required CC tool set，任意
metadata `tool_result_ids` 不得冒充运行结果；compatible/intentional-diff 必须有双边证据；三个 kloop-only row
还受 logical-id 精确白名单约束，不污染 CC gap/parity 统计。

Plan 53 的 AskUserQuestion、EnterPlanMode 和 standalone Workflow 旧 missing 已由真实 kloop Rust
report 纠正；Ask parser 使用 empty/type/null/options/unknown-field 正交负例，report 只按显式 row/dimension
mapping 投影。StructuredOutput 因没有 CC nested schema-child calling fixture 而将运行维度退回
`unknown`，没有新增伪 `same`。Plan 59 另以独立 core-dispatch Rust report 组合五条跨链，scope 明确区分
CC `target_entrypoint=local-cli` 与 kloop `kloop_entrypoint=core-dispatch-test-harness`，不把测试 context 冒充
CLI startup wiring；覆盖 worktree FileState/cwd 恢复及隔离 Git config、step-boundary 后台回灌精确计数、
selective ToolSource generation refresh、共享 trace 的 approval-before-dispatch 与 deny 后零 source call、Ask/Plan/Workflow approve/reject/cancel，以及 Notebook/LSP 负边界。
MCP transport、Monitor、PTY 和 LSP 的缺席明确标成 seam/surface/negative boundary，不冒充执行。

最终结论只适用于 manifest 固定的 exact 2.1.220、darwin-arm64 local CLI、`team=false`、
`remote=false` 已执行 profile：**受限行为兼容**。这不是全工具对齐、wire/schema/UI 同一，也不是
drop-in replacement。5 个 SendMessage non-blocking missing、安全 intentional-diff、condition-bound
unknown、profile/surface n/a 与 kloop-only 能力均继续显式保留。精确产物导读见
`refs/claude-code-2.1.220/README.md`。

重放入口（目标二进制必须仍与 manifest 的版本、大小和 SHA-256 精确一致）:

```bash
python3 -B refs/claude-code-2.1.220/collect.py identity
python3 -B refs/claude-code-2.1.220/collect.py list
python3 -B refs/claude-code-2.1.220/collect.py collect --all
python3 -B refs/claude-code-2.1.220/build_matrix.py
python3 -B refs/claude-code-2.1.220/verify.py
python3 -B refs/claude-code-2.1.220/verify.py --corpus-only
```

默认 `verify.py` 读取并校验本机精确 target identity 与 bundle locator bytes；`--corpus-only` 只跳过这两项，仍执行 fixture/hash/normalization/tamper、matrix/pair 生成一致性、fake provider、敏感信息和可在当前平台运行的 kloop Rust semantic report。POSIX descriptor/ctime/symlink/publication/PTY collector 自检只在 POSIX 执行；Windows 改验全部 case declaration、unsafe path rejection 与 collector/PTY fail-closed，并保留 immutable corpus/matrix/pair 门。Plan 59 native report 明确是 Darwin arm64-only，Windows 不伪造运行；这些平台边界都不能升级 pinned matrix 结论。

`collect --all` 会采出带新临时路径/端口的 raw 文件，它是重放审计，不是改写 Plan 48 历史
capture 的授权。应在可丢弃副本运行，或比较 normalized 后恢复 immutable legacy raw/manifest
条目；`verify.py` 会故意拒绝 40 份历史 hash 漂移。只在 case 契约本身变更时发布对应 pair，随后
运行 `build_matrix.py` 与 `build_matrix.py --check`。

完整矩阵、fixture 方法和 Plan 49–59 拆分见
`docs/plan/48-claude-code-2.1.220-tool-parity.md`；各行为簇与最终验收见
`docs/plan/49-file-search-parity.md`、`docs/plan/50-bash-foreground-parity.md`、
`docs/plan/51-background-monitor-parity.md`、`docs/plan/52-agent-task-team-parity.md`、
`docs/plan/53-interaction-control-parity.md`、`docs/plan/54-discovery-extension-parity.md`、
`docs/plan/55-web-remote-parity.md`、`docs/plan/56-worktree-parity.md`、
`docs/plan/57-notebook-lsp-parity.md`、`docs/plan/58-scheduling-parity.md`、
`docs/plan/59-tool-parity-acceptance.md`、`docs/plan/61-file-tool-correctives.md` 与
`docs/plan/62-windows-shell-tools.md`。Plan 59 的通过只产生上述受限行为兼容结论，不产生
“全工具已对齐”或“可替换 Claude Code”的产品声明。

## 调研结论(三轮调研的浓缩)

1. **codex**:地基最硬——分层循环(任务→主循环→provider 故障转移→请求重试→流消费,各一层)、append-only 历史硬规则、多模型工具画像(model_info 按模型切工具形态)、unified exec 持久 shell 会话。弱在:上下文耐力(门控全部基于已测量用量,无 predictive;此缺口 2026-07 已在其 fork 上试补过一轮,见下"预演记录")、恢复语义少、工具默认不并行、shell 万能导致权限粒度粗。
2. **claude-code**:赢在生存层——七层上下文防线(含 predictive/reactive 压缩)、丰富恢复语义(输出截断升级重试、fallback 模型、孤儿 tool_result 修补、Terminal 原因枚举)、专用工具(Read/Edit/Grep/Glob)+ `isConcurrencySafe(input)` 按入参动态并发(只读批并发上限 10)、子 agent 递归复用同一 query() 循环。
3. **claw-code**(agent 自治维护的 Rust 克隆,精读过 11.6 万行):约 60% 真实 / 25% 孤儿 / 15% 表演;压缩是假的(不调模型,关键词模板套 `<summary>` 戏服,触发数学错误)、工具严格串行、Worker/Cron 是内存模拟。
4. **两边独立收敛的"必然解"**(直接照抄不必发明):tool_use 有无判续跑(别信 stop_reason)、deferred 工具 + tool_search、超长输出落盘 + 回读工具、MCP `server__tool` 命名。
5. 多模型编辑工具实测数据点(2026-07-09):同一"修 bug 并验证"任务,gpt-5.4-mini 和 claude-sonnet-5 都能首试用对 Edit(old_string/new_string)形态并理解 offload 指针——apply_patch 不构成选型约束。

## cc 压缩层设计参考(kloop 压缩已按此实现;扩展时对照)

**Predictive**(cc `src/query.ts:848-888`):
```
currentTokens = 最后一条带真实 API usage 的 assistant 锚点 + 其后消息的 chars/4 估算
predictiveThreshold = effectiveContextWindow - estimateMaxTurnGrowth
   // effectiveWindow = contextWindow - min(maxOutput, 20_000)
   // estimateMaxTurnGrowth = min(maxOutput, 20_000) + 15_000(工具结果增长预留)
超线 → 立即完整压缩,同轮继续发请求
```
要点:阈值是绝对余量不是百分比;predictive 用裸有效窗口,不叠加 autocompact buffer(避免双重预留,cc 注释明确);与阈值压缩("已超线")互补。**cc 盲点(kloop 已修):窗口 ≤ 增长预留时阈值非正,变成"永远压缩"——必须守卫。**

**Reactive**(cc `src/query.ts:1349-1470` + `services/api/errors.ts`):检测匹配 `'prompt is too long'`(大小写不敏感);正则抽 `actual > limit` 算溢出缺口;每 turn 单发守卫 → 完整压缩 → 继续循环重试,失败才浮出错误(流式期间错误对 UI 暂扣);摘要请求自身溢出时按"整 API 轮"分组丢头部(缺口定量,兜底 20%,最多 3 次);连续 3 次压缩失败熔断。

**其余 cc 常数**(移植时对照):autocompact buffer 按窗口 50k/30k/13k;手动 compact 预留 3k;警告带 20k;单消息工具结果预算 200k 字符、单工具默认 50k;摘要 prompt 九段式(kloop 的 COMPACT_INSTRUCTION 是其精简版);全量压缩后重注入最近读过的 ≤5 个文件现状。

## 会话持久化对比(2026-07-09,plan 7/7b 调研)

三家的落盘语义收敛:JSONL 逐行追加、append-only、延迟建文件、恢复时修补配对。关键分歧与结论:

- **cc 存树不存链**(`src/utils/sessionStorage.ts`):每行是带信封的 Entry(20+ 种类型),消息行带 `uuid`/`parentUuid`/isSidechain/gitBranch;恢复 = 选 leaf 沿 parentUuid 回走再反转(`buildConversationChain`,:2106,带环检测 + 找回被单亲遍历孤儿化的并行 tool_result)。fork/rewind/分支会话全长在这三个字段上。文件在 `~/.claude/projects/<cwd slug>/<uuid>.jsonl`,集中放 home。
- **压缩持久化两派**:codex 标记行内嵌 `replacement_history`(恢复零重算,拿磁盘换简单);cc 只写 `compact_boundary` 标记 + `isCompactSummary` 普通消息,恢复时对 >5MB 文件做字节级 chunked 前向读、流中截断到最后边界(多 GB transcript 的性能工程,50MB 读上限)。kloop 取 codex 派 + cc 的信封字段(id/parent/ts,plan 7b)。
- **配对修补 cc 是双向的**(`ensureToolResultPairing`,messages.ts:5580):正向补合成错误块、反向删引用不存在 tool_use 的孤儿 tool_result,且是每次发请求前的防御校验,不只恢复时用。kloop 7b 起同为双向(恢复时)。
- cc 的性能工程(chunked 读、RSS 优化、200 文件缓存)是规模驱动,不改格式可后补,不属于底子。

## 权限系统对比(2026-07-09,plan 8 调研;kloop 已按此实现)

两家深调后的收敛点与分歧(细节可再查:cc `src/utils/permissions/permissions.ts:1179` 的
`hasPermissionsToUseToolInner`、`packages/builtin-tools/.../bashPermissions.ts`;codex
`codex-rs/shell-command/`、`core/src/exec_policy.rs`、`core/src/tools/orchestrator.rs`):

- **管线次序是 cc 的精华**:deny 规则 → 工具自查 → content-ask/safetyCheck → bypass → allow
  规则 → 兜底 ask。两条不变量:deny 永远先于 allow;敏感检查(`.git/`、shell rc、`.claude/`)
  在 bypass 之前——bypass 模式也拦不住。kloop 照搬。
- **bash 解析两家都上真语法树**(cc 新路径 tree-sitter AST + Haiku 注入分类器兜底;codex
  tree-sitter-bash word-only 白名单遍历)。codex 的遍历纪律:允许的节点仅
  program/list/pipeline/command/word/string/raw_string/number/concatenation,运算符仅
  `&& || ; |`;子 shell/重定向/替换/赋值前缀一律 bail → 不可分析。手写 `&&`/`;` 字符串拆分
  就是注入洞的温床(kloop 自己踩过三个)。kloop 直接移植 codex 版(`core/src/shell.rs`)。
- **只读分类器要审查选项不只看名字**(codex):`find -exec/-delete`、`rg --pre/-z`、
  `base64 -o`、`sed` 仅 `-n Np`、git 全局选项注入(`-C`/`-c`/`--git-dir`/`--exec-path`,子命令
  `--output`/`--ext-diff`)、`git branch` 仅列表形。安全/危险是**两个独立分类器**(危险 =
  `rm -f/-rf`、`sudo <cmd>` 递归),中间地带才是"要不要问"的判定区。
- **匹配的非对称性**(cc):allow 前缀规则不匹配复合命令(按段各自判);deny 必须匹配复合
  命令(任一段命中即拒)且匹配前剥 env 前缀/wrapper 到不动点,防 `FOO=1 rm` / `sudo rm` 绕过。
- **"always allow" 记前缀不记整条**(cc):bash 记两词前缀(`Bash(git commit:*)`),文件记
  目录 glob(`Edit(dir/**)`);落盘进 settings 的 allow/deny/ask 三数组,session 级只进内存。
- **拒绝 = is_error tool_result + 改道引导文案,turn 继续**;只有 abort 才终止(两家一致)。
  codex 的决策枚举更丰富(Approved/ApprovedForSession/ApprovedExecpolicyAmendment/Denied/
  TimedOut/Abort),批准请求走 per-call oneshot channel,掉线默认 Abort(fail-safe)。
- **codex 独有、kloop 暂不做**:sandbox 与 approval 双轴配合(受限沙箱内不问、失败后
  "升级为询问再裸跑"的 escalation 环)、execve 拦截级 execpolicy、Starlark 规则文件、
  自动规则修正(带 `bash`/`sudo`/`python -c` 这类 BANNED_PREFIX 黑名单)。这些依赖沙箱
  基建,等 kloop 有沙箱再回来抄。cc 独有暂不做:AI 分类器 auto 模式、updatedInput 改写、
  permission modes 全集(kloop 只取 default/acceptEdits/bypass 三档)。

## codex 后台/交互进程与多 agent 调研(2026-07-10,plan 14 第三片后)

背景:kloop 已有后台 bash(run_in_background + bash_output/kill_bash + 进程组 + monitor task)与同步 task 子 agent,评估 codex 还有什么可借。细节可再查:unified_exec 在 `codex-rs/core/src/unified_exec/`(process_manager.rs 编排、head_tail_buffer.rs 截断)+ 工具面 `core/src/tools/handlers/unified_exec/`;多 agent 在 `core/src/tools/handlers/multi_agents_v2/`;并行锁 `core/src/tools/parallel.rs`。

- **unified_exec 的本质差异是交互**(codex 上游机制,codex 只加审批/沙箱面):`exec_command`(cmd/tty/yield_time_ms 默认 10s/max_output_tokens)先等一会,等不完就存进程返回 `session_id`;`write_stdin(session_id, chars)` 续写,**空 chars = 纯轮询**(默认 5s,上限 300s)。可持续写 stdin(REPL/ssh/交互确认)是 kloop 后台 bash 没有的能力;PTY 可选。生命周期:上限 64 个,LRU 淘汰(保护最近 8、优先淘汰已退出),turn 结束全清,无空闲超时。输出 HeadTailBuffer:1MiB,头尾各 50%,中间截断。
- **codex 没有"完成主动通知模型"的通道**:后台进程靠模型轮询;`notify` 配置是通知用户的外部命令(fire-and-forget,输出丢弃)。三个参考里只有 cc 做了 task-notification 回灌模型。
- **多 agent 是异步体系**:`spawn` 立即返回 agent_id → `wait`(mailbox 更新摘要,新用户输入可提前打断)→ `send_message`;**子 agent 终态时投递父 agent mailbox(turn 中途回灌,`session/mod.rs` forward_child_completion_to_parent)——"通知通道"的现成先例,且只对子 agent 做、无需全局任务框架**。role 化(config 分层覆盖 model/effort/系统提示)、complexity 分级、CSV 批量 fan-out 均体量巨大,明确不抄。
- **工具并行 codex 比 kloop 粗**(反向借鉴,保持 kloop 现状):全局单把 RwLock + 每工具静态 supports_parallel 布尔,读锁共享写锁独占;无路径粒度、无 kloop 的"连续只读成批、遇写切断"顺序性。
- **借鉴清单**:① `write_stdin` 最小切片(挂 plan 14 备选);② 子 agent 异步最小形态 spawn+wait+mailbox 回灌(挂 plan 17);③ 小卫生件:HeadTailBuffer、进程表上限+LRU(有痛感时整段抄)。不抄:ToolOrchestrator 审批沙箱耦合、多 agent 全家桶、notify、SubagentStart/Stop hooks 引擎、parallel.rs 全局锁。

## steering / 中途注入对比(2026-07-13,plan 22 回源;kloop 已按此实现机制 + 用户 steering)

三家真读代码交叉核对(细节可再查:cc `src/utils/messageQueueManager.ts` + `src/query.ts:1829-1904`
drain + `src/utils/messages.ts:5988` framing;codex `core/src/session/input_queue.rs` +
`session/turn.rs:229` drain + `session/mod.rs:3903` `steer_input` / `:1881`
`forward_child_completion_to_parent`;claw `rust/crates/runtime/src/conversation.rs:325`):

- **收敛点(cc 与 codex 用完全不同架构独立都做,kloop 已抄)**:① **steering = 入队,绝不 abort
  turn**——干活时打字进队列;硬断(Ctrl+C/Esc / `Op::Interrupt`)是**另一条结构上独立的路径**
  (cancellation token / AbortController)。② **绝不插进在途请求**——只在 step/round 边界、下一次
  采样**之前** drain;cc 注释点破原因"interleave tool_result 与 regular user 消息会 API 报错",
  所以排到该轮 tool_results 之后再作 user 消息。③ 一个 per-turn(cc 优先级队列 / codex `TurnInput`
  队列)队列,循环边界 drain。④ **子 agent 回灌 = 把子终态投进父队列、step 边界交付**,带
  delivery-phase 闸门(工具后折进后续请求 / 终答后推迟下一 turn)+ autowake 唤醒空闲父;**Interrupted
  子不回灌**(codex `is_final=false`)。
- **分歧(judgment,不必都抄)**:注入 framing——cc 包"The user sent a new message while you were
  working…IMPORTANT: 完成当前任务后必须处理",codex 记为**裸 user prompt** 不 framing;kloop 取
  **cc 式轻 framing**(便宜、对弱模型有用,`STEERING_PREFIX`)。回灌摘要截断——codex completion
  上限 1000 token、**仅 error 分支截 900**、成功摘要原样透传(**订正**:本 README 上文"截 900
  token"是旧记,只对 error 成立)。
- **claw-code 是反面教材**:无 steering,纯阻塞 REPL(`conversation.rs:325` 同步 `run_turn`,
  读一行→整轮跑完→再读);Ctrl+C 只设 `AtomicBool` 且仅 hook 子进程查(`hooks.rs:327/804`),
  连模型流/工具都断不了。kloop 早有 turn 级 cancel,远高于它。
- **kloop 取舍**:机制 = step 边界注入队列(`Config.inbox`,`agent.rs` round 顶 drain + 收尾兜底),
  首客户用户 steering(TUI Enter-while-running 入队);**子 agent 回灌整套依赖异步派发**(收敛点 ④),
  kloop 现为同步 task,故回灌 + 异步派发留独立 plan(建议 plan 26,机制留接口:push framing 摘要进
  inbox 即可,drain 侧零改)。plain/server 的 enqueue 侧挂账(plain 阻塞读、server `turn/steer`)。
- **教训**:验收覆盖最弱目标模型这条(教训 13)对 steering 尤其成立——framing 是给 gpt-5.4-mini 这类
  只认声明工具/需要显式上下文的模型留的显式通道;裸 user 消息 sonnet-5 能懂,弱模型未必分得清"这是
  中途插话还是新任务"。

## 编辑审批 diff 呈现对比(2026-07-13,plan 21 回源;kloop 已按此实现)

三家的编辑审批 diff 交叉核对(细节可再查:cc `src/utils/diff.ts` `structuredPatch` +
`src/components/FileEditToolDiff.tsx` `getPatchForDisplay`,`packages/color-diff-napi`
渲染;codex `tui/src/diff_render.rs` `create_diff_summary`(`diffy` crate)+ `git-utils/
src/turn_diff.rs`(`similar`);claw `refs/claw-code/rust/crates/runtime/src/file_ops.rs`
`make_patch`):

- **收敛点(两个正经实现独立都做,kloop 已抄)**:① **读文件、应用编辑、整文件 diff 带
  真实行号**——不是直 diff old_string/new_string 两串;改动显示在文件里的真实周围上下文 +
  真实行号。cc 甚至有"文件读不到/超大(old_string 是整文件)就退化成直 diff 两串"的分级,
  kloop 照抄为兜底。② 3 行上下文;③ 省 `@@` 头改用逐行行号;④ 审批处内联渲染 diff;
  ⑤ 着色 add 绿 / del 红 / context 暗。
- **分歧(不必抄)**:hunk 分隔符 cc 用 `...`、codex 用 `⋮`(kloop 取 `⋮` 与 codex 一致);
  intraline 词级高亮只有 cc 做(`color-diff` 对相邻 -/+ 对做 wordDiff,>40% 变化率放弃);
  语法高亮 cc/codex 有、nice-to-have;new-file 呈现 cc 语法高亮整段无 `+`、codex 全 `+`
  (kloop 取全 `+` 与 codex 一致);长行 cc/codex 都**换行**、kloop **截断**(因弹层不可滚动,
  见 plan 25);行数硬截断 kloop 独有(两家审批处靠可滚动 overlay 不截,plan 25 补滚动后放宽)。
- **claw-code 是反面教材**:`make_patch` 全 `-`+全 `+` 朴素拼接(无 LCS、无上下文、无对齐),
  行号 old_start/new_start 恒为 1 无意义,且 headless CLI 审批处根本不渲染 diff(只把
  `structuredPatch` 塞进 tool_result JSON 对齐 cc 输出契约)。kloop 质量远高于它。
- **教训**:plan 21 初版按 plan 备忘"edit 直接成 diff"跳过回源(教训 11 复发),漏了行号 +
  读文件这对收敛点;用户追问后回源补齐。收敛信号(教训 14)在这里很干净:cc 与 codex 用
  完全不同的库(structuredPatch vs diffy/similar)得出同一"读文件+整文件 diff+行号"取舍。

## 预演记录(codex fork,2026-07-09)

kloop 的压缩设计曾先在 codex fork 上完整实现过一轮(分支 `codex/worktree/predictive_reactive_compaction`,提交 57c746ef7,Buildbot 绿,未合入 main):predictive 插在 `run_pre_sampling_compact`、reactive 插在采样错误分支、Feature 双旗标、compact_fork_tests.rs 四个集成测试。价值:验证了设计、抓出小窗口负阈值盲点。教训:kloop 才是项目,参考库不用于开发。
