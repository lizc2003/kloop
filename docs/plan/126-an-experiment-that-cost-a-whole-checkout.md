# Plan 126 — 一个问题，换来一次完整 checkout

> 来源：2026-09-08 第六轮三方对照。同一天里用户连发三个审查任务，每个都同时交给
> claude、codex、kloop 跑同一个 gateway 仓库（`被审仓库`），
> 读三份 rollout 做的对比。
>
> **对照条件已核实**：kloop 与 codex 同 provider（`gw_router`）、同模型
> （`gpt-5.6-sol`）、同 API family（Responses）、同 effort（`xhigh`），**沙箱语义也
> 相同**——codex 是 `workspace-write` + `/tmp` + `TMPDIR`、网络关，kloop 的
> `SandboxPolicy::workspace`（`sandbox/mod.rs:117`）是 cwd + `/tmp` + `$TMPDIR`。
> claude 走 `permissionMode: auto` 且模型直连，速度与命中率都不可比，只用它的
> findings 做交叉验证。
>
> **kloop 这轮加载的是 plan125 版 SKILL.md**。两次 `skill` 调用的返回体里都有
> plan125 新增的三句（`The list you can still see is the list that survives.`、
> `**The change's own words are not that reason.**`、`**A fact only the user has is
> asked, not assumed.**`），plan124 版（8114 字节 / 157 行）没有，plan125 版是
> 9777 字节 / 181 行。会话 14:32 启动而 `fe3cefc` 15:19 才提交，看着像跑的旧版，
> 其实不是：`BUILTIN_SKILLS` 用 `include_str!` 编译进二进制，而启动脚本
> `~/.local/bin/kloop` 每次 `cargo build` 编译的是**工作区**——SKILL.md 那时已经
> 改好躺在工作区里了。**排查这类问题不能只比 HEAD 的时间戳，要拿 rollout 里的
> 返回体逐句验。**

## 一、三组结果

组 1 `审查：307d7034，e0c6d61e`（14:30 起）：

| | 墙钟 | 采样 | 工具 | 未缓存 in | 命中率 | 产出 | turn 判死 |
|---|---|---|---|---|---|---|---|
| claude | 11.8 min | 56 | 142 | 0.1k | 95.6% | 2 缺陷 + 8 排除 | 0 |
| codex | 61 min | 51 | 31 | 1.15M | 81.8% | 1 条 P2 | 0 |
| kloop | **222 min** | 47 | 127 | 2.98M | 50.6% | **无报告** | **5** |

组 2 `审查：d7aa2779`（18:14 起，被审的是 claude 自己两小时前写的提交）：

| | 墙钟 | 工具 | 产出 |
|---|---|---|---|
| claude（自审） | ~7 min | 16 | 4 条 + 8 排除 + 未验证项 |
| codex | ~23 min | — | 4 条（1 P1 + 3 P2） |
| kloop | **280 min，仍在跑** | 129 | **无报告** |

组 3 `审查：2a3013e4`（18:54 起）：claude 自审 3 条 + 6 排除（~6 min），codex
0 findings + 4 条排除（5 min），**kloop 没跑到**——用户跑完三组时它还卡在第二组。

两条交叉验证的结论，都说明 kloop 的审查判断本身没问题：

- 组 1 中断前的推理已经覆盖了另外两家的**全部**发现：runbook 把内置 `30s` 写成
  「框架默认 `0s`」（= claude finding 2）、`StreamFallbackConfig` 有
  `UnmarshalJSON` 无 `MarshalJSON`（= codex 唯一的 P2）、`startup.json` 框架级
  `stream_fallback` 被 strict decoder 拒（= claude 排除清单第 6 条）。
- 组 2 里 kloop 独立找到了 codex 那条 P1（生成器 `<generator>.py:1030` 仍把
  `capacity` 写进 provider `config`，新 `<builder>` 严格解码会 FATAL），
  claude 自审完全没看到这条，**用户是直接把 codex 的 P1 原文粘给 claude 去修的**。

**输的不是审查，是没能把 turn 跑完。**

## 二、222 分钟花在哪

| 块 | 时长 | 归因 |
|---|---|---|
| 沙箱升级审批阻塞 | 107 min | 本片第三节 |
| 5 次 turn 判死 + 4 次手打「继续」 | ~40 min | 重试预算，与审查无关，见第六节 |
| 正常调查 | ~75 min | 任务本身 + plan125 |

kloop 的采样轮数是 47，codex 是 51——**它并没有想更多轮**。差的是每轮塞多少：
127 次工具调用对 31 次，4 倍。

## 三、根因：一次性程序被写成了测试文件

SKILL.md 第 95 行（plan123 起就在，不是 plan125 加的）：

> Prove the claim locally instead: read the branch, follow it to the caller, or
> **write a throwaway program that runs the path and prints what comes out.**

两家都照做了，形状完全不同。

**codex**：6 个一次性 `main` 程序，文件写到 `/tmp`，**以仓库为 workdir 跑 `go run`**，
直接 `import "内部 Go 包"`。每个 5–10 行，一次循环验 2–3 个
输入。仓库一个字节没动。

**kloop**：写 `review_probe_test.go` + `go test -run TestReviewProbe`。测试文件必须
落在包目录里，而只读审查不许写仓库——于是唯一的出路是
`git archive <commit> | tar -x` 解一棵完整的树出来装它：

| | 工具 | bash | 建临时树 | `go test` | 写探针测试 |
|---|---|---|---|---|---|
| 组 1 | 127 | 36 | **12** | **12** | **11** |
| 组 2 | 129 | 88 | 6 | 7 | 5 |

12 次解包、12 次冷编译四个包。第一次就撞上沙箱：`GOCACHE` 是
`~/Library/Caches/go-build`、`GOMODCACHE` 是 `~/go/pkg/mod`，两个都不在可写根内。
`bash.rs:241` 的 `escalate_sandbox` 无超时地等用户批准，用户不在电脑前——
**6450 秒**。证据是那次 tool_result 的开头：`[Re-ran without the sandbox after user
approval.]`，以及 364s 发起、6814s 才回来的时间戳。同批另外两个工具的结果被一起
挂住，恢复后第一次采样 `cache_read=0 / uncached=101,902`，缓存全丢。

「一次性程序」在任何有测试框架的语言里，最自然的联想都是探针测试。skill 说了要写
程序，**没说它该住在哪**，剩下的代价全是这一句的下游。

## 四、plan125 漏掉的一半：排除不需要 finding 的证据

plan125 把排除的门槛提上去是对的——上一轮 kloop 的 0 findings 是靠采信提交自己的
文档蒙对的。但它写的是：

> An exclusion names the consequence you traced and why it is harmless.

`traced` 到什么强度，没说。于是排除和确认拿到了同一套证据要求。codex 那 4 倍的
差距就在这里：**它的实证只花在要报的 finding 上**，组 1 结尾三条排除全是一句话
推理——「已有 Nacos-first/冻结重启流程」「由现有测试明确固定」「早于本提交，未
重复归因」，没有一条跑了东西。

这两件事本来就不对称：finding 要写出确定的 failure scenario，所以值一次实验；
排除只要说清这条路径的后果无害，读到调用点通常就够了。

## 五、要做的四条

前三条都进 SKILL.md 的 `Verifying` 节，全部语言中立——**skill 是 kloop 的通用能力，
不能假设被审仓库里有一份写好下限的 AGENTS.md**（全文现在没有任何语言或工具名，
新增的也不许有）。

### 1. 一次性程序住在仓库外

接在第 95 行那句之后：

> Keep the experiment small enough that it stays an experiment. A throwaway
> program belongs *outside* the repository — written to a temp path, run with the
> repository as its dependency — not inside its source tree. The form that has to
> live in a package directory (a test file, usually) cannot be written at all
> under a read-only review, and the usual way around that, copying the whole tree
> somewhere writable to hold it, turns one question into a full build. One
> question is worth one small program, not a second checkout.

### 2. 隔离到某个 revision 之前，先问它变没变

kloop 是跑到组 2 第 101 次工具调用才想起工作区已含后续提交（「当前检出的工作树
已经包含后续修复提交，因此刚才那组 Go 测试不能代表 d7」），然后直接又去解包：

> Before isolating a revision to run it, check whether the file even changed
> since then. Usually it did not, and the working tree answers the same question.

### 3. 排除不需要 finding 那一级的证据

补 plan125 漏掉的一半，`excluded` 的定义处：

> An exclusion does not need the evidence a finding needs. A finding has to carry
> a concrete failure scenario, so it earns an experiment; an exclusion has to show
> the consequence is harmless, and reading the path to its caller usually shows
> that. Spend the experiments on what you are going to report.

三句都要进 `skills.rs` 的 `code_review_keeps_its_load_bearing_clauses` 断言列表。
**plan125 的教训照旧：断言片段不能跨行**，SKILL.md 是 wrap 过的 markdown，选片段
前逐条回文件核一遍单行命中。

### 4. 工具链缓存目录进沙箱可写根

改 skill 不能免掉这条：`go run` 冷编译一样要写 `GOCACHE` 和 `GOMODCACHE`，codex
第一次 `go run` 就是 30 秒超时（冷编译），只是它后来有一批带
`GOCACHE=/private/tmp/...` 的已批准命令垫着。这些目录按定义是缓存，写进去没有副
作用，而拒绝它们会把**每一次**编译型语言的实证推向阻塞审批。

**已拍板（用户，2026-09-08）：走 (a)，(c) 另起一个 plan。** 备选的 (b)——给沙箱化的
bash 注入环境变量把缓存重定向到 `$TMPDIR`——被否掉：不扩权限面，但把每次实证都变成
冷编译，codex 第一次 `go run` 那 30 秒超时就是这个形状。

(a) 的形状：新增 `WritableRootOrigin::ToolchainCache`，`SandboxPolicy::workspace`
在 `/tmp` + `$TMPDIR` 之后再 push 一个**平台约定的**用户缓存目录——macOS 的
`~/Library/Caches`，其余平台的 `$XDG_CACHE_HOME` 或 `~/.cache`。只给约定位置：被
环境变量指到别处的工具链是用户自己的配置，`[sandbox]` 的 extra root 已经覆盖它。
单独一个 origin 而不是复用 `Temporary`，因为 `for_workspace` 必须保留它（换 worktree
不会改变 Go 把 `GOCACHE` 放在哪），而且它是唯一一个为编译器而不是为模型存在的根。

**(c) 探查后拆出去**：`escalate_sandbox` 加超时不是一处改动。TUI 的
`Approver::confirm`（`tui/src/events.rs:94`）把请求塞进 `interactions` 队列后 await 一个
oneshot；外层 `timeout` 掉这个 future 只会 drop 接收端，**审批面板会一直挂在屏幕上**，
要让它消失得让 App 扫描并丢弃 `reply` 已关闭的 `PendingInteraction`。CLI 侧更麻烦：
`cli/src/ui.rs:74` 在 `spawn_blocking` 里读 stdin，而 **plan 80 已经证过 Tokio 的
blocking stdin read 不可取消**——超时后那次读还在，会吃掉用户下一行输入。再加上
server 侧的 approver、一个新的 `EscalationOutcome::TimedOut` 和对应的
`ESCALATION_UNANSWERED`（措辞不能像 `DENIAL_HINT` 那样邀请 `disable_sandbox` 重试，
那会去等同一个不在的用户）。四个 surface，与本片的 skill 改动无关，**和第六节那条
重试预算是同一类问题：turn 被外部挂住时 kloop 要能自己走出来**，两条一起做才合算。

## 六、非目标

- **不弱化 plan125 的方向。**「不许拿改动的自述当理由」那条一个字不动。第四节补的
  是它漏掉的一半，不是往回退。先把基础设施修好再看它值多少钱——现在就削它，就永远
  不知道 107 分钟里有多少本来就不该算在它头上。
- **重试预算单独一个 plan。** 那 5 次 turn 判死全是
  `upstream_error: Upstream request failed`（`retryable: true`、
  `semanticOutput: false`），走的正是 plan123 修好的那条路，但 `MAX_ATTEMPTS = 3`
  加 250ms/500ms 退避不到 1 秒就烧完；第 2 段是铁证——用户 7719s 打「继续」，7737s
  又死，18 秒烧完一个 turn 的全部重试预算。codex 是 `stream_max_retries = 5` +
  `request_max_retries = 4`、200ms×2ⁿ。这条与审查无关，不在本片。
- **不动 plan120 的结论。** kloop 组 2 命中率 19% 是单轮增量的算术天花板，plan120
  第六、七节已经定论并拍板走第 2 条。本片第三节减少的解包和冷编译会顺带压低增量，
  但那是副产品，不作为目标，也不重开那个取舍。
- **不调工具调用节奏本身。** 127 对 31 的差距里，本片只处理「同一个问题选了贵 10 倍
  的做法」这一份。

## 七、验证

- 三句进 SKILL.md，三个片段进 `skills.rs` 的 `code_review_keeps_its_load_bearing_clauses`。
  **plan 125 的教训照旧**：SKILL.md 是 wrap 过的 markdown，断言片段不能跨行——
  `not a second checkout`、`check whether the file even changed`、
  `does not need the evidence a finding` 三条都回文件核过，各单行命中一次。
- 沙箱那条的单测 `the_user_cache_dir_is_writable_and_survives_a_workspace_switch`：
  断言缓存根存在、origin 是 `ToolchainCache`、保护同样的 escalation 子路径，且
  `for_workspace` 之后仍在；机器上没有约定缓存目录时，反过来断言策略**没有凭空造一个**。
  按 plan 91 的教训先 `--list` 证明命中（`1 passed`，不是 `0 passed` 的假绿）。
- `cargo fmt --all --check` + `clippy --workspace --all-targets -D warnings` +
  `cargo test --workspace` 全绿，一次 commit。
- **下一轮 dogfood 复查**：同一形状的审查任务，看建临时树/`go test` 的次数是否降到
  个位数、有没有再出现审批阻塞。本片的数字（组 1 的 12/12/11）就是基线。

## ✅ 已完成（2026-09-08；提交 SHA 以本条所在提交为准）

**skill 三句**（`crates/core/src/skills/code-review/SKILL.md`，全部语言中立）：
`Verifying` 节 throwaway program 那句之后补「实验要小到还算实验，一次性程序住在
仓库*外*——写到临时路径、以被审仓库为依赖跑，不在它的源码树里」和「隔离一个
revision 之前先查这个文件到底变没变」；`excluded` 的定义处补「排除不需要 finding
那一级的证据」。三条片段
（`not a second checkout`、`check whether the file even changed`、
`does not need the evidence a finding`）进 `skills.rs` 的
`code_review_keeps_its_load_bearing_clauses`，按 plan 125 的教训逐条回文件核过单行
命中。

**沙箱**（`crates/core/src/sandbox/mod.rs`）：新增
`WritableRootOrigin::ToolchainCache`；`SandboxPolicy::workspace` 在 `/tmp` +
`$TMPDIR` 之后 push 平台约定的用户缓存目录（macOS `~/Library/Caches`，其余
`$XDG_CACHE_HOME` 或 `~/.cache`），目录不存在则不 push。单独一个 origin 而不是复用
`Temporary`，因为 `for_workspace` 必须保留它。README 的沙箱一节同步了可写根描述与
理由。

### 测试

- `the_user_cache_dir_is_writable_and_survives_a_workspace_switch`：缓存根存在、
  origin 为 `ToolchainCache`、保护同样的 `.git/hooks` / `.git/config` / `.kloop`
  子路径，且 `for_workspace` 之后仍在；机器上没有约定缓存目录时反过来断言策略没有
  凭空造一个。按 plan 91 的教训先 `--list` 证明命中，再 `--exact` 跑到 `1 passed`。
- `code_review_keeps_its_load_bearing_clauses` 增三条片段。

### 验证

`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -D warnings`、
`cargo test --workspace`（33 个 target、1436 passed、0 failed）全绿。

**过程中踩到教训 111(b) 的一个新形状**：第一次跑全量用的是
`cargo test --workspace | grep ... | head -20`，`head` 收满 20 行就关管道，cargo
被 SIGPIPE 提前杀掉，输出里 10 个 target / 167 passed 看着像全绿的完整结果。
**验证命令里不许有 `head`**——它会把"跑完了"和"被截断了"渲染成同一个样子。

### 本次没做

第五节第 4 条的 (c)（`escalate_sandbox` 超时）按拍板拆出，与第六节的重试预算合并为
下一个 plan。
