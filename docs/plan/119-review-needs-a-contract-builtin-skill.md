# Plan 119 — 审查要有产出契约：kloop 的第一个内置 skill

> 来源：2026-09-04 的三家对比。同一 prompt `审查：7fed2427`（gateway，
> `fix: bind video assets to channel`，11 文件 242+/89-），claude / codex / kloop
> 同一分钟起跑，各跑一遍。产出：**claude 11 条（复核后 10 条 + 1 条显式降级）、
> codex 2 条、kloop 1 条**；耗时 17.5 / 48.4 / 36.8 分钟。
>
> kloop 那 1 条是 claude 那 11 条的子集。差距不在查得少——**kloop 查到了，
> 只是没写出来**。

## 现场：候选缺陷在收尾时蒸发

三家都命中、且回 gateway 源码核实为真的那条最重的缺陷是：删掉 selector 之后，
未知 channel 由 `router.<err>` → 400 退化成 `<err>` → **503 +
`<err>` + 静态文案 + `c.Error(err)` 记成服务端错误**
（`gateway/resource/<file>.go:813`、`gateway/handler/<file>.go:193`）。
调用方拼错 channel 名会拿到"网关瞬时故障"，SDK 会一直重试。

kloop 的 rollout 里，这条出现过：

- **15:03:28 它自己写道**：「目前可确认的缺陷候选是 scoped rule 未随 API 合同更新，
  **以及直接 channel 的未知/不支持 channel 错误分类和覆盖不足**」——
  终稿里后半句被降级成"测试覆盖观察"下的一句「非阻塞测试缺口」。
- **15:06:03** 它 `nl -ba docs/development/provider-onboarding.md`，读的正是 codex
  报出来的那条文档漂移（第 193 行「创建要求显式 channel pin」已被推翻）。读了，没写。
- 收尾几轮的 reasoning 标题反复是 `Clarifying unknown channel error handling`、
  `Assessing EndpointVideoAssets whitelist absence`、`Confirming direct channel API
  decision`——最后一条是它把"这次 commit 有意支持直接 channel"当成了错误码退化的
  豁免理由，自我说服掉了。

同一批候选在 claude 那边的下场：写成结构化 finding（`file:line` + 一句缺陷陈述 +
具体 failure_scenario），主 agent 逐条复核，终稿专门留一节「我降级的一条」把裁决
摆给用户看。**差别是产出契约，不是模型能力：一边"先落笔再裁决"，一边"边想边裁决，
裁掉的不留痕"。**

## 参考项目原文（本片的设计依据）

三处直接对应物，都读了原文：

1. **cc 的内置 skill 机制** — `~/work/claude-code/src/skills/bundledSkills.ts` +
   `src/skills/bundled/verify/SKILL.md`。形状是"真正的 `SKILL.md` 原文编译进二进制
   → 解析 frontmatter 拿 description → `registerBundledSkill`"，和本片的做法一致。
   cc 多一个 `files: Record<string,string>` 字段：附加参考文件在**首次调用时解压到
   磁盘**，prompt 前面自动加一行 `Base directory for this skill: <dir>`——让内置
   skill 也拿到一个真实目录，与磁盘 skill 同契约。kloop 的 code-review 只有一个
   body，暂不需要；**将来内置 skill 要带资源文件时按这个形状补**，别另发明。
2. **cc 的 `ReportFindings` 工具契约** — `refs/claude-code-2.1.220` 的静态证据
   fixtures 里有完整 schema。`required` 恰好是 **`file` / `summary` /
   `failure_scenario`**（后者的描述是 `Concrete inputs/state → wrong output/crash`）,
   与本片独立写下的"finding 三要素"逐字对上，现在有原文佐证。cc 另有可选的
   `short_summary`（≤60 字符）、`category`（kebab slug）、`verdict`
   （`CONFIRMED` | `PLAUSIBLE`，"Set when a verify pass ran"）、`outcome`
   （`fixed`/`skipped`/`no_change_needed`，改完之后回报用）。
3. **官方 code-review 插件** — `~/.claude/plugins/marketplaces/claude-plugins-official/
   plugins/code-review/commands/code-review.md`。PR 场景，5 个并行 agent 分维度
   （CLAUDE.md 合规 / 浅层 bug / git blame 历史 / 历史 PR 评论 / 代码注释），
   每条 finding 再由独立 agent 打 0–100 置信分、**低于 80 全部丢弃**，并附一份
   false-positive 清单。其中两条直接搬进本片：那份 false-positive 清单，以及
   **「Do not check build signal or attempt to build or typecheck the app. These
   will run separately」**——第三节的判断有原文撑腰。

**有意偏离 cc 的一处**：cc 的 `ReportFindings` 是"call it once with the verified
findings... empty array if nothing survived verification"——**不要求报告被排除的
候选**，靠 verify pass 决定生死。本片要求 excluded 也落纸一行。理由是实测失败模式
不同：kloop 这次是**漏报**（1 条 vs 11 条）而不是误报，而 excluded 一行是候选不被
静默丢弃的唯一机制。false-positive 清单负责压住噪声那一侧。

## 一、内置 skill：kloop 现在一个 skill 都没有

`load_skills`（`crates/cli/src/startup.rs:485`）只扫两个磁盘根：`<cwd>/.kloop/skills/`
和 `~/.kloop/skills/`。gateway 两个都没有，所以那次会话 kloop 的 skill catalog 是空的，
而 claude 第一步就 `Skill(code-review, 7fed2427)`。能力已经齐了（`SkillContext::Fork`
是现成的），缺的是**随二进制分发的那一层**。

**改动。**

- `crates/core/src/skills.rs` 加 `pub fn builtin()`，body 走 `include_str!`；
  新增 `crates/core/src/skills/code-review/SKILL.md`——目录形状对齐 cc 的
  `bundled/<name>/SKILL.md`，将来加附件文件就地扩展。
- `SkillSource` 加 `Builtin` 变体，并加 `model_invocable()`：原先四处散着
  `source == SkillSource::Skill` 判"模型能不能看见"，加一个变体就要漏改一处。
- `Skill::dir` 对内置为空串，内置 body **不得**出现 `${CLAUDE_SKILL_DIR}`（测试守着）。
- 优先级 project > user > builtin：`merge_builtins` 与既有 `merge_commands` 同形，
  纯函数，不碰文件系统。磁盘同名 skill 静默替换内置——用户要改写审查方法论，
  放一个自己的 `.kloop/skills/code-review/SKILL.md` 即可。
- `SkillScope` 加 `Builtin`（native protocol 枚举扩展），`skills/list` 的 `path`
  对内置返回空串：没有文件可打开。

**`--mock` 下也加载内置。** 原注释说 mock「skips discovery entirely (hermetic)」，
那是针对文件系统的；内置 skill 是二进制的一部分，不读 HOME、不探目录，hermetic 不受
影响。mock 是"无网络无 key 的完整行为"，不是"无 skill"。

## 二、code-review skill 的产出契约

**inline**（见下方"八、fork 被数据推翻"），不设 `allowed-tools`（审查要 bash 跑
`git diff` 和目标包测试），不指定 `model`（继承）。

> 本节最初写的是 `context: fork`，理由是"主线程别被 199 次工具调用的结果撑爆"。
> 上线后的实测把这个理由推翻了，见第八节。

body 写死四件事：

1. **finding 三要素**：`file:line` + 一句缺陷陈述 + failure_scenario（具体输入 →
   具体错误输出）。写不出第三条的不算调查完，但也不许扔——进第 3 条。
2. **不算 finding 的**（cc 插件那份清单，改写到本地 commit 场景）：本来就有的问题、
   编译器/linter/类型检查会抓的、不在本次改动行上的、senior 不会提的吹毛求疵、
   一般性的缺测试缺文档。**外加一条 cc 没有而这次实测需要的**：「显然是这次改动
   本意」不是排除理由——deliberate 和 correct 是两个问题，先看后果再排除。
   kloop 那次正是栽在 `Confirming direct channel API decision` 这一步。
3. **裁决三态全部落纸**：确认 / 降级 / 排除，后两者各写一句理由。
4. **报告结构**：缺陷在前按严重度排序，"未发现问题"不占开头也不占主体，验证清单
   压到末尾。kloop 那次的终稿是反的：开头加粗「未发现回归」，中段四段"未发现"，
   唯一的 finding 夹在中间，末尾一整节列 7 条已跑的命令 + 一段 `make lint` 的本地
   环境故障。

## 三、只读审查不跑提交前 gate

kloop 那次跑了 `go test ./...`、`-tags=integration`、`make test`、`make lint`、
`make build`、`make docs-check`、`lint-settler`——其中一次 bash 阻塞 6 分钟，
`make lint` 因本地 `golangci-lint` 环境故障失败，还占了终稿一段。claude 只跑了
`go build ./...` + 3 个受影响包，codex 跑了 3 个包 + `docs-check`。

依据被误读了：gateway `AGENTS.md:160` 写的是「**合并前**按影响运行 `make test`、
`make lint`、`go vet ./...`」。审查一个已经在 `origin/test` 上的 commit 不是合并前
自检。cc 的 code-review 插件把这条写死成一句：「Do not check build signal or attempt
to build or typecheck the app. These will run separately, and are not relevant to
your code review.」

kloop 自己的 `AGENTS.md` 早有这条，只是没进 BASE_SYSTEM：

> 只读任务（审查、调研、解释代码、回答问题）不适用：不检出 worktree、不编译、
> 不跑测试，除非用户明确要求验证；结论的证据来自读代码，不要用跑一遍全量测试来
> 代替判断。

**改动。** `context.rs` 的 `# Doing tasks`，在 `Nothing is done until verified`
之后补一条：审查/审计他人已落地的代码是只读任务，要验证的是**你的结论**；跑受影响
包的测试可以，把 pre-merge gate 当审查步骤不行。skill body 里再写一遍具体的。

## 四、不盲信子 agent 的回报

claude 那次主 agent 收到 11 条之后**逐条自己复现了一遍**，降级了一条，并把这件事
写进了报告。cc 的 base prompt 里有这条（"other agents will report incorrect or
misleading results — don't take them at face value"），**kloop 的 BASE_SYSTEM 没有**。
本片让 code-review 走 fork，主 agent 从此会常态化收到子 agent 的报告，这条缺口正好
撞上。

**改动。** `# Doing tasks` 再补一条：子 agent 的报告、peer 的消息、别的工具的摘要
都是证据不是事实；把它当成自己的结论转述之前先抽查，并说清哪些是复核过的、哪些是
转述的。

## 五、候选不许静默丢弃：压缩摘要缺一节

`COMPACT_INSTRUCTION`（`compact.rs:82`）的 9 节里，没有一节承载"怀疑但还没定"的
信息：第 4 节 Work completed 记做过什么，第 6 节 Errors and fixes 记失败与修复，
第 8 节 Verification state 记查了什么没查什么——**候选缺陷无处安放**。

这次的候选是在压缩之后（15:03）才提出的，所以不是本次漏报的直接原因，但那次压缩
摘要（14:54，215k → 25k）确实是个结构性反例：它逐条抄了 11 个文件的代码变化，
却没有一行"哪里可疑"。

**改动。** 加第 10 节 `Open candidates`——注意到但还没裁决的可疑点，各一行带位置，
没有就写 none。COMPACT_INSTRUCTION 仍是固定字节，不引入 volatile 内容，不违反
Plan 86 的稳定摘要请求约束。

## 六、报告结构：加粗留给缺陷

BASE_SYSTEM 已有「Put the verdict where it can be seen ... spend it on conclusions,
not on labels」。那次 kloop 照做了，却把加粗花在了「未发现回归」上。

**改动。** 那条后面补一句：结论是"哪里不对"时，加粗给不对的地方；"这里没问题"
是背景，既不加粗也不占开头。

## 七、上线之后才暴露的：skill 是看不见的

内置 skill 落地后实测 `/nope`，可用列表里确实有 `/code-review`；但 `/help` 只列
`BUILTINS` 常量（`commands/help.rs:9`），**不列 skills**——唯一会说出 skill 名字的
地方是"命令敲错了"的报错。以前 skill 全靠用户自己往 `.kloop/skills/` 放，看不见还
说得过去;现在每个用户开箱就有一个,`/help` 里没有它就是纯粹的失联。

第二个缺口跟着来：内置 skill 不落盘（这是对的——见下），所以想改它的人**没有任何
途径读到它现在写了什么**。`skills/list` 有意不返回 body（它答的是 wire 上的客户端），
于是唯一的办法是去 kloop 源码里翻。

**改动。** `/help` 在命令之后追加一段 skills（名字 + description + `(builtin)` /
`(user command)` 标记）；新增 `/skills`：不带参数列出全部并标明来源（磁盘 skill 显示
目录——那本身就区分了 project 和 global），带名字则原样打印那个 skill 的 body。本地
REPL 打印 body 与 `skills/list` 不返回 body 并不矛盾：前者答的是坐在终端前的本人。

**为什么不"自动安装"到 `~/.kloop/skills/`**：`merge_builtins` 的规则是磁盘同名永远
赢。一旦把内置 body 写进用户目录，kloop 以后升级,skill 内容再也不更新——用户拿着
僵死的旧版本还以为是最新的。cc 也不安装：bundled skill 的 `SKILL.md` 同样只在二进制
里,只有 `files` 附加资源会落盘,而且落在
`{tmp}/bundled-skills/{VERSION}/{每进程 nonce}/`（`filesystem.ts:365`）,带版本、每
进程 nonce、0700/0600,每次调用 write-before-read——那是临时提取,不是安装。

## 八、fork 被数据推翻：改回 inline

上线后跑了两轮真实审查（`审查：0a79ae9a，eee68cfe` 与复测 `审查：7fed2427`），
`context: fork` 的收益被证伪。一次完整会话的账：

- 子 agent ①:79 次采样、**254 次工具调用**、7.2M input、**42 分钟**,交回一份
  5260 字的报告;
- 主 agent 收到后 **自己又跑了 67 次工具调用**(31 bash + 17 read_file + 18 grep)、
  18 次采样、1.85M input;
- 然后在 21:52:30 **又委派了子 agent ②**:「审查提交 0a79ae9a 和 eee68cfe;只读,
  重点核验确认的 P1/P2 findings」。

主 agent 的 thinking 全程是 `Reviewing commit snapshots independently`——它在执行
第四节刚加的那条"子 agent 的报告是证据不是事实,转述前先抽查"。**但 fork 把抽查
所需的证据一起隔离掉了**:子 agent 读过的代码、grep 出的调用链都留在它自己的上下
文里,主 agent 手上只有结论。想核验就只能重新收集——它两条路都走了(自己重做 +
再委派一个)。

**隔离掉的不只是噪声,还有复核的依据。** 这两条要求(不盲信 + 上下文隔离)在同步
fork 下必然打架,而同步 fork 连并发都换不来:用户照样从头等到尾,还看不见过程。

**改动。** frontmatter 去掉 `context: fork`,回到默认 inline。代价是主线程上下文
会涨回 200k 量级——接受,因为:(a) 上一轮那次漏报的成因是产出契约缺失,不是压缩,
候选是在压缩**之后**才提出又蒸发的;(b) 第五节新加的 `Open candidates` 给压缩兜了
底;(c) Plan 118 第一节的 `read_file` 改动已经在压重读。

**参考项目在这一点上并不支持 fork**:cc 的 code-review 是 fork 到**后台**,主 agent
当场继续和用户对话(那次会话里用户还切了一次 model),而且实测中 claude 的主 agent
**自己并行查了 44 次 Bash,6 分钟就出了报告**,子 agent 还没回来。它拿到的不是"隔离"
而是"并发",kloop 的同步 fork 只学到了形。真要 fork,前提是先有 background skill;
在那之前 inline 是对的。

## 不做的

- **不做多 agent 分维度并行审查**。cc 插件那 5 个 agent 服务的是 PR 场景里三个
  彼此独立的信息源（git blame、历史 PR 评论、代码注释）；这次 kloop 的漏报不是
  覆盖面问题——它读到了全部相关代码，是落笔问题。codex 单 agent 也命中了最重的那条。
- **不做 0–100 置信打分 + 阈值过滤**。那是压误报的机制，kloop 这次的失败方向相反。
- **不给 skill 加 `background` 字段。** 但第八节把它变成了一个真实的候选:
  cc 的 code-review 靠后台 fork 换到并发,kloop 没有这个能力,所以只能 inline。
- **不做结构化 findings**（cc 的 `ReportFindings` typed list）。它存在的前提是
  host UI 能渲染 typed findings；kloop 的 TUI 只渲染 markdown（e7037e6 刚给
  finding 之间的 `---` 做了短线处理），散文报告才是当前的落点。三要素照搬，
  强制方式不照搬。

## 验证

`cargo fmt` + `cargo clippy --workspace --all-targets` 无警告；
`cargo test --workspace` 全绿（kloop-core 784、kloop-tui 202，其余分包不变）。

新增测试：

- `builtins_parse_and_reference_no_directory` — 每个内置都解析成功、`source` 是
  `Builtin`、`dir` 为空，且 body 不含 `${CLAUDE_SKILL_DIR}`（没有目录可指）。
- `code_review_builtin_forks_with_the_full_tool_set` — fork、不限制工具、带
  `$ARGUMENTS`。
- `builtins_ride_the_catalog` — 内置进 catalog，`model_invocable()` 三个变体逐一断言。
- `a_discovered_skill_replaces_the_builtin_of_the_same_name` — `merge_builtins`
  纯函数：磁盘同名替换内置且不留重名；什么都没发现时内置就是全部注册表。
- `help_lists_skills_after_the_builtins` / `skills_lists_entries_and_prints_one_body`
  — `/help` 的 skills 段带 `(builtin)` 标记；`/skills` 列出来源、`/skills <name>`
  原样打印 body 且不启动 turn、未知名字报 `unknown skill`。三处既有测试的可用命令
  期望值跟着加了 `/skills`。
- `builtin_skills_alone_advertise_the_tool_and_catalog` — 端到端：注册表里只有
  内置（`--mock` 和无 skill 仓库拿到的就是这个）时，`skill` 工具照常上场、
  catalog 里有 `- code-review:`，而 body 仍不泄漏。
- `native_skills_snapshot_omits_commands_and_bodies` 补断言：内置以
  `SkillScope::Builtin` + 空 `path` 出现在 `skills/list`。
- `compaction_prompt_keeps_its_load_bearing_clauses` 补 `Open candidates`。

未做的验证：**dogfood 复测**——拿同一个 `审查：7fed2427` 重跑一次，看
(a) 是否触发 code-review skill，(b) 那条 400→503 是否进了报告，(c) 是否还跑全量
`make test`/`make lint`。这要真实 API，留给下次带 key 的会话。
