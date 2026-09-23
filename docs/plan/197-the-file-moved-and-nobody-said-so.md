# Plan 197 — 文件在模型背后变了,没有人告诉它

> 来源:2026-09-23,plan 195 落地后当天。plan 195 把"文件变过"从 `edit_file` 的准入判据降级
> 成一句诊断,并在完成记录里写明:**它只说"变过了",给不出"变成什么样"**,而补上后半截是
> 对照产品的做法(每个工具轮次扫读戳表、变了就自动重读并把 diff 注进上下文)。用户「都做」
> 之后先量了数,量完的结论是**不抄那个做法,做一个便宜十倍的版本**。

## 一、量出来的东西

本机 191 个会话的 rollout,结构统计(不看内容)。口径:遍历
`~/.kloop/projects/v1/*/sessions/*.jsonl`,按 `content[]` 里 `type == "tool_use"` 数调用、
`type == "tool_result"` 的文本匹配那三句拒绝/提示;"bash 既写又提到已读文件"= 同一会话内
先出现过的 `read_file` 路径 basename 出现在一条 bash `command` 里,且该命令匹配
`sed -i|gofmt -w|cargo fmt|prettier|ruff format|black|tee|>|>>|mv|cp|patch|apply`。
**这是一次性脚本量的,`>`/`cp`/`mv` 会误命中,所以上界偏高。**

| | 次数 |
|---|---|
| `read_file` | 3996 |
| `edit_file` / `write_file` | 161 / 28 |
| stale 拒绝(`changed since it was read`) | **7** |
| never-read 拒绝 | 10 |
| bash 命令里**既有写动作、又提到已读文件** | **245**(分布在 **62/191** 个会话) |

两个边界:

- **下界 7**:一次改动真的拦住了一次 mutation。191 个会话 7 次,约每 27 个会话一次。
- **上界 245**:三分之一的会话里至少发生过一次"写到自己读过的文件",平均每会话 1.3 次。

**落差就是问题本身:事件很常见,几乎从不被发现,模型从不被告知。** 而 plan 195 之后,那 7 次
已经变成 7 次成功编辑 + 一句提示,于是**剩下的 238 次里,模型可能一直拿着过时内容,没有任何信号**。

**这个数不足以支撑贵的那个版本,也不足以否掉便宜的那个**,原因要写死在这里:

- 245 里**大部分是模型自己发的** `sed -i` / `cargo fmt`——它知道自己干了什么。它不知道的是
  **格式化器把它没看过的那些行重排成了什么样**。
- 真正看不见的那一类(用户在编辑器里改、另一个进程改)**在 rollout 里不留任何痕迹**,
  从这份语料里量不出来,只能从"它值多少"去判断。
- **⚠️ 这两个数是数量级,不是严谨测量。** `scripts/tool-usage.py` 开头第一条坑就是
  "按常量变更的日期把语料切开,不切就是拿旧常量的会话在评新常量"——而这 191 个会话横跨
  plan 155(读的覆盖面放宽)与 plan 195(stale 降级)前后。**开工第一件事:用那个脚本
  按 plan 195 的落地日期重新切一次。**

## 二、裁决

**做"点名不给内容"的那一版,不抄"自动重读 + diff 注入"。**

在轮次边界上,对读戳表里的路径 stat 一遍;**内容指纹变了**的,发一句点名这些路径的提示,
**不发内容**。约每条路径十几个 token,按上界每会话响 1.3 次。模型自己决定要不要重读。

对照产品那一版每轮对每个读过的文件 stat + 变了就整文件重读进上下文;它的算盘和 kloop 不同:
**它的 stale 是硬拒绝**(模型撞上去就得重来,所以提前喂内容是在省一整个往返),而且它有整条
压缩流水线兜 token。kloop 在 plan 195 之后 stale 只是一句话,提前喂内容省不下任何往返,
只买信息量——**那就该按信息量的价格付费,不是按一整个文件的价格**。

**判据(写死,别顺手扩):内容指纹变了才说话。** `touch`/`chmod` 不说话——plan 195 已经为
`edit_file` 建立了这条判据(`FileVersion::same_content`),这里复用同一个,两处不许分叉。

## 三、形状(开工时要定的点都在这)

现成的地基两样:

- `FileObservation` 存整文件 sha256(`core/src/file_state.rs`),所以"变了没有"是一次 stat +
  (元数据动了才)一次 hash,不必存内容快照。比较用 `FileVersion::same_content`
  (`file_state.rs:492`,plan 195 加的),**本条复用它,不要再造一个**。
- **已经有一个同类的提示机制**:`reread_advisory`(`core/src/tools/fs.rs:73`,挂在
  `tools/mod.rs:1516`)——一句 `<system-reminder>`,**骑在 tool result 上**,而不是自己占一条
  history。它的 doc 写明了为什么:*"so that a replayed rollout reproduces it in the same place"*。

**难点就在这里,这是开工必须问用户的那个点:**

本条的提示**不属于任何一次 tool result**——它说的是"上一轮结束到这一轮开始之间,盘上变了"。
三个选项:

1. **骑在下一次 tool result 上**(沿用 `reread_advisory` 的形状)。replay 可复现,不改 rollout
   格式;但"下一次"是哪一次很任意,而且那一轮没有工具调用时它就丢了。
2. **自己占一条 history 条目**(对照产品的 attachment 就是这样)。语义最干,但**改 rollout 的
   内容**:replay/resume 都要认这条,是本条唯一有兼容面的决定。
3. **只在 mutation 的准入路径上说**(即不做轮次边界,把 plan 195 那句话扩展成"你读过的另外
   N 个文件也变了")。最省,但它只在模型动手时才说,而本条要解决的正是"模型没动手、只是
   拿着过时印象回答"。

**倾向 2**,理由是 1 和 3 都在回避本条的前提。但它动 rollout,所以**开工前必须问**。

## 四、坑

- **自己造成的改动要不要点名。** 模型刚跑完 `cargo fmt`,再告诉它"这 12 个文件变了"是噪声——
  但格式化器重排的正是它没看过的行。倾向**照样点名**(它知道"我跑了 fmt",不知道"哪些我读过
  的文件被动了"),但这句提示必须**短到即使冗余也不刺眼**。
- **提示的频率要量,不要拍。** `REREAD_ADVISORY_EVERY = 3` 是 replay 了 1691 次真实
  `read_file` 量出来的(见 `fs.rs:53` 那段 doc),本条的"每会话/每轮最多说几次"照同样的规矩办,
  **用 `scripts/tool-usage.py`,别拍一个 3**。
- **`ContextReads` 与 `ReadCoverage` 是两件事**(`file_state.rs:80` 的 doc 写了),别拿错:
  前者是"这些行还在对话里"(压缩就死),后者是"模型看过足够多的当前字节"(字节变就死)。
  本条要的是**后者的版本比较**,不是前者。
- **不要在这里重新收紧 plan 195。** 点名之后仍然不拒绝任何编辑;这条只增加信息,不增加门。

## 五、验收

- `make check` 全绿。
- 一条测试钉住:读 A 和 B → 外部改 A → 下一轮的提示点名 A **且不点名 B**,且**不含 A 的内容**。
- `touch`/`chmod` 不触发(与 plan 195 同一个 `same_content` 判据,一条测试锁住两处不分叉)。
- 频率上限有测试,且那个常量的 doc 里写着它是**量出来的**以及用什么命令重算。
- 选了形状 2 的话:一条 replay/resume 测试证明它在同一位置复现。

## 六、开工时定(问用户)

1. **形状 1/2/3 选哪个**(第三节)。这是唯一挡住开工的点——它决定要不要动 rollout。
2. 点不点名"模型自己造成的改动"(第四节第一条)。倾向点名。

## 七、开工时定的两个点(2026-09-23,问过用户)

1. **形状 2**,用户「可以,按 2」。问之前先查到一条 plan 里没写的事实,它把这个问题改小了:
   **形状 2 在仓库里已经有先例,而且不改 rollout 格式**——plan 190 的 todo 提醒
   (`agent.rs` 的 `remind_todos`)就是在轮次边界 `history.record(Message::user_text(..))`,
   rollout 里它只是一条普通 message,replay/resume 原样复现。第三节说的"改 rollout 的内容、
   有兼容面"不成立:不需要新的条目类型。
2. **点名模型自己造成的改动**,用户「可以,点名」。区分"谁改的"要给每条 bash 命令记它碰过
   哪些路径,做不准;而它知道自己跑了 fmt,不知道自己读过的哪些文件被动了。

## 八、✅ 完成(2026-09-23,6c37114)

`make check` 全绿(fmt + clippy + 全部测试)。

### 重切的数(第一节要求的"开工第一件事")

`scripts/tool-usage.py` 加了 `--changed-reads`:按会话回放,`read_file` 记已读、
`edit_file`/`write_file` 刷新(模型自己的写不算在背后变)、bash 按命令文本标脏,每条 assistant
消息前是一个轮次边界。**按 plan 195 落地日切**(`--since 20260922`):只剩 **9 个会话**,响 4 次、
每次点 1 个名——**样本太薄,定不了常量**,所以常量用全量语料定,并照实写明它横跨 195 前后。
这个回放测的是"提醒会响几次",不依赖 stale 是拒绝还是提示,所以跨 195 的语料对它没有第一节
担心的那种污染(那种污染咬的是"stale 拒绝 7 次"那一栏)。

| 口径(全量 191 会话) | 响几次 | 每会话 | 每次点几个名 |
|---|---|---|---|
| 第一版正则 | 162 | 0.85 | 中位 1,p90 9,max 30 |
| 修掉 awk `NR>=1` 被当成重定向 | 91 | 0.48 | 中位 2,p90 9,max 30 |
| **加"每读一次最多说一次"(落地规则)** | **68** | **0.36** | **中位 1,p90 4,max 23** |

- 第一版有两个会话各"响" 30–60 次,全是只读的 `git show X:path | awk 'NR>=1 …'`——
  **看分布之前先看最大的那几个会话**,这次一眼就是假阳性。
- 第一节的"245"是**事件数**,这里的 68 是**提醒数**(同一轮多个事件合成一次、点过名的不再点),
  两个数不是一回事,和 plan 151 的"234 次重叠 → 15 次提醒"同形。
- **每次点名上限 `CHANGED_READS_NAMED_MAX = 10`**:68 次里 66 次 ≤ 8 个名,另两次 21/23 个
  (整棵树格式化),8–20 之间任取一个值切掉的都恰好是这两次。**不设每会话上限**:最坏的会话
  11 次、每次一两行,总共几百 token。

### 与第三节的形状差异

- **"每读一次最多说一次"是落地时加的**,第三节没写。点过名的路径在模型重读或自己写它之前
  **连 stat 都不做**;否则一个被持续追加的日志文件每轮都会被点名,而模型手里那份印象根本没变。
  实现是 `FileState` 的每个条目挂一个 `DiskCheck`(`Unchecked` / `SameContent(version)` /
  `Named`),**挂在条目上而不是 observation 上**,于是任何替换 observation 的读或写自动重新武装它。
- `SameContent` 记下 `touch`/`chmod` 之后那次 hash 的 metadata,下一轮 metadata 不动就不再 hash。
  **没有去改 observation 自己的 version**——那会改掉 `edit_file` 提交前 CAS 比的东西(plan 195 的
  唯一新鲜度守卫)。
- 文件被删了也点名,标 `(deleted)`;其他 IO 错误(权限、写到一半)不说话,下一轮再看。
- IO 在锁外做;回写前核对 observation 没被替换过,被替换了就不动(新的那份是更新的印象,没被查过)。
- **每个 depth 都做**:子 agent 有自己的 `FileState` 和自己的 history。
- 路径按 workspace cwd 显示相对路径(observation 的键是 canonical 的,cwd 两种形式都试)。

### 测试

| 测试 | 锁住什么 |
|---|---|
| `names_the_changed_read_alone_without_its_content_and_only_once_per_read` | **第五节第一条**:读 A、B → 外部改 A → 提示整串等于"只点名 A",不含 A 的新旧内容;下一轮安静;再改 A 仍安静;重读 A 之后再改才再点名 |
| `a_metadata_bump_is_silent_at_the_boundary_and_on_edit_file_alike` | **第五节第二条**:同一次 `chmod` + 改 mtime,轮次边界与 `edit_file` 都一个字不多;随后真改内容,两处都说话。把边界处的比较临时换成整个 version 相等,这条测试会挂(已验证) |
| `a_bash_write_is_named_an_edit_is_not_and_a_deletion_says_so` | 模型自己的 bash 写点名、自己的 `edit_file` 不点名、删除标 `(deleted)` |
| `a_long_list_is_cut_to_the_measured_cap_and_counted` | **第五节第三条**:12 个文件变了 → 前 10 个 + `and 2 more` |
| `a_changed_read_is_named_between_rounds_and_replays_in_place` | **第五节第四条**:真实 `run_turn`(读 → bash 改 → 回答),提示是一条独立 user 消息,夹在 bash 那轮的 tool_result 与回答之间;`load_session_snapshot` 与 `resume_session` 读回的 messages 与内存 history 整体相等 |

### 真实 API 实测(2026-09-23)

`make check` 之外在真实 provider 上跑了四个 headless 场景(responses 轨、deepseek 系模型,
与 plan 195 实测同一条;网关名与用量不进仓库)。`--permission-mode bypass`,每个场景一个
独立 scratch 目录。rollout 里的消息序列逐条核过:

| 场景 | 发生了什么 | 轮次边界的提示 | 模型接下来 |
|---|---|---|---|
| **A** 读 `status.txt` → `sleep 23`;sleep 期间**进程外**把 `green` 改成 `red` | 别人改的,模型没动手 | `- status.txt`,夹在 sleep 那轮的 tool_result 与下一条 assistant 之间 | reasoning 原文 "The file changed. Re-read it." → 重读 → 报 **red** |
| **A0** 同一句 prompt,文件不动 | — | **无** | 读和 sleep 并行发,**不重读**,直接报 green,自己注明"读在等待之前" |
| **B** 读 `a.rs`、`b.rs` → 模型自己跑 `rustfmt a.rs b.rs` | a.rs 被重排,b.rs 本来就是格式化好的 | **只有** `- a.rs`,不点 b.rs | 重读 a.rs → 正确报出新的第一行,并说明 b.rs 没变 |
| **C** 读 `settings.txt` → `touch && chmod 600` | 只动了元数据 | **无** | 直接答,自己说"touch 只更新了时间戳" |

读数:

- **A 对 A0 是这条的价值所在。** 没有提示时模型不会主动重读——A0 里它明确知道读发生在等待
  之前,仍然拿那份印象作答。A 里它唯一能知道"文件变了"的来源就是那句提示。(A 的第一句话里
  它自己说过"check it again in case it changed",但决定重读时的 reasoning 引的是提示本身。)
- **B 就是用户拍板"自己造成的也点名"的那个形状**:模型知道自己跑了 rustfmt,不知道两个文件里
  哪个被动了;提示只点了真变了的那个。
- A 的第一次跑是**作废**的:守护脚本 `pgrep -f 'sleep 23'` 命中了 kloop 自己的命令行(prompt
  里就有这串字),在模型读之前就改了文件。换成 `pgrep -xf` 精确匹配才对。**进程外注入改动的
  实测,触发条件要匹配子进程本身,不要匹配一个可能出现在父进程参数里的字符串。**

真实 API 验不到的:子 agent 那一层(同一个函数、depth 无关,只有单测),以及点名上限(要一次
让十几个已读文件同时变,单测覆盖)。
