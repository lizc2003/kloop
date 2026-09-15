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
- `~/.kloop/config.toml` 根键 `model_reasoning_effort` 被 `validate_root` 接受但**从没读过**(codex 兼容遗留;本 plan 里先改成真读,后按用户意见改名为 `effort`)。
- `model_providers.<id>.effort` 目前硬性只允许 responses wire。

三条轨其实都有 effort 参数(2026-08 核对):

| 轨 | 请求字段 | 取值 |
| --- | --- | --- |
| Anthropic Messages | `output_config: {"effort": …}`(GA,无 beta 头,默认 `high`) | `low` / `medium` / `high` / `xhigh` / `max` |
| OpenAI Responses | `reasoning: {"effort": …, "summary": "auto"}` | `minimal` / `low` / `medium` / `high` |
| OpenAI Chat Completions | `reasoning_effort: …` | `minimal` / `low` / `medium` / `high` |

## 已拍板设计

### 1. 词表:六档,来自真实测量;不按轨门禁(2026-08-28 测完定稿)

`ReasoningEffort { None, Low, Medium, High, XHigh, Max }`(serde/`FromStr` 小写单词,`xhigh` 不带下划线)。

**这六档是测出来的,不是查出来的。** 首版按训练期印象写了两条错误的东西,真实跑一次全被推翻:

1. **按 `api_family` 硬编码「每轨接受子集」**(Anthropic `low..max`、OpenAI 家族 `minimal..high`)并在命令层当场拒绝集外档位。
2. 词表里带了 `minimal`。

实测(同一代理,`real_effort_sweep_contract` 走完整会话:`/effort <level>` 后各跑一次真实 turn):

| 档位 | responses / gpt-5.6-sol | chat / gpt-5.6-sol | anthropic / claude-sonnet-4-6 |
| --- | --- | --- | --- |
| (不发字段,对照) | ✅ | ✅ | ✅ |
| `none` | ✅ | ✅ | ✅(经 `thinking: {"type":"disabled"}`) |
| `minimal`(已删) | ❌ 400 | ❌ 400 | ⚠️ 未定论 |
| `low` / `medium` / `high` | ✅ | ✅ | ✅ |
| `xhigh` / `max` | ✅ | ✅ | ✅ |

端点原话:`Unsupported value: 'minimal' is not supported with the 'gpt-5.6-sol' model. Supported values are: 'none', 'low', 'medium', 'high', 'xhigh', and 'max'.`

Anthropic 的 ⚠️ 是代理限流,不是档位被拒:该代理会隔一个请求 429 一次,一轮里失败行呈 `none`✗ `low`✓ `medium`✗ `high`✓ `xhigh`✗ `max`✓ 的交替形态,而 `medium`/`xhigh` 在别的轮次里成功过。sweep 因此改成**把 429 与「档位被拒」分开**:429 自动重试三次(间隔 30s),仍 429 就记 `INCONCLUSIVE` 而非 `REFUSED`。加重试后该轨 `low..max` 全绿,只剩 `none` 仍失败——**用户指出根因**:Anthropic 的「不推理」语义归 `thinking: {"type":"disabled"}` 管,不是 effort 的取值。改成 `none` 在该轨渲染为 `thinking: disabled`(且不发 `output_config`)后,整表 ✅、不再有未定论行。

据此定稿:据此定稿:

- **删掉 `minimal`**。我能测到的模型没有一个支持它(gpt-5.6-sol 明确拒绝并列出不含它的支持集;Anthropic 文档的集合也是 `low..max`),它是训练期旧印象的残留。用户拍板「不用考虑兼容性」,直接删。
- **不按轨门禁**。那张表两个方向都错:多拒了 `xhigh`/`max`(假阴性,挡住 gpt-5.6-sol 上的合法配置),又漏了 `none`。而且**两条轨实测结论完全一致**——差异是我编出来的。接受集是模型属性,provider 的 400 直接枚举支持值,比任何本地表都准。
- **`none` 按轨渲染到不同字段**:Anthropic 走 `thinking: {"type":"disabled"}`(并压过 profile 配的 `thinking`,因为它是更晚的会话级指令),OpenAI 两轨走各自的 effort 字段。即「同一档位在不同轨上可能落在完全不同的参数里」,不只是字段改名。
- 分工:**kloop 只管自己的拼写**(`/effort hgih` 当场拒并列出词表,打错字不该等到下一轮),**档位合法性交给模型**,provider 的 400 原样透出。
- **`off` 改名 `unset`**。设计 `off` 时 `none` 还不在词表里;测出 `none` 是真实档位后,「off(不发字段)」和「none(发 `effort:"none"`,要求不推理)」并排会被读成同义词。`unset` 精确指「不发这个字段」。

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

`KLOOP_EFFORT` > 根键 `effort`(原 codex 名 `model_reasoning_effort`,既没被读过、名字又长,直接改名)> `model_providers.<id>.effort` > 不发。根键/环境变量只作用于**被选中的** provider,与既有 `selected_base` / `selected_credential` 同形。`model_providers.<id>.effort` 解除「只允许 responses」的限制,改为按本 profile 的 wire 校验取值。

## 关键文件

- `crates/protocol/src/lib.rs` — `ReasoningEffort` + `ProviderApiFamily::accepted_efforts` + `ActiveProviderRoute.effort`。
- `crates/provider/src/lib.rs` — 删 `OpenAiResponses.effort`;`stream_attempt` 增参;三轨渲染;`MockRequest.effort`。
- `crates/core/src/provider_route.rs` — catalog `default_effort`;`SessionState.effort`;`effort()` / `set_effort`;frozen route/attempt 携带;换轨回落。
- `crates/core/src/commands/effort.rs`(新)+ `commands/mod.rs`(注册 + `route_changed` 更名)。
- `crates/core/src/agent/sampling.rs`、`crates/core/src/compact.rs` — `stream_attempt` 传 attempt 的 effort。
- `crates/cli/src/provider_config.rs` — profile effort 解析成 `ReasoningEffort` 并按 wire 校验;`selected_effort`(env > 根键 > profile);`ResolvedProviderSettings` 透出。
- `crates/cli/src/user_config.rs` — 根键改名 `effort` 并真读出来。
- `crates/tui/src/lib.rs` / `crates/server/src/lib.rs` / `crates/cli/src/main.rs` — `route_changed` 更名。
- `crates/tui/src/render.rs` — 页脚 `provider / model · r1 · high · 12% ctx`(effort 为 None 时不加这一段)。
- `rust/README.md` — provider 配置段、slash 命令段、wire 段同步。

## 非目标

- 不做「按轨/按模型的接受集表」(首版做了,被真实测量推翻,见上)。
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
- **cli**(`provider_config.rs`):profile `effort` 解析为 `ReasoningEffort` 并按本 profile 的 wire 校验(**解除「只允许 responses」**);新增 `selected_effort`(`KLOOP_EFFORT` > 根键 `effort` > profile `effort`,同一个词三个作用域;只作用于被选中的 provider,与 `selected_base`/`selected_credential` 同形);根键从 codex 遗留的 `model_reasoning_effort` 改名为 `effort`(从没被读过,不留别名)。
- **server**:三处从 rollout 快照投影的路由用 catalog 默认 effort 填(effort 不进时间线,磁盘上的路由报的是「在那儿开会话会用什么」);`SwitchError::EffortUnsupported` 映射到 `unsupported_effort` kind。
- **TUI**:页脚 `provider / model · r1 · high · 12% ctx`(effort 为 None 时不占宽度)。
- **测试**:端到端两个(`compaction_samples_at_the_session_effort` / `turn_samples_at_the_session_effort` —— 会话态 `set_effort` → frozen route → attempt → mock 记录到的请求带该 effort,证两处 `stream_attempt` 调用点都传对);protocol 2 个(词表往返/拒绝、每轨接受集整对象);provider 3 个(三轨 body 渲染 + `None` 无字段);core 3 个(`set_effort` 被本轨拒绝且 frozen attempt/child 继承、catalog 拒绝本轨不接受的默认值、切 provider 的 pinned/回落/`off` 也粘);commands 1 个(`/effort` 显示/设置/幂等/拒绝/未知/清空 + `route_changed`);cli 2 个(env>根键>profile 且只作用于选中项、三种非法来源的报错文案)。
- **验证**:`cargo fmt --all --check` 干净;`cargo clippy --workspace --all-targets` 零 warning;`cargo test --workspace` 1335 passed / 0 failed。**如实边界**:三条真实 rail 上「改完 effort 下一轮确实按新档位采样」须用户 dogfood 复验(自动化只锁到发给 provider 的请求体形状与会话态流转);未把 KLOOP_EFFORT 之外的任何 endpoint/credential 写进提交文件。
- **真实 provider 验证**(2026-08-28,用户提供 key/代理后执行;凭据只从 gitignored 的 `.kloop/env.local` source,未落任何提交文件):三条轨的完整 sweep 结果见上文第 1 节表格。责任分工:`crates/provider/tests/effort_probe.rs`(ignored 诊断,直接打 provider 层,逐档位 ACCEPTED/REJECTED,带「不发字段」对照行——正是这行让 anthropic 轨的全行失败被认出是代理限流而非 effort 被拒);`crates/server/tests/server.rs::real_effort_sweep_contract`(ignored 契约,走完整 server 会话:`/effort <level>` → 真实 turn,跑全词表并打表,429 自动重试后仍失败记 INCONCLUSIVE,只断言 `low`/`medium`/`high` 与「不发字段」对照必须成功、未知拼写被 kloop 当场拒)。

- **README**:built-in 命令表补 `/provider` 与 `/effort`;新增「Reasoning effort」段(词表、每轨字段与接受集、不静默降级、优先级、随 frozen route 到子 agent/compaction、不进时间线故 resume 回落、切 provider 的粘/回落、改 effort 会作废 Anthropic prompt cache);provider 配置注释块与 server `/provider` 例外那句同步。
