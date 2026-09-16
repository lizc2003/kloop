# Plan 151 — 模型在原地打转,没有人发现

> **2026-09-16 完成,两次提交。**第一件(重读提醒)开工前用户问"你推荐呢",量完三个
> plan 没算过的数后改了三处判定(第二节"三处改写");第二件(每工具超时)按教训 143
> 先读了 cc/dsh 的取值,又改了两处(第二节"两处改写")。收尾见第七节。

> 来源:2026-09-15,借鉴项目调研后按 macOS-only 前提重排的第三条。两件都来自
> `refs/deepseek-harness` 的 `packages/guard/*`(见 `refs/README.md` 2026-09-15 节),
> 是设计语义而非代码——dsh 是 TS,这里按规格重实现。

## 一、两个洞,都在同一个位置

`dispatch_tools`(`core/src/tools/mod.rs:697`)→ `execute_tool`(`:1247`)这条路上:

**洞一:没有人数过模型是不是在重读自己已经拿到的东西。** 全仓 grep `repeat` 只命中字符串
`"x".repeat(201)` 和文档里的 "repeating it verbatim",没有任何循环检测。模型反复读同一个
文件的同一段,现在没有任何反馈——它看不到自己在打转,只看到又一个正常的工具结果。

> **2026-09-16 实测:这个洞是真的,但本 plan 原来写的判定看不见它。**详见第二节第一小节
> 开头那段——按 `(tool_name, canonical(args))` 计数在 5 139 次真实调用里触发 **0** 次。

**洞二:工具执行没有统一超时。** `bash` 自己有 `timeout_ms`(`tools/bash.rs:217`,默认
60s),`hooks` 有 `DEFAULT_TIMEOUT_MS`(10s)。但 `execute_tool` 是裸的 async,一个
外部 `ToolSource`(MCP server、程序源)挂住就是无限期挂住。取消机制是有的
(`ctx.cancel`),缺的是"到点自动取消,并给模型一个说得清的 timeout 错误"。

## 二、做什么

### 一、重读提醒(advisory,绝不阻断)

> **判定已于 2026-09-16 改写,原方案作废。**原方案是"按 `(tool_name, canonical(args))`
> 计数,第 3/5/8 次提醒"。拿本机 rollout 重放(`scripts/tool-usage.py`,语料按常量变更
> 日期切到 2026-09-03 之后):
>
> ```
> bash 2028 次 → 触发 0     read_file 1691 次 → 触发 0
> grep 1113 次 → 触发 0     合计 5139 次 → 触发 0 次
> 同一 (tool, args) 在一个会话里最多只出现过 2 次(不清零的上界也只触发 3 次)
> ```
>
> **照原方案实现出来是死代码。**plan 106 其实早就记过原因:`<file>.go` 读了 60 次,
> 其中 **56 个不同的 `(offset, limit)`** ——模型打转的时候参数是变的,不是不变的。
>
> 真正的信号是**范围重叠**。同一份语料按"本次读的行区间与本会话已读过的区间相交"来数:
>
> | read_file(2026-09-03 后,1 691 次) | |
> |---|---|
> | 首次读这个文件 | 933(55.2%) |
> | 接着翻新页(不重叠) | 232(13.7%) |
> | **重读已读过的范围** | 526(31.1%) |
> | 扣掉压缩能解释的(压缩换走了旧结果,重读正当) | **234(13.8%)** |
>
> 13.8% 是真冗余:内容还在上下文里,模型又读了一遍。清零之后单个文件最多重叠 **13 次**
> (`<file>.go`;不清零时是 18 次)。复算命令:
>
> ```
> scripts/tool-usage.py read_file --since 20260903 --overlap
> ```

#### 三处改写(2026-09-16,用户拍板"同意")

原判定(范围重叠、只做 read_file、压缩后清零)**认**,复算一致:1 691 次 read_file,
重读已读区间 234 次(13.8%)。但拿完整判定(3/5/8 + 三个清零点)去数**提醒真正响几次**,
出来的是另一回事:

| 清零规则 | 重叠 | 提醒响 |
|---|---|---|
| 只按压缩清零 | 234 | **15** |
| + 用户消息清零 | 183 | **7** |

1. **去掉「新用户消息清零」。**判定问的是"行还在不在上下文里"。压缩清零、文件变了清零
   都能从这句话推出来;用户发一条消息**不会把工具结果换走,行还在**——这一条推不出来,
   它是"换话题了别唠叨"的礼貌规则。代价还大(见表),而且撞用户自己的风格:语料里
   212 条纯用户消息,两条之间的 read_file 次数**中位数是 0**、平均 8,一句"嗯,继续"
   就把计数器抹掉。
2. **门槛 3/5/8 改成「第 3 次,之后每 3 次」。**172 条重叠序列的分布是:重叠 1 次 130 条、
   2 次 28 条、3 次 11 条、4 次 2 条、7 次 1 条。3/5/8 一共响 15 次,其中 **14 次来自
   第 3 档,第 5 档响过 1 次,第 8 档一次没响**。三个常量三条边界测试,换来的行为等于
   一个常量 `REREAD_ADVISORY_EVERY = 3`。
3. **验收 #15 的期望值本来是错的。**234 是*重叠次数*不是*提醒次数*,照字面复算会看到
   15 然后误判"判定又漂了"。改成分开核(见第六节)。

**顺带定下的一件**:清零不挂在 `edit_file`/`write_file`/`notebook_edit` 三个工具名上,
挂在 **`FileVersion` 变了**上。理由是量出来的:语料里 `edit_file` **零次**、`write_file`
**5 次**——改文件几乎都走 `bash`,盯工具名会漏掉绝大多数写。按版本判则 bash heredoc、
别的进程改的都算。(这个缺口实际只占 234 次重叠里的 1 次,0.4%,但按版本判并不更贵。)

**还需记着的前提**:语料 98.5% 来自单一项目桶(脚本自己警告 ⚠),13.8% 是"读 Go 后端
代码改 bug"这类负载的下界,不是通用结论。

- **只做 `read_file`。**它是唯一一个"重复"有严格定义的工具:读的行区间与本会话已读区间
  相交 ⟹ 那些内容**确实还在上下文里**。`grep` 没有这个性质——同一个 pattern 换一个 path
  是新结果,不是重读(同 pattern 重复在语料里有 195 次,但无法证明它冗余)。**grep 的判定
  另算,本 plan 不做**,理由写在这里免得下一个人当成漏掉了。
- 按 `(agent, path)` 维护已读区间集合:`offset`(缺省 1)到 `offset + limit`,`limit` 缺省
  或为 0 时是"到文件尾"。新一次读与集合中任一区间相交,就给这个 `(agent, path)` 记一次重读。
- 第 3 次重读、之后每 3 次(6、9、…),在该次工具结果后追加提醒,**点名这个文件**、说明
  那些行已经在上下文里、建议先往回看或换做法。点名用的是**规范化后的绝对路径**:计数本来
  就按它归并,同一个文件的两种写法共用一个计数,那就得共用一个名字。
- **按 agent 分别计数**:子 agent 和 root 各算各的(`agent_type.rs` / `LocalAgentId` 已有
  身份),否则并行子 agent 会互相污染计数。
- **压缩后清空区间集合。**压缩把旧的工具结果换走了,那些行**不再在上下文里**,此时重读是
  正当的——不清空就会对着正当行为发提醒,那比不提醒更糟。这一条把 31.1% 降到 13.8%,
  是本判定正确性的一半。
- ~~**新的用户消息清零**~~。**去掉了**,理由见上面第 1 条。
- 永不阻断、永不延迟、也不替换结果内容。"与其提醒不如直接回一句『你已经读过』"是另一个
  设计(它会改变工具输出语义),本 plan 不做。

### 二、每工具 cooperative 超时

- 每个工具自带上限(不是全局一个数):`bash` 沿用它已有的 `timeout_ms` 不动;
  只读的 `grep`/`glob`/`read_file` 给一个短上限;外部 `ToolSource` 给一个较长的。
- 到点触发 `ctx.cancel` 的子 token,等取消 settle 之后再给模型 timeout 错误——
  **不是**到点就撕掉 future,那会留下半跑的进程和写了一半的文件。
- 错误信息要诚实:**说明工具可能仍在后台跑**。一个不理会取消的工具,这套机制停不住它,
  dsh 自己也这么写在文档里。不要在措辞上假装它被杀死了。

#### 两处改写(2026-09-16,实现时)

1. **"等取消 settle"不能只等——还得兜底丢掉。**plan 写的是"绝不撕掉 future"。可是
   **`ToolSource` 这条路上根本没有 cancel token**(`fn call(&self, tool, input)`,没有
   signal 参数),而外部 source 恰恰是第一节点名的那个洞。只取消不丢,挂住的 MCP 一样
   停不下来,整件事对它的主目标无效。改成两段:**先取消并等**(会看 token 的工具自己
   停稳,进程杀掉、临时文件删掉、锁放掉——这半段是 plan 的原意,必须保留);**等不到就丢**
   (Rust 里丢 future 本身就是取消,`Drop` 会把 in-flight 请求解开——这是 TS 那边**做不到**
   的事,所以 dsh 的文档只能停在"必须遵循 signal")。settle 窗口 `CANCEL_SETTLE_GRACE`
   = 5s,按 budget 取 min。
   **给 ToolSource 加 cancel token 是另一件事**,加了也仍然需要这一段兜底,没在本次做。
2. **计时挂在 `execute_tool` 外面,不是 `run_gated` 外面。**budget 是给干活的,不是给
   等人的:pre-tool hook 和**权限审批**都在 `run_gated` 里,一条 `ask` 规则下等人 90 秒的
   `read_file` 会被 60s 的 budget 砍成 timeout。量出来这不是假想——本机语料里最慢的调用
   就是审批等待(`skill` 3 304s、`write_file` 211s)。

**档位的依据**(教训 143:补参数前先读参考里的那一处)。两个参考都读了,**都和 plan 的
"外部给一个较长的"对不上,而且对不上的方向相反**:

| | 外部工具超时 |
|---|---|
| **cc** | `DEFAULT_MCP_TOOL_TIMEOUT_MS = 100_000_000`(≈27.8 小时,**等于没有**),`MCP_TOOL_TIMEOUT` 环境变量可覆盖。(`MCP_TIMEOUT_MS = 30_000` 是**连接**超时,不是工具调用) |
| **dsh** | 只有**声明了 `timeoutMs` 的工具**才受管,未声明的原样放行;它只给两个 web 工具声明了 30s |
| **kloop** | `EXTERNAL_TOOL_TIMEOUT = 300s`,`ToolSource::call_timeout` 可按工具覆盖 |

dsh 的核心设计决定是"**预算声明在工具自身,不在插件里的名字表**"(原文:消除拼错名称
导致策略不生效的问题)——这条照抄了:built-in 的预算在 `Builtin::timeout` 的穷尽 match 里,
外部的在 `ToolSource::call_timeout` 上。"保守默认"也照抄:没声明就不管。

只读那档(60s)是量出来的,而且量出来**基本不会响**:742 次"整轮只有它一个"的调用里
`read_file` 最大 0.03s、`glob` 0.05s、`grep` 4.65s。dsh 明确不给 grep/glob 预算,理由
同此。留着只因为机制已经在了、多一行不要钱。**量的时候有个坑**:同批并发的调用共用一个
结果时间戳(= 批里最慢那个),不拆开看会读成"grep 跑了 6 450 秒"。

## 三、坑

- **计数放哪儿**。放 `ToolCtx` 里会随每轮重建;应该挂在会话级(`agent` 侧)并通过 ctx 借用。
  放错地方会让提醒永远触发不了或者永远触发。
- **提醒算不算 model-visible**。它进模型上下文,所以必须能被 rollout 重放出来
  (HANDOFF 的 append-only 硬规则)。不能是只在 UI 上显示的东西。
- **别和 compaction 打架**,而且是两层意思。(a) 提醒是短文本,但压缩后重放要保持一致;
  跟着工具结果走,不要单独成一条 history 条目。(b) **压缩必须清空已读区间集合**——见第二节,
  这是判定的一部分,不是优化。
- **文件被改过就清掉它的区间。**同一文件的重读在那之后是正当的。**别挂在三个工具名上,
  挂在 `FileVersion` 变了上**(理由见第二节"顺带定下的一件")。`file_state` 已经在跟踪
  版本,清理挂在那里而不是新起一套跟踪。
- **超时不能改变并发批的语义**。`dispatch_tools` 会把只读调用批量并发
  (`is_concurrency_safe`),一个超时不该让整批失败。
- `bash` 的 `timeout_ms` 是模型可以传的参数;新加的统一超时**不要覆盖模型的显式选择**。

## 四、开工时问用户(先问,再动手)✅ 已问

**只有一个点:第二节那个改写过的判定(read_file 区间重叠、压缩后清零、只做 read_file),
认不认?**

**2026-09-16:问了,用户回"你推荐呢"。量完三个新数后给的推荐是"判定认,但改三处",
用户回"同意"。** 三处见第二节。

两件要不要拆成两次提交**不用问,拆**:它们唯一的共同点是都落在 `execute_tool` 附近,
一个是对话质量、一个是资源安全,按仓库纪律("两条路之间选干净的那条,别问")直接分两次
提交,各自全绿。

## 五、非目标

- **不做 dsh 的其余 guard**。那边还有别的策略包,本 plan 只要这两件。
- **不引入工具级重试**。超时后重试是另一个决定,`provider` 侧已有重试语义,别在这里再来一套。
- **不做"检测到打转就自动收尾"**。提醒是给模型的信息,不是控制流。真要自动终止是另一个设计。
- 不改 `is_concurrency_safe` 的批量规则。

## 六、验收

### 重读提醒

全部落在 `core/src/tools/plan151_acceptance_tests.rs`,十条,断言的都是"每次读有没有带
提醒"这个 bool 序列的**整对象**,不是"包含提醒"。

1. ✅ **边界精确**:同一窗口读 10 次,提醒恰好落在第 4、7、10 次(= 第 3/6/9 次重读),
   其余七次**没有**。`the_advisory_fires_on_every_third_reread_and_no_other_read`
2. ✅ **翻页不算重读**:连读 8 段互不相交,**零提醒**。本判定的负对照,缺了它等于没测。
   `paging_forward_through_a_file_is_never_a_reread`
3. ✅ **相交就算**,哪怕参数完全不同:完全落在内部、部分相交、以及不给 `limit` 的整文件读
   都计数。`an_intersection_counts_however_the_arguments_differ`
   (外加一条 plan 没写的边界:读到文件尾之外**观察不到任何行**,既不算重读也不让下一次
   变成重读。`a_read_past_the_end_of_the_file_observes_nothing`)
4. ✅ **压缩后清零**:重叠读 3 次 → 真跑一次 `run_compaction` → 再重叠读 3 次,零提醒。
   `compaction_clears_what_the_model_is_held_to`
5. ✅ **改过就清零**,两条:`edit_file` 走工具那条(`editing_the_file_clears_the_count`),
   和**直接写盘**那条——模拟 bash heredoc,盯工具名的实现会在这条上挂
   (`a_changed_file_clears_the_count_whoever_changed_it`)。
6. ✅ **按 agent 隔离**:child 读 3 次 + parent 读 4 次,提醒只在 parent 的第 4 次响。
   共用一个计数会让 child 的第 3 次也响——`true` 的**位置**同时钉死了两个方向。
   `a_sub_agent_counts_separately_from_its_parent`
7. ~~**用户消息清零**~~ **删除**,见第二节第 1 条。
8. ✅ **永不阻断**:触发那次的结果 = 不触发时的完整结果 + `\n` + 提醒,整串相等。
   `the_read_that_triggers_it_still_returns_everything`
9. ✅ 提醒进了 history(→ rollout):跑一整个 turn,四次 read_file,recorded 的
   tool_result 里恰好一条带提醒。`the_advisory_is_recorded_as_part_of_the_tool_result`

### 每工具超时

10. ✅ 一个**永不返回、且不看任何 token**的 source 工具到点后:模型收到 timeout 错误,
    错误文本整串断言,**明说该工具可能仍在后台跑**。
    `a_tool_that_never_returns_times_out_and_says_it_may_still_be_running`
11. ✅ **取消先 settle**:工具的清理路径跑完之后才有错误返回——到点就丢 future 的实现
    会在这条上挂。`the_call_is_cancelled_first_and_allowed_to_settle`
12. ✅ **并发批不连坐**:`answer / wait / answer` 三个并发只读调用,中间那个超时,
    两边整对象相等地正常返回。`one_timeout_does_not_fail_the_rest_of_the_batch`
13. ✅ **不覆盖显式选择**:`bash` 的预算 = 模型给的 `timeout_ms` + grace,四个取值都断言,
    并断言外层**严格大于**内层(杀进程树的那个必须先赢)。
    `bash_keeps_the_timeout_the_model_asked_for`
    - 外加一条 plan 没写的:**整张表**断言(只有 `bash`/`read_file`/`grep`/`glob` 有预算),
      免得以后加一个变体顺手填个数字。`only_the_tools_that_can_be_bounded_carry_a_budget`

14. 仓库完成标准照旧(fmt / clippy -D warnings / test,各自取退出码)。两次提交各自全绿。
15. **判定要有语料背书**,分开核两个数(原文把这两个混成一个,见第二节第 3 条):
    - **重叠次数** = 234。`scripts/tool-usage.py read_file --since 20260903 --overlap`。
    - **提醒次数** ≈ **15**(172 条重叠序列,门槛"第 3 次、之后每 3 次")。
      不是 234,那是重叠;也不是 7,那是带用户消息清零的旧方案。
    ✅ 实测对上。

## 七、✅ 完成(2026-09-16,两次提交)

### 第一件:重读提醒

落点:

- `file_state.rs`:`Inner` 多一张 `context_reads` 表(与 `observations` 同键、不同寿命),
  `note_context_read` 一次调用完成"版本对不上就清零 → 严格相交判定 → 记下区间 → 返回第几次
  重读",`forget_context_reads` 给压缩用。顺手把 `ReadCoverage::merge` 里的区间归并抽成
  `coalesce`,两处共用。
- `tools/fs.rs`:`REREAD_ADVISORY_EVERY = 3` 与提醒文本;`reread_advisory` 从
  `FileStateUpdate::Observe` 进入——`Observe` 是 `read_file` 独有的,所以"只做 read_file"
  是类型保证的,不靠字符串比工具名。
- `tools/mod.rs`:挂在 `run_one` 里 `aftermath.file_state_update` 那个条件上。**同一个条件
  是有道理的**:一次结果模型根本没看见的读(post-hook 期间被取消),不能算它"已经拿到"。
  新增 `append_notice` 把提醒追加进 tool_result,和结果同生共死。
- `compact.rs`:`compact_once` 里 `history.replace_all` 之后清表。

**两个实现层面的教训:**

1. **plan 的"坑"第一条(计数放 `ToolCtx` 会随轮重建,应挂会话级)指错了地方。**会话级的
   容器早就有,就是 `FileState`——而且它的寿命恰好是要的那个:`subagent_from` 给子 agent
   一份新的、进 worktree 也给新的、从不持久化。"按 agent 隔离"这条验收**一行代码都不用写**,
   是既有约束的推论。新起一套跟踪会白白重建这三条性质。
2. **提醒文本里别用序数词。**第一版写 `the {rereads}th time`,3 出来是 `3th`。改成
   "{rereads} reads of this file have now done that",顺带不用管 1st/2nd/3rd。

### 第二件:每工具 cooperative 超时

落点:`Builtin::timeout`(穷尽 match,与 `concurrency_safe` 同一处)、
`ToolSource::call_timeout`(默认 `EXTERNAL_TOOL_TIMEOUT`)、`tools/mod.rs` 的
`CallDeadline`(`arm` 两段:取消 → settle 窗口 → 丢)、`bash::foreground_timeout`
(把 `timeout_ms` 的默认值收成一处,外层预算从它派生而不是第二次读同一个字段)。

`CallDeadline::expired` 还有第二个用处:`run_one` 的 select 里,取消分支和已完成的
`gated` 在每次超时都同时就绪,不看这个标志的话"模型听到 interrupted 还是 timeout"由
select 随机决定。这正是 dsh 文档里那句"把 `timeoutOf` 限定到 `TOOL_TIMEOUT`,避免嵌套的
外层截止时间被误读为本插件的超时"——同一个问题,Rust 这边的形状是子 token + 一个标志。

**没做、留给以后的**:给 `ToolSource::call` 加 cancel token(现在这条路上没有 signal,
外部工具只能靠丢 future 停);超时不可配置(cc 有 `MCP_TOOL_TIMEOUT` 环境变量,kloop 走
常量 + 每个 source 覆盖)。两件都不影响本 plan 的验收。
