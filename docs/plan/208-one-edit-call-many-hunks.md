# Plan 208 — 一次调用,改好几处

> 来源:2026-09-24 读 `refs/pi`(`earendil-works/pi@d5629e2`,MIT)后与用户逐条定的,
> 出处见 `refs/README.md`「Pi 全面复查(2026-09-24)」第 5 条。

## 一、为什么

`edit_file` 一次只接一对 `old_string`/`new_string`(`core/src/tools/builtin.rs:772-787`)。
同一个文件要改三处,模型就发三个 `edit_file`。

**先纠正 README 那条的隐含前提:这不省轮次。** 一条回复里的多个 `edit_file` 本来就在同一轮里
执行——它们不是 concurrency-safe,由 `dispatch_tools` 按顺序逐个跑(`core/src/tools/mod.rs:980-996`),
不需要模型等结果再发下一个。真正的代价在别处:

1. **不原子。** 顺序执行里一个失败不会拦住后面的(`mod.rs:980-996` 对每个调用照跑 `run_one`),
   于是三处改了两处、文件停在半截状态。模型得逐条读结果才知道哪几处落地了,再补剩下的——
   而补的那一处的 `old_string` 可能恰好压在刚改过的地方。
2. **三次审批、三份 diff。** 不在自动放行范围内时,每个调用各弹一次确认,每次只看到一处改动
   (`core/src/diff.rs:157`);用户要在脑子里拼出整体。
3. **后一处对着前一处改完的文件匹配。** 模型写三段 `old_string` 时看的是原文件;顺序执行下
   第二段匹配的是第一段写完之后的内容。两段不相交时没事,一旦相邻、共用一行,就会莫名 not found。

pi 的做法(`packages/coding-agent/src/core/tools/edit.ts:21-41`、`edit-diff.ts:300-363`):参数是
`edits[]`,**每段都对原文件匹配**、各自要唯一、按位置排序后**不许重叠**,全部定位成功才一次写回;
任一段失败整次不写。

## 二、形状

### 2.1 参数

在现有参数旁边加一个 `edits`(是否保留旧形状见第四节第一问,下文按「保留」写):

```json
{"path": "...", "edits": [{"old_string": "...", "new_string": "...", "replace_all": false}, ...]}
```

- 条目字段名沿用 `old_string`/`new_string`/`replace_all`,不照抄 pi 的 `oldText`/`newText`——
  一个工具里同一个概念只有一种拼法。
- `edits` 与顶层 `old_string` **同时出现 → 拒绝**,不像 pi 那样把两者合并
  (`edit.ts:125-133`)。理由同 `ARGUMENT_SYNONYMS` 的原则(`mod.rs:861-868`):不替模型猜。
- `edits` 为空数组 → 拒绝。条目里 `old_string` 为空、或与 `new_string` 相同 → 拒绝并点名 `edits[i]`
  (现有单段检查在 `core/src/tools/fs.rs:650-655`)。
- **`edits` 以 JSON 字符串到达时先解析一次**:pi 记录到部分模型会把数组序列化成字符串发来
  (`edit.ts:108-121`)。只接受解析后是数组的情况;单个对象不包成数组。
- 条目里的 `old_str`/`new_str` 同义词**不改名**:`ARGUMENT_SYNONYMS` 只管顶层键,嵌套改名
  是另一件事,等真出现了再说。

### 2.2 语义

- **每段对原文件匹配**,各走自己的三层(原样 → LF 回退 → 标点容错,plan 201)。
  与 pi 不同:pi 只要有一段走了模糊匹配,就把**整个文件**切到规范化视图里做所有替换
  (`edit-diff.ts:318-320`);kloop 的容错层本来就是逐段映射回原字节的,所以各段独立定层,
  互不牵连。
- 每段未设 `replace_all` 时必须唯一;设了则取该段全部命中(第四节第二问)。
- 把所有段的**原始字节区间**(容错层取整个命中区间,不是裁掉共有前后缀之后的那一小段)按起点
  排序,相邻两段 `前.end > 后.start` 即重叠 → 拒绝,点名两段编号,并建议合并成一段。
  首尾相接(`前.end == 后.start`)允许。
- 全部定位成功后从后往前拼接,**一次** `atomic_replace`。任一段失败 → 不写,文件原样。
- 拼完与原文逐字节相同 → 拒绝(pi 的 `getNoChangeError`,`edit-diff.ts:282-290`)。

### 2.3 结果与报错措辞

- 成功:`edited <path> (3 edits, 4 replacement(s)){note}`;有段走了容错层时,逐段补一行
  `edits[1]: punctuation/whitespace-tolerant match at line N`。单段形状的措辞一字不变。
- 失败:**把所有失败的段都列出来**,每段带自己的诊断(not found 走 plan 201 的最近匹配,
  `fs.rs:1358`;重复走 "matches N times"),开头一句 `edit_file: no edits were applied to <path>`。
  pi 只报第一段;全列一次省掉「修好一段又撞下一段」的往返。
- 目标未读时的坐标提示(`unread_edit_hint`,`fs.rs:1342`)取**第一个能定位的段**。
- plan 195 的 `note`(读后文件变过)照旧只挂一次,位置不变。

## 三、改在哪

- **`core/src/text_edit.rs`**:现在 `apply_text_edit`(`:47`)直接返回拼好的整串
  (`TextEditOutcome.updated`),多段没法在它上面叠。拆成两步:
  `locate_text_edit(current, old, new, replace_all) -> TextEditPlan`(命中计数、层、首行、
  以及一组 `Splice { raw: Range<usize>, replacement: String }`),和 `apply_splices(current, &[Splice])`。
  `apply_text_edit` 保留为两者的薄包装,**现有测试一个不改**。三层各自改成产出 splice
  (原样层 `:63-79`、LF 回退层 `:121-136`、容错层 `:193-230`,后者的 replacement 由
  「原前缀 + delta + 原后缀」拼成)。多段的排序、重叠检查、拼接放在这里,作纯函数。
- **`core/src/tools/fs.rs`**:`edit_file_tool`(`:640`)解析两种形状,都归一成
  `Mutation::Edit { edits: Vec<EditSpec> }`;单段就是长度 1。`commit_mutation` 的
  `Mutation::Edit` 分支(`:1121-1185`)改成遍历。路径锁(`mutate_file` 的 `lock_path`,`:704`)、
  FileState 的清除/恢复/刷新(`:726-775`)、CAS 与原子替换(`:1220`)**全都不用动**——
  本来就是「一次读、一次写」,多段只是中间多拼几刀。
- **`core/src/diff.rs`**:`file_change_preview_with_context` 的 `"edit_file"` 分支(`:67`)和
  `edit_preview`(`:157`)接 `edits`,走同一个 `locate`,审批时看到的是**一份整文件 diff**。
  定位不全时的降级:逐段 `numbered_diff(old, new)`,段间用 `⋮` 隔开。
- **`tui/src/toolrow.rs`**:`edit_diff_lines`(`:297`)只读顶层 `old_string`;多段形状显示第一段的
  `- old`/`+ new`,再加一行 `(+N more edits)`。
- **工具描述**(`builtin.rs:775`,`mod.rs:3083` 整串断言它):补一句,草稿
  `Or pass edits: [{old_string, new_string, replace_all?}] to change several places at once; each is matched against the original file, they must not overlap, and either all apply or none do.`
  不提容错层(plan 201 的立场)。

## 四、开工时必须问用户的点(一次问一个)

### 第一问:旧的单段形状留着,还是只留 `edits[]`?

- **两种都收(推荐)**:`required` 从 `["path","old_string","new_string"]` 降为 `["path"]`,
  执行时二选一。理由:
  - **parity 会红,而语料我们改不了。** `make check` 的 parity 跑
    `plan49_parity_tests::emit_plan49_parity_report`,把 kloop 的 `edit_file` 调用
    (`plan49_parity_tests.rs:299-316`,顶层 `old_string`)与对照语料里的同一场景逐个比入参。
    只留 `edits[]` 这份报告就变形;语料在 `refs/` 下、只读且不进版本控制,没法跟着改。
  - 大多数模型的训练里 `old_string` 顶层形状最常见(`ARGUMENT_SYNONYMS` 那段注释正是为此),
    单处修改仍是绝大多数;强迫它包一层数组,换来的只是一种拼法。
  - 代价:schema 里两条路,描述多一句;「两者都给」要多一条拒绝。
- 只留 `edits[]`(pi 的选择):schema 最干净。代价是上面的 parity 与训练分布,而且记忆
  「不考虑兼容」管的是 wire/存储,不是模型的调用习惯。

### 第二问:`edits[]` 的条目里允许 `replace_all` 吗?

- **允许(推荐)**:单段形状有,多段里拿掉会让「改名 + 顺手改一处注释」退回两次调用。
  重叠检查对该段的每个命中区间都做。
- 不允许(pi 没有 `replace_all`):语义更简单,要全替换就用单段形状。

开工前先量一下这件事值多少:在本机 rollout 里数「一条 assistant 回复里对同一 `path` 发了 ≥2 个
`edit_file`」的次数,以及其中有一个失败的次数。`scripts/tool-usage.py edit_file --key path` 只给
跨回复的重复率,不够用;写个一次性查询即可,**不要为它扩脚本**,除非顺手。数字写进完成记录。

## 五、不做

- 不把 `write_file`/`notebook_edit` 也改成多段。
- 不做嵌套条目里的同义词改名(2.1 末条)。
- 不学 pi 把旧 `oldText` 合进 `edits`(2.1 第二条)。
- 不改 hook 的输入形状:`pre_tool_use` 看到的就是模型发来的 JSON。解析 `old_string` 的外部 hook
  脚本在多段形状下要自己看 `edits`——完成时在 DESIGN.md hooks 段落提一句即可。

## 六、测试

`text_edit.rs`(纯函数,整对象断言 `TextEditPlan`/结果字符串):
- 两段不相交 → 两处都改,整文件逐字节断言。
- 第二段的 `old_string` 只在原文里存在、在第一段改完后才会出现/消失 → 仍按原文定位(对原文匹配的核心语义)。
- 两段重叠 → 拒绝,点名 `edits[0]` 与 `edits[1]`;首尾相接 → 允许。
- 一段原样、一段标点容错 → 各自定层,容错段的共有前后缀保留原字节,原样段不受影响。
- CRLF 文件两段都走 LF 回退 → 换行风格不变。
- 一段 `replace_all` 命中三处、另一段落在其中一处里 → 重叠拒绝。
- 拼完与原文相同 → 拒绝。
- 既有 `apply_text_edit` 测试全部原样通过。

`fs.rs`(真实文件 + FileState,整串断言结果文案):
- 三段成功 → `edited <path> (3 edits, 3 replacement(s))`,FileState 刷新为写后版本。
- 第二段 not found → 文件字节不变,报错开头 `no edits were applied`,列出 `edits[1]` 的最近匹配。
- 两段都失败 → 两段诊断都在。
- `edits` 与 `old_string` 同给 → 拒绝;`edits: []` → 拒绝;`edits` 为 JSON 字符串 → 正常执行。
- 读后文件变过 → plan 195 的 note 只出现一次。
- 未读 → 坐标提示取第一个能定位的段。

其余:
- `diff.rs`:多段预览是一份整文件 diff,含两处 hunk;有段定位失败 → 逐段降级,`⋮` 分隔。
- `toolrow.rs`:多段显示第一段 + `(+2 more edits)`。
- `mod.rs:3083` 描述整串断言同步;新增断言 schema 的 `required` 为 `["path"]`、含 `edits`。
- mock provider 端到端一条:一次回复发一个三段 `edit_file`,在需要审批的配置下**只弹一次**确认
  (抓 `ConfirmRequest` 计数与 preview 内容)。
- `plan49_parity_tests` 不改,`make parity` 仍绿(若第一问选了「只留 `edits[]`」,这一条要另议)。

## 七、完成时要一起做的

- `rust/DESIGN.md`:先读 `edit_file` 那段(约 1849-1905 行,「requires an existing UTF-8 target」
  到 unread 提示)与「Change previews」(约 963 行),判断哪些句子仍成立;在匹配规则段补多段语义
  (对原文匹配、各段独立定层、不重叠、全有或全无、失败全列),预览段把「the `old_string` doesn't
  uniquely match」的降级改写成对多段也成立的说法;toolrow 那句(约 1081 行)补多段显示。
- HANDOFF.md:若拆 `locate`/`apply_splices` 时踩到容错层区间的坑(裁剪后区间 vs 整个命中区间),记一条。
- `refs/README.md` Pi 一节第 5 条标注「已由 plan 208 吸收」。
- 本文件补 ✅、日期、提交号与两问的答案,以及第四节那次测量的数字。
