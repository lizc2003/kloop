# Plan 155 — 改大文件的一行,先付六次读

> 来源:2026-09-15,用户问"基础工具的能力有缺失吗"。查下来工具**没有**缺失
> (差点建议的 MultiEdit,在 `refs/claude-code-2.1.220/fixtures/normalized/` 的工具清单里
> 根本不存在,cc 2.1.220 只有 `Edit`——kloop 与基线一致)。缺的是一项能力约束的代价。

## 一、三个事实凑在一起

1. `edit_file` 要求 **"The entire file must have been freshly read in this session"**
   (`core/src/tools/builtin.rs:744` 的工具描述),资格判定是 `fs.rs:1283` 的
   `if !expected.is_complete()`。
2. `read_file` 单次上限 `READ_CONTENT_CHARS = 30_000`(`core/src/tools/fs.rs:92`)。
3. **kloop 自己 147 个 `.rs` 文件里,55 个超过 30 000 字符——37%**:

   | 文件 | 字符 | 读满要几次 |
   |---|---|---|
   | `core/src/permissions.rs` | 166 059 | 6 |
   | `tui/src/app.rs` | 156 513 | 6 |
   | `core/src/tools/mod.rs` | 145 017 | 5 |

改 `permissions.rs` 的一行,先得付 6 次 `read_file`、约 4 万 token 才拿得到编辑资格。
一个会话里要动三个大文件各一处,入场费约 17 次读、12 万 token——而真正需要的上下文是三处。

## 二、为什么这笔钱花得特别冤

**教训 117 已经量化过这个模式**:那次"把 2556 行 diff 用 6 个并发 `read_file` 一次全读
回来"是全程**最贵的一轮**(单轮 +33k、整轮零缓存命中);codex 同样的活是 `wc -l` 先探大小
再分批取。也就是说,这条资格约束正在**逼**模型去做教训 117 认定为最贵的那件事,
而且在 kloop 自己的仓库里有 37% 的概率触发。

cc 没有这笔开销。plan 49 第 44 行白纸黑字记着这是**有意**的差异:

> kloop 对 existing Write/Edit 要求完整 fresh Read;CC 接受部分 Write 资格、unread/partial
> Edit,并可对唯一 old string stale-recover,因此执行和 lifecycle 保留有意差异。

**所以本 plan 的实质是:要不要推翻 plan 49 的这条裁决。** 见第五节。

## 三、范围:只动 edit,不动 write

- **`edit_file` 改一处却要读全文,不成比例**——这是本 plan 的目标。
- **`write_file` 替换全文,要求读全文是成比例的**(`builtin.rs:729`:"Overwriting an existing
  file requires a complete, fresh read")。**不动。**
- **`notebook_edit` 要求完整读是对的**(cell-aware,`fs.rs:1281` 有专门的报错)。**不动。**

## 四、做什么

放宽的是**资格**,不是**新鲜度**。这两件事必须分开说:

- **新鲜度**(文件自读取后变了吗)一点都不放宽。外部改动仍然让 edit 失败。
- **资格**(你读过要改的地方吗)从"整个文件"缩到"`old_string` 落在你读过的区间里"。

好消息是数据结构本来就是按区间设计的:`file_state.rs` 的 `FileObservation.coverage`
存 ranges(`DEFAULT_MAX_RANGES = 32`),`complete` 是 `recompute_complete()` 从区间**推导**
出来的(`:392`/`:416`),不是一个独立标记。判定"某区间是否被覆盖"不需要新结构。

步骤:

1. 工具进程内读文件定位 `old_string` 的字节范围(它本来就要读文件做替换,这一步不进上下文)。
2. 用该范围查 `coverage`:被读过的区间覆盖则放行,否则拒绝。
3. **把拒绝信息变得有用**:现在是"整个文件必须完整读过";改成告诉模型**该读哪一段**——
   "`old_string` 在第 N 行附近,你没读过那里,先 `read_file(path, offset≈N-20, limit≈60)`"。
   这条比放宽本身更能省轮数:模型不必猜要读多少。
4. `replace_all` 的情形要求**所有**匹配位置都被覆盖,否则退回要求完整读。

## 五、开工时问用户(先问,再动手)

**要不要推翻 plan 49 的那条有意裁决?**

支持推翻的证据在第一、二节:37% 的文件触发、单文件 6 次读、教训 117 已量化这是最贵的模式。

支持保留的理由也要摆出来:"完整读过才能改"是一条**极其好解释**的规则,模型不会误解,
审计时也一眼看得懂;换成区间资格之后,"我能不能改这里"变成一个依赖历史读取记录的动态
判断,出错时更难解释。plan 49 当初选它不是疏忽。

**另外要确认一个降低本 plan 价值的事实**:edit/write 成功后,观察被刷新为 **full**
(`fs.rs:665` 的 `FileObservation::full_with_identity(&outcome.bytes, …)` + `:669` 的
`FileStateUpdate::Replace`)。所以入场费**只付一次**——同一会话里对同一文件的后续 edit
不需要再读。本 plan 省的是"每个大文件在每个会话里的首次编辑",不是每次编辑。
这个折扣要先认下来,再决定值不值得做。

## 六、坑

- **并发 mutation 的资格捕获**。`fs.rs:609-610` 那条注释是现成约束:"Capture the
  qualification before waiting. Two concurrent mutations based on one Read must not let the
  second inherit the first mutation's refresh." 区间资格下这条更微妙——两个并发 edit 各自
  的区间可能都合法,但第一个改完之后第二个的字节偏移已经变了。**先看懂这条注释再动手。**
- **区间是行形还是字节形**。`file_state.rs:493` 的测试注释点明:"Mutation refreshes are
  byte-shaped while text Read ranges are line-shaped." 资格判定要在这两种形状之间换算,
  换算错的方向必须是**拒绝**,不能是放行。
- **`DEFAULT_MAX_RANGES = 32` 的上限**。读了 40 段之后 ranges 会被合并或淘汰,资格判定
  要能容忍这种有损记录——淘汰导致的"查不到"必须判成拒绝。
- **别顺手放宽 stale-recover**。cc 还有"对唯一 old string 做 stale 恢复"这一条,那是另一个
  裁决,不在本 plan 里。

## 七、非目标

- **不动 `write_file` 与 `notebook_edit` 的完整读要求**(第三节)。
- **不做 cc 的 stale-recover**。
- **不改 `READ_CONTENT_CHARS`**。调大单次读上限是另一条路,而且方向相反——它让单轮更贵,
  正是教训 117 警告的。
- **不碰 `old_string`/`new_string` 形态本身**。grok 在试第三条路(`hashline_read`/
  `hashline_edit`,行哈希锚点 + 漂移恢复,见 `refs/README.md` 2026-09-15 节),但它自己有三个
  候选方案和一个 benchmark harness、**尚未收敛**;kloop 的架构决定 1 是拿实测选的 Edit 形态,
  要动它得有同等强度的实测。

## 八、验收

1. **首次编辑大文件不再需要读全文**:读 `permissions.rs` 的一段(比如 offset 3000、limit 60),
   对该段内的一处做 `edit_file`,成功。改动前这一步应当失败。
2. **区间外仍然拒绝**:同一状态下对**未读区间**的 `old_string` 做 edit,失败,且错误信息
   点名该读哪一段(含 offset 与 limit 的具体建议值)。
3. **新鲜度没有被放宽**:读过某区间后,由外部改动该文件,再 edit 该区间,**仍然失败**。
   这条是本 plan 不能破的底线。
4. **`replace_all` 全覆盖才放行**:匹配点跨越已读与未读区间时,拒绝。
5. **write / notebook 行为逐字节不变**:`existing_write_requires_a_complete_fresh_read_and_refreshes_state`
   (`fs.rs:2422`)与 notebook 的完整读断言原样通过,一条不改。
6. **并发语义不变**:`fs.rs:609` 那条约束有回归测试守住——两个基于同一次 Read 的并发
   mutation,第二个不继承第一个的 refresh。
7. **量一次真实收益**:记一次"改三个大文件各一处"的 token 对比进 ✅ 节。**省不到预期的话,
   如实写下来并考虑撤销**——plan 49 那条裁决的可解释性是有价值的,不该用一个收益存疑的
   改动去换。
8. 仓库完成标准照旧(fmt / clippy -D warnings / test,各自单独取退出码)。

## ✅ 已完成(2026-09-16;提交 SHA 以本条所在提交为准)

**第五节那个问题:推翻了。** 用户同意,理由比 plan 摆的多一条——plan 拿"可解释性"当保留理由,
但完整读买到的安全性比看起来少:`edit_file` 早就强制 `old_string` **唯一**(`fs.rs:1262`),
"没看全文、改到两个长得一样的地方里错的那个"这个主要风险是唯一性挡住的,不是完整读挡住的。
完整读剩下的价值是"让模型看见周边语境",那是质量诉求,工具层强制不了。

**但形状不是 plan 设计的那个。** 用户一句「看参考项目」,照教训 143 先回源,五家的资格规则:

| 项目 | edit 的资格 | 新鲜度靠什么 |
|---|---|---|
| cc 2.1.220 | **读过一次**(任意 offset/limit);`validateInput` 连这个都不要求,`call()` 才要求 `lastRead` 存在(`FileEditTool.ts:449`) | mtime > 读取时刻 → 拒;full read 时 content 相等可豁免(Windows 误报) |
| codewhale | **读过一次**(任意范围,`require_fresh_file_read`,`tools/spec.rs:997`) | 整文件 snapshot 相等 |
| deepseek-harness | **observed 过一次**(`fs-observation-policy` 的 `editIntent`) | 观察到的 version 做 CAS |
| grok-build | **无**——`skip_read_before_edit` 的注释写着 "Deprecated runtime no-op"(`search_replace/mod.rs:102`),只剩配置期"toolset 里得有个 Read 工具"的要求 | 全靠 `old_string` 精确匹配 + no-match 时的 user-edit 提示 |
| codex | **无** | patch 的 context 行自己就是校验 |

**没有一家实现区间资格。** plan 第四节把"放宽"等同于"缩小粒度"(整文件 → 区间),那是个很自然
但零参考支持的中间态;三家选的是更粗的"存在性"。于是落地成 `ReadRequirement`
(`CompleteFile` / `CompleteNotebook` / `AnyRead`)——`edit_file` 只要求该路径有一份观察,
`write_file` 与 `notebook_edit` 一个字节没动。

**这个粒度让第六节四个坑里的三个直接消失**:行形/字形换算不用做(字节形区间只出现在
`full_with_identity` 里,而它恒为 `complete`,走短路)、`DEFAULT_MAX_RANGES` 的有损记录不用
容忍、`replace_all` 的"全覆盖才放行"这条规则随它要守的检查一起没了(新增
`replace_all_spans_read_and_unread_lines` 钉死跨区间放行)。第一个坑(并发资格捕获)查完
发现**本来就不用动**:`fs.rs:609` 捕获的是等锁**之前**的观察,第二个并发 edit 手里那份
`version` 在第一个提交后必然不匹配,`validate_observation_version` 拦掉——新增
`one_partial_read_cannot_authorize_two_edits` 用**残缺**观察复现了这条。

**没跟到最松的那一档。** grok/codex 那条路(什么都不要求)会拆掉新鲜度的比较基准;
"读过一次"正是**保住新鲜度的最小资格**,cc 的 `call()` 里 `!lastRead` 仍然抛错,理由一样。
验收第 3 条(新鲜度不能破)由 `a_narrow_read_does_not_survive_an_external_change` 守住。

**提示词切回 plan 原本的形状。** 资格放宽之后,"读 `old_string` 周围那 60 行"第一次成为
**照做就能解锁**的建议,所以 plan 156 那条"第一行还没读过的行"的形状(教训 148(a) 的产物)
退役:现在报 `offset=N-20, limit=60`(`UNREAD_HINT_LEAD_IN` / `UNREAD_HINT_LIMIT`),
验收第 2 条到此成立。`file_state.rs` 的 `first_unread_unit` 随之删掉(只有那个提示在用)。

**验收第 7 条,量了一遍**(148 个 `.rs`,55 个超 `READ_CONTENT_CHARS` = 30 000,仍是 37%):

| 文件 | 字符 | 旧:读满要几次 | 旧:入场费 |
|---|---|---|---|
| `core/src/tools/mod.rs` | 166 140 | 6 | ~41.5k tok |
| `core/src/permissions.rs` | 165 177 | 6 | ~41.3k tok |
| `tui/src/app.rs` | 156 320 | 6 | ~39.1k tok |

"改三个大文件各一处":**18 次读 / ~122k token → 3 次读 / ~1.8k token**(每处一个 60 行窗口)。
plan 第一节估的 17 次 / 12 万 token 核对无误。

**plan 没算到的第二笔账**:失败的 mutation 会 eagerly `Clear` 观察
(`fs.rs:693`,"只有最终成功的 tool_result 才把授权装回去")。旧规则下一次 `old_string`
不唯一的失败 edit 要**再付 6 次读**才能重试;新规则下付 1 次。这笔比 plan 认下的折扣
("每文件每会话首次编辑")更频繁。

**改了什么**:`fs.rs`(`ReadRequirement` + 三处调用点 + 新提示词)、`file_state.rs`(删
`first_unread_unit` 及其测试)、`builtin.rs`/`mod.rs`(工具描述:"The entire file must have been
freshly read" → "any range qualifies")、`README.md` 三处。**测试**:改写 1 条、新增 4 条;
negative control 跑过——把 `AnyRead` 换回 `CompleteFile`,4 条新测试全红。
fmt / clippy `-D warnings` / `cargo test --workspace` 各自单独取退出码,全绿。

**非目标照旧没动**:`write_file` / `notebook_edit` 的完整读要求(验收第 5 条,
`existing_write_requires_a_complete_fresh_read_and_refreshes_state` 原样通过)、
stale-recover、`READ_CONTENT_CHARS`、`old_string`/`new_string` 形态。
