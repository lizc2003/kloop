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
