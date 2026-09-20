# Plan 168 — 想多深,只留一个旋钮

> 来源:2026-09-20,plan 167 收尾后用户接着问 "cache 字段是做什么的"、"这两个字段有意义吗"、
> "thinking 与 effort 重叠了"。查证 Anthropic 当前契约后发现重叠是真的、而且**当前配置正在踩坑**。
> 几轮来回的裁决记在下面(包括我提过又被否掉的三个方案,别再提)。

## 现状与它造成的实际后果

kloop 的 Anthropic 轨有两个 reasoning 相关旋钮:

- `providers.x.thinking` = `"off" | "adaptive" | <正整数>` → `ThinkingMode`,**烘进
  `Provider::Anthropic` 构造**,默认 `Unset`(不发 `thinking` 字段)。
- `providers.x.effort` / `KLOOP_EFFORT` / `/effort`(plan 102)→ `output_config.effort`,
  逐请求传。

查证到的 Anthropic 契约(经 `claude-api` 技能核对,非记忆):

| 模型 | 省略 `thinking` | `adaptive` | `budget_tokens` |
|---|---|---|---|
| Opus 5 / Sonnet 5 / Fable 5(.1) | 跑 adaptive | 接受 | **400** |
| Opus 4.8 / 4.7 | **完全不思考** | 唯一的"开" | **400** |
| Opus 4.6 / Sonnet 4.6 | **完全不思考** | 推荐 | 已废弃,仍可用 |
| Haiku 4.5 及更早 | 不思考 | 不接受 | **必需**,≥1024 且 < max_tokens |

于是用户本机 `gw-claude`(`model = "claude-opus-4-8"`,`effort = "xhigh"`,未写 `thinking`)
**一直在不思考的状态下跑**——省略 `thinking` 对这一代就是关闭,而 `effort` 管的是深度。
同 profile 的 `models` 还横跨三代:`sonnet-4-6` 没有 `xhigh`,`haiku-4-5` 根本不吃 effort 字段。

## 裁决

### 一、两个旋钮合成一个:`effort`

`thinking = "off"` 与 `effort = "none"` 今天就是同一件事(`lib.rs` 把 `ReasoningEffort::None`
翻译成 `ThinkingMode::Off`,plan 102 亲手接的线)。其余取值的重叠与否**随模型变**——
`adaptive` 在 5 系冗余、在 4.x 必需;`budget` 与 effort 互斥。

所以它们本就不是两个旋钮,而是**一个意图(想让它想多深)在三代模型上的三种线协议表达**。
用户不该承担这个翻译。**`effort` 是唯一的用户/会话旋钮**(它已经有 `/effort`、有跨轨接受集
校验、有换 provider 时的粘性回落),`thinking` 降级为渲染结果。

旁证:今天能写出自相矛盾的组合——`thinking = "off"` + `effort = "xhigh"` 会**同时**发
`{"type":"disabled"}` 和 `output_config:{"effort":"xhigh"}`,在 Opus 5 上是明确的 400。
没有任何地方拦。两个平级旋钮管一件事,必然要写这种跨旋钮耦合规则。

### 二、budget 方言:`[models.x].thinking_budget`,键即档位

```toml
[models."claude-haiku-4-5"]
thinking_budget = { low = 2048, medium = 8192, high = 16384 }
```

- **表的键 = 这个模型接受的档位**,取代该模型的 `efforts`(两个同写报错)。于是"haiku 不吃
  xhigh"不再靠沉默:`/effort xhigh` 在命令层被现有 `effort_supported` 拒掉并列出 low/medium/high。
  这同时填掉 plan 158 留下的真空——`parse_efforts` 拒绝空数组,"这个模型完全不接受 effort 字段"
  此前**表达不出来**。
- 值 ≥ 1024(API 下限;今天 kloop 接受任何正整数,写 512 本地过、上游 400)。
  `< max_tokens` 自动成立:`Budget(n)` 会把 max_tokens 抬成 `8192 + n`。

### 三、渲染规则(全部由 effort 驱动)

| 情形 | 发出去的 |
|---|---|
| 模型有 `thinking_budget`,effort = `medium` | `thinking: {"enabled", budget_tokens: 8192}`,**不发** `output_config` |
| 模型无 `thinking_budget`,effort = `xhigh` | `thinking: {"adaptive"}` + `output_config: {"effort":"xhigh"}` |
| `/effort none`(任何模型) | `thinking: {"disabled"}` |
| effort 未设 + 有 budget 表 | 不发 `thinking` —— 正是该模型自己的 API 默认 |
| `providers.x.thinking = "unset"` | 什么都不发(逃生口,见下) |

**messages rail 的默认从"不发 thinking"翻成"发 adaptive"。** 这是本 plan 唯一的行为默认变更,
也是修好 opus-4-8 不思考那件事的地方。代价:会被 `thinking` 字段噎住的第三方网关就没退路了,
所以 profile 保留 `thinking`,但语义收窄成**兜底/覆盖**,取值只剩 `"unset" | "off" | "adaptive"`
(整数移走,它属于 `[models.x]`)。`unset` 这个词沿用 plan 102 的定义:"精确指不发这个字段"。

### 四、`cache` → `prompt_cache`

用户:"cache 这个字段名起的不明晰"。对——它没说**谁的**缓存、**哪一种**,而同一份代码里
`cache_key`(网关会话路由)、`cache_read`/`cache_creation`(用量计数)是另外两件事。
`prompt_cache` 与 Anthropic 自己的特性名(prompt caching)对齐,用户读的文档和配置是同一个词。
照 `auth_header` 那条"命名线上的东西"的规矩会得到 `cache_control`,但 `cache_control = true`
读起来别扭,而 `prompt_cache` 离 400 报文里的 `cache_control` 只有一步。语义、默认值、三个断点
的位置都不动,**只改名**,不留兼容别名(同 plan 167)。

### 五、缝:`thinking` 从 Provider 构造挪到逐请求

按上面的规则,`thinking` 由 (模型, effort) 决定,而模型是逐请求才知道的(`child_route` 能在
`allowed_models` 里换模型)。所以:

- `Provider::Anthropic` **删掉** `thinking` 字段;`stream_attempt` 增参 `thinking: ThinkingMode`。
- `ProviderCatalogEntry` 增 `default_thinking`(来自 profile),与既有 `default_effort` 并列。
- `ModelKnowledge` 增 `thinking_budgets: Option<BTreeMap<ReasoningEffort, u64>>`;
  `declared_efforts` 在该表存在时返回它的键。
- `ResolvedRoute` 带上解析所需的两样(profile 默认 + 本 route `allowed_models` 的 budget 表),
  `FrozenProviderRoute::attempt(model)` 处解析成 `ThinkingMode` 存进 `FrozenProviderAttempt`。

**这正是 plan 102 对 `effort` 做过的同一件事**,它给的理由原样适用:"唯一真源移到会话态,
避免两处 default 打架"。生产调用点两个(`compact.rs`、`agent/sampling.rs`),测试构造点十余个。

## 非目标(讨论中提过又被否掉的,别再提)

- **`reasoning = "implicit" | "explicit" | "budget"` 三值枚举。** 我提过让每个模型声明它的
  "方言"。`implicit`/`explicit` 的区分**不可观测**——5 系明确接受显式 `adaptive`,4.x 必需,
  那么永远发 adaptive 对两边都对。为看不见的差别造枚举是纯手工负担。用户原话:
  "把已有的模型需要 explicit,自动带上就好,不需要手工配了"。
- **拿 `efforts` 是否声明当"支不支持 effort"的开关。** `efforts` 缺省的现有语义是
  **不设限**,借用它会让每个没写 `[models.x]` 的模型静默翻转行为。
- **`thinking_budget` 放 profile 级。** 试过一轮。作用域对不上:budget 方言是模型属于哪一代的
  事实,而 profile 装着跨代的 `models` 列表——为让 haiku 能思考而写上它,opus-4-8 当场 400。
- **不内置 模型→方言 的代码表。** 同 plan 158:知识写在配置里,代码只做渲染。

## 顺带

README 里 `KLOOP_CACHE` / `KLOOP_THINKING` 那半句是过时的——`provider_config.rs` 没有任何地方
读这两个环境变量(只有两个 hermetic 测试拿 `KLOOP_CACHE=not-a-boolean` 断言垃圾值不影响
`--mock`)。这次连同改名一起删掉那半句。

## 完成标准

- fmt / clippy `-D warnings` / 全 workspace test 全绿;`cargo run -p kloop -- --mock` 可跑。
- 测试:budget 表解析(键即档位、与 `efforts` 同写报错、值 <1024 报错)、三条渲染规则各一条
  wire 断言、`/effort xhigh` 在 budget 模型上被拒、messages 默认发 adaptive、`"unset"` 不发字段、
  `prompt_cache` 改名后的既有断言。
- README、本 plan ✅ 与提交号、HANDOFF 教训一并更新,一次 commit。

## 完成记录 ✅（2026-09-20）

- **provider**:新增 `Reasoning { effort, thinking }`——两半是一个设置,一起走(也正好让
  `stream_attempt` 回到 clippy 的 7 参数线内,那条 lint 这次指对了地方)。
  `Provider::Anthropic` 删掉 `thinking` 字段、`cache` 改名 `prompt_cache`;渲染时 budget 方言
  不发 `output_config`,`none` 不发 `output_config`。
- **protocol**:`ReasoningEffort` 加 `PartialOrd, Ord`(声明顺序本就是语义顺序,注释锁住);
  新增 `ANTHROPIC_MIN_THINKING_BUDGET = 1024`。
- **core**:`ModelKnowledge.thinking_budgets`;`ProviderCatalogEntry.default_thinking`;
  新 `ThinkingRouting{default_mode, budgets}` 随 `ResolvedRoute` 走(frozen route 活得比
  catalog 查询久,`child_route` 还能换模型,所以解析所需的东西必须跟着 route);
  `FrozenProviderAttempt.reasoning` 在 `attempt(model)` 处解析。
- **cli**:`prompt_cache` 改名;profile `thinking` 收窄成 `unset | off | adaptive`、默认
  messages rail = `adaptive`;`[models.x].thinking_budget` 解析(键即档位、与 `efforts` 同写
  报错、`none` 不许有预算、值 ≥1024、空表报错)。
- **测试**:`a_budget_table_declares_both_the_budgets_and_the_accepted_efforts`、
  `a_budget_table_fails_closed_on_contradictions_and_unusable_budgets`、
  `the_messages_rail_thinks_unless_told_otherwise`(cli);
  `effort_renders_into_whichever_reasoning_field_the_model_reads`(core,四种模型×档位组合);
  `effort_and_thinking_render_as_one_knob`、`a_disabled_mode_sends_no_effort_alongside_it`
  (provider wire 断言)。
- **README**:schema 样例加 `[models."claude-haiku-4-5"]`;新增"reasoning 是一个旋钮"与
  "prompt_cache 做什么、何时关"两段;删掉过时的 `KLOOP_CACHE`/`KLOOP_THINKING` 半句。
- 本机 `~/.kloop/config.toml`:三个 profile 的 `auth_header` 已是 167 的形状,这次补上三个
  Claude 模型的 `[models.x]`(opus-4-8 五档、sonnet-4-6 四档无 xhigh、haiku-4-5 budget 表),
  0600 未变,不进 git。
- 验证:`cargo fmt --all`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo test --workspace`(1595 passed)全绿;`cargo run -p kloop -- --mock --headless` 跑通。
  **真实网关未验**——opus-4-8 由"不思考"变成 adaptive 的实际差异要花用户额度,单独确认。
- 提交:`0e5de72`。

### 追加 — `config/config-demo.toml`(同日)

> 用户:"写一个三种 wire_api 的示例配置吧" → "放到项目的 config 目录下,名字叫
> config-demo.toml,不要写敏感信息,用来给其他人参考的"。

仓库新增 `config/` 目录与 `config-demo.toml`:三条 rail 各一个 profile
(`k-claude`/`k-gpt`/`k-chat`)+ 四个 `[models.x]`,把本 plan 与 plan 167 定下的 schema 写成
可抄的形状。**主机名一律 `gateway.example.com`、凭证一律 `REPLACE-ME`**,不含任何真实端点或
密钥(用户明确要求)。

验收方式值得记:不是读一遍,是**把它当真配置跑**——拷进一个隔离的临时 HOME(0700/0600),
三条 rail 各起一次 headless。三次都走完解析、路由、拼 URL,最后停在占位主机的 DNS 失败:

```
anthropic       request to https://gateway.example.com/v1/messages failed
openai-responses request to https://gateway.example.com/v1/responses failed
openai-compat   request to https://gateway.example.com/v1/chat/completions failed
```

三条 URL 同时印证了注释里写的拼接规则(messages 的 base 不带 `/v1`,另两条带)。
注释里"`prompt_cache`/`thinking` 只在 messages 合法,写 `false` 也报错"这句也单独验了一次
——示例文档里的断言和代码一样会过期,能跑的验收才算数。
