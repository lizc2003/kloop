# kloop 能力对比报告(vs claude-code / codex)

> 基线:2026-07-30,plan 1–51 完成（plan 39 仍按已完成切片计）；精确 Claude Code 2.1.220
> parity corpus、文件/搜索、前台 Bash 与后台 lifecycle 闭环见 plan 48–51。
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

### 2. 权限系统——✅ 核心(两家之合),宽度缺口如下

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| permission modes 全集(plan 档) | 概念双家 | **plan 37** | 已立 |
| AI 分类器 auto 模式 / updatedInput 改写 | cc 单家 | 暂不做(plan 8 判) | 有小模型基建再议 |
| execpolicy(execve 级 Starlark 规则) | codex 单家 | 暂不做 | 沙箱已有,重;痛感驱动 |

### 3. OS 沙箱——🟡 macOS 完整,平台 1/3

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| Linux(bwrap+seccomp) | 双家 | **plan 19 余片**(设计已定:sibling `linux.rs`) | 仓库推远端、Linux CI 可跑 |
| Windows(spawn-owning trait 改缝) | cc 单家 | plan 19 更后 | 有 Windows 用户 |

### 4. 工具面——🟡 主干齐；文件/搜索簇与前台 Bash 已完成 exact 2.1.220 parity（plan 49–50）

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| PDF 读入 | 双家(cc Read 判 MIME;codex view_image 族) | plan 49 明确返回 unsupported；不在 canonical provider wire 未统一时伪兼容 | 真实 PDF 需求 |
| 交互 stdin(write_stdin) | codex 单家,cc 明确不做 | **⛔ plan 30 判不做**(REPL 逃逸不过门) | 真痛感再重启,连 PTY 一起 |
| 自动后台化 / stall 探测 | cc 单家 | Plan 51 保留 intentional-diff：精确 gate/时钟未形成可运行 fixture，kloop 坚持显式后台与前台 no-survivor | 新证据或 dogfood 痛感 |
| model-visible Monitor（逐 stdout 行 / WebSocket frame） | cc 单家、server flag 默认关 | Plan 51 明确不伪造；`tengu_amber_sentinel` 真 profile 不可由本地 harness 权威开启，matrix 保留 `unknown` | 官方暴露该 profile 或出现逐事件 watch 需求 |
| 后台完成/失败/取消通知、下一 step 回灌、session 清理 | cc 单家 | **✅ Plan 51（2026-07-30）**：shell + agent/program 共享外部 lifecycle，不合并内部 registry | 已完成 |
| HeadTailBuffer / 进程表 LRU | codex 单家 | 挂账"小卫生件" | 有痛感整段抄 |
| notebook 编辑 | cc 单家 | 不立 | 无需求 |

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
裁决一次终态，session shutdown 先 cooperative cancel、deadline 后 abort/SIGKILL，再清 worktree。
自动后台化、stall 和逐事件 Monitor 仍是明确的产品边界。

### 5. 子 agent / 多 agent——✅ 收敛解全齐

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| worktree 隔离(并行写不互踩) | 双家 | **plan 35** | 已立 |
| send_message(与运行中子 agent 持续对话) | codex 单家 | 未立 | 异步子 agent dogfood 痛感 |
| 并发上限(cc 10) | cc 单家 | 不做(kloop uncapped 先例,教训见 plan 24) | 失控实例出现 |
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

### 11. MCP——🔴 传输宽度是当前最大生态缺口

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| streamable HTTP 远程传输 | 双家 | **plan 34** | 已立 |
| OAuth(授权码+回调+keyring) | 双家 | plan 34 挂账,独立 plan 级体量 | 接需 OAuth 的真实 server |
| MCP resources(@-mention)/ prompts | cc 单家 | 未立 | 生态需求 |
| 旧版 SSE 传输 | cc legacy,codex 不做 | ⛔ 不做 | — |
| MCP server 模式(kloop 自身作为 server) | cc 单家 | 未立 | 被集成需求 |

### 12. skills / slash / steering——✅ 核心生态位完整

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| 用户自定义 commands 目录 + `!cmd`/`@file` 注入 | cc 单家(已折进 skills) | **plan 36** | 已立 |
| skills 余项(effort/bundled 懒解压/paths/远程 skills/开关字段) | cc 单家 | plan 28 挂账清单 | 生态兼容痛感 |
| plain 前端 steering enqueue | — | 平台事实(阻塞读),接受 | TUI/server 已覆盖 |

### 13. 前端(TUI / plain / server)——🟡 功能齐、打磨差距最大

| 差距项 | 收敛 | 补齐路径 | 触发条件 |
|---|---|---|---|
| 语法高亮 / intraline 词级 diff | cc(词级仅 cc) | 未立,nice-to-have | dogfood 体感 |
| 输入补全(slash/@file/历史)| 两家皆有 | 未立 | dogfood 体感 |
| 粘贴图片入口 / `--image` 实时占位行 | — | plan 29 挂账 | 图片 dogfood |
| TUI 模式/档位状态栏显示 | 两家皆有 | 可并入 plan 37 决定 3 | plan 37 开工时定 |
| server 不转发 thinking delta | — | HANDOFF 记录在案 | client 需要时 |
| vim mode / 主题 / statusline | cc 单家 | 不立 | 定位外 |

### 14. headless / 脚本化——🔴

**plan 33**(已立):位置参数/stdin、`--json` 事件流(复用 server wire)、审批默认
拒、退出码。双家收敛;同时是 CI 端到端与 dogfood 自举的入口。

### 15. 测试 / CI / 工程质量——✅ 纪律同级

| 差距项 | 补齐路径 | 触发条件 |
|---|---|---|
| 远端 CI 首次实跑(workflow 只做过本地等价验证) | 不占编号小事 | 用户解除暂缓、推远端 |
| Linux 平台测试(连带沙箱 Linux 片) | 同上 | 同上 |
| 自审遗留:低危项与重复代码清理 | 教训 25 尾注挂账 | 顺手 |

### 16. 实战里程——**最大差距,唯一抄不来**

两家被海量用户长期锤过;kloop 的真 key 验收是每能力单场景闭环。长会话稳定性、大
repo 性能、并发边角、UI 体感只有用出来。**收敛路径 = dogfood**:
- 建议第一步:**用 kloop 自己做 plan 33**(自举,顺带把 headless 需求真跑一遍);
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

- **T0 已立编号**:plan 33(exec,最便宜、解锁 dogfood/CI)→ 34(MCP 远程,生态价值
  最高)→ 37(plan mode)→ 36(commands)→ 35(worktree,靠真实并行写痛感)。
- **T1 有明确外部触发**:Linux 沙箱 + CI 首跑(推远端后);responses 契约销账 +
  `/compact` 真 key 验收(拿到官方/真 key 时顺手)。
- **T2 痛感驱动**(出现痛感时从三节明细里提):hooks JSON 协议、`/cost` 累计花费、
  压缩后重注入、TUI 打磨件、HeadTailBuffer、send_message、MCP OAuth、子目录懒加载、
  会话性能工程。
- **⛔ 已判不做(别再议,除非前提变)**:write_stdin(plan 30)、token budget(教训
  24)、并发 pacing、V8 引擎、旧版 SSE 传输、worktree 自动合回、bedrock/vertex、
  vim/主题。

## 五、维护纪律

- 每完成一个 plan:对应行销账(✅ + 日期 + 提交号),T0 队列前移。
- dogfood 出的新痛点:先进对应域的明细表(标"痛感已现"),再决定立不立 plan。
- 新回源发现改变收敛强度时(如教训 32 的否定断言翻转):当行更新并注日期。
- 本报告与 HANDOFF 分工:HANDOFF 记"已有什么、怎么实现的",本报告记"缺什么、
  何时补";能力落地后细节归 HANDOFF,本报告只留销账行。
