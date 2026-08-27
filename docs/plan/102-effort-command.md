# Plan 102 — `/effort`:跨轨的会话内 reasoning effort 旋钮

> 状态:✅ 已完成(提交见「完成记录」)
>
> 依赖:Plan 84(provider contracts)、Plan 92(session provider switching / `SessionProviderState`)、Plan 89(reasoning continuity)。
>
> 挂账来源:Plan 28「`effort` 挂账:是 provider 层参数」、Plan 53「per-agent `effort` 尚无 session-local provider seam」。

## Context

用户要 `/effort` 命令,且**要能在对话中改**。

现状:

- `effort` 只存在于 responses 轨,且是**启动期烘焙**——`Profile.effort: Option<String>` 进 `Provider::OpenAiResponses { effort }`,由 catalog 的 factory + `OnceLock` 缓存住,会话中无法更改。
- Anthropic 轨完全没有 effort(只有 `ThinkingMode`);chat 轨也没有。
- `~/.kloop/config.toml` 根键 `model_reasoning_effort` 被 `validate_root` 接受但**从没读过**(codex 兼容遗留)。
- `model_providers.<id>.effort` 目前硬性只允许 responses wire。

三条轨其实都有 effort 参数(2026-08 核对):

| 轨 | 请求字段 | 取值 |
| --- | --- | --- |
| Anthropic Messages | `output_config: {"effort": …}`(GA,无 beta 头,默认 `high`) | `low` / `medium` / `high` / `xhigh` / `max` |
| OpenAI Responses | `reasoning: {"effort": …, "summary": "auto"}` | `minimal` / `low` / `medium` / `high` |
| OpenAI Chat Completions | `reasoning_effort: …` | `minimal` / `low` / `medium` / `high` |

## 已拍板设计

### 1. 词表:kloop 自有 bounded 枚举 + 每轨接受子集

`kloop-protocol` 新增 `ReasoningEffort { Minimal, Low, Medium, High, XHigh, Max }`(serde/`FromStr` 都是小写单词,`xhigh` 不带下划线)。每条轨声明自己接受的子集:

```
ProviderApiFamily::accepted_efforts(self) -> &'static [ReasoningEffort]
  AnthropicMessages        → [Low, Medium, High, XHigh, Max]
  OpenAiResponses          → [Minimal, Low, Medium, High]
  OpenAiChatCompletions    → [Minimal, Low, Medium, High]
  Mock                     → 全部
```

**为什么 bounded 而不是透传字符串**:kloop 一贯对自己暴露的词表用 bounded 类型,好处是 `/effort hgih` 当场报错并列出本轨接受集,而不是等到下一次模型调用吃 400。代价是上游加了新档位要改这一张表——表上写 doc 注明「升级 provider 时逐条复核」。

**不做隐式降级**:在 responses 轨上打 `/effort max` 直接报错并列出 `minimal/low/medium/high`,不悄悄降成 `high`。

### 2. Seam:effort 从 Provider 构造挪到每次请求

- `Provider::OpenAiResponses` **删掉** `effort` 字段(唯一真源移到会话态,避免两处 default 打架)。
- `Provider::stream_attempt` 增参 `effort: Option<ReasoningEffort>`,按轨渲染成上表的字段;`None` 一律不发字段(保持今天的行为)。
- `MockRequest` 记录 `effort`,让 mock 测试能整对象断言。

### 3. 会话态:`SessionProviderState` 持有 effort,随 frozen route 流到 attempt

- `ProviderCatalogEntry` 增 `default_effort`(来自 profile),catalog 内部保存。
- `SessionState` 增 `effort: Option<ReasoningEffort>`,`new` / `from_route` / `from_timeline` / `restore` 都从 catalog 的当前 provider default 播种。
- 新 API:`SessionProviderState::effort()`、`set_effort(Option<ReasoningEffort>) -> Result<(), EffortError>`(按 **当前** 轨的接受集校验)。
- `FrozenProviderRoute` / `FrozenProviderAttempt` 携带 effort;`child_route`(子 agent、compact)原样继承。三个前端本来就在 `route_changed` 后 re-freeze `cfg`,所以改完立刻对下一轮生效。
- `ActiveProviderRoute` 增 `effort` 字段:`/provider` 输出、TUI 页脚、server `thread/provider/changed` 都能显示。

**换 provider 时的规则**:会话 effort 是**粘的**;只有当它不被新轨接受时,才回落到新 provider 的配置 default(仍不接受则清空),并在切换回执文案里明说回落了。理由:不允许存在「当前路由不接受的 effort」这种无效态,同时不无故丢用户显式设的值。

### 4. 命令

`crates/core/src/commands/effort.rs`,进 `BUILTINS`(于是 `/help` 和 TUI 的 `/` 菜单自动带上):

- `/effort` —— 显示当前 effort、当前轨接受集、用法。
- `/effort <level>` —— 设置;不被本轨接受则报错并列出接受集。
- `/effort off` —— 清空(不发字段,由 provider 自己的默认决定)。

`SlashResult.provider_changed` 更名 `route_changed`(effort 变化也要三个前端 re-freeze `cfg`,名字得诚实);`/effort` 成功改动时置位。

### 5. 配置优先级

`KLOOP_EFFORT` > 根键 `model_reasoning_effort`(**从接受但忽略变成真读**)> `model_providers.<id>.effort` > 不发。根键/环境变量只作用于**被选中的** provider,与既有 `selected_base` / `selected_credential` 同形。`model_providers.<id>.effort` 解除「只允许 responses」的限制,改为按本 profile 的 wire 校验取值。

## 关键文件

- `crates/protocol/src/lib.rs` — `ReasoningEffort` + `ProviderApiFamily::accepted_efforts` + `ActiveProviderRoute.effort`。
- `crates/provider/src/lib.rs` — 删 `OpenAiResponses.effort`;`stream_attempt` 增参;三轨渲染;`MockRequest.effort`。
- `crates/core/src/provider_route.rs` — catalog `default_effort`;`SessionState.effort`;`effort()` / `set_effort`;frozen route/attempt 携带;换轨回落。
- `crates/core/src/commands/effort.rs`(新)+ `commands/mod.rs`(注册 + `route_changed` 更名)。
- `crates/core/src/agent/sampling.rs`、`crates/core/src/compact.rs` — `stream_attempt` 传 attempt 的 effort。
- `crates/cli/src/provider_config.rs` — profile effort 解析成 `ReasoningEffort` 并按 wire 校验;`selected_effort`(env > 根键 > profile);`ResolvedProviderSettings` 透出。
- `crates/cli/src/user_config.rs` — 根键 `model_reasoning_effort` 真读出来。
- `crates/tui/src/lib.rs` / `crates/server/src/lib.rs` / `crates/cli/src/main.rs` — `route_changed` 更名。
- `crates/tui/src/render.rs` — 页脚 `provider / model · r1 · high · 12% ctx`(effort 为 None 时不加这一段)。
- `kloop/README.md` — provider 配置段、slash 命令段、wire 段同步。

## 非目标

- **effort 不进 `ProviderRouteReceipt` 时间线,不跨 resume 持久化**。revision/receipt 是路由身份(reasoning replay 靠它),effort 不影响 replay 兼容性;塞进去会牵动 `validate_timeline` 的 revision 单调约束和 wire。resume 后回落到配置 default——这条写进 README。
- 不建模「某模型在某 effort 下不接受 thinking disabled」这类模型级规则(Opus 5 在 `xhigh`/`max` 下拒绝 `thinking: disabled`)。provider 的 400 照旧原样透出。
- 不做 per-agent-type / per-skill 的 effort 覆盖(plan 17/28 挂账继续挂着)。
- 不动 `ThinkingMode`、不动 cache 断点、不动 reasoning 续传。

## 需要如实说明的边界

- 改 effort 会打断 Anthropic 的 messages prompt cache(前缀变了),下一轮 cache 命中率归零——README 记一句。
- chat 轨的 `reasoning_effort` 只对推理模型有效,发给非推理模型会 400;默认不发字段,只有用户显式设了才发。

## 测试 / 验证

- protocol:`ReasoningEffort` 的 `FromStr`/`as_str` 往返、大小写不敏感、未知词报错;每轨接受集整对象断言。
- provider:三轨 body 渲染(Anthropic `output_config`、responses `reasoning`、chat `reasoning_effort`),`None` 时字段不存在。
- core:`set_effort` 被当前轨拒绝;换 provider 后不兼容 effort 回落;`child_route` 继承;frozen attempt 带 effort 到 `stream_attempt`(mock 记录断言)。
- commands:`/effort` 三种形态的输出;`route_changed` 置位。
- cli:配置优先级(env > 根键 > profile)、按 wire 校验取值、非 responses 轨也能配 effort。
- `cargo fmt` + `cargo clippy --workspace --all-targets` + `cargo test --workspace` 全绿。
- 真实 API 的跨轨冒烟由用户 dogfood 复验(自动化只锁请求体形状)。

## 完成标准

- `/effort` 在 anthropic / responses / chat 三轨都能在对话中改并对下一轮生效;不被本轨接受的档位当场报错。
- effort 单一真源在会话态,`Provider` 不再烘焙。
- fmt + clippy + test 全绿;README 同步;plan 补 ✅ 与提交号;HANDOFF 补教训;一次 commit。

## 完成记录

- **protocol**(`crates/protocol/src/lib.rs`):新增 `ReasoningEffort`(6 档,`FromStr` 大小写不敏感、`xhigh` 无下划线、`join` 供帮助/报错行)与 `UnknownReasoningEffort`;`ProviderApiFamily::accepted_efforts/accepts_effort` 是唯一的每轨接受集表(doc 注明升级 provider 时逐条复核);`ActiveProviderRoute` 增 `effort`(`skip_serializing_if = "Option::is_none"`)。
- **provider**(`crates/provider/src/lib.rs`):`Provider::OpenAiResponses` 删掉烘焙的 `effort`;`stream_attempt` 增 `effort: Option<ReasoningEffort>` 参数,三轨分别渲染 `output_config.effort` / `reasoning.{effort,summary}` / `reasoning_effort`,`None` 一律不发字段;`MockRequest` 记录 effort。
- **core**(`crates/core/src/provider_route.rs`):catalog 增 `default_effort`(构造时按本轨接受集校验);`SessionState` 增 `effort` + `effort_pinned`;新增 `effort()` / `accepted_efforts()` / `set_effort()`(按当前轨校验,新增 `SwitchError::EffortUnsupported`);`FrozenProviderRoute` / `FrozenProviderAttempt` 携带 effort,`child_route` 与 fallback attempt 继承;`switch_with` 按「pinned 粘、不被新轨接受则回落该 provider 配置」更新。`sampling.rs` / `compact.rs` 两处 `stream_attempt` 传 `provider_attempt.effort()`。
- **命令**:新增 `crates/core/src/commands/effort.rs`(`/effort` 显示、`/effort <level>` 设置、`/effort off` 清空、非法档位列出本轨接受集),进 `BUILTINS`(`/help` 与 TUI `/` 菜单自动带上);`SlashResult.provider_changed` 更名 `route_changed`(effort 变化也要三个前端 re-freeze `cfg`),`/provider` 切换回执补报 effort。
- **cli**(`provider_config.rs`):profile `effort` 解析为 `ReasoningEffort` 并按本 profile 的 wire 校验(**解除「只允许 responses」**);新增 `selected_effort`(`KLOOP_EFFORT` > 根键 `model_reasoning_effort` > profile,只作用于被选中的 provider,与 `selected_base`/`selected_credential` 同形);根键 `model_reasoning_effort` 从「接受但忽略」变成真读。
- **server**:三处从 rollout 快照投影的路由用 catalog 默认 effort 填(effort 不进时间线,磁盘上的路由报的是「在那儿开会话会用什么」);`SwitchError::EffortUnsupported` 映射到 `unsupported_effort` kind。
- **TUI**:页脚 `provider / model · r1 · high · 12% ctx`(effort 为 None 时不占宽度)。
- **测试**:端到端两个(`compaction_samples_at_the_session_effort` / `turn_samples_at_the_session_effort` —— 会话态 `set_effort` → frozen route → attempt → mock 记录到的请求带该 effort,证两处 `stream_attempt` 调用点都传对);protocol 2 个(词表往返/拒绝、每轨接受集整对象);provider 3 个(三轨 body 渲染 + `None` 无字段);core 3 个(`set_effort` 被本轨拒绝且 frozen attempt/child 继承、catalog 拒绝本轨不接受的默认值、切 provider 的 pinned/回落/`off` 也粘);commands 1 个(`/effort` 显示/设置/幂等/拒绝/未知/清空 + `route_changed`);cli 2 个(env>根键>profile 且只作用于选中项、三种非法来源的报错文案)。
- **验证**:`cargo fmt --all --check` 干净;`cargo clippy --workspace --all-targets` 零 warning;`cargo test --workspace` 1335 passed / 0 failed。**如实边界**:三条真实 rail 上「改完 effort 下一轮确实按新档位采样」须用户 dogfood 复验(自动化只锁到发给 provider 的请求体形状与会话态流转);未把 KLOOP_EFFORT 之外的任何 endpoint/credential 写进提交文件。
- **README**:built-in 命令表补 `/provider` 与 `/effort`;新增「Reasoning effort」段(词表、每轨字段与接受集、不静默降级、优先级、随 frozen route 到子 agent/compaction、不进时间线故 resume 回落、切 provider 的粘/回落、改 effort 会作废 Anthropic prompt cache);provider 配置注释块与 server `/provider` 例外那句同步。
