# Plan 200 — 裁的是请求,不是历史

> 来源:2026-09-23,读 `refs/chord`(`keakon/chord@cce05db`,MIT)后用户同意「给请求级裁剪立个 plan」。
> 出处与取值见 `refs/README.md`「chord 固定源码调研(2026-09-23)」一节。本 plan 做完,
> `refs/chord` 的本地 clone 即退休(见第八节)。

## 一、为什么

kloop 现在控制上下文只有两道:**记录时 offload**(单条 > 32000 字符 spill、一轮合计 > 128000
字符按大小降序 spill,`core/src/history.rs` 的 `OFFLOAD_CAP_CHARS` / `enforce_round_budget`)和
**压缩**(`core/src/compact.rs`,预测性 + 溢出后被动)。中间是空的:一个 25000 字符的 `read_file`
结果不够 offload 的线,于是**原样**跟着每一次请求走,直到压缩把它连同其他东西一起折掉。长任务里
这类"读过、用过、早已不再需要"的工具输出是请求体的大头。

`docs/capability-report.md` 生存层那张表的 microcompaction 行挂着这件事。cc 与 ZCode 的做法是
"清掉旧的 tool result、换成固定占位串";chord 是真正独立的第二家,做法更细,也更适合 kloop:

- **只改发出去的请求,不改历史**——rollout、`History::items`、resume、fork 一概不动;
- **按工具换成有形状的 stub**,而不是固定占位串——模型还知道那次调用拿到过什么、去哪儿找回原文;
- **不破 prompt cache**——这是整件事成立的前提,见第三节。

## 二、形状

每次采样前,在 `provider_request_view` 之后、注入 synthetic 首条 user 消息之前,对请求视图做一次
**纯内存**的裁剪:把满足条件的 `ToolResult` 的 `Text` 内容换成 stub。只动 `ToolResult` 的
`content`,`tool_use_id`、`is_error`、消息顺序与条数全部不变——tool_use/tool_result 配对因此
**结构上**不可能被破坏,不需要 chord 那套切点规则。

### 2.1 年龄

**年龄 = 这条结果之后出现过几条 assistant 消息**(= 又采样了几轮)。同一条 assistant 响应里的
并行调用,结果落在同一条 user 消息里,年龄相同。年龄只看历史位置,**确定性**,resume 后重算得
出同一个值。

### 2.2 谁能被裁(白名单,不在名单里的一律不动)

| 工具 | 条件 | stub 保留什么 |
|---|---|---|
| `read_file` | **仍有效的 read 永不裁**;变成 stale 或 superseded 之后,只要 > 3000 字符就裁 | 路径、行范围、字符数、失效原因(`stale: edited by a later edit_file` / `superseded by a later read`)、原文位置 |
| `grep` / `glob` | 年龄 ≥ 2 且 > 3000 字符 | 命中的不同文件路径(至多 20 个)+ 总行数 + 原文位置 |
| `bash` / `powershell` / `bash_output` 成功结果 | 年龄 ≥ 2 且 > 3000 字符 | 尾部至多 20 行(≤ 1500 字符)+ 总字符数 + 原文位置 |
| 上面几类的 `is_error` 结果 | 年龄 ≥ 4 才按同类规则裁 | 同上 |
| MCP / `call_tool` 转出的外部工具、`web_fetch`(不在 `Builtin` 里,`tools/web.rs` 单独注册) | 年龄 ≥ 3 且 > 1500 字符 | 头部 500 字符 + 总字符数 + 原文位置 |

**永不裁**:`skill`(加载进来的是指令,不是数据)、`run_agent` / `wait_for_activity` /
`send_message` 等子 agent 结果、`ask_user_question` 的回答、`todo_write`、plan mode 两件、
`edit_file` / `write_file` / `notebook_edit` 的结果(它们是改动证据,且通常很小)、
`ToolResultContent::Blocks`(带图片的结果;图片的去留是另一件事)。

阈值取自 chord(`internal/agent/compaction.go:78-98`,字节换成 kloop 的字符口径),
**开工时不必重新论证,先照抄、写进 DESIGN**;以后按 dogfood 调。

已经被 offload 的结果(1500 头 + 500 尾 + 指针,约 2300 字符)天然低于 3000 线,不会被二次裁。

### 2.3 "仍有效的 read" 怎么判

**只看历史,不看磁盘**——判据必须对 resume 稳定,而 `FileState` 刻意不持久化。对一条
`read_file` 结果,向后扫同一规范化路径(`file_state::normalize_absolute_path`):

- 之后有 `edit_file` / `write_file` / `notebook_edit` 命中该路径 → **stale**;
- 之后有覆盖它的 `read_file`(同一路径,范围包含它)→ **superseded**;
- 都没有 → 仍有效,不裁。

外部进程(bash、用户手改)改了文件,历史里看不见——那是 plan 197 `remind_changed_reads` 的事,
它已经在轮边界告诉模型"这个文件变了",本 plan 不重复做磁盘检查。

### 2.4 原文放哪儿、stub 长什么样

被裁的原文**第一次裁时**写进 offload 目录,复用 `spill_to_disk` 的 `off-NNNN.txt` 命名——
**不新造文件名**:权限门与沙箱对 offload 的豁免是按文件名(`off-NNNN.txt`、`bg-N.out`)开的,
新名字就得去动那道安全边界(DESIGN 第一条 bet 里 "Two gates had to open" 那段)。stub 里带这条
绝对路径,措辞与现有 spill 指针一致(就地查询,不要整篇读回)。

写盘失败 → **这条不裁**,原样发出。和 round budget 同一个取舍:为了省上下文而销毁内容是更坏的交易。

resume 后内存里没有"哪条已经写过盘"的记录,会再写一份新的 `off-NNNN.txt`。**接受**:
代价是少量重复文件,换来不持久化任何裁剪状态。

### 2.5 召回反馈

模型重新发出一个与**已被裁掉**的调用完全相同的 `tool_use`(同名、规范化后的 input 相等),说明
裁过头了:把**新**那次调用的 `tool_use_id` 加进豁免集,它的结果以后永不裁。旧的那条保持裁掉的
样子(动它就是改已发送前缀)。对 `read_file` 而言,这恰好就是 superseded——旧的被新的取代,新的受保护。

## 三、不破 prompt cache

kloop 三条轨都靠前缀缓存:Anthropic 轨在最后一个 block 上放移动断点
(`provider/src/anthropic.rs` 的 `messages_value`),Responses 轨带 `prompt_cache_key`,
Chat 轨靠后端自动前缀缓存。**请求视图里任何一个位置的字节变了,从那里往后全部重新计费写缓存。**

两条规则:

1. **一旦裁了,永远裁。** 已发出的 stub 冻结,之后每次请求逐字节复用同一个 stub。冻结集按
   `tool_use_id` 记,存在会话的内存状态里(与 `History` 同寿命;`/clear`、rebase 时清空;
   **不进 rollout**)。stub 本身也必须是结果内容与冻结时参数的纯函数——不能带"几轮之前"这种
   会随时间变的字眼。
2. **深处的新裁剪要攒够了才动。** 按年龄新到线的结果永远在尾部附近(年龄 2~4),动它只重写
   最后几轮,代价很小,**直接生效**。但 stale / superseded 可能发生在很深的位置(三十轮前读的文件
   刚被改了),动它等于把整段尾巴重新写一遍缓存。这类提议先进**待定集**,满足下式才一起生效:

   ```
   待定集合计省下的 token × 30 ≥ 9 × (最早那条待定位置到请求末尾的 token)
   ```

   即省下的至少是要重写的尾巴的 30%(chord `compaction_policy.go:640-669` 的取值;开工时按
   kloop 三轨的 cache read / write 价格复核一遍这个比例,结论写进 DESIGN)。

   **缓存本来就凉了的时候,待定集无条件全部生效**,因为这时不存在可保护的前缀:
   - 刚压缩过(`replace_all` 之后);
   - 刚换过 provider / 模型(route revision 变了);
   - 会话刚 resume(第一次请求);
   - 距上一次请求已超过 5 分钟(Anthropic ephemeral 默认 TTL;另两轨取同值,保守)。

"直接生效"和"进待定集"的分界开工时定一个简单判据:**提议位置之后的 token 超过某个量(先取
`KEEP_RECENT_TOKENS` 的一半,1 万)就算深处**。不要做成两套规则。

## 四、与现有机制的关系

- **offload**:记录时、改历史、单条/一轮的硬上限;本 plan:请求时、不改历史、按年龄与有效性。
  两者串联,互不感知,只在 2.2 那句"offload 过的天然不会被二次裁"上相交。
- **token 估算与预测性压缩**:`History::estimated_tokens` = 上一次 provider 报的 usage(已经是
  **裁过之后**的请求大小)+ 之后新增消息的字符估算(**未裁**)。所以估算偏保守、只在尾部偏大,
  不需要改;`compact_predictively` 照旧。开工时加一条测试钉住"裁剪生效后下一轮估算下降"。
- **压缩的摘要输入**:仍用**未裁**的历史。chord 会先裁再送去总结,省摘要请求的钱,但摘要质量
  可能受损;这一步先不做,记为后续候选。
- **`/context`**:历史那几行加一行"request reduction: N results stubbed, ~X tokens saved"。
  `/context` 的数字要反映**实际发出去的**,不然它会系统性高估。
- **子 agent**:`sample_with_retry` 对所有深度共用,裁剪自然覆盖子 agent,各自一份内存状态。

## 五、实现要点(开工时核)

- **新模块** `core/src/request_reduction.rs`(`history.rs` 已近 2000 行,不往里塞)。对外两样:
  一个 `ReductionState`(冻结集、待定集、豁免集、已写盘路径、上次请求时间),一个
  `reduce(view: &mut [Message], state: &mut ReductionState, now) -> ReductionStats`。
  判定部分写成**不碰 IO 的纯函数**,写盘经一个 trait 注入,测试用内存假实现。
- **挂在哪里**:`agent/sampling.rs` 的 `sample_with_retry` 现在拿的是 `&History`,
  而裁剪要改状态。两条路开工时选:状态放进 `History` 用内部可变性,或者把裁剪提到
  `turn_rounds` 里(那里有 `&mut History`)、把裁好的视图传进去。**倾向后者**——
  `sample_with_retry` 的三次重试必须发同一份字节,在外面裁一次正好保证这点。
- **工具名从哪来**:`ToolResult` 只有 `tool_use_id`,要向前找配对的 `ToolUse` 拿 `name` 与
  `input`。先建一张 `tool_use_id → (name, input, 所在消息下标)` 的表,一次遍历完成。
  内置工具的名字经 `Builtin` 解析(`tools/builtin.rs:243` 那张表),不要手写字符串比较;
  `web_fetch` 与外部工具不在那张表里,开工时看 `tools/mod.rs` 现在怎么区分它们再定。
- **重置点**:`replace_all`、`rebase`、`/clear`、provider 切换 —— 都要么清空状态、要么把
  "缓存已凉"置位。开工时 grep 这几个入口,一处不漏。
- **开关**:`config.toml` 加 `[context] request_reduction`(bool)。默认值见第六节。

## 六、开工时必须问用户的点(只有一个)

**默认开还是默认关?**

- **默认开(推荐)**:这是省钱的主力,而默认关的功能几乎没人会去打开;行为完全在请求视图里,
  历史不动,关掉即恢复原样,风险可逆。
- 默认关:先在 dogfood 里手动开一段时间,观察模型有没有因为 stub 反复重读,再翻默认值。

## 七、测试

- **纯函数层**(不起 agent):
  - 各工具类型的 stub 整对象断言;白名单外的工具、`Blocks` 结果、offload 过的结果不动;
  - 仍有效 / stale / superseded 三种 `read_file`,判定只依赖历史;
  - 年龄计算:同一响应里的并行调用年龄相同;
  - **确定性**:同一份历史连续裁两次,输出逐字节相同;
  - **单调**:冻结的 stub 在之后任何请求里逐字节不变,哪怕阈值条件已经不再成立;
  - 摊销门:深处的小提议留在待定集,攒够后一起生效;"缓存已凉"的四种信号各一条,触发全部生效;
  - 召回:重发相同调用后,新结果进豁免集,旧 stub 不变;
  - 写盘失败 → 原样不裁。
- **集成层**(mock provider,抓请求体):
  - 多轮任务后,抓到的请求里旧 `read_file` 结果是 stub,而 **rollout 文件与 `History::items`
    一个字节没变**;
  - 请求里的 tool_use / tool_result 配对完整、顺序不变;
  - Anthropic 轨的移动断点仍在最后一个非 thinking block 上;
  - 三次重试发出的请求体完全相同;
  - resume 后第一次请求按"缓存已凉"处理:视图 = 不中断时的视图再加上当时待定集全部生效
    (除 offload 文件名外逐字节可预测);之后的请求照常冻结。
- `make mock` 的会话短,碰不到阈值,**不应该**有任何输出变化;若 `make parity` 的本机语料
  因请求体变化变红,按教训 156 的办法改语料,不改实现。

## 八、完成时要一起做的

- `rust/DESIGN.md`:第一条 bet 写着"nothing is rewritten except by compaction"——那句仍成立
  (裁剪不改历史),但"offload at record time"那一段之后要**补上请求时这一层**,讲清两者的分工、
  永远裁与摊销门、为什么不持久化状态。先读那段现在还成不成立,再决定改写还是追加。
- `docs/capability-report.md`:microcompaction 行销账,写本 plan 号与提交号。
- `refs/README.md`:chord 那一行从"待退休"改为"已退休",注明本地 clone 已删、回源重新 clone 即可;
  删除本地 `refs/chord`(删除前确认 HEAD 仍是 `cce05db`、工作树干净)。
- HANDOFF.md 记新教训(如果有)。

## 九、完成记录

(未开工)
