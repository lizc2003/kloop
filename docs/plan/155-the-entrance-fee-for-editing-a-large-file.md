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
