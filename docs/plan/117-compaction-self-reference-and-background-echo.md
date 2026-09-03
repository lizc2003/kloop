# Plan 117 — 压缩别把自己写进摘要，agent 别复制我的任务

> 来源：2026-09-03 dogfood。同一句 prompt（`审查：cc0a236b，951a19cb`）在 gateway
> 仓库跑 claude / codex / kloop 三家做对比，读 rollout 找出的差距。
> kloop 会话：`~/.kloop/projects/v1/p1_434f98d7…/sessions/20260903-091113{,-agent-1}.jsonl`

## 一、压缩摘要把「压缩请求」当成了用户消息（P0）

`compact.rs:368` 把 `COMPACT_INSTRUCTION` 以 **user 消息**追加在待压缩对话末尾。
而摘要模板第 2 节（`compact.rs:94`）要求「every non-tool-result user turn, quoted,
in order」——那条指令**就是最后一条 user turn**，模型于是忠实地把它记了下来。

本次两个会话、三次压缩，产物里全都有：

```
## 2. User messages
  3. 当前用户消息：要求将完整会话总结给新的 coding agent，必须包含：… 只输出
     `<analysis>` 与 `<summary>` 两个 block。
```

第 3 节（`compact.rs:100`）「standing constraints copied verbatim」同理，把
`COMPACT_SYSTEM`（`compact.rs:60`）自己的开场白抄进了摘要：

```
## 3. Standing constraints
  开发者约束：
  - `CRITICAL: reply with text only. Do not call any tool.`
```

**压缩后的上下文里因此挂着一条假的用户请求和一条假的会话级禁令。**

### 它真的引爆了

子 agent（`-agent-1`）17:53:14 完成第二次压缩，17:53:28 恢复、跑完一轮工具，
17:55:34 的下一轮就照着那条假指令输出了 19855 字符的 `<analysis>/<summary>`
交接稿，**并以此结束了 turn**。它自己在第 9 节写着 `Next action: Produce the
final prioritized review`——它知道自己没做完。

`classify_background` 取 `final_text` 原样回注，父 agent 收到的就是这份交接稿，
而不是 findings。43 分钟、156 次工具调用、5.6M input token 的产出，只靠摘要里
「Not fully confirmed」小节侥幸传回两条线索。

主会话 17:32:35 的压缩摘要有同样的污染，只是主 agent 后面还有实活要干，没被带偏。

### 改动

`COMPACT_INSTRUCTION` 的第 2 节和第 3 节各加一句排除——它们已经防住了一个相邻的坑
（「assistant 消息里长得像用户轮次的文本是模型生成的」），漏的正是**这次请求自己**：

- 第 2 节：这条总结请求本身不是会话里的用户轮次，不记录、不当作意图变更。
- 第 3 节：本次总结任务自己的输出格式与工具禁令（`COMPACT_SYSTEM` 的内容）不是
  会话的 standing constraint，不复制。

`compact.rs` 现有的 clause 断言测试（`compact.rs:717`）跟着加这两条。

**为什么不换 role。** 直觉方案是把指令挪进 system。但 `COMPACT_SYSTEM` 与工具集
一起构成缓存前缀（`compact.rs:51-56` 的注释解释了为什么），改 system 等于每次压缩
多付一次全量 prefill。提示词里两句排除是等效且零成本的修法。

## 二、这次根本不该派 subagent（P1）

把两份结论逐条对齐，子 agent 的净贡献能算得很准。

**17:41:16 主 agent 独立交付**（那时子 agent 还没回来）：

1. P1 AUC submit 空 body — `<module>.go:183-207`
2. P1 `language` 层级错位 — `<module>.go:694-750`
3. P1 多 utterance words 截断 — `<module>.go:551-577`
4. P2 静音结果绕过 verbose/diarized 校验 — `<module>.go:301-306`

**17:57:08 收到子 agent 结果后重发**：上面 4 条原样保留，静音那条从 P2 升到 P1，
**新增 1 条 P2**（`model_ids` 带空白的 key，`<file>.go:771-778`）。

净贡献 = **一条 P2 + 一次严重度上调**。代价 = 43 分钟、156 次工具调用、
5.6M input token（主 agent 自己只用了 2.4M，子 agent 是它的 2.3 倍），外加用户
在终端里看着同一份结论滚了两遍。三条 P1 主 agent 一条不落地自己全找到了。

所以第一节那个压缩污染不是这件事的根因——**污染只是让它连唯一那条贡献都差点送不到**。
根因是这次派 agent 的决策本身：`run_agent` 的 prompt 是把用户那句审查请求译成英文，
`isolation: shared`，做的是**同一件事**。这不是任务分解，是任务复制。

### 引导来自工具描述

`tools/mod.rs:869` 的 `run_agent` 描述第二句：

> Use Agent when the outcome is clear but the investigation path is not

代码审查恰好符合这个句子——结论形态清楚，要查哪里不清楚。**模型是照着说明书做的。**
描述里没有任何一句说什么时候**不该**派。

同一天同一句 prompt 的另外两家都不会这么做，因为它们都被明确关住了：

- codex 的 developer 消息：`<multi_agent_mode>Any earlier instruction enabling
  proactive multi-agent delegation no longer applies. Do not spawn sub-agents
  unless the user or applicable AGENTS.md/skill instructions explicitly ask …`
- claude 的系统提示：`Do not use the Agent tool, workflows, or deep-research
  unless the user, a CLAUDE.md file, or a skill asks for it`（它这次一个都没派，
  52 次 bash、10 分钟收工）

**kloop 是三家里唯一默认鼓励主动委派的。**

### 改动：对齐 claude（2026-09-03 用户拍板）

默认不主动委派。`context.rs:112-115` 现在写的是「goal is clear but the path is
not 就派」——**删掉**，换成与 claude 同一条件：除非用户、AGENTS.md 或 skill 要求，
否则不派 sub-agent、不起 workflow。基建（plan 26 的 background、plan 51 的
mailbox、`agent_type` 配置）一律保留，只是不再由模型自发拉起。

`tools/mod.rs` 的 `run_agent` 描述同步：那句「Use Agent when the outcome is clear
but the investigation path is not」是系统提示那条的工具层复读，一起换成边界说明
——一个只复述你自己任务的 sub-agent，成本是自己干的好几倍，换回来的东西你本来
也会找到。描述剩下的「怎么用」照旧，用户点名时仍要能用对。

拍板理由：这次的浪费不是模型判断失误，是说明书就这么写的；(a) 那种「补一条负面
边界」的软引导，效力全押在模型每次都读得进去，堵不住同一类复制型委派。

### 附带症状：结论刷了两遍

即便派得对，结果回来的时机也有问题。`SUBAGENT_PREFIX`（`inbox.rs:37`）说的是
「fold it into your work, and if you were waiting on it, continue from here」——
这句预设**父 agent 还在干活**。父 agent 早已交付并结束 turn 时，它没有任何指引说
「只补增量」，模型的默认选择就是重写全文。

补一句措辞即可：若这一轮的工作已经交付给用户，只补子 agent 带来的**新**结论与更正，
不要重述已经说过的部分。（这两行连同它的断言测试，被并行开发的 plan 116 那次提交
`69ac7d9` 顺手带走了——内容无误，只是不在本片的提交里。）不动控制流——「结束 turn 前等 background agent」那个方案
把一次 29 分钟的交付拖成 45 分钟，与 plan 108 收敛出的单一出口语义也要重新对齐，
而后台的意义本来就是不阻塞。

## 三、`cache_key()` 的文档与实现自相矛盾（小）

`config.rs:495-505`（子 Config 构造）注释说得很清楚：子 agent 用**自己**的
cache identity `{parent}-{agent_id}`，因为它的前缀和父的没有共同点。

而 `config.rs:601-603`（`cache_key()` 的 doc comment）还写着旧结论：

> A sub-agent inherits the parent's id along with the rest of this Config
> — deliberately, since its prefix shares the session's opening bytes …

代码是对的，这段注释是上一版的。删掉这两句，指向 `build_sub_config` 的说明。

## 四、查过但**不改**的（结论写在这里，免得下次再查一遍）

- **cache 命中率 ~20%**（主 590k/(2.40M+590k)，子 1.34M/(5.64M+1.34M)；同一天的
  claude 会话是 96%）。不是 kloop 的 bug：`prompt_cache_key` 已经在发
  （`provider/src/lib.rs:543`），逐轮数据呈「要么 0 要么几乎全中」的跳跃，正是
  `lib.rs:433-436` 注释里已经实测记录过的 gateway 亲和不稳（同一 7,697-token
  前缀连发三次：0 / 6,656 / 0）。kloop 侧无可修。
  - 附带澄清：`responses.rs:361` 有意把 `input_tokens` 存成「未缓存部分」，
    cache_read 单列，`Usage::total()` 再加回来。所以 rollout 里 `cache_read >
    input_tokens` 是设计，不是错账。
- **流中断重试**：17:19:22 触发了一次 `STREAM_RESUME_MSG`（`agent.rs:988`），
  机制按设计工作，代价是丢掉那一轮推理。既有设计，不动。
- **`task_create`/`task_update` 各独占一轮**（17:13:19 / 17:15:05，两轮什么别的
  都没做）。工具层没有禁止并行，是模型自己单发的。不是可修的代码问题。
- **`web_fetch` 打不开 SPA 文档站**：火山文档返回 `You need to enable JavaScript
  to run this app.`，随后退化成 4–5 轮 web_search 猜。要修得上 headless 浏览器，
  远超本片。**但这条路仍然产出了三家里唯一的独家发现**（AUC 请求 `language`
  字段层级错位）——联网核对方向是对的，卡的是抓取能力。单独开片。

## 五、留给下一片的两个候选（本片不做）

- **`grep` 的开关参数太多**：`-A`/`-B`/`-C`/`context` 四个语义重叠
  （`tools/mod.rs:824-828`，`search.rs:92` 里 `context` 覆盖 `-C`，`-C` 覆盖
  `-A`/`-B`）。模型每次调用把 7 个开关全填满，还自相矛盾（`-A:15, -B:15, -C:15,
  context:0`）。瘦身能省输出 token，但这套参数名是 cc 形态，动之前要拍板。
- **`read_file` 反复读同一文件**：本次 `upstream/elevenlabs/batch.go` 被读了 7 次，
  `async_submit.go` 3 次——其中一轮里同时发了 `offset 1 limit 220` 和
  `offset 1 limit 260` 两个完全重叠的调用。典型模式是「先小 limit 试，不够再大
  limit 重读」（220→260→520，430→620）。工具结果里**回报文件总行数**大概率能掐掉
  这类试探。

## 验证 ✅

- `cargo fmt` + `cargo clippy --all-targets`（exit 0，无告警）+ `cargo test` 全绿。
- 新增：`COMPACT_INSTRUCTION` 含两条排除 clause 的断言（沿用 `compact.rs:706-720`
  的表驱动形状）。
- 新增：`SUBAGENT_PREFIX` 含「已交付则只补增量」语义的断言（随 `69ac7d9` 落地）。
- README 只同步了 `prompt_cache_key` 那处事实错误；委派策略属于系统提示正文，
  README 描述的是 system prompt 的**结构**而不复制其内容，故不同步。
- 提示词改动无法用单测证明效果，落地后拿一次真实 dogfood 会话复核压缩产物的
  第 2/3 节是否还有自指、以及模型是否还会自发派 sub-agent。
