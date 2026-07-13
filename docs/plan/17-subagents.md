# Plan 17 — 子 agent 升级

> 体量偏大,开工时选片,可能不止一个会话。开工前先读 docs/plan/HANDOFF.md。参考:cc 的 agents 机制(`.claude/agents/*.md` frontmatter:独立 system prompt、工具白名单、模型 override;并行派发)、codex 的 subagent(SubagentStart/Stop 挂点、SubagentHookContext)。回源核对(教训 11)。

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

**挂账(未选切片,原样保留在下方候选)**:片 2(自定义 agent 类型——本次调研已备齐 cc frontmatter 字段表/路由/报错形态,见回源结论)、片 3(历史持久化,先做 plan 18)、片 5(hook 事件带 agent 字段 + SubagentStart/Stop)、片 6(异步派发 + mailbox 回灌)。另:并发批无上限(cc 是 10)、子 agent 的 note(重试/压缩提示)不带标签混在主 note 流——都等有痛感再修。

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
