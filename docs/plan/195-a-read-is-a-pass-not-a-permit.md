# Plan 195 — 读戳是问路的凭证,不是编辑的许可证

> 来源:2026-09-22 的审查对话,承 plan 194 同一场。用户问「像 edit_file 命令,有什么值得
> 完善的吗」,又追问「参考项目怎么做的」,最后点名「还有 deepseek-harness、grok-build」。
> 三轮查下来,原本以为要选的那道题(编辑时格式归一化 vs 换锚点方案)**问错了**:kloop 实测
> 的编辑失败里,真正的锚点漂移是 0 次。用户看完证据一句「同意」。

## 一、那两次失败,锚点一直是好的

真实会话里 50 次 `edit_file`,2 次失败,都在同一轮同一个文件。模型自己跑过一次
`gofmt -w`,于是:

```
#397 失败   old_string sha=e2511d70d4d3 (54 字节)
#397 失败   old_string sha=b1870509a1e2 (68 字节)
      ↓ 中间只隔一次 read_file(offset=93, limit=30)
#403 成功   old_string sha=b1870509a1e2   ← 逐字节相同
#403 成功   old_string sha=e2511d70d4d3   ← 逐字节相同
```

**同样的 `old_string`,一个字没改,重发就成了。** 那两行从头到尾没被动过;`gofmt` 动的是
文件别处。失败的原因是观察表是**整文件级**的:文件任何地方变了,读戳整张作废,连锚点
完全有效的编辑一起拒。

代价是两个往返(拒绝 → 读 → 重发),而中间那次读**不验证任何东西**——因为 `edit_file`
要的是 `ReadRequirement::AnyRead`,读哪 30 行都算数。`fs.rs:1359` 的 doc 自己写着:

> `edit_file`: one read of the path is the whole entrance fee (plan 155). A narrow read
> qualifies the file, **including for an `old_string` outside the range that was read**.
> What keeps the edit honest is `old_string` …

plan 155 已经承认了**读的范围与编辑的正确性无关**。这条走完剩下的一半:**读过与否
与编辑的正确性也无关**——真正的锚是 `old_string` 在当前内容里精确且唯一地匹配,
而这个检查在拒绝发生时根本还没跑。

## 二、裁决

**`edit_file` 的准入条件从"读过且没变过"换成"读过 + `old_string` 当前唯一匹配"。**
"文件变过"不再是准入判据,降级成**诊断维度**。

改完之后 `edit_file` 保证的:

1. 模型至少读过这个文件一次(任意范围)——**保留**。"从没看过就改"是另一回事,
   `unread_edit_hint`(`fs.rs:1396`)那条带行号的提示也继续服务它。
2. `old_string` 在**当前**内容里精确且唯一匹配——这是真正的锚,一直都是。
3. 写入相对于"刚读到的那份内容"是原子的——`fs.rs:1282` 的 `ExpectedTarget` CAS
   **一个字不动**。它保证的是"从 kloop 读到 kloop 写之间没人插队",和本条无关。

不再保证的,只有一条:文件自模型读过之后没被改动。它变成一句话告诉模型。

**只改 `edit_file`。** `write_file`(`fs.rs:1194`/`1206`)是整文件覆盖,没有锚,
覆盖一个变过的文件就是丢掉别人的改动,守卫必须留;`notebook_edit`(`fs.rs:1292`/`1303`)
要的是 `CompleteNotebook`,也留。**这三条路径的判据从此不同,plan 里写死,别顺手统一。**

### 为什么换得过

唯一的风险是:模型的意图基于过时的上下文——`old_string` 还在,但它周围的代码已经
不是模型以为的那回事。对着这个风险,**现状挡不住**:模型读任意 30 行就能重新盖章,
那 30 行可以完全不含 `old_string` 所在的区域。所以现状收的是过路费,不是保护。

而改完之后模型**知道文件变过**了——现在它重新盖完章,对发生过什么变化一无所知。
按"模型手上的信息"算,新形状比旧形状多给,不是少给。

先例两条:claude-code 的 `edited_text_file` 附件(文件在模型背后被改就把片段给它,
`refs/claude-code/src/utils/attachments.ts:2170`),deepseek-harness 落盘即更新观察表
(`refs/deepseek-harness/packages/fs/tool-str-replace-editor/src/index.ts:325` 的
`ctx.emit('fs/observed', …)`)。

## 三、形状

`fs.rs` 的 `Mutation::Edit` 分支(1233–1287)现在的顺序是:

```
validate_observation_metadata(…, AnyRead, unread_edit_hint)   // 1241 变过就 bail
read_bounded → snapshot
validate_observation_version(…, AnyRead)                      // 1252 变过就 bail
apply_text_edit                                               // 1262
  match_count == 0        → bail                              // 1264
  match_count > 1 && !all → bail                              // 1266
  否则 → 写入(CAS)+ "edited …(N replacement(s))"              // 1275
```

改成:前两道**不再 bail**,而是产出一个 `stale` 事实往下传;三条出口都用它。

- **`expected` 为 `None`(从没读过)仍然 bail。** 这一支不动。
- `old_string` 没找到 → 原错误 + 一句"这个文件在你读过之后变过了",这恰恰是最该
  说出来的时刻(现在这个信息在准入处用完就丢了)。
- 多处匹配 → 同上。
- 唯一匹配 → **放行**,成功文案里带上变过的事实。

于是 `stale` 从"用一次就扔的准入判据"变成"三条出口共用的诊断",这是这条改动真正的
形状,不是"把一个检查删掉"。

`validate_observation_metadata`(1411)与 `validate_observation_version`(1444)要把
"变过"从 `Err` 改成返回值的一部分;两个函数都是三条路径共用的,**签名怎么改不能让
`write_file` / `notebook_edit` 悄悄跟着放宽**——最稳的做法是让 stale 只对
`ReadRequirement::AnyRead` 生效,其余照旧 bail,这样判据的分叉写在类型里,不写在调用方。

## 四、坑

- **`ExpectedTarget` 的 CAS 不许动。** 它和本条要放宽的检查长得像(都在比 version),
  但它比的是"kloop 自己读到的那份"与"kloop 要写的那一刻",与模型读过什么无关。
  动它就是把 TOCTOU 的门拆了。
- **`fs.rs:1224` 那条 `changed since it was read` 是另一件事**:`current` 为 `None`
  且读过 = 文件被删了。别一起改。
- **失败清观察表的顺序。** 现在第一个 edit 因"变过"失败会清掉 observation,同一轮
  第二个 edit 于是报"从没读过"——一个根因两种诊断(真实会话里就是这么发生的)。
  本条改完,"变过"不再导致失败,这条路径大半消失;**剩下的那半(真失败时要不要清)
  开工时顺手确认一次**,别留着。
- **测试面比改动面大。** `fs.rs` 里至少四条测试按"变过就拒"钉着
  (`stale_edit_rejects_even_when_old_string_remains_unique` 这个名字本身就是本条要
  推翻的断言、`edit_over_text_read_limit_fails_before_temp_and_clears_authority`、
  2662/2771/2805/2978/3525 几处 assert)。**逐条问它测的是哪一层**:测
  `write_file` 的照旧绿,测 `edit_file` 的要改成"放行且结果里说了变过",
  测 never-read 的不动。这是 plan 192 教训 169 的同款——别按名字猜。
- **文案在两处出现**:成功路径(1275)和两条失败路径(1264/1266)。三处共用一句
  措辞,别写成三个说法。

## 五、验收

- `make check` 全绿。
- 一条测试钉住本条的核心场景:读 → 外部改动文件别处 → 同一个 `old_string` 编辑
  **一次成功**,且结果里说明文件变过。(这正是真实会话里那两次失败的形状。)
- `old_string` 找不到 / 多处匹配时,错误里带上"变过"这一事实。
- `write_file` 与 `notebook_edit` 对同一个场景**仍然拒绝**,各有一条测试钉住。
- 从没读过就 `edit_file` 仍然拒绝,`unread_edit_hint` 的行号提示不变。
- 写入 CAS 的行为一字不变:并发改动仍然失败。

## 六、开工时定(问用户)

1. **成功文案怎么说**。"变过"要不要连"变了多少"一起说(长度差、mtime),还是只说
   事实。倾向只说事实——`FileObservation`(`core/src/file_state.rs:40`)只存
   version + identity + coverage,**不存内容**,给不出 diff,而要给 diff 就得先存
   内容快照,那是单独一件事。
2. **`len` 和 `mtime` 都没变、只有 ctime 变了(touch / chmod)要不要提示**。真变化
   和假变化混在一起会让这句提示变噪声;但要区分就得存内容哈希,又是上面那件事的
   小号版本。倾向第一版不区分,攒到噪声真出现再说。
3. 本条与 **plan 194** 无交集(一个动 `tools/fs.rs`,一个动 `permissions.rs` 与三个
   前端),可任意顺序。与 **plan 179**(拆 `core/src/tools/fs.rs`)**有交集**:
   179 要把 fs.rs 按平台拆开,本条改的 `Mutation::Edit` 分支和两个 validate 函数都在
   共用部分。两条都小,先做哪条都行,但**不要并行开两个 worktree**。
