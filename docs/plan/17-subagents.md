# Plan 17 — 子 agent 升级

> 体量偏大,开工时选片,可能不止一个会话。开工前先读 docs/plan/HANDOFF.md。参考:cc 的 agents 机制(`.claude/agents/*.md` frontmatter:独立 system prompt、工具白名单、模型 override;并行派发)、codex 的 subagent(SubagentStart/Stop 挂点、SubagentHookContext)。回源核对(教训 11)。

## ✅ 完成记录(2026-07-14,切片 5:hook agent 字段 + SubagentStart/Stop)—— plan 17 全部切片完成

**选片**:片 5(最后一片)。开工时用户定 **Option B(对齐两家)**,而非只加 agent 字段的最小 Option A——依据:两家独立收敛(教训 14)+ 片 3 刚落的子会话 transcript 让富 payload 可交付 + 用户"最合理不为省事砍"基调。

**回源(两个 Explore 真读 cc + codex,file:line,坐实 plan 17 回源节的④)**:
- **两家都成对有 SubagentStart + SubagentStop**,且都是"SessionStart/Stop 的子 agent 变体"——子 agent 触发 Subagent\* **而非**普通 Stop(cc `hooks.ts:3805` `subagentId ? 'SubagentStop' : 'Stop'`、frontmatter Stop 转 SubagentStop;codex `hook_runtime.rs:304-305` "Root turns run Stop; child turns run SubagentStop")。
- **两家 SubagentStop 都带子会话 transcript + 结果摘要**:cc `agent_transcript_path` + `last_assistant_message`(`coreTypes.generated.ts:158-165`);codex `agent_transcript_path`(子)+ `transcript_path`(父)+ `last_assistant_message`(`schema.rs:579-595`,集成测试 `subagent_notifications.rs:750-766` 锁定)。SubagentStart 都极简(agent_id/type)。
- **pre/post tool 都带可选 `agent_id`+`agent_type`**(仅 thread-spawn 子 agent 有,主 agent 省略,`skip_serializing_if`),都**无 depth**(cc depth 只在 analytics 埋点、不进 payload)。

**kloop 取舍(照抄两家的分裂形状,字段名对齐,粒度按地基)**:
- **两半**:① pre_tool/post_tool 加 `agent` 字段(`with_agent` 仅非空插入,主 payload 逐字节不变——对齐两家"主 agent 省略 agent_id");② 独立 `subagent_start`/`subagent_stop` 事件,子 agent(`agent_label` 非空)在 `run_turn` 里走这两个**而非** pre_turn/post_turn。
- **subagent_stop payload**:`agent` + `agent_transcript_path`(子会话文件,`History::rollout_path`,in-memory 省略)+ `last_assistant_message`(`outcome.final_text`)。字段名照抄两家。**不抄**:父 transcript_path(kloop `session_id` 已是父 id,可派生)、depth(两家都无)、`agent_type`(kloop 未在子 Config 存类型名,记可能性)、stop_hook_active(kloop 无递归 stop 概念)。
- **subagent_start** 可 block(同 pre_turn,`can_block` 含它),subagent_stop 只 context/不 block(同 post_turn)。
- **kloop 一个子 agent = 一次 run_turn**,所以 post_turn 本可统一表达(见下方"为何仍分裂"),但分裂能带 transcript+result 富 payload,值得——照两家形状。
- `HookEvent` 加两枚举 + name/parse;`run_event` 的 block 判定抽成 `event.can_block()`;`Rollout::path`/`History::rollout_path` getter;cli `load_hooks` 错误串列全 6 事件;matcher 仍只对 tool 事件(subagent 事件的 agent_type matcher 记可能性)。

**为何仍分裂而非用 post_turn+agent 统一**(教训点):kloop `post_turn` 本是两家 Stop/SubagentStop 的统一版(一个子 agent = 一次 run_turn,post_turn 恰好每子触发一次)。但分裂成独立事件能让 subagent_stop 带**子 agent 专属的富 payload**(transcript 路径 + 结果),这是通用 post_turn 塞不进去的(会污染主 agent 的 post_turn 形状);且"主 agent 结束"与"某子 agent 结束"是 hook 脚本想分别挂的两件事。所以此处**照两家的分裂**,而非 kloop 地基更省的统一——与片 3"粒度按地基做更细"是一体两面(见教训 23)。

**测试**(399,+5):hooks 单元(tool 事件 agent 仅子有主无、subagent_start 可 block、subagent_stop payload 全字段 + 不 block、in-memory 省略 transcript、6 事件名往返)+ agent.rs 路由集成(子 agent turn 触发 subagent_start/stop 而非 pre/post_turn、stop 带 agent/transcript/result)。

**真 key 验收**(anthropic sonnet-5,`--plain --yolo`,配 pre_tool/subagent_stop/post_turn 三 hook):模型派子 agent 跑 `echo HOOKTEST_OK`——`pre_tool` 主 agent 的 `task` 调用无 agent 字段、子 agent 的 `bash` 调用带 `agent=agent-1`;`subagent_stop` 一次带 `agent=agent-1` + `agent_transcript_path=.kloop/sessions/…-agent-1.jsonl` + `last_assistant_message`(结果)+ `session_id`(父);`post_turn` 只主 agent 触发(keys 仅 event/session_id,子 agent 未触发)。**路由 + agent 字段 + 富 payload 全链闭环。**

**提交**:6c585cc(fmt/clippy/test 全绿,399 测试)。**至此 plan 17 全部切片(1/2/3/4/5 + 片 6 由 plan 26 承接)完成,无挂账。**

## ✅ 完成记录(2026-07-14,切片 3:子 agent 历史持久化)

**选片**:片 3(= plan 26 挂账的切片 5)。开工时用户定**统一落所有子 agent**(同步并行 task + 异步 background 一视同仁),依据是回源发现 cc/codex 都无差别对所有子 agent 落盘,且独立子文件能看到子 agent 的**完整工具调用过程**(父 tool_result 只有 final_text)。用户基调延续 plan 26:"要最合理的方案"。

**回源回填**(两个 Explore agent 真读 cc + codex,file:line;修正 HANDOFF/refs 的二手冲突):
- **收敛的必然解**(照抄):① 每个子 agent 独立 rollout 文件,不塞进父文件线性流(cc `subagents/agent-<id>.jsonl`、codex `rollout-{ts}-{child_id}.jsonl`);② 父子关联记在**会话文件里的元数据**(cc 每消息 `sessionId/agentId/isSidechain`;codex 首行 `SessionMeta.parent_thread_id/source`);③ 子会话可单独 resume/查看;④ 默认不进顶层列表/resume 选择器(cc 子目录隐藏 + isSidechain 过滤;codex source 过滤);⑤ 实时逐条追加;⑥ interrupt 不删文件。
- **修正冲突**:HANDOFF 记 cc "独立子文件"、refs/README:37 记 cc "存树不存链同文件 isSidechain"——回源坐实**都对但各说一半**:cc 主文件是 uuid/parentUuid 树 + isSidechain=false;子 agent 消息按 `isSidechain && agentId` **物理分流**到 `subagents/agent-<id>.jsonl`(`sessionStorage.ts:1251-1255`),两说不矛盾。
- **分歧 → kloop 取舍**(平铺范式一致 + 最小,不预抽象,教训 16/17):平铺(codex 派,不抄 cc 子目录——kloop 无目录嵌套范式)+ 首行 `subagent_of` 元数据(对应 codex SessionMeta.parent_thread_id,但做到**行级**——kloop 单文件+行 id 现成的红利,比两家 session 级更精确);**不抄** codex SQLite `thread_spawn_edges`(kloop 无 state DB,扫首行够,会话量小)、cc 每消息带 agentId(子文件文件级归属够)、cc 的 `agent-<id>.meta.json` 恢复受限环境(kloop resume 子会话当通用 agent,agent_type 的 system/工具限制不随 resume 还原,记取舍)。

**实现**(磁盘 schema 往上接线):
- `rollout.rs`:`LineMeta` 加 `subagent_of: Option<String>`(仅首行,serde skip_if None,前向兼容);`Rollout::new_subagent(path, subagent_of)` + `next_meta` 首行填、其余 None;`last_id()` getter(父取触发行 id);`SessionOrigin{Fork,SubAgent}` + `session_origin`(读首行,subagent_of 优先于 parent)+ `is_subagent_session`(resume 过滤);fork 的 remeta/resume 补 `subagent_of: None`(fork 是独立分支,血缘是 parent 指针不是 subagent)。
- `config.rs`:`Config` 加 `sessions_dir: PathBuf`(子落盘要用,随 clone 继承);`history.rs`:`History::rollout_last_id()`;`tools/mod.rs`:`ToolCtx` 加 `parent_rollout_id`;`agent.rs`:`turn_rounds` 构造 ToolCtx 时填 `history.rollout_last_id()`(此时父已 record 含 task tool_use 的 assistant 行,正是子的血缘点)。
- `tools/task.rs`:抽 `sub_history(cfg, agent, subagent_of)`——父有持久会话(`subagent_of` Some 且 `session_id` 非空)时 attach `Rollout::new_subagent`,子文件 `{父id}-{agent-N}`;两条 spawn 路径(同步 handle + 异步 spawn_background)共用;spawn_background 返回文本加 `child_session_note`(子会话 id,父←→子双向可跳,cc 派)。mock/无 session 降级内存(同现状)。
- `cli/main.rs`:`session_line` 用 `session_origin` 标 `[forked from …]` / `[sub-agent of …]`;`resumable_sessions` 过滤子会话(Continue/pick_session 用,`--list-sessions` 仍列全);`server/lib.rs`:`thread/list` 同样过滤子会话(Config.sessions_dir 走 config_from_env 工厂,已自动带)。

**测试**(394,+6):rollout(首行 subagent_of 往返、session_origin 区分 fork/subagent/fresh、fork 子会话变 Fork 不再是 SubAgent)、task(同步子落盘到 `{父id}-agent-N` + 首行 subagent_of 指回父触发行 + 完整转录 + 归为 sub-agent、background 落盘 + 返回文本带子会话 id、无 session 不落不提)。

**真 key 验收**(anthropic sonnet-5,`--plain --yolo`):模型派 agent-1 跑 `echo SUBAGENT_RAN_OK`;落盘坐实——`20260714-080150-agent-1.jsonl` 首行 `subagent_of=20260714-080150#2`(父 assistant tool_use 行)、`parent=None`、完整 4 消息转录(user→tool_use→tool_result→text);`--list-sessions` 两个都显示、子标 `[sub-agent of 20260714-080150#2]`;`--resume` picker 只给父。**即 plan 17 片 3 "子会话落盘 + parent 链 + `--list-sessions` 标从属" 的完整闭环。**

**提交**:a2bd470(fmt/clippy/test 全绿,394 测试)。

## ✅ 完成记录(2026-07-10,切片 2:自定义 agent 类型)

**选片**:片 2。文件形态开工时问用户,用户选 **A(`config.toml` `[agents.<name>]` 表)**——理由:kloop 一贯把配置收在 config.toml,B 的 `.md` frontmatter 要引 YAML 依赖或手写解析(违教训 8/9)。其余按 cc 形态直接定:system **完全替换**不拼接、model 省略继承父、tools 省略继承全集、未知类型报错列可用清单、清单进 task description、仍深度 1。

**实现**:
- `core/src/agents.rs`:`AgentType { name, description, system, model, tools }`(后三者 Option)+ `lookup`(未知报错列可用)+ `tool_available`(白名单门,`read_offloaded` 恒真——基建例外,否则受限子 agent 读不回自己 offload 的大输出被卡)+ `agent_types_hint`(列表进 task description,config 派生故 session 稳定、缓存友好)。
- `Config` 加 `agent_types: Arc<Vec<AgentType>>`(注册表,随 clone 继承)+ `tool_allowlist: Option<Arc<HashSet<String>>>`(仅受限子 agent 设,主 agent 恒 None)。
- task 加 `agent_type` 参数:`lookup` → 命中则 sub_cfg 覆盖 system/model、tools 设 `tool_allowlist`;label 带 `[type]` 前缀。
- 两处工具门:`turn_rounds`(agent.rs)按 allowlist `retain` 过滤 defs + depth-0 时把 hint 追加进 task def description;`run_one`(tools/mod.rs)入口拒非白名单调用(在 locked/hooks/权限**之前**——capability 先于一切;防模型幻觉出被过滤掉的工具名)。
- provider `MockRequest` 加 `model` 字段(录制请求模型,测 override;录制器本就该记请求的模型)。

**测试**(298 个,+7):agents 单元(lookup 命中/空/未知列清单、tool_available 白名单 + read_offloaded 例外、hint 列表/空)、task 路由(agent_type 把 system/model/过滤后 tools 送进录制请求、未知类型 is_error 列可用)、dispatch 白名单拒绝(bash 被拒、grep 放行、read_offloaded 恒放行)、cli `[agents.<name>]` 解析(字段往返 + 7 种畸形拒绝:缺 description、类型错、tools 非数组/非串、未知键、[agents] 非表)。

**真 key 验收**:双轨 `--plain --yolo`。anthropic:派 researcher 子 agent(model=haiku override 确实进请求——代理无 haiku 返 503 点名,反证 override 生效;换 sonnet-5 完整闭环)自报"只有 grep/glob/read_file/read_offloaded、无 bash",grep 命中目标文件。gpt-5.4-mini:searcher 类型(model 省略继承 gpt)路由 + 工具限制同样生效,只报 grep/glob/read_file。三项 override(system/model/tools)全部端到端证实。

**提交**:8a65327(fmt/clippy/test 全绿,298 个测试)。

## ✅ 完成记录(2026-07-10,切片 1+4)

**选片**:片 1(并行 task,cc 同步形态)+ 片 4(UI 呈现)。开工前对 cc(AgentTool 全链路)与 codex(multi_agents_v2)各做了一轮回源深调,关键事实沉淀在下方"回源调研结论"节。

**实现**:
- 并行:`is_concurrency_safe` 对 `task` 恒 true(cc 同款硬编码),连续 task 调用进现有并发批(join_all,无上限——与现有 bash 只读批一致;cc 的上限是 10,kloop 暂不设,真实模型很少一次发这么多)。结果按 tool_use_id 配对回请求序;单个失败(坏参数/出错/panic)只是自己的 is_error tool_result。工具描述追加"连续 task 并行"提示。
- 标签:`Config.agent_label`("" = 主 agent;task 用进程级全局 `AGENT_SEQ` 发 `agent-N`,教训 2 同款),随 sub_cfg 克隆传播。
- Ui trait:`tool_start`/`tool_end` 加 `agent` 参数;新增 `agent_start(agent, task_preview)`/`agent_end(agent, ok)`(默认实现退化为 note,plain 零改动)。task_tool 保证 start/end 严格配对(panic 分支也 end)。
- TUI:`Cell::Agent` 每个子 agent 一行活动行(cc AgentProgressLine 形态)——子 agent 的工具调用折叠为计数 + 最近调用预览,不再混入主流;结束折叠成 `✓ agent-1 <task> (N tool uses)`;interrupt 掉的 Running 行在 TurnEnded 补成 ✗(task future 被 drop 时 agent_end 不会来)。
- server:新增 `agent/started`/`agent/completed` 通知;子 agent 的 `tool/started`/`tool/completed` 带 `"agent"` 字段,主 agent 的形状逐字节不变。
- plain:`CliApprover` 加 tokio Mutex——并行子 agent 并发询问时一次只有一个提示占终端。

**测试**(266 个,+7):并行契约(文件屏障证真并发——两个子 agent 的 bash 互等对方 touch 的文件,串行必超 3s;每个 start 配对成功 end 且标签互异)、失败隔离 + 请求序配对、task_preview 截断、TUI 事件契约(agent 字段 + AgentStart/End 全序)、App 折叠(两 agent 交错事件各归各行)、TurnEnded 补 ✗、渲染三态、server 子 agent 通知契约(agent 字段有无)。

**真 key 验收**:双轨 `--plain` 各一次"一次响应发两个并行 task"。sonnet-5:两 agent 均 started 后才各自跑 bash、结束,汇总正确。gpt-5.4-mini:同样并行派发(它自发给 task 加了 max_rounds:1),`date` 触发审批弹出干净(mutex 生效),EOF deny 后子 agent 走轮限收尾——恢复语义符合预期。

**提交**:f2944c5(fmt/clippy/test 全绿,266 个测试)。

**挂账(全部已清)**:~~片 2(自定义 agent 类型)~~ **✅**、~~片 3(历史持久化)~~ **✅ 2026-07-14**、~~片 5(hook agent 字段 + SubagentStart/Stop)~~ **✅ 2026-07-14(Option B,见顶部完成记录)**、~~片 6(异步派发 + mailbox 回灌)~~ **✅ plan 26 承接**。**plan 17 五片全清**。残留小账(非切片,等痛感):并发批无上限(cc 是 10)、子 agent 的 note(重试/压缩提示)不带标签混在主 note 流、subagent 事件的 agent_type matcher。

## 回源调研结论(2026-07-10,两家对齐)

**收敛的"必然解"**(四样):① agentId 贯穿事件流(cc progress data;codex 子线程独立事件 + 父时间线折叠摘要,双通道);② 子会话独立落盘、与父关联(cc `<session>/subagents/agent-<id>.jsonl` isSidechain,resume 父会话不重放子过程;codex 每子线程独立 rollout + agent_graph 父子边);③ agent 类型 = 命名定义 + model/prompt/工具 override(cc frontmatter:name/description/tools/model/effort/maxTurns 等,system prompt **完全替换**不拼接,模型默认 inherit 且裸别名同 tier 沿用父的精确串,未知类型报错并列可用清单,清单进 task 工具 description;codex role = config 分层覆盖);④ SubagentStart/Stop 挂点 + pre/post tool 带 agent_id(两家字段几乎一致,Stop 额外带 transcript_path)。

**并行形态**:cc Task `isConcurrencySafe` 硬编码 true(AgentTool.tsx:1467),连续调用进并发批,上限 10(`CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY`),失败转 is_error 不拖批(AbortError 例外整批打断);cc 深度控制不是计数器,是把 Agent 工具从子 agent 工具池里删掉。codex 是全异步:spawn 非阻塞返回 canonical task_name(非 thread-id;V1 才返 agent_id)→ wait(min 10s/default 30s/max 1h,可被 steer 打断,只回摘要)→ mailbox 存全文;并发默认 3(max_concurrent_threads 4 - root);V2 无深度门禁。

**turn 中途回灌**(片 6 的分水岭,两家收敛):只在 step 边界注入,绝不插进在途请求。cc:task-notification 入队,工具循环里主线程只 drain 自己的,转 attachment 进本 turn 的 toolResults;turn 之间则作为 user 消息喂下一 turn。codex:mailbox delivery phase 闸门——tool-call 后 accept、final answer 后 defer 到下一 turn;子终态走 `forward_child_completion_to_parent`(V2 是子 session 终态事件回调,不是 V1 的 watcher),`SubagentAutowake` 触发父空闲时起新 turn;Interrupted 不是终态、父收不到通知。失败语义:codex 把子 agent 错误截 900 tokens + "换个任务再派"引导回父。

## 现状(升级的起点)

task 工具:深度限 1、`is_concurrency_safe` 标死 false(连续 task 调用串行跑)、子 agent 与主 agent 同 system/同模型/全量工具、历史不持久化(resume 后只剩 tool result,过程丢失)、产出只有 final_text。继承已对:permissions/hooks/session_id/tool_sources 都走 Config 克隆。

另:plan 14 第三片已落 `BackgroundShells`(`tools/bash.rs`):id/状态/输出落盘/查询(block 语义)/进程组终止/monitor task 生命周期——如果本 plan 把 task 做成异步派发,它需要的正是同一套东西。

## 候选切片(开工时和用户定选哪几片、什么顺序)

1. **并行 task**:连续 task 调用并发跑(cc 形态)。子 agent 本来就是独立 tokio task,缺的是把 task 从"永不并发"改成可并发批。要解决:并发写文件的风险怎么表达(权限门是共享的、会各自询问——询问并发弹出的 UI 排队 TUI 已有,plain 会乱,开工时定);UI 上多个子 agent 的 tool 行混流怎么区分(见片 4)。
2. **自定义 agent 类型**:config 定义命名 agent(独立 system prompt、工具子集白名单、模型 override),task 工具加 `agent_type` 参数,模型可见的类型清单进 task 的 description。文件形态(`.kloop/agents/*.md` frontmatter 还是 config.toml 表)开工时问用户。便宜模型跑搜索型子任务是主要收益。
3. **子 agent 历史持久化**:子会话落 `.kloop/sessions/`,信封 parent 指回父会话该 turn 的行(plan 7b/18 的链地基复用);`--list-sessions` 里标出从属关系。价值:审计 + 断点续查子 agent 干了什么。
4. **子 agent 的 UI 呈现**:TUI 里子 agent 的 tool 行折叠成一组(带前缀/缩进),turn 结束折叠为一行摘要;server 通知加 `agentId`。现状是子 agent 的 tool_start/tool_end 直接混在主流里。
5. **hook 挂点**:pre/post_tool 事件带 depth 或 agent 字段;SubagentStart/Stop 独立挂点(对齐 codex 命名)开工时定要不要。
6. **统一任务注册表(方向,开工时定做不做)**:cc 已把后台 shell/异步 agent/远程会话收敛成一套 Task 框架(统一 id、status、output 文件、TaskOutput/TaskStop、`<task-notification>`)。kloop 的对应路径:若选片 1 且做成异步派发(fire-and-forget + 查询),把 `BackgroundShells` 泛化成 `Tasks` 注册表(id 前缀 `bg-`/`agent-`,bash_output/kill_bash 泛化或加别名),那是第二个客户出现、有真实需求驱动的时刻——不要提前抽象(2026-07-10 拍板:不单开 plan)。真正的分水岭是 **turn 中途通知通道**(任务完成时主动喂给模型,免轮询):要动主循环采样时机,是独立架构决定,开工时问用户;没有它,统一注册表的收益只剩命名整齐。codex 的最小先例(2026-07-10 调研,见 refs/README.md):异步 spawn 返回 agent_id + `wait` 工具(mailbox 更新摘要,可被新用户输入打断)+ 子 agent 终态投递父 mailbox 的 turn 中途回灌——只对子 agent 做通知、不建全局框架,是介于"纯轮询"和"cc 全量 task-notification"之间的中间形态;它的 role/complexity/CSV fan-out 全家桶不抄。
   - **进展(2026-07-13,plan 22)**:这个"turn 中途通知通道"的**注入侧机制已由 plan 22 交付**——step 边界注入队列(`Config.inbox`,round 边界 drain + 收尾兜底,成 user 消息),首个客户是用户 steering。子 agent 回灌复用同一队列,但**它的前置(异步派发引擎:spawn 立即返回 + 完成侦测 + autowake + 注册表泛化)仍挂账**;plan 22 回源确认了两家的收敛"子 agent 回灌整套依赖异步派发",故留独立 plan(建议 plan 26)。届时:子终态回调 push 一条 framing 过的摘要进父的 `inbox` 即可,drain 侧零改动。

## 不做(维持现状)

深度 >1 的递归(cc 也限 1,复杂度/失控风险不值);子 agent 独立权限策略(共享父的 Permissions 是对的——人的最后一道不该因为进了子 agent 变松)。

## 测试

并行片:两个 task 并发完成、结果按请求序配对、单个失败不拖垮批;类型片:agent_type 路由到对应 system/工具子集/模型(mock provider 断言请求里的 model 与 tools)、未知类型报错;持久化片:子会话文件 parent 链、resume 父会话不误吞子文件;UI 片:事件契约测试加 agentId。

## 完成标准

fmt/clippy/test 全绿;真 key 手工验收按所选切片定(至少:一次真实并行 task 或一次 agent_type 派发闭环);README、HANDOFF 更新;未选的切片在本文件记挂账。
