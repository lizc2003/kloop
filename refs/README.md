# refs — 参考资料与调研结论

kloop 设计时对比研究过三个代码库。本文件是关于"别人代码"的全部知识:导读 + 调研结论 + 可移植设计参考。项目自身的状态与教训见 docs/plan/HANDOFF.md。

## 导读

| 参考 | 位置 | 看什么 |
|---|---|---|
| **codex** | `refs/codex` | codex 生产级 fork。分层循环:`codex-rs/core/src/session/turn.rs`;工具注册:`core/src/tools/spec_plan.rs`;并行锁:`tools/parallel.rs`;压缩全家桶:`compact*.rs`、`fork_proactive_trim.rs`;会话落盘:`rollout/`;扩展范式:`ext/worktree`;集成测试:`core/tests/suite`(mock SSE + wiremock 范式) |
| **claude-code(逆向 TS 版)** | `~/work/claude-code` | 主循环:`src/query.ts`(七层压缩流水线在 queryLoop 每轮开头);压缩:`src/services/compact/*`;工具并发分批:`toolOrchestration.ts`(partitionToolCalls);子 agent 递归:`AgentTool/runAgent.ts`;重试:`withRetry.ts`;溢出检测:`services/api/errors.ts` |
| **claw-code** | `./claw-code/`(本地拷贝,已删 target/) | **不可作底座**(见结论 3)。仅三样值得抄:① `rust/crates/mock-anthropic-service` + `rusty-claude-cli/tests/output_format_contract.rs` 的 mock 契约测试纪律;② `rust/crates/api/src/providers/openai_compat.rs` 的 tool_calls 流式翻译状态机;③ `rust/crates/runtime/src/compact.rs:129-166` 的压缩边界回退(不切开 tool_use/tool_result 对) |

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

**裁决边界**:三个参考库只解释独立收敛、分歧和移植成本;公开文档只帮助设计 probe。当前
工具的注册条件、schema、空值/默认、权限层序、截断、后台通知和状态机最终只由精确 2.1.220
bundle + 隔离黑盒 fixture 裁决。

Plan 48 已提交以下可重放基线:

- `refs/claude-code-2.1.220/manifest.json`:6 个完整条件 profile、38 组 raw/normalized
  capture，包含 clean determinism pair 和独立的 ExitPlanMode PTY approve/reject/cancel；
- `static-evidence.jsonl`:46 条带 target SHA、bundle offset 或固定 commit locator 的静态证据；
- `tool-matrix.json`:43 个“逻辑能力 + profile + 可见变体”行，逐项记录 registration→schema→
  parser→executor→permission→concurrency→output→lifecycle；
- `collect.py`/`verify.py`:采集前 exact identity guard，隔离 HOME/config/cwd、本地 provider/Web/MCP，
  受限 normalization、hash/determinism、证据引用、文件集合、环境/网络和敏感信息 fail-closed 校验。

当前矩阵有 47 个 `compatible`、43 个 `intentional-diff`、20 个 `missing`、215 个
`unknown`、19 个 `n/a` 单元，**没有 `same`**。这不是漏标：`same` 必须同时有对应 CC fixture
与锁定相同行为的 kloop golden；静态 locator、名字或相似 schema 都不够。WebFetch 本地 URL 在
2.1.220 的 domain-safety 层先被拒、stub 收到 0 请求，因此只证明权限顺序，重定向/认证/大响应
executor 仍为 `unknown`。ToolSearch 则已黑盒证明 `ENABLE_TOOL_SEARCH=true` 下首请求 defer、
`select:` 引入目标 MCP schema，以及 `tools/list_changed` 后 100→101 刷新。

重放入口（目标二进制必须仍与 manifest 的版本、大小和 SHA-256 精确一致）:

```bash
python3 -B refs/claude-code-2.1.220/collect.py identity
python3 -B refs/claude-code-2.1.220/collect.py list
python3 -B refs/claude-code-2.1.220/collect.py collect --all
python3 -B refs/claude-code-2.1.220/build_matrix.py
python3 -B refs/claude-code-2.1.220/verify.py
```

完整矩阵、fixture 方法和 Plan 49–59 拆分见
`docs/plan/48-claude-code-2.1.220-tool-parity.md`。基线只完成事实盘点，不表示 kloop 已全工具
对齐或可替换 Claude Code；产品行为要由 Plan 49–59 逐簇实现与验收。

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
