# Plan 158 — 模型的事实,被当成了用户配置

> 来源:2026-09-17。用户让我审他自己的 `~/.kloop/config.toml`(`model_provider = "gw_cn"`
> 挂 deepseek,另一条 `gw_router` 挂四个 gpt),一路问下来变成了一次 provider schema 的
> 重新设计。**全程用户拍板**,而且过程中推翻了我提的三个方案(内置模型知识表、`models`
> 改数组-of-表、补 `env_key`)——都记在第六节非目标里,别再提第二遍。
>
> 同一次会话已经落地的是 `wire_api = "anthropic"` → `"messages"`(提交 `066c6b4`,理由
> 记在 plan 40 的「后续变更」)。本 plan 是那之后的第二批,规模大得多,而且是**破坏性
> schema 变更,不做兼容**。

## 一、根因:provider 表里混着两类东西

- **配置**——用户的选择:用哪家、哪个模型、网关地址、key、这次想要多少 effort。
- **知识**——模型的客观事实:窗口多大、支持哪些 effort 档位。

kloop 现在把第二类钉在 provider 级(`context_window` 和 `effort` 都是 profile 字段),
于是一个 provider 挂多个模型时,它们被迫共享同一份"事实"。用户那条 `gw_router` 挂四个
gpt 模型、共用一个 `context_window = 258400`,就是这个形状直接的产物。

plan 92 定这套 schema 时写明过:「本计划不增加 model-specific settings overlay」。
**当初就知道缺,是主动推迟的**,不是漏了。

两个已经在用户身上发生的后果:

1. 启动在 `gw_cn`(声明 1000000),`/provider gw_router` 切过去之后压缩预算**还是 1000000**
   ——`initial_context_window` 只取启动时选中那一个(`provider_config.rs` 里
   `ResolvedProviderSettings.initial_context_window` 的注释写明了),切换不重算。对一个
   258400 的窗口,要等服务端拒一次、`note_overflow_at` 记下实测天花板才自愈,白烧一次
   长上下文请求。
2. 顶层 `effort = "max"` 实际只作用在**选中**的 deepseek 上(`selected_effort`:选中的用
   root,没选中的用自己的 profile),`gw_router` 反而什么都没有——而 deepseek 这种 flash
   模型吃不吃 `max` 根本没人验证过,responses rail 会无条件发
   `reasoning: {effort, summary:"auto"}`。

## 二、目标 schema

```toml
# ~/.kloop/config.toml   (0600, 目录 0700)

provider = "gw_cn"          # 顶层只剩这一个选择器:这次用哪家

[providers.gw_cn]
wire_api = "responses"
base_url = "https://gw-cn-direct.example.com/v1"
http_headers = { Authorization = "Bearer ..." }
model = "deepseek-v4-flash-0731"

[providers.gw_router]
wire_api = "responses"
base_url = "https://gw-cn.example.com/v1"
http_headers = { Authorization = "Bearer ..." }
model = "gpt-5.6-sol"
models = ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5"]
context_window = 258400      # 这个网关截断到多少
effort = "xhigh"

[providers.anthropic]
wire_api = "messages"
base_url = "https://api.anthropic.com"
http_headers = { x-api-key = "sk-ant-..." }
model = "claude-sonnet-5"
cache = true                 # 只有 messages rail 能写
thinking = "adaptive"

# ── 模型知识:查一次,写一次,之后不动 ──
[models."deepseek-v4-flash-0731"]
context_window = 131072
efforts = ["none", "low", "high"]

[models."gpt-5.5"]
context_window = 262144
```

**读法**:`[models.x]` 说模型能吃多少、支持哪些档位(跨 provider 共用一份);
`providers.x.context_window` 说这个网关肯给多少。两者都是可选的,实际预算是
**min(模型的, 网关的, 兜底常量)**。

## 三、六项改动

### 1. `[models."<id>"]` 知识段 + 三源取 min

新的顶层键 `models`(和 provider 下那个 `models` 数组同名但不同层,一个是知识表、一个是
目录;如果觉得会混,开工时可以换个名,但**别叫 `model_info`**——那是 codex 的内部结构名)。

字段只有两个:

- `context_window`:正整数(复用现有 `optional_integer` 的 0 拒绝规则)。
- `efforts`:`ReasoningEffort` 的数组。

语义要钉死的四条:

- **不写 `efforts` = 不声明 = 不校验**(六档全放行,服务端说了算)。这是"还没验证过"的
  诚实表达。缺省**不能**是某个子集:子集拦不住真正要拦的(`max` 在 deepseek 上),却会
  误拦合法的(`medium` 是一等档位,`/effort` 的错误信息里就列着),而且会把"没声明"
  报成"不支持",是句假话。
- **`efforts = []` 拒绝解析**,当手滑处理。要表达"别推理"就写 `["none"]`。
- **`none` 不受 `efforts` 约束**,任何模型都放行。它在 messages rail 上根本不是一个
  effort 值——`provider/src/lib.rs:525` 把它翻译成 `thinking: {type: "disabled"}`,
  是**关闭思考的唯一开关**。被 `efforts` 排除掉的话,没写 `efforts` 的 anthropic 模型
  就关不掉 thinking 了。
- **兜底常量不是知识**。`DEFAULT_CONTEXT_WINDOW = 200_000`(`startup.rs`)保留,它的注释
  已经写明定位("has to be safe for the smallest model anyone routes to")——它不声称
  任何模型是 200k,只保证什么都没写时不炸。

校验:provider 的 `effort`(以及 `KLOOP_EFFORT`、`/effort`)要落在该模型的 `efforts` 里,
不相容**启动时**或命令当场 fail closed,不要等服务端 400。

### 2. `model_providers` → `providers`,`model_provider` → `provider` ✅

理由:内部代码里模型 provider 就叫 `Provider`(`ProviderCatalog`/`provider_route`/
`FrozenProviderRoute`),而搜索那边叫 `SearchBackend`、**不叫 provider**——配置跟内部真相
对齐。两个一起改,不能只改表不改选择器。

`[web] search_provider` 不动:它有限定词,不冲突。

### 3. `default_model` → `model` ✅

**这是把 plan 92 改过的名字改回去,两边理由都要留在文档里**,否则下一个人顺着 plan 92 读
会以为是疏漏、再翻一次(plan 67 那条教训就是这么来的)。

- plan 92 当初改成 `default_model`:引入 `models` allowlist 之后,`model = "a"` 会让人问
  "那 b、c 算什么"。
- 现在改回 `model`:**层级本身表达作用域**,内层覆盖外层是 TOML 的惯用读法(codewhale
  就是 `[providers.deepseek] model = ...`);而且新 schema 里 `models` 段只在多模型时才写,
  大部分 provider 就 `model` 一行,那个歧义本来就不出现了。

连带:`rejects_legacy_profile_model_and_invalid_membership` 这条测试要**反过来**——
profile 里的 `model` 变成合法键,`default_model` 变成 unknown key。

### 4. 删顶层 `model` 和顶层 `effort` ✅

顶层 `model` 不只是冗余,是**耦合陷阱**:它只对选中的 provider 生效且必须在其 allowlist 里,
所以改 `provider` 却忘了改 `model`,启动直接炸。删掉之后顶层只剩 `provider` 一个选择器,
职责单一。`GlobalFile.initial_model` 整个字段可以去掉,解析链短一截:

```rust
let initial_model = rail_model.or(KLOOP_MODEL).unwrap_or(profile.model);
```

顶层 `effort` 同理删掉,effort 跟着 provider 走。`KLOOP_EFFORT` 与 `/effort` 保留——env 和
命令是临时覆盖的正当位置,配置文件不是。

### 5. 删 `fallback_model` ✅

它现在的语义(`agent.rs:649`):一个 turn 内 primary 返回 `Sampled::Failed` 时换成 fallback
重试这一轮,之后 `active_attempt` 一直是 fallback;作用域是 `Turn`,下一 turn 回到 primary。

删的理由,第三条是决定性的:

1. 重试已经在下面吃掉瞬时故障(`is_retryable()` + status 白名单 + stream 层重试),能走到
   `Sampled::Failed` 的是重试之后仍然失败的。
2. 它改变结果质量而提示只有一行 Note——turn 做到一半从 sol 掉到 5.5,后半个 turn 在改代码。
3. **schema 强制 fallback 必须在同一个 provider 的 `models` 里**,也就是同一个 `base_url`、
   同一个 key、同一个网关。而最常见的故障恰恰是 provider 级的(网关 500、限流、key 失效)
   ——这些它一个都救不了,两个模型一起挂。真正想要的是**跨 provider** 的故障转移,那是
   另一个设计(要处理 rail 不同、reasoning 连续性断裂),不该复用这个字段名硬凑。

⚠️ **删除有一处要当心**:`ProviderAttemptKind::Fallback` 进了 rollout 持久化
(`rollout.rs:883`),删枚举变体会碰到历史 receipt 的读取。按"不考虑兼容性"可以直接删,
但要在实现时确认旧会话打不开时的表现是**明确报错或落到默认**,不是 panic。plan 132 已经
定过"记录的 provider 连参考都不是,重开一律用当前默认",可以顺着那条路。

### 6. `/provider` 切换后重算压缩预算

现在 `cfg.context_window` 在启动时定死(`startup.rs` 里
`runtime.context_window_env.unwrap_or_else(|| provider.initial_context_window()...)`),
而 `Config` 是 `Arc<Config>`、不可变,所以 `/provider` 换了 provider 却不换预算。

**这是本 plan 最难的一项**,而且是唯一一项动的不是 schema 而是运行时结构。开工时先决定:
把 `context_window` 从 `Config` 挪到 `SessionProviderState`(它已经持有 route 和 revision,
切换时本来就要重建),还是让 `Config` 在切换时整体重建。不要在没想清之前动手——`Config`
被 `compact`、`agent`、`commands`、`tui` 多处读。

有了 `[models.x]` 之后,重算的输入是 **(provider, model)** 这一对,不再只是 provider。

## 四、提交切分

建议两次,中间状态可用:

1. **schema 改造**(第 2/3/4/5 项):纯改名与删除,机械、可一次做完。做完之后配置文件必须
   跟着改,`[models.x]` 还不存在,窗口仍是 provider 级——能跑。
2. **知识段与预算**(第 1/6 项):新能力加运行时结构调整。

一次做完也行,但别把第 6 项和改名混在同一个 diff 里——它俩一个是机械替换、一个需要想清楚,
混在一起 review 不动。

## 四之二、✅ 第一阶段完成记录(2026-09-17,提交 SHA 以本条所在提交为准)

第 2/3/4/5 项已落地,fmt / clippy `-D warnings` / `cargo test`(34 个 `test result: ok`,
零 failure)/ `--mock --headless` 全绿。开工时没预料到的五件：

1. **`ProviderAttemptKind` 整个枚举删掉了,不只是 `Fallback` 变体。** 删掉 fallback 之后它
   只剩 `Primary` 一个变体——单变体枚举不携带任何信息,连同 `ProviderAttemptIdentity`、
   `ProviderResponseProvenance`、`ProviderUsageRecord` 三处的 `attempt_kind` 字段一起去掉。
   rollout 里 `attemptKind` 那一项也从 usage fixture 中消失。
2. **`Sampled::Terminal` 与 `Sampled::Failed` 的 match arm 删掉 fallback 后逐字相同**,
   合并成一条。**注意**:这说明这两个变体在唯一的消费点上行为已经一致,区分只剩在 stream 层
   的重试分类里——要不要合并枚举本身没有动,留给以后。
3. **`Turn.frozen_route` 成了死字段**,被 clippy 抓出来。它唯一的读者就是 `fallback_attempt()`。
4. **重试上限吃得下两次瞬时错误,第三次就耗尽。** 两条测试原本写三次 `MockTurn::Error`,
   靠 fallback 接手才 Completed;删掉 fallback 后它们开始报错。改成两次——这也说明
   **fallback 此前在掩盖"重试已经耗尽"这件事**。
5. **一条测试整条删掉:`fallback_fails_closed_on_incompatible_reasoning_history`。** 它测的是
   "一个 turn 内换模型后 reasoning 不能重放",而 fallback 是一个 turn 内换模型的**唯一**机制,
   场景随之不复存在(跨 turn 的那条路由 `history.rs` 的 switch 测试覆盖)。另删
   `fallback_model_takes_over_after_retries`,它断言的 Note 文案已不存在。

`with_test_models` 的签名顺势从 `(primary, fallback)` 改成 `(&[&str])`——"允许这些模型,
第一个是 primary",不再暗示第二个模型有特殊角色。

## 五、验收

- `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo test`、`cargo run -p kloop -- --mock --headless` 全绿。
- 旧键全部 fail closed 且错误信息列出合法拼写:`model_providers`、`model_provider`、
  `default_model`、`fallback_model`、顶层 `model`、顶层 `effort`。
- 三源取 min 有整对象断言:只写模型的、只写网关的、都写、都不写(落兜底)四种组合。
- `efforts` 四条语义各一条测试:不写=不校验、`[]` 被拒、`none` 永远放行、不相容的
  `effort` 在**启动时**被拒(不是运行时)。
- `/provider` 切换后预算按新的 (provider, model) 重算——用两个窗口差异大的 provider
  断言切换前后的有效窗口不同。
- 本机 `~/.kloop/config.toml` 按新 schema 改写后,真实 headless 跑通(用户的 key 问用户要,
  不进任何提交文件)。
- README 的配置样例、plan 40 的「后续变更」、本 plan 的 ✅、HANDOFF 的待办表与教训一并更新。

## 六、非目标(讨论中明确砍掉的,别再提)

- **不内置模型知识表。** 我提过让 kloop 代码里带一份 per-model 的窗口/档位表(codex 的
  `ModelInfo`、grok 的 GB 那层),用户否掉:kloop 要对接任意第三方网关,内置表永远追不全、
  还会过期,反而制造"代码说 1M、网关只给 258k"的第二个真相源。**知识在配置里,代码里只有
  兜底常量。**
- **`models` 不改成数组-of-表。** 我提过 `[[providers.x.models]]` 每条带 `id`/
  `context_window`/`efforts`。知识挪进 `[models.x]` 之后它没有存在理由了,保持字符串数组。
- **不补 `env_key` / `env_http_headers`。** codex 和 grok 都有,codex 源码还写着
  `experimental_bearer_token` 是 "discouraged in favor of env_key"。但那是面向大量用户的
  产品给的默认建议;kloop 这边文件 0600、目录 0700、拒 symlink/FIFO、模型的
  `read_file`/`grep`/`bash` 读它被硬拒、`ResolvedProviderSettings` 连 `Debug` 都不实现、
  子 shell 的 provider key 被 scrub——**kloop 自己的威胁模型里 env 才是更需要防的那侧**。
  用户明确说 `http_headers` 明文更直观,保留。
- **不做跨 provider 的故障转移。** 见第 5 项理由三。要做是另立 plan。
- **不碰 `[web] search_provider`**、不碰 `query_params`(kloop 主动拒绝 URL query,补它是
  Azure 场景的事,现在没有)、不碰 retry/timeout 可配(codex 有三个字段,单用户场景是运维
  旋钮,价值低)。
- **不碰 `<think>` 标签流。** codewhale 有 `reasoning_stream_style = "inline_tags"`,kloop
  完全没有这个处理。跟本 plan 同源发现(用户在用 deepseek),但那是 provider 流解析的事,
  另立 plan。
