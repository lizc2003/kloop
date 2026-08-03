# kloop 能力对比报告(vs claude-code / codex)

> 基线:2026-08-03,plan 1–58 完成（plan 39 仍按已完成切片计）；精确 Claude Code 2.1.220
> parity corpus、Plan 49–58 各工具簇闭环见对应计划。
> 用途:**补齐能力时对着本报告挑项**——每项差距标了出处、收敛强度、补齐路径与触发
> 条件;完成后在对应行销账(标日期 + 提交号)。项目状态细节在 `docs/plan/HANDOFF.md`,
> 参考库知识在 `refs/README.md`,本文件只管"差在哪、补不补、何时补"。

## 一、总评

- **规模**:kloop 9 crate 共 ~3.1 万行 Rust;codex codex-rs ~118 万行、cc(逆向
  TS)~71 万行。kloop 用约 3% 的代码量覆盖核心引擎面——对比按"同一能力的质量"看,
  不按面积。
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
| stale-safe 原子文件修改 | cc 接受 partial Write / unread-partial Edit 且可 stale-recover、会隐式创建缺失父目录；kloop 完整 fresh Read + keyed lock + 审批前 parent FD + descriptor-relative sync/rename fail closed | plan 49 |
| 双轨 provider 对等 + 三线协议 | 两家各自单主轨 | plan 15 |
| MCP 工具名消毒比 cc 严(`-`→`_`) | cc 规则语法不兼容风险 | 教训 10 |

## 三、分域差距明细

状态:✅ 齐平/反超 🟡 主干齐、长尾缺 🔴 空缺(已立 plan) ⛔ 判不做(有依据)。
收敛强度:**双家** = cc 与 codex 独立都做(教训 14 强信号);单家 = 只一家做。

### 1. 生存层(压缩/恢复/token 记账)——🟡,对 codex 反超

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| 压缩后重注入最近读过的 ≤5 文件 | cc 单家 | 未立 | dogfood 出现"压缩后失忆"痛感 |
| microcompaction / 工具结果分档预算(200k/50k) | cc 单家 | 未立(offload 已覆盖大头) | 超长工具输出场景痛感 |
| autocompact 警告带(剩 20k 提醒) | cc 单家 | 未立,小件 | 顺手做 |

### 2. 权限系统——🟡 gate 核心齐，Project/Session 作用域待纠偏

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| permission modes 全集（含 plan） | 概念双家 | **✅ Plan 37（2026-07-16）** | 已完成 |
| Config 生命周期 + project-scoped durable permission | 内部架构/安全边界 | **Plan 63**：global 只留 deny/ask；用户私有 ProjectStore 保存 allow；显式 Runtime/Project/Session/Agent/Workspace 所有权 | **已立，且应先于 Plan 61/62 实施** |
| AI 分类器 auto 模式 / updatedInput 改写 | cc 单家 | 暂不做(plan 8 判) | 有小模型基建再议 |
| execpolicy(execve 级 Starlark 规则) | codex 单家 | 暂不做 | 沙箱已有,重;痛感驱动 |

### 3. OS 沙箱——🟡 macOS 完整,平台 1/3

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| Linux(bwrap+seccomp) | 双家 | **plan 19 余片**(设计已定:sibling `linux.rs`) | 仓库推远端、Linux CI 可跑 |
| Windows(spawn-owning trait 改缝) | cc 单家 | plan 19 更后 | 有 Windows 用户 |

### 4. 工具面——🟡 主干齐；Plan 49–58 已完成 exact parity 分簇闭环

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| PDF 读入 | 双家(cc Read 判 MIME;codex view_image 族) | plan 49 明确返回 unsupported；不在 canonical provider wire 未统一时伪兼容 | 真实 PDF 需求 |
| 交互 stdin(write_stdin) | codex 单家,cc 明确不做 | **⛔ plan 30 判不做**(REPL 逃逸不过门) | 真痛感再重启,连 PTY 一起 |
| 自动后台化 / stall 探测 | cc 单家 | Plan 51 保留 intentional-diff：精确 gate/时钟未形成可运行 fixture，kloop 坚持显式后台与前台 no-survivor | 新证据或 dogfood 痛感 |
| model-visible Monitor（逐 stdout 行 / WebSocket frame） | cc 单家、server flag 默认关 | Plan 51 明确不伪造；`tengu_amber_sentinel` 真 profile 不可由本地 harness 权威开启，matrix 保留 `unknown` | 官方暴露该 profile 或出现逐事件 watch 需求 |
| 后台完成/失败/取消通知、下一 step 回灌、session 清理 | cc 单家 | **✅ Plan 51（2026-07-30）**：shell + agent/program 共享外部 lifecycle，不合并内部 registry | 已完成 |
| HeadTailBuffer / 进程表 LRU | codex 单家 | 挂账"小卫生件" | 有痛感整段抄 |
| notebook cell 读取与编辑 | cc 单家 | **✅ Plan 57（2026-08-03）**：`read_file(.ipynb)` internal adapter + strict `notebook_edit`（CC：`NotebookEdit`）；完整 fresh cell-aware qualification、ordered 保真与原子提交 | 已完成 |
| model-visible LSP / language-server client | cc 条件分支 | **Plan 57 保留 unknown**：env gate 单独开启仍未注册，正常发现链还依赖 enabled plugin；kloop 不加推测性 client | 取得权威 hermetic enabled-plugin profile 与完整 stdio lifecycle 时 |
| CC WebFetch `prompt` + 二级模型/cache/Markdown pipeline | cc 单家 | **Plan 55 intentional-diff**：kloop 保留 strict `{url}` 有界纯抓取与更强逐跳安全边界 | 出现必须“按指令读页并总结”的产品需求再另立 adapter |
| WebSearch timeout/large/dynamic concurrency/CCR proxy、remote=true/cloud lifecycle | cc 条件分支 | **Plan 55 保留 unknown**：本地 fake provider 已闭合 success/empty/error；remote 仅证明 gate-false fallback | 取得合规 hermetic profile 或官方暴露入口 |

Plan 49 已销账 Read/Write/Edit/Glob/Grep 的主干正确性：session-only 完整读取资格、stale-safe
原子 mutation、UTF-8 安全的 7k Read/Search 输出预算、Grep context/`-o`/分页，以及搜索
ignore/VCS/敏感路径策略均有 exact CC fixture + kloop golden。补强证据门后，descriptor-locked
Pre/Post hook barrier 证明 Read/Glob/Grep 真并发，dependent Edit/Edit/Write/Write pair 证明 mutation
串行；generated pair contract 再以相同规范化调用输入与真实 `dispatch_tools` Rust call/event report 比较，
跨 profile cell 必须额外引用覆盖该维度的 exact-bundle bridge。当前 matrix 保留 237 个跨其他
工具簇或不可运行分支的 `unknown`；8 个 `same` 只覆盖上述并发/串行分类与 Glob/Grep 无孤儿
lifecycle，不外推“全工具一致”。默认 verifier 校 exact binary，corpus-only 同语义门已进双平台 CI。

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
文件指针（不塞命令输出），TUI 空闲自动唤醒、plain/server 下一 turn 交付；两套 registry 各自原子
裁决一次终态，session shutdown 先 cooperative cancel、deadline 后 abort/SIGKILL，再完成后台
registry teardown；session active worktree 无 remove intent 时保留。
自动后台化、stall 和逐事件 Monitor 仍是明确的产品边界。

Plan 52 将 Agent、Task registry、Team mailbox/ListAgents 与 remote/cloud 拆开取证，corpus 现为
96 captures/137 static evidence，matrix 为 56 行/448 单元（67 compatible / 119 intentional-diff /
23 missing / 207 unknown / 22 n/a / 10 same）。exact CC Agent 要求 description+prompt、默认后台；kloop
保留原生 task 默认同步与参数面。Task lifecycle 已固定稳定 ID、owner/metadata、依赖、完成/删除，但产品
决定不实现独立 registry；todo/wait/stop 不冒充 Task*。SendMessage 只闭合 unknown-recipient，ListAgents/
team/remote 因无权威 hermetic true-profile 保持 unknown。新增两相 sampling gate + native report 锁定 task
真并发、后台终态与 inbox 边界；未接真实 cloud/team/mailbox。

Plan 53 将一般问答、Plan control、Workflow 与 CC `StructuredOutput` / kloop `structured_output` 分开落地：`ask_user_question`
使用独立 Questioner seam，plain/TUI/server 支持而 headless/断线 fail closed；`enter_plan_mode`
与 `exit_plan_mode` 固定 depth-0 工具数组并由共享 ModeState 精确保存/恢复前态。独立 `workflow`
始终后台，只暴露 agent 编排原语，不是 `run_program` alias；managed run store、phase/terminal identity、
stop/shutdown、resume hit/miss 和 edited script 均已接通。`structured_output` 仅在 Workflow schema child
中作为 synthetic tool 临时注入，本地 JSON Schema 复验后以原生 JSON Value 回传，不进入主 registry。
Named/nested workflow、token budget、remote execution 与 per-child provider effort 保持 intentional-diff。

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
systemd timer 或登录项。最终 corpus 为 218 captures / 211 static evidence，matrix 为
62 行 × 8 维 = 496 cells（125 compatible / 170 intentional-diff / 24 missing /
129 unknown / 24 n/a / 24 same），7 个 executable pair；其中新增
`scheduler-cron-schema`、`scheduler-cron-contract`、`scheduler-concurrency`。timed fire、
DST/clock jump、server-selected jitter、restart/re-arm 与 enabled dynamic-loop 成功路径仍为
`unknown`。

### 5. 子 agent / 多 agent——✅ 本地执行齐，registry/team 有意保留

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| task worktree 隔离（并行写不互踩） | 双家 | **✅ Plan 35 + Plan 56**：task-owned，clean 自动清理、changed/probe-failure 保留 | 已完成 |
| session Enter/Exit worktree | CC 单家 + kloop 原生 | **✅ Plan 56（2026-07-31）**：strict name/path、显式 keep/remove/discard、effective cwd 全链切换 | 已完成 |
| ownership/provenance-safe removal | 安全产品边界 | **✅ Plan 56**：仅当前 session Managed 可删，External/task/previous-session 与 provenance mismatch fail closed | 已完成 |
| Worktree 条件注册 | 表面策略分歧 | kloop 保留 depth-0 + `SurfaceCapabilities.worktree` gate，matrix 记 `intentional-diff` | 有意保留 |
| 独立 stable-ID Task registry | CC/CodeWhale 有独立状态层 | **Plan 52 拍板不实现**；todo 与执行 registry 分层保留 | 原生多 agent 共享任务分配成为产品需求时另立计划 |
| send_message / addressable mailbox | CC/Codex 均有，但 routing 契约不同 | **Plan 52 仅取证**；内部 Inbox 不暴露 | 需要向运行中子 agent 追加消息或横向协作时 |
| ListAgents / team roster / remote | exact bundle 有 descriptor/gate；本机 true-profile 不权威 | 保持 `unknown`，不接真实团队/云 | 有 hermetic transport/profile 与明确产品需求时 |
| Agent 并发上限 | exact 2.1.220 当前 profile 未测出统一 cap | 保留 kloop：同步批并发；后台 agent/program 每 session 8 | native 压测或失控实例证明需排队策略时 |
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
| subagent 事件 agent_type matcher | — | plan 17 残留小账 | 顺手 |

### 10. 上下文工程(指令文件/caching/deferred)——✅

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| 子目录懒加载(conditionalRules + paths glob) | cc 单家(codex/claw 均无) | plan 32 片 3 挂账,弱收敛 | 大 monorepo 痛感 |
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
| server 不转发 thinking delta | — | HANDOFF 记录在案 | client 需要时 |
| scheduled prompt 空闲投递 | kloop 原生；CC timed runtime 证据仍有 unknown | **✅ Plan 58（2026-08-03）**：TUI idle Wake、plain stdin/Inbox select、server single-flight 完整 turn；不伪造 `turnId:0` | 已完成 |
| vim mode / 主题 / statusline | cc 单家 | 不立 | 定位外 |

### 14. headless / 脚本化——✅

**Plan 33（2026-07-16）已完成**：位置参数/stdin、`--json` 事件流（复用 server wire）、无人审批默认拒绝、稳定退出码与 mock/headless 回归均已落地。

**Plan 58 补充**：headless 主 turn 完成后先关闭 scheduler，再关闭 background task/shell；
session-only scheduled job 消失，durable job 保留，等待同 owner 在可交互 frontend 恢复。

### 15. 测试 / CI / 工程质量——✅ 纪律同级

| 差距项 | 补齐路径 | 触发条件 |
|---|---|---|
| 远端 CI 首次实跑(workflow 只做过本地等价验证) | 不占编号小事 | 用户解除暂缓、推远端 |
| Linux 平台测试(连带沙箱 Linux 片) | 同上 | 同上 |
| 自审遗留:低危项与重复代码清理 | 教训 25 尾注挂账 | 顺手 |

### 16. 实战里程——**最大差距,唯一抄不来**

两家被海量用户长期锤过;kloop 的真 key 验收是每能力单场景闭环。长会话稳定性、大
repo 性能、并发边角、UI 体感只有用出来。**收敛路径 = dogfood**:
- 建议下一步：继续用 kloop 自己实施 **Plan 63**，用真实多 workspace/server session 压力验证新的 Project/Session 权限归属；
- 之后每个 plan 的实现会话尽量在 kloop 里跑,痛点直接变本报告新行;
- 自审(教训 25 的 7 路并行精读)每完成 4–5 个 plan 复跑一轮,盯五类边界(多字节、
  大小写、Drop/Weak、预算耗尽、截断累积)。
- **销账:2026-07-20 对 plan 33–38(baseline b3e79d1)跑了一轮多 agent 对抗式自审
  (8 区域精读 → 每发现两路对抗验证)**,查实并已修 4 个 correctness bug + 1 个一致性
  项(提交 fe2224e/86cc139/d38beae/be1d511/c5e3766,均带回归测试):① bypass 下带
  重定向的 opaque bash 逃过 deny+危险命令层被自动执行(安全绕过,最重);② steer 时贴
  的图片被静默丢弃 + 误导占位行;③ 召回含大粘贴的历史条目重发时发出占位符字面量;
  ④ setup_terminal 失败泄漏 raw 模式 + git worktree;⑤ 未高亮代码块 tab 缩进丢失。

## 四、补齐路线图(按序挑,顺序可按意愿调)

- **T0 架构与 correctness**：Plan 63（Project/Session 权限归属）→ Plan 61（文件工具纠偏）→ Plan 62（Windows shell，依赖 61）；三者修改面重叠，严格串行。
- **T0 parity 余线**：Plan 58 已完成（2026-08-03）；仅 Plan 59 仍按其母计划与证据闸门推进，不因 Plan 63 的内部重构改写 exact CC 结论。
- **T1 有明确外部触发**：Linux 沙箱 + CI 首跑（推远端后）；Responses 回放契约销账 + `/compact` 真 key 验收（拿到官方 key 时）。
- **T2 痛感驱动**：hooks JSON 协议、`/cost` 累计花费、压缩后重注入、TUI 打磨件、HeadTailBuffer、send_message、MCP resource templates/prompts/双向 request、子目录懒加载、会话性能工程。
- **⛔ 已判不做(别再议,除非前提变)**:write_stdin(plan 30)、token budget(教训
  24)、并发 pacing、V8 引擎、旧版 SSE 传输、worktree 自动合回、bedrock/vertex、
  vim/主题。

## 五、维护纪律

- 每完成一个 plan:对应行销账(✅ + 日期 + 提交号),T0 队列前移。
- dogfood 出的新痛点:先进对应域的明细表(标"痛感已现"),再决定立不立 plan。
- 新回源发现改变收敛强度时(如教训 32 的否定断言翻转):当行更新并注日期。
- 本报告与 HANDOFF 分工:HANDOFF 记"已有什么、怎么实现的",本报告记"缺什么、
  何时补";能力落地后细节归 HANDOFF,本报告只留销账行。
