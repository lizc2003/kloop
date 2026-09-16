# Plan 153 — 四个挂点看不见的时刻

> **2026-09-16:只做了第三节那笔旧账**(agent_type matcher),一次提交,提交 SHA 以本条
> 所在提交为准。**第二节那四个新挂点没做,仍然挂着**,等第一个真实需求——它们是从借鉴
> 项目倒推的,不是从使用里长出来的(第五节的建议,用户拍板照办)。验收按第七节
> 「若只做旧账」那两条,外加用户加的一条(配了 matcher 却被忽略的那个 case 现在必须
> 真的不触发)。

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

## 三、顺带清一笔旧账 ✅(2026-09-16)

能力报告第 9 节第三行:**subagent 事件的 `agent_type` matcher**,标注是"plan 17 残留小账/顺手"。
现在 `HookDef.matcher` 只对工具名做精确匹配(`hooks.rs:255`),子 agent 事件没法按类型筛。
这一条和上面四个挂点改的是同一个文件的同一片区域,一起做,一次 commit。

### ✅ 做完了什么

**一处开工前的更正**:plan(和开工时的描述)都把这条说成"配了 matcher 既不报错也不筛"。
不对——`startup.rs` 的 `load_hooks` 有一道 `if !event.is_tool_event() { bail! }`,`turnmatcher`
那条测试正锁着它。**从配置文件这条路,子 agent hook 配了 matcher 会直接报错,装都装不进
`Hooks`**;`run_event` 那个 fail-open 的 `if let (Some(matcher), Some(tool))` 只在有人直接构造
`HookDef`(库的 embedder、或测试)时才够得着。所以这笔账是"配不上",不是"配了不生效",
而且**放开配置校验本身也是这次的工作量之一**,plan 没写。

三段改动:

1. **类型从目录送到挂点**。`agent_type` 只活在 live Agent 目录里(`register_child` 写进
   `LiveEntry`),挂点上原来只有 `agent` 标签(`agent-N`,一个进程级自增的**启动序号**,
   按它筛等于按启动顺序筛)。加 `LiveAgentDirectory::agent_type` → `LocalAgentContext::agent_type`
   → `Config::agent_type`,`run_turn_with_options` 在子 agent 分支读一次,两个挂点共用。
2. **matcher 按它筛**。`run_event` 的第二个参数从 `tool_name` 改名 `subject`;
   `HookEvent::is_tool_event` 换成 `matcher_subject() -> Option<&'static str>`(工具名 /
   agent 类型 / 无),配置校验和错误文案都走它。
3. **无类型的子 agent 报 `DEFAULT_AGENT_TYPE = "default"`**,见下。

### 无类型子 agent 怎么办:四家参考 2:2,拍了后一条

开工前照教训 143 把四家都读了(codewhale 没有子 agent hook):

| 参考 | 无类型时送进 matcher 的值 | 配了 matcher 的 hook |
|---|---|---|
| **cc** | `agentType ?? ''`(`hooks.ts:3833`) | **触发** —— 空串 falsy,`matchQuery ? filter : all`(`:1817`)整个筛选被跳过 |
| **grok** | `None` —— `match_value()` 结尾 `.filter(\|v\| !v.is_empty())`(`event.rs:610`) | **触发** —— `matcher_allows` 的 `_ => true`,注释明写 fail-open,还有测试 `subagent_match_value_is_none_when_type_empty` |
| **codex** | **不可能为空** —— `agent_role.unwrap_or(DEFAULT_ROLE_NAME)`(`hook_runtime.rs:1048`),值是 `"default"` | **不触发**,除非 matcher 写 `default`/`*` |
| **dsh** | **不可能为空** —— 常量 `SUBAGENT_TYPE = 'general-purpose'`(`hooks-claude-code/src/index.ts:304`) | **不触发**,注释明写具体 kind 如 `code-reviewer` 不 fire |

分歧不在"触发还是跳过",在**要不要让"没类型"这件事存在**。拍了 codex/dsh 那条:给一个
可被点名的默认类型名。理由:(a) 这笔账的起因就是"配了却不按预期生效",fail-open 会留一个
小号同类陷阱(配 `matcher = "reviewer"`,一个无类型子 agent 照样触发);(b) `hooks.rs` 顶部
那条 fail-open 讲的是"hook 脚本坏了不许 brick agent",说的是**执行故障**,matcher 筛不中不是
故障,不该共用那条理由;(c) 默认名能表达"只筛无类型那批",fail-open 表达不了;(d) 名字进
payload,于是它是自我说明的,不是只存在于代码里的魔法串。

### 验收(第七节「若只做旧账」+ 用户加的一条)

1. ✅ **按类型筛**:`subagent_matcher_filters_by_agent_type` —— matcher `reviewer`,
   `reviewer` 触发、`explorer` 不触发;`matcher = "default"` 选中无类型那批、不选中有类型的。
2. ✅ **配了 matcher 却被忽略的那个 case 现在真的不触发**:同一个测试里,无类型子 agent
   遇到 `matcher = "reviewer"` 不触发(旧实现会触发)。
3. ✅ **payload 字节**:`every_event_payload_is_pinned` 用整对象断言把六个事件全钉住,
   四个主 agent 事件与改动前逐字节相同(`hooks.rs:138` 那条约束)。
4. ✅ **端到端**:`subagent_hooks_carry_the_registered_agent_type` —— 真 `register_child(Some("reviewer"))`
   + 真 `run_turn`,matcher 命中的 hook 拿到 `agent_type: "reviewer"`,另一类型的 hook 不跑。
   负对照验过:把 `agent.rs` 那条线断掉(传 `None`)这条测试变红。
5. ✅ 配置侧:`load_hooks_full_round_trip` 收了一条 `subagent_stop` + `matcher`;
   `turnmatcher`(`pre_turn` + matcher)仍然拒。
6. ✅ fmt / clippy -D warnings / test 各自单独取退出码,均为 0。

**payload 变了一处(有意)**:两个子 agent 事件新增 `agent_type` 字段。第六节"不动现有六个
事件的 payload 字节"那条写在"加四个新挂点"的语境里,本次只做旧账;而没有这个字段,
`default` 这个名字就只存在于文档里,没配 matcher 的 hook 也分不出类型——四家参考都把它放在
payload 里。主 agent 的字节一个没动。

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
