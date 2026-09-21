# Plan 154 — 一条结果有上限,一轮结果没有

> 来源:2026-09-15,用户问"tool 层还有能更进一步的优化吗"。与 149–153 不同,本 plan 的两条
> **不是从借鉴项目倒推的**,是读 kloop 自己代码读出来的。cc 的对照点见
> `refs/README.md` 的"调研结论"第 2 条。

## 一、工具层已经很齐,先说清楚不用动什么

`dispatch_tools`(`core/src/tools/mod.rs:697`)已有:连续同类调用打成并发批、
每个 `tool_use` 必配一个 `tool_result`(取消时补 `interrupted`,历史保持合法)、
envelope 在分类之前统一解包(`normalize_tool_uses`)、按**入参**动态判并发安全
(`is_concurrency_safe` → `Builtin::concurrency_safe(input)`)。

offload 也已经比参考项目走得远:超过 `OFFLOAD_CAP_CHARS`(32 000 字符)的结果写进
`{offload_dir}/off-NNNN.txt`,回给模型的是 head 1500 + tail 500 的预览**加真实文件路径**,
外带一句"就地查询,别读回来"的建议(`history.rs:498` 的 `spill`,注释在 `:506`)。
沙箱把 offload 目录从 private state root 的 deny 里单独 carve 出来可读不可写
(`cli/src/startup.rs:811`)。**这两块都不要在本 plan 里动。**

## 二、缺口一:并发批没有上限

`tools/mod.rs:731` 是 `futures::future::join_all(futs).await` —— 整批一起上,**没有任何上限**。

cc 的同一机制有上限 10(`refs/README.md` 调研结论第 2 条:"按入参动态并发(只读批并发上限 10)")。
kloop 抄了形态,没抄上限。

放大后果的是 kloop 自己的一个设计:`builtin.rs:482` 让**只读 bash 也算并发安全**
(`analyze_bash` 拆出的 argv 全部 `argv_is_readonly` 时)。于是一轮 N 个只读 `bash`
= 同时起 N 个进程。外部 source 同理(`source.is_readonly(name)`),N 个并发请求全部打向
同一个 MCP server。`read_file` 并发 50 只是文件描述符,`bash` 并发 50 是 50 个进程。

## 三、缺口二:offload 按单条算,不按一轮算

`history.rs:390` 只有一句判定:

```rust
if content.chars().count() > self.cap   // 32_000
```

**单条**超了才落盘。一轮 10 个工具、每个返回 31 999 字符,一条都不触发——
32 万字符(约 8 万 token)原样进上下文。全部 offload 的话是 10 × 约 2000 字符预览
(约 5000 token),**差 16 倍**。

offload 的设计意图是"别让一个结果撑爆上下文";在多结果的情况下,这个意图失效。
这条直接打在 `docs/capability-report.md` 第 1 节挂着的生存层账上。

## 四、做什么

### 一、并发批限流

- 用信号量(`tokio::sync::Semaphore`)给批内并发设上限,**不要把批切小**。
  分组语义("连续同类打一批")和 `ToolSearch` 作为排序屏障的设计是咬合的
  (`builtin.rs:469-473` 那段注释解释了为什么 `ToolSearch` 必须 `false`),
  切批会改动那个语义。
- 上限取多少要定;cc 是 10。**进程类(只读 bash)与纯读类(read_file/grep/glob)
  是否该用同一个上限**,见第六节。

### 二、一轮的结果总预算

- 在**一轮工具结果进 history 之前**判总量,超了就把其中若干条走现成的 `spill`。
  形态完全复用——模型看到的还是"预览 + 路径 + 就地查询建议",不需要新概念。
- **聚合点有两处**:`agent.rs:836` 和 `agent.rs:1036`(ordinary_calls)。
  只改一处会留下一条不设防的路径。

## 五、坑

- **不要把单条 cap 调小来解决轮预算问题**。32 000 是单条的合理线,调小会让本来正常的
  单个结果无谓落盘。两条 cap 是两件事。
- **spill 失败时当前是截断**(`history.rs:522`:`[offload to disk failed; output truncated]`)。
  轮预算触发的 spill 如果失败,不要让它把一条本来能内联的结果截断掉——回退成内联更合理,
  但这会让轮预算失守。**这个取舍要明确写进代码注释**,别留给下一个人猜。
- **offload id 是进程级 `AtomicUsize`**(`history.rs:28` 的 `NEXT_OFFLOAD_ID`),
  并发批里多条同时 spill 要确认 id 不撞、文件不互相覆盖。现在是串行 record,改并发前要看清。
- **限流不能改变结果顺序**。`dispatch_tools` 的返回必须与请求顺序一致
  (`dispatch_preserves_request_order_across_mixed_batches` 已经在测这条,`tools/mod.rs:3588`)。
- **子 agent 各自有自己的 history**,轮预算是 per-agent 的,不是全局的。

## 六、开工时问用户(先问,再动手)

**两个数,和一个取舍。**

1. **并发上限取多少,进程类和纯读类是否分开?** cc 是统一 10。kloop 因为把只读 bash
   也放进并发批,一个 bash 的代价比一个 `read_file` 高一个量级,分开设(如纯读 10 / 进程 4)
   更贴合实际,但多一个概念。
2. **一轮总预算取多少?** 可以从"单条 cap 的若干倍"起(如 3× = 96 000 字符),
   也可以按模型上下文窗口的比例算。后者更自适应,但要接 `usage`/`context` 那套估算。
3. **超预算时先 offload 哪些?** 最大的几条(保住小结果,对模型更友好)还是按顺序
   (可预测、可解释)。我倾向前者,但这会让同一轮的行为依赖结果大小排序,调试时不好复现。

## 七、非目标

- **不动 offload 的形态**(预览 + 路径 + 就地查询建议)和它的沙箱 carve-out。
- **不动单条 `OFFLOAD_CAP_CHARS`**。
- **不动并发分组规则**与 `is_concurrency_safe` 的判定,包括只读 bash 算安全这一条——
  那是有意的设计,本 plan 只给它加上限。
- **不做工具级重试**(grok 有 `tools/retry.rs`)。重试语义在 provider 层已有一套,
  工具层再来一套要单独想清楚。
- 不碰 plan 151 的两件(单工具超时、重复调用提醒)。那管"一个调用",本 plan 管"一轮多少个"。

## 八、验收

1. **并发上限生效且可观测**:构造一批 N 远大于上限的只读调用,断言同时在跑的数量
   不超过上限(用一个会记录并发峰值的 fake 工具源)。
2. **顺序不变**:`dispatch_preserves_request_order_across_mixed_batches` 仍绿;
   再补一条 N > 上限 时顺序仍与请求一致的断言。
3. **分组语义不变**:`ToolSearch` 作为排序屏障的行为有回归测试守住
   (前后各一个只读调用,断言它们没有跨过屏障合批)。
4. **轮预算生效**:一轮 10 条各 31 999 字符,断言进 history 的总量落在预算内,
   且每条被 spill 的都拿到了路径与预览(不是被截断)。
5. **两处聚合点都设防**:`agent.rs:836` 与 `:1036` 两条路径各有一条测试。
6. **未超预算时零行为变化**:一轮两条小结果,history 内容与改动前逐字节相同。
7. 仓库完成标准照旧(fmt / clippy -D warnings / test,各自单独取退出码)。

## ✅ 已完成(2026-09-16;提交 SHA 以本条所在提交为准)

开工时用户只说了一句「看参考项目」,三个待定的点全部由参考实现回答,没有一个靠拍脑袋:

| 参考 | 并发上限 | 一轮总预算 |
|---|---|---|
| **cc** | `toolOrchestration.ts:9` 统一 **10**(env `CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY`),滚动池 `all(gens, cap)` | `applyToolResultBudget` / `MAX_TOOL_RESULTS_PER_MESSAGE_CHARS` = **200 000**,单条 `DEFAULT_MAX_RESULT_SIZE_CHARS` = 50 000 → **4:1** |
| **deepseek-harness** | `agent-loop/src/constants.ts:6` 统一 **10**,bounded rolling pool(槽位空出时重新分类后续调用) | 无轮预算,每个工具自己 bounded(`ItemRetainer`/`TextRetainer`) |
| **grok-build** | 普通工具**没有**全局上限;只给 media-gen 按工具名设额(image 8 / video 4),且不排队是**拒绝**(前 K 个跑,尾部 error tool_result;超 2× 判 spam 整批重采样 + 提醒) | 无 |
| **codex** | 无批上限,`tools/parallel.rs` 的 `RwLock<()>` 只做并/串互斥 | 无 |

由此定下并落地的三条:

1. **统一上限 10,不分类。** 开工前我提过"进程类(只读 bash)/纯读类/起 agent 类分三档",
   **参考里没有先例**:cc 的 `AgentTool.isConcurrencySafe()`(`AgentTool.tsx:1467`)也返回
   `true`,子 agent 和 `read_file` 共用同一个 10;grok 唯一分类的那次分的不是成本类别,
   是"少数极贵的工具各自一个名额",kloop 没有 `image_gen` 那种量级的工具。实现是
   `dispatch_tools` 的 safe 分支里一个 batch-局部 `Semaphore`,**批不切小**(分组语义与
   `ToolSearch` 排序屏障原样),`join_all` 仍按请求顺序返回,完成的调用立刻放permit
   —— 上限数的是"真在跑的",不是"占着槽等人收结果的"。等 permit 期间被取消的调用不会
   执行:`run_one` 先看到 cancelled token,直接 settle 成 interrupted。
2. **轮预算 = 4 × 单条 cap = 128 000 字符**,照 cc 的比例。落在 `History::record`。
3. **按大小降序贪心 spill,回预算内即停**(cc 的 `selectFreshToReplace` 同款);
   **落盘失败回退内联**(cc 的 `if (replacement === null) continue` 同款),单条 cap 那条路
   没有这个选项,继续截断。两种取舍的理由都写进了 `enforce_round_budget` / `spill` 的注释。

**参考里没有、但实现时必须补的一条**:结果太小时,spill 会让一轮**变大**——preview
(head 1500 + tail 500)加上指针那段话本身就有两千多字符,一轮 80 条 1800 字符的结果
(144 000 > 预算)如果照 largest-first 一路 spill 下去,换来的是 80 个指针、总量涨到二十万。
所以每次 spill 后比一次长度:指针不比原文短就把文件删掉、结果放回内联,并**就此停手**
(结果是按大小降序走的,后面的只会更不划算)。cc 的 `selectFreshToReplace` 没有这道门
——它按"减去整条大小"估算,选完就 persist——因为它的单条阈值是 50 000、预览 2 000,
两者差 25 倍,小结果进不了那条路;kloop 的比值窄得多,这道门是必需的。

**plan 第四节的"两处聚合点"是错的,已在实现里消解。** `agent.rs:1036` 的
`dispatch_structured_tools` 只**返回**结果,它和普通路径最后都汇到 `agent.rs` 那一句
`history.record(Message::tool_results(results))` —— 生产里把工具结果写进 history 的地方
**只有这一处**(另一处 `rollout.rs` 是 resume 时给孤儿 tool_use 补 `interrupted`)。预算做进
`record`(单条 spill 已经在那里)就天然覆盖两条路,不需要两处设防;`a_wide_round_lands_under_budget_on_both_dispatch_paths`
仍按验收把两条路各跑一遍,钉住这个结构。

**cc 有一半复杂度 kloop 不需要**:它的 `seenIds`/`replacements` 冻结 + 写进 transcript,
是因为它在**组请求时**才替换,同一条结果每轮重算,不冻结就掉 prompt cache。kloop 在
`record` 当场落盘、一次写死,天然稳定,这套状态机整个不用要。

顺带把 `spill` 拆成 `spill_to_disk`(`io::Result`,只管写盘与指针)+ 自由函数 `preview`,
两条 spill 路径共用同一段 head/tail,失败与成功的文本到指针为止逐字相同。

测试(全部有 negative control):
- `a_concurrent_batch_runs_at_most_the_call_limit_at_once` —— 25 个只读外部调用,
  `ConcurrencyProbe` 记录峰值;把 permit 数换成 `Semaphore::MAX_PERMITS` 后断言 25 ≠ 10 失败。
  同一条测试断言顺序仍是 t0..t24。
- `tool_search_still_splits_the_batch_it_sits_between` —— 屏障两侧不合批(第一条阻塞时
  第二条永远起不来),**同测试内**带负对照:去掉中间的 `tool_search`,同样两条调用峰值为 2。
- `a_wide_round_spills_its_largest_results_until_it_is_under_budget` —— 十条结果 175 000 字符,
  只有最大的两条(且不在请求序的前两位)落盘,其余八条逐字未动,总量回到预算内;
  去掉 `enforce_round_budget` 调用即失败。
- `a_round_under_budget_is_recorded_verbatim` —— 整对象相等,且 offload 目录**不存在**。
- `a_round_of_results_too_small_to_shrink_is_left_alone` —— 80 条 1800 字符(144 000 超预算),
  一条都不落盘、整对象相等、offload 目录里 0 个文件;去掉长度比较那道门即失败。
- `a_spill_that_cannot_reach_disk_stays_inline_for_the_round_budget_only` —— 在 offload 目录
  的位置放一个文件让 `create_dir_all` 失败:轮预算那一轮整体逐字保留,单条超 cap 的那条
  仍带 `offload to disk failed ... output truncated`。
- `a_wide_round_lands_under_budget_on_both_dispatch_paths` —— agent 层,ordinary 与
  structured 两条 dispatch 路径各一轮六条。

`cargo fmt` / `cargo clippy --workspace --all-targets -D warnings` / `cargo test --workspace`
各自单独取退出码,全绿。README 的赌注 1、赌注 3 与"Parallel sub-agents"三节同步;
`docs/capability-report.md` 销掉两笔账(工具结果分档预算、Agent 并发上限)。

**非目标全部守住**:offload 形态、沙箱 carve-out、`OFFLOAD_CAP_CHARS`、并发分组规则与
`is_concurrency_safe`(含只读 bash 算安全)一行未动;没做工具级重试,没碰 plan 151。

## 已知边界:轮预算能 spill 掉"读回 offload 文件"的那次读

收尾时用户问"`read_file` 的 30 000 字符合理吗",顺着查出来的一条**本次改动新开的门**,
记在这里,当前不修。

`OFFLOAD_CAP_CHARS` 的注释写着它的存在理由:读回一个 spill 文件的回复**不能再次 spill**,
否则模型拿到的是同一份内容换个路径的预览,"逃生口在恰好需要它的尺寸上失效"。plan 111
删掉 `read_offloaded` 之后,这条不变量的担保人就是 `READ_CONTENT_CHARS = 30_000 < 32_000`
—— 而这是一条**单条结果**的保证。

轮预算是按**一轮**算的,于是:`read_file(off-0007.txt)` 只要和另外四个 ~30 000 字符的读
同轮(5 × 30 000 = 150 000 > 128 000),就可能被选中落盘,模型在"我要读那个文件"的地方
拿到 `off-0012.txt` 的又一个指针。

**当前不修,理由三条**:(a) 要凑够五个满额结果同轮,且被读回的那个恰好排进最大的几条;
(b) 模型能自救——用更窄的 `offset` 重读,或直接 grep;(c) 轮预算本来就是"这一轮太占上下文"
的判断,一轮 150 000 字符确实该瘦身,给某个工具开豁免是收益不明的复杂度。

**真要修的话形状是现成的**:cc 的 `enforceToolResultBudget` 带一个 `skipToolNames`,
给"自己已经 bounded 的工具"(`maxResultSizeChars: Infinity`)豁免于轮预算 —— kloop 对应的
就是 `read_file`,它的 `READ_CONTENT_CHARS` 正是那种自我约束。触发条件:真见到一次模型
读 offload 文件却拿回指针。

## 顺带结的一笔账:`READ_CONTENT_CHARS = 30_000` 该不该抬——量过了,不该

同一次问答里我提过一个改动建议,**量完自己否掉了,记在这里免得下次再提一遍**。

建议是:那条"`read_file` 上限必须全局低于 offload 阈值"的不变量,其实只有"读 offload
文件"这一条路径需要(plan 111 把担保责任交给它时就是为这个),窄化成"目标在 offload 目录
内时才封顶",读上限就能跟 cc 一样往上走。当时的依据是"kloop 自己 37% 的 `.rs` 文件
超过 30 000 字符,整文件读要分页"。

**这个依据是错的:文件大小的分布不是行为的分布。**拿本机 141 份 rollout 量了一遍
(`scripts/tool-usage.py read_file --since 20260903 --lines-vs limit`):

| | 7 000 时代(≤2026-09-02) | 30 000 时代(>2026-09-03) |
|---|---|---|
| 成功读取 | 1 499 | 1 654 |
| **被字符预算咬到** | **628(41.9%)** | **24(1.5%)** |
| — 给了 `limit` 却少拿到行 | 593 | 12 |
| — 整文件读被截断 | 35 | 12 |
| 模型自己带 `limit` | 39.6% 的读取带续读提示 | **95.7% 的调用带 limit**,中位 150 行、p90 280 |
| 结果字符数 | 中位 7 009 | 中位 5 485,p99 30 037 |
| 同一目标重复读 | — | **70%**(498 个目标 / 1 654 次) |

两条结论:

1. **plan 106 那次抬对了,而且收益已经吃完了。**41.9% → 1.5%。再抬三倍,最多再省下这
   1.5% 里的一部分——而模型 95.7% 的读取自己只要 150 行,30 000 字符(Go 代码约 800 行)
   已经是它要的五倍。
2. **往返真正花在重复上,不在分页上。**30 000 时代重复率仍有 70%,单个文件最多被读
   25 次;plan 106 当年测到 82%,一年多没动过。这是 **plan 151**(重复工具调用 advisory)
   的活,不是读上限的活:抬上限受益 24 次,告诉模型"这个文件你已经读过 25 次"受益 1 156 次。

**证据的边界**:1 665 / 1 691 次来自同一个项目桶(一个 Go 仓库的代码审查),一个模型、
一类任务;且整份语料里 `edit_file`/`write_file` 只有 8 次,**所以它不能用来判断 plan 155**
那条"改一行先付六次读"——那条的前提在这份数据里没有样本。

顺带记下的对照:**cc 超限是抛错不是截断**,而且有实验记录(`FileReadTool/limits.ts` 头部,
#21841,2026-03):试过改截断,tool 错误率下降但平均 token 上升(抛错回 ~100 字节,截断回
25K token),于是回退。kloop 现在是截断。cc 的 Read 上限是 25 000 token(≈100 000 字符)、
persist 阈值 50 000,**大读取允许落盘换预览**——它不立 kloop 那条不变量。真要重开这个话题,
先读 plan 106 片 16:它证明这类改动的收益落在 1.8 倍的运行方差里,量不出来。
