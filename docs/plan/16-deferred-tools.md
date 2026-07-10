# Plan 16 — deferred 工具 + tool_search ✅(提交 c527f63)

> 一个会话完成。开工前先读 docs/plan/HANDOFF.md。参考:refs/README.md 调研结论 4——这是 cc 和 codex **独立收敛的必然解**,直接抄形态不必发明;具体机制两边源码回源核对(教训 11)。

## 目标

工具一多(MCP 接几个 server 就到了;现在 >30 只是警告)定义就挤爆上下文。做延迟加载:模型默认只看到核心工具 + 一个 `tool_search`,其余工具的 defs 被 defer;模型搜索命中后"解锁",解锁的工具从下一轮起进 defs、可正常调用。

## 设计要点

- **defer 判定**:内置工具永不 defer;MCP 工具默认 defer 还是超阈值才 defer(以及阈值),开工时定。config 给 per-server 强制 pin 的口子(形态开工时定)。
- **模型怎么知道有哪些 deferred 工具**:cc 形态是名字清单进 system(-reminder);kloop 的注入点(system 尾部 / 首条 user 上下文)开工时定,和 plan 13 的组装缝对齐。
- **解锁的生命周期**:per-session(一次解锁整个会话可用)先做,per-turn 记为可能性。数据结构:`all_tool_defs` 现在是 turn 开头算一次——解锁要求轮与轮之间可变,挪进循环或加共享解锁集(`Arc<RwLock<HashSet>>`?)开工时定。
- **tool_search 返回什么**:匹配工具的 name + description(schema 不回,解锁后自然在 defs 里);支持 `select:name` 精确取和关键词模糊两种查询形态(抄 cc)。
- **未解锁工具被直接调用**:友好报错并提示先 tool_search(比 unknown tool 好),不解锁——解锁只能经搜索,行为要测试锁死。
- **权限不变**:defer 只影响可见性,权限判定仍走整工具名,记忆粒度不变。
- 子 agent:共享同一解锁集还是独立,开工时定(共享更省,倾向共享)。

## 测试

阈值内全量直出、不出 tool_search;超阈值 defer 生效、defs 里有 tool_search;搜索命中 → 解锁 → 下一轮 defs 含该工具且可调用;未解锁直调的报错形态;mock ToolSource 造大工具集。

## 完成标准

fmt/clippy/test 全绿;真 key 手工验收:接一个多工具 MCP server(官方 filesystem 即可,人为调低阈值),观察模型搜索→解锁→调用的闭环;README、HANDOFF 更新(>30 警告文案同步改)。

## 完成记录(2026-07-10,提交 c527f63)

开工调研回源核对了两个参考库(教训 11 又一次兑现):plan 预设的"解锁后进 defs"两家现行实现都不是——cc 旧快照走 SearchExtraTools+ExecuteExtraTool 中转(tools 数组永不变,保 prompt cache),codex 靠 Responses API 服务端 `defer_loading`(搬不到 /v1/messages);而 cc 现行 harness 已演化到第三形态:**tools 数组静态,schema 随搜索结果返回,模型直调未声明名字**。用户拍板"必须缓存友好",选第三形态:

- 阈值形态:总工具数(depth-0 视角)> `defer_threshold`(默认 30,`AGENT_DEFER_THRESHOLD` 覆盖)才 defer,内置永不 defer;阈值内行为与之前逐字节相同。
- 名单注入:复用 plan 13 的首条合成 user 消息缝,列全部 deferred 名字且**不随解锁缩减**(与 defs 数组一样会话内字节稳定)。
- 解锁 = `Config.unlocked_tools`(`Arc<RwLock<HashSet>>`)insert,只开 `run_one` 顶部的分发门(在 hooks/权限之前,锁定调用不该惊动任何人);未解锁直调报错引导 tool_search,不解锁。子 agent 经 Config clone 共享解锁集。
- **call_tool 兜底(验收中发现的必要项)**:gpt-5.4-mini 会搜索、解锁,但任何措辞下都拒绝对不在 tools 数组里的名字发 tool_use(明说"can't issue that deferred tool call from this interface");cc 旧版 ExecuteExtraTool 正是为这类模型存在的中间站。kloop 版在 dispatch_tools 入口拆包信封,权限/hooks/并发/UI 全见真名,零层侵入;history 保留模型原始信封,回放保真。
- pin 口子不做(阈值即主逃生口,配置键以后加向后兼容);per-turn 解锁、per-tool pin 记为可能性。

真 key 验收(filesystem MCP 14 工具 + 阈值 5):sonnet-5 首试 `select:` 多选 → 直调;gpt-5.4-mini 搜索 → `call_tool` 信封,均闭环。测试 259 全绿。
