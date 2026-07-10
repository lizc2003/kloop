# Plan 17 — 子 agent 升级

> 体量偏大,开工时选片,可能不止一个会话。开工前先读 docs/plan/HANDOFF.md。参考:cc 的 agents 机制(`.claude/agents/*.md` frontmatter:独立 system prompt、工具白名单、模型 override;并行派发)、codex 的 subagent(SubagentStart/Stop 挂点、SubagentHookContext)。回源核对(教训 11)。

## 现状(升级的起点)

task 工具:深度限 1、`is_concurrency_safe` 标死 false(连续 task 调用串行跑)、子 agent 与主 agent 同 system/同模型/全量工具、历史不持久化(resume 后只剩 tool result,过程丢失)、产出只有 final_text。继承已对:permissions/hooks/session_id/tool_sources 都走 Config 克隆。

另:plan 14 第三片已落 `BackgroundShells`(`tools/bash.rs`):id/状态/输出落盘/查询(block 语义)/进程组终止/monitor task 生命周期——如果本 plan 把 task 做成异步派发,它需要的正是同一套东西。

## 候选切片(开工时和用户定选哪几片、什么顺序)

1. **并行 task**:连续 task 调用并发跑(cc 形态)。子 agent 本来就是独立 tokio task,缺的是把 task 从"永不并发"改成可并发批。要解决:并发写文件的风险怎么表达(权限门是共享的、会各自询问——询问并发弹出的 UI 排队 TUI 已有,plain 会乱,开工时定);UI 上多个子 agent 的 tool 行混流怎么区分(见片 4)。
2. **自定义 agent 类型**:config 定义命名 agent(独立 system prompt、工具子集白名单、模型 override),task 工具加 `agent_type` 参数,模型可见的类型清单进 task 的 description。文件形态(`.kloop/agents/*.md` frontmatter 还是 config.toml 表)开工时问用户。便宜模型跑搜索型子任务是主要收益。
3. **子 agent 历史持久化**:子会话落 `.kloop/sessions/`,信封 parent 指回父会话该 turn 的行(plan 7b/18 的链地基复用);`--list-sessions` 里标出从属关系。价值:审计 + 断点续查子 agent 干了什么。
4. **子 agent 的 UI 呈现**:TUI 里子 agent 的 tool 行折叠成一组(带前缀/缩进),turn 结束折叠为一行摘要;server 通知加 `agentId`。现状是子 agent 的 tool_start/tool_end 直接混在主流里。
5. **hook 挂点**:pre/post_tool 事件带 depth 或 agent 字段;SubagentStart/Stop 独立挂点(对齐 codex 命名)开工时定要不要。
6. **统一任务注册表(方向,开工时定做不做)**:cc 已把后台 shell/异步 agent/远程会话收敛成一套 Task 框架(统一 id、status、output 文件、TaskOutput/TaskStop、`<task-notification>`)。kloop 的对应路径:若选片 1 且做成异步派发(fire-and-forget + 查询),把 `BackgroundShells` 泛化成 `Tasks` 注册表(id 前缀 `bg-`/`agent-`,bash_output/kill_bash 泛化或加别名),那是第二个客户出现、有真实需求驱动的时刻——不要提前抽象(2026-07-10 拍板:不单开 plan)。真正的分水岭是 **turn 中途通知通道**(任务完成时主动喂给模型,免轮询):要动主循环采样时机,是独立架构决定,开工时问用户;没有它,统一注册表的收益只剩命名整齐。

## 不做(维持现状)

深度 >1 的递归(cc 也限 1,复杂度/失控风险不值);子 agent 独立权限策略(共享父的 Permissions 是对的——人的最后一道不该因为进了子 agent 变松)。

## 测试

并行片:两个 task 并发完成、结果按请求序配对、单个失败不拖垮批;类型片:agent_type 路由到对应 system/工具子集/模型(mock provider 断言请求里的 model 与 tools)、未知类型报错;持久化片:子会话文件 parent 链、resume 父会话不误吞子文件;UI 片:事件契约测试加 agentId。

## 完成标准

fmt/clippy/test 全绿;真 key 手工验收按所选切片定(至少:一次真实并行 task 或一次 agent_type 派发闭环);README、HANDOFF 更新;未选的切片在本文件记挂账。
