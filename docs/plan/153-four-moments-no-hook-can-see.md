# Plan 153 — 四个挂点看不见的时刻

> 来源:2026-09-15,借鉴项目调研后按 macOS-only 前提重排的第五条。参考 codex 的
> `codex-rs/hooks`(9 类事件,hook 可以是 MCP tool,带 `output_spill`)与 grok 的
> 16 事件 macro 表驱动。见 `refs/README.md` 2026-09-15 节。
>
> **先读这条**:`docs/capability-report.md` 第 9 节已经盘点过这块,判定是**备选池**,
> 触发条件写的是"写复杂 hook 的痛感"。本 plan 不推翻那个判定,只是把"真要做的话做什么"
> 写下来,并把其中一笔早就立过的小账一起清掉。要不要现在动手,见第五节。

## 一、现在有六个

`core/src/hooks.rs`(776 行):`pre_turn` / `post_turn` / `pre_tool` / `post_tool` /
`subagent_start` / `subagent_stop`。子 agent 触发后两个**而不是**前两个(`agent.rs:214`)。

形态是每个事件一个方法,自己拼 payload JSON 再调 `run_event`(`hooks.rs:126` 起),
`HookDecision::Allow { context }` / `Block`,post 类事件永不 block。匹配靠 `HookDef.matcher`
(`:86`)做工具名精确匹配,超时 `DEFAULT_TIMEOUT_MS` 10s,阻断退出码 2(`:299`,cc 约定)。

加一个事件的机械成本很明确:enum variant + `name()` + `parse()` + 一个 `Hooks::xxx()` 方法 +
一处调用点。没有隐藏的注册表要同步。

## 二、看不见的四个时刻

按"dogfood 时真会写 hook 去接"排序,不是按 cc 的事件表排序:

| 新挂点 | 为什么想要 | 调用点在哪 |
|---|---|---|
| `permission_denied` | 审批被拒是唯一一个"agent 想做但没做成"的信号,现在外部完全看不到。想统计自己被什么挡住最多,只能翻转录 | `core/src/permissions.rs` 的决策出口 |
| `pre_compact` / `post_compact` | 压缩是唯一会丢信息的步骤。想在压缩前把完整转录归档一份,现在没有任何时机 | `core/src/compact.rs` |
| `session_end` | `post_turn` 不等于会话结束。想在收工时发通知、跑清理,现在没有挂点 | **见坑:出口不止一个** |

`stop_failure`/`stop_cancelled`(grok 有)先不做——kloop 的取消语义和它不是一回事,
硬抄会引入两个含义不清的事件。

## 三、顺带清一笔旧账

能力报告第 9 节第三行:**subagent 事件的 `agent_type` matcher**,标注是"plan 17 残留小账/顺手"。
现在 `HookDef.matcher` 只对工具名做精确匹配(`hooks.rs:255`),子 agent 事件没法按类型筛。
这一条和上面四个挂点改的是同一个文件的同一片区域,一起做,一次 commit。

## 四、坑

- **`session_end` 的出口不止一个**:正常退出、Ctrl-C(`plain_pty` 两个测试正测这条路)、
  致命错误、以及 server 模式下的 thread 关闭。**只挂一处会变成"有时候不触发"的挂点,
  比没有更糟**。要么找到唯一汇合点,要么明确只覆盖正常退出并写进文档。
- **`permission_denied` 会很吵**。ask 模式下每次拒绝都触发。要确认它触发的是"最终拒绝"
  而不是"每一次询问的否定分支"。
- **压缩事件不能阻断**。`pre_compact` 若允许 Block,一个坏 hook 就能让会话在上下文满了之后
  卡死。这两个都必须是 post 类语义(`HookDecision::Block` 走 `unreachable!`)。
- **payload 的向后兼容**。现有 payload 刻意保持"主 agent 的字节与 cc 一致"(`hooks.rs:138`
  的注释),新事件是 kloop 自己的,不必对齐 cc 的字段名,但要在注释里写明这是有意的,
  否则下一个人会拿 cc 的 schema 来"修"。
- 事件多了以后再考虑 grok 那种 macro 表驱动(每事件带 gate/matcher 三元组)。**六个变十个
  还不值得引入宏**,教训 131/133 是"重复到编译器该点名了才收",不是提前抽象。

## 五、开工时问用户(先问,再动手)

**现在做,还是等痛感?**

能力报告把这块判成备选池,触发条件是"写复杂 hook 的痛感"。到今天为止,仓库里没有记录
任何一次"想写个 hook 但挂不上去"的具体事件——**本 plan 的四个挂点是我从借鉴项目倒推的,
不是从你的使用里长出来的**。

诚实的建议:**只做第三节那笔旧账**(agent_type matcher,本来就立过账、成本小),
四个新挂点挂着等第一个真实需求。真到那天,需求会直接告诉你要哪一个,而不是四个都要。

## 六、非目标

- **不做 stdout 结构化 JSON 协议**(decision/updatedInput/additionalContext)。能力报告
  第 9 节第一行,独立一笔账,和加挂点无关。
- **不做 hook 并行执行**。同上。
- **不做 hook-as-MCP-tool**(codex 有)。那要把 MCP 客户端接进 hook 执行路径,是另一个量级。
- **不做 `output_spill`**(codex 有)。hook 输出溢出目前没有观察到问题。
- 不动现有六个事件的 payload 字节。

## 七、验收

**前置**:第五节的建议是只做第三节那笔旧账。以下分两段,按实际拍板的范围取。

### 若只做旧账(建议)

1. `HookDef.matcher` 能按 `agent_type` 筛子 agent 事件,有测试:两种类型的子 agent,
   matcher 指定其一,另一种不触发。
2. **现有六个事件的 payload 字节不变**——尤其主 agent 的 payload 要与改动前逐字节相同
   (`hooks.rs:138` 那条刻意保持的约束)。用整对象断言锁住。

### 若连四个新挂点一起做

3. 四个挂点各有触发测试。
4. **`session_end` 的出口全覆盖**:正常退出、Ctrl-C、致命错误、server 端 thread 关闭
   四条路各有测试;做不到就只实现正常退出,并在 `README` 与 hook 文档里**明写它不覆盖
   哪几条路**。一个"有时候不触发"的挂点比没有更糟。
5. `pre_compact` / `post_compact` **不能阻断**:构造一个返回退出码 2 的 hook,断言压缩
   照常进行(走 `unreachable!` 那条 post 语义,不是让会话卡死)。
6. `permission_denied` 触发的是最终拒绝,不是每一次询问的否定分支——有测试区分这两者。
7. 新事件的 payload 字段在注释里写明"这是 kloop 自己的,不对齐 cc",防止下一个人拿 cc
   的 schema 来"修"。

8. 仓库完成标准照旧(fmt / clippy -D warnings / test,各自取退出码)。
