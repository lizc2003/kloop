# Plan 169 — 逃生口不是旋钮

> 来源:2026-09-20,plan 168 落地后用户看着 demo 配置里的 `thinking = "adaptive"` 说
> "还是令人困扰,因为还是与 effort 有重叠"。查下来他是对的,而且**168 只是把矛盾挪了个位置**。

## 现状(168 之后)

profile 的 `thinking` 剩三个值,逐个看它买到了什么:

- **`adaptive`** —— 就是默认值。写它等于没写。
- **`off`** —— 与 `effort = "none"` 一字不差。**而且它是唯一还能配出自相矛盾请求的值**:
  `thinking = "off"` + `effort = "xhigh"` 时 `ThinkingRouting::resolve` 因为 effort 不是
  `none` 而直接返回 `default_mode`(= `Off`),渲染层再照常发 `output_config`,于是同一个请求
  里既有 `{"type":"disabled"}` 又有 `{"effort":"xhigh"}`——在 Opus 5 上是确定的 400。
  **教训 164 把这条写成了"已修",不实,本 plan 一并订正。**
- **`unset`** —— 唯一一个别处说不出来的:这个网关根本收不了 `thinking` 字段。

## 裁决

枚举整个拿掉。

> **⤴ 当天二次修订(见文末「一次加了又删」)**:第一版只留了个逃生口
> `thinking_param = false`(网关收不了这个字段)。用户看完当场追问"这个是不是说明这个参数名
> 错了,让它能兼容这种情况"——顺着查下去,结论是**这个键本来就不该存在**,最终版把它也删了。
> 下面这段保留原样,因为它是那一步的推理过程。

只留逃生口,并**用它真正的身份命名**——那不是推理设置,是一条**网关能力声明**,
和 `prompt_cache = false` 完全同类:

```toml
[providers.k-claude]
effort = "xhigh"          # 唯一的推理旋钮

# 两个网关能力开关,都默认 true,只在网关收不下这个字段时设 false
# prompt_cache   = true   # 不认 cache_control
# thinking_param = true   # 不认 thinking 字段
```

于是 `effort` 在配置文件里是**可证的**唯一推理旋钮:没有任何一个键能和它争,矛盾组合从构造上
不存在。名字用 `thinking_param`(用户拍板;备选 `send_thinking`)——`thinking = false` 会被读成
"不要思考",而它的意思是"不要发这个字段",模型照样可能思考,那个歧义正是要躲的。

### `thinking_param = false` 时 `none` 无处可落 → 拒绝,不静默

Messages 的"不要推理"就是 `thinking: {"type":"disabled"}`。网关既然收不下这个字段,
`effort = "none"` 在那里**无法表达**。两处都拒:

- 启动时(`provider_config.rs` 既有的 `effort_supported` 检查旁边)
- `/effort none` 时(`commands/provider.rs::apply` 里同一道门,它本来就在按目标模型的
  `efforts` 拒)

静默发不出去正是这几轮一直在清的东西。

## 改动

- **cli**:profile 键 `thinking` → `thinking_param: bool`(仍是 messages-only,与 `prompt_cache`
  同一条校验);删 `parse_thinking_string` 与 `default_thinking(wire)`;`Profile.thinking_param`。
- **core**:`ProviderCatalogEntry.default_thinking: ThinkingMode` → `sends_thinking: bool`;
  `ThinkingRouting.default_mode` → `send_param: bool`,`resolve` 首行 `!send_param → Unset`;
  新 `ProviderCatalog::sends_thinking(provider_id)` 给两道门用。
- `ThinkingMode` 四个变体都还在(Off 由 `effort = "none"` 产生,Budget 由模型表产生),
  变的只是**配置面**不再能直接点名它们。
- demo 配置、README、教训 164 的订正。

## 完成标准

- fmt / clippy `-D warnings` / 全 workspace test 全绿。
- 测试:`thinking_param = false` 渲染成不发字段;`thinking_param` + `effort = "none"` 启动被拒;
  `/effort none` 在这种 profile 上被拒;旧键 `thinking` 落 unknown key。
- demo 配置重新按"拷进隔离 HOME 真跑三条 rail"验一遍。

## 完成记录 ✅(2026-09-20)

- **cli**:profile 键 `thinking` → `thinking_param: bool`(默认 true,messages-only,与
  `prompt_cache` 同一条校验);删 `parse_thinking_string` 与 `default_thinking(wire)`;
  旧键落既有的 unknown key 报错,不留别名。
- **core**:`ProviderCatalogEntry.default_thinking: ThinkingMode` → `sends_thinking: bool`;
  `ThinkingRouting.default_mode` → `send_param: bool`,`resolve` 首行 `!send_param → Unset`、
  非 budget 模型直接 `Adaptive`;新 `ProviderCatalog::sends_thinking(provider_id)`。
- **两道 `none` 门**:启动(`provider_config.rs`)与 `/effort none`
  (`commands/provider.rs::apply`,和既有的 `efforts` 拒绝同一处)。
- **测试**:`none_is_refused_where_the_thinking_field_cannot_be_sent`(cli,含两条正例)、
  `a_gateway_that_omits_the_thinking_field_sends_no_mode_at_all`(core,budget 模型也照样静音)、
  `effort_none_is_refused_where_the_thinking_field_cannot_be_sent`(command,并验 `/effort high`
  仍可用)、`the_messages_rail_thinks_unless_told_otherwise` 改写(旧 `thinking` 键落 unknown key)。
- **文档**:`config/config-demo.toml` 把那两行注释改写成"两个网关能力开关,不是推理设置";
  README 同步;**教训 164 订正**——那条把矛盾写成了"已修",实际 168 只挪了位置。
- 验证:fmt、clippy `-D warnings`、`cargo test --workspace`(1598 passed)全绿;demo 配置拷进
  隔离 HOME 重跑三条 rail,URL 一如既往;新增负例真跑一次:
  `Error: provider 'k-claude' omits the thinking request field (thinking_param = false),
  so effort = 'none' cannot be expressed there`。
- 提交:`f7dd296`。

## 一次加了又删:`thinking_param`

用户追问"`thinking_param = false` 之后 `effort = "none"` 无处可落,两处都拒不静默——这个是不是
说明这个参数名错了,让它能兼容这种情况"。查下来:

**名字没错,矛盾是真的。** 这个键的字面意思就是"网关收不了 `thinking` 字段";这句话若为真,
`{"type":"disabled"}` 也发不出去,而 Messages 的 `output_config.effort` 只有 low..max、
**没有 none 这一档**,所以"不要推理"确实无处可落。改叫什么都一样。

**但同一类约束在模型层已经有更好的解法。** Fable 5.1 直接拒 `disabled`、Opus 5 在
effort > high 时拒——这些就是"这里说不了 none",而它们只要在 `[models.x].efforts` 里不列
`none`,现成的门就拦下了,一个新键都不用加。

**那 `thinking_param` 剩下的唯一用途是"网关整个不认这个字段",而这个情况是我编的。**
没有任何一个这样的网关的证据。为一个没见过的故障加了一个键、两道门、一条要写进文档的矛盾。
用户拍板:删掉。

删干净之后的形态比第一版更好:

- `ProviderCatalogEntry.sends_thinking`、`ThinkingRouting.send_param`、两道 `none` 门全消失
- `ThinkingRouting` 只剩「这条 rail 有没有这个字段」+ budget 表,**profile 一点发言权都没有**
- `resolve` 三行:非 Messages rail → `Unset`;`none` → `Off`;有 budget 表 → `Budget/Unset`;
  否则 → `Adaptive`
- 顺带修掉一个第一版带进来的脏点:chat/responses 的 attempt 原本也会带上 `Adaptive`(惰性无害
  但读着不对),现在由 rail 决定,Mock 跟 Messages 同组(与 `ANTHROPIC_MAX_OUTPUT_TOKENS` 的
  既有分组一致)
- `effort = "none"` 在所有 provider 上重新可用

代价认下来:真撞上那种网关时没有退路,只能吃一个 400——但那是**响亮**的失败,错误信息里有
endpoint URL(plan 167 加的),到那天再把键加回来是 15 行的事。**带着证据加,才是加它的时机。**

## 完成记录 ✅(2026-09-20)

- **cli**:profile 键 `thinking` 删除(不留别名,落既有 unknown key);`thinking_param` 加了又删,
  最终 profile 里**没有任何键谈论 thinking**;`prompt_cache` 的 rail 校验独立成一条错误信息。
- **core**:`ThinkingRouting{rail_has_field, budgets}`,`ProviderCatalogEntry` 不再带任何
  thinking 相关字段;`resolve` 完全由 (rail, 模型, effort) 决定。
- **测试**:`a_profile_has_no_say_over_thinking`(cli,两个退役键都落 unknown key,
  `effort = "none"` 重新可用)、`the_openai_rails_carry_no_thinking_field`(core,连 budget 表
  也变不出一个 rail 没有的字段)、`effort_renders_into_whichever_reasoning_field_the_model_reads`
  (plan 168 的,现在不再依赖 profile 默认值)。
- **文档**:`config/config-demo.toml` 明写"这里故意没有 thinking 的键";README 同步;
  **教训 164 订正**(它把 168 的矛盾写成了已修)。
- 验证:fmt、clippy `-D warnings`、`cargo test --workspace`(1596 passed)全绿;demo 配置拷进
  隔离 HOME 重跑三条 rail;并验 `effort = "none"` 现在走得通(停在占位主机 DNS,不再被拒)。
- 提交:`f7dd296`(第一版)+ `f5012c9`(删掉 `thinking_param`)。
