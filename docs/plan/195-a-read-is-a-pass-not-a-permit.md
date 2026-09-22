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

**先例这一栏,开工时重查了,原来写的两条都不成立**(2026-09-22)。六个参考里有读戳的三家
——claude-code、deepseek-harness、CodeWhale——**3/3 都拒 stale 编辑**;剩下两家(codex 的
`apply_patch` 靠 context 行 + `seek_sequence` 的 fuzzy 匹配,grok-build 的主 `edit`)**连读戳
都没有**,什么都不检查。原先引的两条各自是别的东西:deepseek 的 `fs/observed` 是"落盘刷新
读戳",kloop 一直这么做,与放不放宽无关;claude-code 的 `edited_text_file` 是下面那条
**备选方案**的先例,不是本条的。

**没有一家做本条这个中间态,而这不构成反对——因为没有一家面对过这道题。** 三家都用同一条
新鲜度规则同时管整文件覆写和带锚编辑:两个工具共用一张读戳表、一个 observed version、一个
tracker。kloop 在 plan 155 已经按读的覆盖面把准入判据劈成两半(`CompleteFile` vs
`AnyRead`),新鲜度沿同一条轴分家是**把已有判据讲一致**,不是发明新规矩。覆写一个变过的
文件是丢掉别人的改动;换掉一个当前仍然唯一存在的字符串,别人的改动一个字都没丢。

真正支持本条的只有内部论证:**一条随便读 30 行就能满足、而那 30 行可以完全不含
`old_string` 的规则,不是安全规则。** 另有一条顺序上的先例:deepseek 的 `edit-intent` 只拒
"从没读过"和"读到的是确认不存在",stale 不是一道门,而是被交给写入当 CAS 基准——于是它的
执行顺序是"读当前内容 → 匹配 old_str(不中/多处各自带行号报错)→ 最后才由 CAS 判 stale"。
**它从不发生 kloop 那两次失败的形状:锚是好的却先被 stale 拦掉、锚的结论根本没算出来。**
本条把 stale 从准入降级成诊断,得到的正是这个顺序。

(grok-build 同向一笔:`old_string` 不中时失败结构里挂当前文件内容,注释写明下游不得再回盘
取提示——失败要自带现场。CodeWhale 另有一半新鲜度是**模型自己传的参数**:可选的
`expected_hash`,从上次读拿到再传回来,不传就不检查,CAS 做成工具参数。)

**备选方案,记账不做**:claude-code 的办法是**不放宽门禁,而是让模型到不了拒绝那一步**——
每个工具轮次(不是每个用户回合)扫一遍读戳表,mtime 动过的文件自动重读,把 diff 片段当
`edited_text_file` 附件喂给模型并刷新读戳。真实会话那次 gofmt 与 edit 分在两轮,**它第一发
就成**。这是唯一真正补上本条"只提示不给 diff"那一半的做法,也确实更强;代价是每轮对每个
读过的文件 stat 一遍、变了就整文件重读进上下文,在 fmt/codegen 密集的会话里是持续开销。
kloop 的 `ContextReads` 有一半地基,没有回读注入。**本条是地板(锚成立时不该被拒,有没有
回读注入都对),那条是在它上面加信息量**,单开一条。

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

## 七、开工时定的三个点(2026-09-22,查完参考项目后定)

用户先追问「参考项目怎么做的」「最合理最干净的做法是什么」「最合理呢」,三轮之后拍「开工」。
六个参考的调研结论已经回填进第二节。三个点这么定:

1. **成功文案只说事实**(照倾向)。三条出口共用一句 `; the file changed since you read it`,
   位置统一在"路径那一段"之后:成功是 `edited <path> (N replacement(s)); …`,两条失败是
   `… in <path>; …`。不说长度差、不说 mtime——`FileObservation` 不存内容,给不出 diff。
2. **真假变化要区分,而且是免费的。plan 的前提查错了**:它写"要区分就得存内容哈希",
   但 `FileVersion` 里本来就有 `fingerprint: [u8; 32]`(整文件 sha256,partial read 也是整
   文件),而这道判断又刚好落在 `read_bounded` 之后、手上就有新 fingerprint。于是判据用
   **内容哈希**而不是元数据:`touch`/`chmod` 一句话都不说。副产品是形状比第三节写的更简单
   ——见第八节。
3. **清读戳:"没写就留下,只有文件没了才忘"。** 第四节让"开工时顺手确认一次",查完发现
   五个参考**没有一家**在编辑失败时清读戳(三家有读戳的都只在成功落盘之后刷新;claude-code
   的 `readFileState` 唯一的 delete 是 ENOENT,注释还专门写了瞬时 stat 失败绝不能删,否则
   下一个 Edit 会因"没读过"失败而模型其实刚读过)。kloop 清掉是 `Clear` 摆错位置的副作用。
   **但不能无条件不清**:`write_file` 在"读过之后文件被删"这一支的恢复路径依赖忘记——读一个
   不存在的路径什么都不记,留着读戳就等于永久拒绝,再没有办法解除。所以判据是**文件还在不在**。

## 八、✅ 完成

`make check` 全绿(fmt + clippy + 900 test,本地这一道就是全部门禁)。

### 与第三节的形状差异(落地时改的,更简单)

第三节要"两个 validate 函数都把'变过'改成返回值的一部分",并担心签名怎么改才不让
`write_file`/`notebook_edit` 悄悄跟着放宽。实际做法把这个担心整个绕开了:

- **`edit_file` 不再调用 `validate_observation_version`。** 那个函数一个字没改,只服务
  `write_file` 与 `notebook_edit`。放宽的代码根本不在共用路径上——**结构上不可能泄漏**,
  不必靠签名约束。
- `validate_observation_metadata` 只加一个早返回:`requirement.tolerates_change()` 为真就在
  新鲜度比较之前 `return Ok(expected)`。分叉写在 `ReadRequirement` 的一个方法里。
- 判据只有一处,在读完之后:`TargetChange::between(expected, &snapshot.version)`,比的是
  `FileVersion::same_content`(新增,只比 fingerprint)。`TargetChange::note()` 是那唯一一句
  措辞的唯一出处。
- 清读戳也没有按"把 `commit_mutation` 拆成 stage/commit"那条路走(原本打算把 `Clear` 挪到
  两者之间)。**那条路是错的**:`Clear` 必须留在原位——它眼下的无条件性守的是"写完了却因
  取消没清,残留读戳能给后面的 `write_file` 盖章"。落地做法是失败之后**按需补回**:
  `target_still_present` 问一句文件还在不在,在就 `FileState::restore_cleared` 放回去。
  `restore_cleared` 只在**槽位为空**时写入——已经提交的那次 mutation 刷新的新读戳,不许被
  一个更老的读戳顶掉。所有原有的顺序/取消性质一字未动。

### 顺带翻掉的一条旧决定

plan 57 第 77 行写的"validation/parse/target/serialize/commit 失败……**保守清除旧资格**"
(只写了"保守",没写挡什么)被本条推翻:`notebook_edit` 一个 `cell_id` 写错,现在不再连带
损失整张 notebook 资格。`plan57_parity_tests` 里那一步的断言与报告键跟着改
(`failure_cleared_qualification` → `failure_kept_qualification`)。对照产品的 NotebookEditTool
也不清——这一条本来就是 kloop 自己保守出来的,不是 parity。

### 测试

| 测试 | 锁住什么 |
|---|---|
| `a_changed_file_still_takes_an_anchored_edit` | **本条核心**:真实会话那两次失败的形状——读整文件 → 外部改动别处 → 同一个 `old_string` **一次成功**,文案里说了变过,且提交把读戳刷新成完整的 |
| `a_narrow_read_survives_an_external_change` | 同上但读的是 10 行、改的在这 10 行之上、编辑落在第 33 行:**三者互不重叠**,锚仍然是唯一判据(这条测试原名 `..._does_not_survive_...`,断言正好反过来) |
| `anchor_refusals_on_a_changed_file_say_it_changed` | 两条失败出口都带那句话(找不到 / 匹配 2 次);**第二次调用能走到这一步本身就证明第一次失败没清读戳** |
| `a_metadata_only_change_draws_no_note` | `chmod 0600` 之后先 assert `metadata_matches` 已经为假(证明这正是旧检查会拒的那种变化),再 assert 文案里**一个字都没多** |
| `a_refused_edit_leaves_the_turn_still_qualified` | 一个 `old_string` 写错的 edit 不毒死同一轮后面的 edit;中间 assert 读戳 `is_some()` |
| `write_file_still_refuses_a_changed_file` | 本条不跨的那条线,上半:同一个场景 `write_file` 整句逐字仍是拒绝 |
| `notebook_edit_still_refuses_a_changed_file` | 下半:`notebook_edit` 仍拒,且保持自己那套措辞 |
| `a_change_inside_the_commit_window_still_loses` | **新增 test-only `CommitFault::ChangeTargetBeforeRename`**。写入 CAS(`verify_target_unchanged`)的 version 分支此前没有任何测试,而本条之后它是 `edit_file` 唯一剩下的新鲜度保障——拆掉一道门不能让门后面那道继续裸着 |
| `one_partial_read_authorizes_two_edits_that_both_still_anchor` | 并行批次的形状:第二次带的是第一次提交之前的读戳,现在两次都落地,各自的锚都是对着当时真正在盘上的字节验的 |
| `edit_over_text_read_limit_fails_before_temp_and_keeps_authority` | 改名 + 翻断言:没写就不忘(原名 `..._clears_authority`) |
| `existing_write_requires_a_complete_fresh_read_and_refreshes_state` | 同款翻断言:`write_file` 的 stale 拒绝之后读戳留着,于是继续报"变过"而不是退化成"没读过" |
| `deleting_a_read_file_does_not_turn_overwrite_into_create` | 新增一句 `is_none()`:**唯一必须忘记的那一支**,恢复路径依赖它 |
| `plan57_parity_tests::guard_report` | 上面那条翻掉的旧决定 |

### 没做(有意)

- **claude-code 那套"每轮回读 + diff 注入"(第二节的备选方案)**。它才是真正补上"只提示、
  不给 diff"那一半的东西,也是唯一能让模型第一发就成的做法;代价是持续 token 开销。
  本条是地板,那条是地板之上的信息量,单开一条。已记进 HANDOFF。
- **`ExpectedTarget` 的 CAS 一个字没动**(只加了一条测试去打它)。
- **`edit_file` 的参数面一个字没动**。调研顺带发现的两处(`path` vs 对照产品的 `file_path`
  没有别名、没有批量 edit)是另一件事,记进 HANDOFF。
