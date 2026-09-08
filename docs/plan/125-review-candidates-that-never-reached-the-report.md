# Plan 125 — 想到了六条，报告里一条都没有

> 来源：2026-09-08 02:27 第五轮三方对照。同一个 prompt「审查：d54c1bfa」，同时
> 交给 claude、codex、kloop 跑同一个 gateway 仓库（`被审仓库`），
> 读三份 rollout 做的对比。被审的 `d54c1bfa fix(gemini-files): limit remote
> reconciliation by region config` 给 Gemini Files 的后台远端对账加了一个启动开关，
> 14 个文件、+275/-21。
>
> **对照条件已核实**：kloop 与 codex 同 provider（`gw_router`）、同模型
> （`gpt-5.6-sol`）、同 API family（Responses）、同 effort（`xhigh`）。kloop 这次
> 加载的 `code-review` SKILL.md 与当时 HEAD（`7b47c0a`，plan 123）**逐字相同**
> ——rollout 里 skill 工具的返回体与仓库文件 diff 为空。也就是说 plan 122 第二轮、
> plan 123 补进去的条款，这一轮是**带着跑的**。

## 一、结果：kloop 结论对了，但对的理由是错的

| | 采样轮 | 工具调用 | 未缓存 input | 命中率 | out | 墙钟 | findings |
|---|---|---|---|---|---|---|---|
| kloop | 26 | 64 | 121k | 96% | 17k | **33.7 min** | 0 |
| codex | 32 | 31 | 280k | 91% | 14k | 29.7 min | 0 |
| claude | 34 | 42 | 0.1k | 直连不可比 | 60k | **8.8 min** | 3 |

（claude 的采样轮按 `requestId` 去重取 34；逐条 usage 记录有 70 条，含流式分片，
按条累加就会重犯 plan 122 第五节那个错。）

claude 报的 3 条 finding，在随后 5 轮追问里**全部倒塌**：finding 2、3 明确撤回，
finding 1 降级为潜在配置风险。推倒它们的是一个仓库里没有的事实——用户说的
「北京的网络访问不了 gemini」，BJ 根本建不出 `upload_uncertain` 行。**所以 0
findings 是正确结论**，kloop 和 codex 都对。

但 kloop 排除这些候选时给的理由是：

> 「符合提交文档所述的『只影响后台远端对账』」、「这是文档明确声明的设计」

它不是靠推翻后果排除的，是靠**采信提交自己写的文档**排除的。而 skill 开头第
15-18 行就写着：a doc the change writes is a promise the change is making —— read
each sentence against what the code actually guarantees。claude 的 finding 3 恰恰
是「文档把影响面说小了」。同一段代码，claude 拿代码验文档，kloop 拿文档验代码，
方向反了。**结论是蒙对的。**

## 二、六条候选进了推理，没进报告

kloop 的 rollout 里，reasoning 标题出现过、最终报告的候选清单里**一条对应都没有**
的至少 6 条：

- `Evaluating quota impact from disabled remote cleanup` ← 这正是 claude finding 1
  的核心后果（死预留占满 scope 配额），kloop 想到了
- `Investigating hidden startup reconciliation bug`
- `Identifying misleading logger message`
- `Identifying unforwarded config flag`
- `Identifying rollout config compatibility issue`
- `Examining test flakiness due to timing`

skill 里对这件事的措辞是全文最重的一句：*A candidate you thought about across
several tool calls and then dropped without a line is the exact failure this skill
exists to prevent.* plan 119 的 commit 标题就叫「审查候选不许静默消失」，plan 122
又点了一次。**这是同一根因第三次复发，且这次是在规则补齐之后。**

文本层面已经无话可加了。缺的是「写在哪里」：**kloop 的 reasoning 正文是加密的**
（rollout 里 `thinking` 字段只有 32 字节标题，正文在 1.6k 的 `signature` 里），模型
自己也读不回来。要求「keep a list of every suspicion」在 26 轮采样、64 次工具调用
之后就是一句空话——候选没有留下痕迹，所以没有裁决。

顺带两条同源观察：报告里排除清单第 3 条「关闭后 `provider_observed_at` **仍会**被
后台更新——排除」假设方向是反的（真问题是不再更新、成本转嫁前台）；以及要不是
reasoning 标题够具体，这 6 条蒸发根本查不出来。

## 三、封闭的报告：只有用户能答的问题，被自己填平了

kloop 全程严格遵守了 skill 的 *do not go looking outward*，没有去查任何外部文档。
但它遇到的关键未知——BJ 的 Nacos 里到底有没有 `google_direct` channel、这个开关
为什么要加——是仓库里查不到、只有用户能答的。skill 现在给这种情况的唯一出口是
「Reserve "not enough evidence"」，而 kloop 没有用它，**它用「文档这么说」把缺口
填平了，报告因此是封闭的，没给用户任何接口。**

claude 的报告结尾是一个问题（「BJ 的 `/v1/files` 上传现在是什么行为？这是已知可
接受，还是也值得一并收掉？」）。就是这个问题引出了后续 5 轮，最终让 claude 定位到
一条它自己确认的真缺陷（`store.go:401` 的 `continue`，咬的是 flag 为 `true` 的 US）。

**一份 0 findings 的报告如果不留下可回答的问题，用户就没法接着往下走。**

## 四、要做的三条

### 1. 候选写在能读回来的地方

`## Every candidate gets a verdict, in writing` 一节，把「keep a list」改成
「候选一产生就写进本轮可见文本」，并说明原因：reasoning 不是能读回来的地方，
二十次工具调用之后，只在脑子里想过的候选连同它的裁决一起消失。

### 2. 排除理由不许是改动自己的话

`excluded` 的定义处收紧：提交信息、新写的文档说「只影响 X」，那是作者的信念，而
**信念是否成立正是手上这条候选**。排除必须落到「这条路径的实际后果是 X，X 无害」。
这条与既有的「deliberate ≠ correct」不同：那条管的是*意图*，这条管的是把改动的
*自述*当证据。

### 3. 只有用户能答的事实，问出来

`Verifying` 一节补出口：本地后果先追到底（大多数看起来需要外部事实的问题其实不
需要），剩下真正只有用户知道的（那个环境部署了什么、这个开关为什么加），把问题写
进报告——追到了哪一步、每个可能的答案会改变什么、各自导向哪个裁决。**悄悄选那个
让自己没有 finding 的读法，不是把问题解决了，是把它藏了。** 这是审查唯一向外看的
地方，且向的是用户，不是上游文档。

`## The report` 一节呼应一条：有开放问题就放在最后，写成用户一行能答的形式。

### 4.（顺带）把 plan 122/123 加的条款也钉进测试

`skills.rs` 的 `code_review_keeps_its_load_bearing_clauses` 断言列表停在 plan 119
那批。plan 122 的「修复建议要验证」「一条触发路径不等于全部」、plan 123 的「改动
接手了一类问题」都没被保护——删掉不会有测试变红。这次一并补入，连同新增三条。

补的时候撞到一件事：**断言片段不能跨行**。SKILL.md 是 wrap 过的 markdown，plan 123
那条在文件里断成 `the rest of that class is in` / `scope.`，`body.contains()` 永远
匹配不到，写下去就是一条必红的断言。选片段时逐条回文件核过一遍（15 条全部单行命中）
才提交。这是教训 111(c)「锚点现读现取」在 markdown 上的形状：**不只是措辞会变，
换行位置本身就是措辞的一部分。**

## 五、非目标

- **不去改 kloop 的 reasoning 落盘**。正文在加密 signature 里是上游形态，模型侧
  连续性没断（这次中途一次 transient 断流后的续传是好的），只是人读 rollout 时看
  不到推理正文。真正要修的是「候选不该只存在于推理里」，不是「把推理挖出来」。
- **不调 kloop 的工具调用节奏**。64 次工具调用是 codex 的 2 倍、墙钟最长，但未缓存
  input 只有它的 43%、命中率高 5 个点，形态与 plan 122 第一轮一致（每轮塞更多工具
  调用），不是这次要动的东西。
- **不因为这次「0 findings 是对的」就放松门槛**。三条改的都是过程，不是激进度。
