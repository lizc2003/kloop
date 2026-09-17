# Plan 159 — 选 provider、选 model、选 effort,是同一条路的三个入口

> 来源:2026-09-17,plan 158 收尾后用户提的 UX 改造。原话:「effort命令应该改为model命令,
> 在选择完model后,继续让用户选择effort,以及应该让用户从列表里选,而不是让用户手工敲。
> provider命令也是同理」。追问 `/effort` 的去留时拍板:**「/effort可以保留,表示在当前
> model上进行选择」**。
>
> 这一条定了整个形状:三个命令不是三件事,是**同一条向导的三个起点**。

## 一、现状:三级里已经有两级

plan 104 已经把四个交互界面统一到内联面板 `tui/src/choice.rs`,provider picker 是其中之一
(`render.rs` 的 `provider_panel`)。`app.rs` 的 `ProviderPicker` 已经是**两级**:

- `provider_cursor` 选 provider,Enter 进入下一级;
- `model_cursor: Option<usize>` 选 model,Enter 合成 `/provider <id> <model>` 发出去;
- ↑↓ / jk / 数字键直选 / Esc 逐级回退都有,而且 `remembered_models` 会记住每个 provider
  上次选过的 model。

入口也已经在:`/provider` 不带参数时返回 `open_provider_picker = true`(`commands/provider.rs`)。

**缺的只有 effort 那一级**——`commands/effort.rs` 至今返回 `open_picker: false`,所以 effort
只能手敲。本 plan 补的是这一级,外加两个新起点。

## 二、目标形状

| 命令 | 从哪一级开始 | 说明 |
|---|---|---|
| `/provider` | provider → model → effort | 完整三级 |
| `/model` | model → effort | 在当前 provider 内换模型 |
| `/effort` | effort | 在当前 model 上换档位 |

带参数时一律**直接执行、不开 picker**,和今天 `/provider <id> <model>` 的行为一致:
`/model <model>`、`/effort <level>` 都保留。列表是给"不想记名字"的时候用的,不是取代参数。

## 三、设计

### 3.1 一个状态机,三个入口

`ProviderPicker` 的两个游标换成一个显式的级:

```rust
enum Stage { Provider, Model, Effort }
```

入口决定初始 `Stage`,Esc 逐级回退,**退到入口那一级就是关闭**——`/effort` 开的 picker 按一次
Esc 直接关,不会掉进它没打算展示的 model 列表。这条是入口语义的一部分,不要用"总是退到
Provider"糊过去。

`remembered_models` 保留。effort **不额外记忆**:它已经有 session 级状态
(`SessionProviderState::effort()`),默认光标就落在当前值上;当前值不在列表里时落在该
provider 的 `default_effort`,再没有就落 `unset`。

### 3.2 effort 那一级列什么

列表 = **`unset` + `none` + (该模型声明的 `efforts`,没声明则全六档)**。

两条都不是凑数的:

- **`unset` 必须是独立一项,它和 `none` 不是一回事。** 2026-09-17 对
  `deepseek-v4-flash-0731` 实测:不发 effort 字段时模型照样推理(`reasoning_tokens: 64`),
  `effort: "none"` 才真的归零(`reasoning_tokens: 0`,输出连 reasoning block 都没有)。把
  "不发字段"和"发 none"画等号,是把一个真实存在的第三态抹掉。
- **`none` 没有特殊待遇**,和别的档位一样:声明了 `efforts` 就按声明列,没列 `none` 就不列。
  (最初设计过"`none` 永远在列表里",2026-09-17 推翻——它在 `chat`/`responses` 两条 rail 上是
  普通 wire 值,模型可以拒绝,豁免它等于放过列表本来要挡的那个值。见 plan 158 §四之四。)
  `unset` 则始终在列表里:它不是档位,发的是"没有 effort 字段",没有列表能约束它。

其余档位**只列该模型声明过的**(plan 158 的 `[models."<id>"].efforts`)。这让知识段有了第二个
用途,也让"声明写漏了"这件事从"某天 `/effort xhigh` 莫名被拒"变成"列表里肉眼可见地少一行",
更容易发现。没声明 `efforts` 的模型列全六档——不声明就是不设限,和校验那边同一个语义。

每行带 `choice::Item::with_detail`,detail 写一句人话(例:`none` → "不做推理";`unset` →
"不发 effort 字段,用服务端默认")。

### 3.3 完成时发**一条**命令,不是两条

picker 走完发 `/provider <id> <model> <effort>` 一条。**不要发 `/provider` 再发 `/effort`**:
那会在 route timeline 上留下两个 revision,transcript 里看起来像用户改了两次主意。

于是参数形式扩成:

- `/provider <id> [model] [effort]`
- `/model <model> [effort]`
- `/effort <level>`

`/effort unset` 的现有拼写保留(`commands/effort.rs` 里那句注释解释过为什么不叫 `off`)。

### 3.4 非 TUI 前端

`open_provider_picker` 在 headless / app-server 里本来就是 no-op。这三个命令**不带参数时仍要
打印可选项的文本列表**(今天 `/provider` 就是这么做的),不能因为有了 picker 就只回一句
"请在 TUI 里选"。新增的 `/model` 要照这个写,`/effort` 现有的状态行也保留。

## 四、验收

- `/provider`、`/model`、`/effort` 三个入口各自从正确的级开始,Esc 在入口级关闭而不是下沉。
- effort 列表:声明过 `efforts` 的模型只列声明的那些 + `unset` + `none`;没声明的列全六档;
  三种情况各一条测试(整列表断言,不是断言某一行存在)。
- 走完三级只产生**一个** route revision——对着 rollout 的 route timeline 断言长度。
- 数字键、↑↓/jk、Esc 逐级回退在新的三级上都有测试(沿用 `app.rs` 现有的 picker 测试形状)。
- 非 TUI 前端下三个命令不带参数都回文本列表。
- `/model` 进 slash 补全菜单与 `/help`。
- fmt / clippy `-D warnings` / `cargo test` / `--mock --headless` 全绿;README 的命令一节同步。

## 六、附录:2026-09-17 的实测数据(§3.2 的依据)

对 `deepseek-v4-flash-0731`(走 gw-cn 的 responses 端点)实测。**这些是那一天、那个网关、
那个模型的测量,不是普适事实**——换模型或换网关要重测,别当常量用。

**(a) 哪些档位不被拒。** `cargo test -p kloop-provider --test effort_probe -- --ignored --nocapture`
(env:`KLOOP_PROVIDER=openai-responses` + `OPENAI_API_KEY` / `OPENAI_BASE_URL` / `OPENAI_MODEL`),
七行全是 `ACCEPTED`——包括 `max`。**"不被拒"是这个探针能回答的全部**,它不报告档位有没有生效。
探针档间 sleep 45 秒(被限流的代理会对每行返 429,那就什么都测不出来),跑完约 5 分半。

**(b) 有没有生效。** 同一道简单题,直接打 `/v1/responses` 看 `usage`:

| effort | reasoning_tokens | output |
|---|---:|---|
| (不发字段) | 64 | reasoning+message |
| `none` | **0** | **message** |
| low / medium / high / xhigh / max | 53~110 | reasoning+message |

**`none` 是真开关**(归零,连 reasoning block 都不再产生),而**"不发字段" ≠ `none`**——这就是
§3.2 要求 `unset` 独立成项的实测依据。中间几档在这道题上没拉开差距,因为题太简单。

**(c) 档位之间有没有区分度。** 换一道真需要算的题(求 n! 恰好 100 个尾零),跑两轮:

| effort | 第一轮 | 第二轮 |
|---|---:|---:|
| low | 509 | 691 |
| high | 1166 | 1629 |
| max | **5418** | **665** |

`low → high` 两轮都稳定翻倍以上,**档位确实生效**;但 `max` 两轮差了八倍。n=2 下不了强结论,
够说明的是 **`max` 的成本不可预测**——如果将来要给 effort 那一级的 detail 写一句人话,`max`
那行值得提这一点。

## 五、非目标

- **不改 `choice.rs` 的渲染模型。** plan 104 定的内联面板照用,本 plan 只多喂一级数据。
- **不做 effort 的 per-model 记忆。** session 级的当前值已经够用,多一层记忆就要回答"换了
  模型之后该记哪个"这种没人问过的问题。
- **不碰 `/provider` 的 `remembered_models`**,也不改它的记忆语义。
- **不动 effort 的 wire 行为**,本 plan 纯 UX:哪些档位合法、`none` 怎么翻译成 thinking
  disabled,都是 plan 158 已经定下的。
