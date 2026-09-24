# Plan 212 — 先量,再调

> 来源:2026-09-24 读 `refs/pi`(`earendil-works/pi@d5629e2`,MIT)后与用户逐条定的,
> 出处见 `refs/README.md`「Pi 全面复查(2026-09-24)」第 10 条。
> **这是一份设计 plan**:产出是一张定稿的决定清单(写进 `rust/DESIGN.md`),不是实现。
> 实现按清单另立 plan。下文 pi 的路径相对 `refs/pi/packages/evals/`。

## 一、为什么

kloop 现在能证明的只有"代码按设计工作":`make test` 用 Mock provider 断言循环、压缩、工具;
`make parity` 对着 Claude Code 2.1.220 的语料断言行为形状。**没有一样东西回答"改完之后,真实
模型把活干得更好还是更差"**。系统提示、工具描述、`request_reduction` 的阈值(`core/src/
request_reduction.rs:44-58` 的注释自己写着 "tune from dogfood, not from first principles")、
edit 容错层——这些改动今天的依据是读代码和翻 rollout(`scripts/tool-usage.py`)。

rollout 统计是事后的、没有对照组:同一个任务不会在"开"和"关"两边各跑一次,所以量出来的差
混着任务差、模型抖动和时间。plan 201 的完成记录就是例子:想用数据决定一问,语料里真正的样本
只有 1 条。要调,先得有一把能做对照的尺子。

## 二、从 pi 借什么(只借方法,不依赖 vitest-evals)

逐条核过 pi 的实现,下面是它真正做的,不是 README 的转述:

1. **先展开、再执行,计划落盘**。`src/plan.ts:39-59` 把 (case, variant, model, runNumber)
   全部展开成任务列表;`expected-runs.json` 是"本该有哪些臂"的真相,后面的配对以它为准,
   而不是以"跑出来了哪些"为准。model 身份必须是 `provider/model` 形(`plan.ts:44`)。
2. **按 run 号交替顺序**。奇数 run 先 control 后 treatment,偶数反过来(`plan.ts:53-54`),
   抵消"先跑的那臂撞上限流/缓存"的顺序偏差。
3. **每臂全新环境,被测者看不到评分器**。`src/docker.ts:98-122`:只读根文件系统、tmpfs、
   每臂独立输出目录、降权 UID(65532)、凭据只读挂载;评测定义与评分器 root 所有、降权后不可读。
   README 自己也承认:容器要联网访问 provider,所以 Docker 证明不了 agent 写的代码不联网。
4. **成对比较、算 lift**。`src/report.ts:249-281` 按 (evalSet, case, model, run) 配对;
   每臂必须**恰好一个**已评分观测,缺失、重复、skipped、pending、errored、unscored 都让整对
   **blocked**。lift = treatment 通过率 − control 通过率(`report.ts:376-396`)。
5. **标出不可信的结果**。`comparisonFlags`(`report.ts:336-357`):no-lift、negative-delta、
   control/treatment 饱和(通过率 = 1)、flaky(同一 case 同一臂在不同 run 结果不同)。
6. **缺的遥测是"不可得",不是 0**(`report.ts:306-318` 与 `formatEvalComparisonReport`)。
7. **产物**:`protocol.json`(模型、镜像 ID、cases、tasks、protocol digest)、`expected-runs.json`、
   `observations.jsonl`、每臂原生报告与原生会话 JSONL、`report.json`/`report.txt`。

**纠正 refs/README 的一处转述**:README 写"任一臂缺失/出错则整对作废、不计入结果但扣总分"。
pi 实际**不扣分**:一个 eval set 里只要有一对 blocked,这个 set 的通过率与 lift 就整个**不发布**
(置 null,报告写 "withheld because pairs are blocked",`report.ts:379, 444-446`),进程非零退出
(`src/cli.ts:192`)。这比"扣分"更严:不拿残缺的对照组出结论。kloop 照这个做。

## 三、kloop 已有的零件,与它们的边界

- **headless**:`kloop --headless [--json] [--max-rounds N]`(`cli/src/args.rs:82-90`),
  `run_headless`(`cli/src/headless.rs:147`)跑**恰好一个 turn**(`HEADLESS_TURN_ID`,
  `headless.rs:69`),按结局退出 0/1(`exit_code`,`headless.rs:61`)。`--json` 的 NDJSON 复用
  server 的通知形状。**两个边界**:① 没有审批者,任何 ask 自动拒绝(`DenyApprover`,
  `headless.rs:75-84`),评测臂必须 `--permission-mode bypass`,于是隔离只能靠外层;② 只有一个
  turn,多轮用例(追问、`/compact`、闲置超 TTL)要走 `--serve` 的 JSON-RPC,或者不做。
- **配置与状态都挂在 `$HOME` 下**:`~/.kloop/config.toml`(`cli/src/user_config.rs:18, 117`),
  会话在 `~/.kloop/projects/v1/*/sessions/*.jsonl`(`scripts/tool-usage.py:58`)。kloop **没有**
  `--config`/`--model` 命令行覆盖,所以"一个 variant = 一份 config.toml",每臂给一个全新的
  `HOME` 即可同时隔离配置、会话和 skills/commands 根(`cli/src/startup.rs:469-474`)。
- **原生会话 JSONL 就是遥测源**:rollout 有 `ProviderUsage`(按请求的 usage,含缓存读写,
  `core/src/rollout.rs:356, 699`)、`TurnTerminal`(`rollout.rs:367`)、`RequestStub`
  (`rollout.rs:385`,reduction 实际落了几个 stub)、`Compacted`(`rollout.rs:362`)。
  观测从这里抽,不另加埋点。
- **Mock provider**:`--mock` 在 CLI 里是一段固定脚本(`cli/src/provider_config.rs:65-72, 131`),
  跑不了任意用例;但 core 的 `Provider::mock` 可逐 turn 编排(`headless.rs:311-319` 的测试就是
  这么用的)。它用来测**评测器自身**的管线,不用来产生评测结论。
- **沙箱不能充当隔离**:macOS seatbelt 只管写和网络,**读是全盘的**(DESIGN.md「OS sandbox」),
  `read_file` 等文件工具也不在沙箱里。本机跑时,评分器只要在磁盘上就能被读到。
- **门禁只有本机 `make check`**,CI 已去掉(AGENTS.md)。真实评测要 key、要钱、非确定,
  **永远不进 `make check`**;进门禁的只有评测器的纯逻辑测试。

## 四、要定下来的形状(决定清单)

每条先写推荐,问答后把定稿写进 DESIGN.md。

### 4.1 单位与计划

- 任务单位 = (case, variant, model, run)。`variant` 固定两臂:`control` / `treatment`,
  一次比较只比一个开关。model 身份写 `provider_id/model`(与 `ProviderUsageRecord` 同字段,
  `core/src/usage.rs:16-23`)。
- 执行前先写 `protocol.json` 与 `expected-runs.json`,之后只追加 `observations.jsonl`。
  中途中断可以按 `expected-runs.json` 补跑缺的臂,已有观测不重跑。
- 顺序照 pi:奇数 run control 先,偶数 run treatment 先。
- **protocol digest** 覆盖:kloop 提交号与二进制 sha256、两份 variant 配置**去掉凭据后**的规范化
  digest、每个 case 目录的 digest、`runs_per_variant`、`max_rounds`。换了其中任何一样就是另一次
  实验,报告不许跨 digest 合并。

### 4.2 一臂怎么跑

- 全新 `HOME`(里面只放这臂的 `config.toml`,0600)、全新工作区(从 case 的 fixture 复制),
  `kloop --headless --json --permission-mode bypass --max-rounds N "<case prompt>"`。
- 评分器在 agent 进程**退出之后**才进入工作区运行;评分只读工作区结果与原生会话,不读 agent
  的最终文本以外的"自述"。
- 超时(墙钟)由外层杀进程,记为 `errored`,整对 blocked。
- 执行器写成独立 crate(推荐 `rust/crates/eval`,二进制 `kloop-eval`),把 kloop 当子进程调用,
  不链接 core——评测的是用户实际拿到的那个二进制。理由:配对/报告逻辑要进 `make check` 测试;
  Python 脚本(像 `tool-usage.py`)进不了门禁。

### 4.3 观测(`observations.jsonl` 一行一臂)

`{identity, outcome: scored{score}|errored|timeout|unscored, exit_code, end_reason,
input/output/cache_read/cache_write tokens, requests, tool_calls, compactions, request_stubs,
wall_ms}`。token 与请求数从会话 JSONL 的 `ProviderUsage` 行加总,子 agent 会话一并计入;
**找不到会话文件时这些字段缺省为"不可得",不写 0**。暂不算美元:kloop 没有可靠的价格表
(refs/README 第 4 条的模型元数据表还没做)。

### 4.4 配对与报告

照 pi 的判据:每臂恰好一个 scored 观测才算有效对;任一对 blocked,该 case 集的通过率与 lift
不发布,进程非零退出。报告列:每集 lift、两臂通过率、配对的 token/请求/工具调用/耗时均值差,
以及 flags(no-lift、negative-delta、两种饱和、flaky)。**单 run 不下稳定性结论**,报告里明写。

### 4.5 产物目录

`.eval/<时间戳>_<短 id>/`,整个目录进 `.gitignore`:`protocol.json`、`expected-runs.json`、
`observations.jsonl`、`arms/<identity 的 sha256>/{stdout.ndjson, stderr.txt, grade.json}`、
`arms/<…>/home/.kloop/projects/…/sessions/*.jsonl`(原生会话原地保留)、`report.json`、`report.txt`。
产物里有提示、回复、生成代码,**不提交**;凭据只从用户给的路径复制进每臂 `HOME`,
`protocol.json` 只记 digest,不记 `base_url`/`auth_header`。

### 4.6 不做

- 不做 LLM-as-judge 作为第一版评分(非确定评分器叠在非确定被测者上,lift 的噪声翻倍)。
- 不做模型矩阵的一次性大跑;不做与 Claude Code 的跨产品对比(那是 parity 的地盘)。
- 不把任何真实跑的结果或会话写进仓库;DESIGN.md 只写方法和结论,不写原始数字表。

## 五、开工时必须问用户的点(一次问一个)

### 第一问:在哪跑

- **本机 git worktree + 每臂独立 `HOME`(推荐起步)**:零搭建,macOS 直接跑,kloop 自己的
  worktree 能力现成。代价是隔离只靠约定——读是全盘的,评分器放在仓库里就能被读到;所以评分器
  与 fixture 要放在仓库外、只在 agent 退出后拷进来,报告里标明"非密封"。
- 本地 Docker(pi 的做法):真隔离,但要交叉编译 Linux 版 kloop、镜像维护、macOS 上的 bind
  mount 与性能。等第一问的结果显示值得认真量了再上。

### 第二问:用例从哪来

- **自己写 8~12 个小的确定性任务(推荐)**:每个一个 fixture 目录 + 一条 prompt + 一个评分脚本
  (跑测试、比文件)。选能碰到要调的那几块的:多处编辑、长读后改、带失败重试的构建。
- 从真实 rollout 抽:最贴近使用,但要把私有项目内容清出来,且缺评分器。
- 公开基准(SWE-bench 类):任务真,但单任务贵、环境重,不适合第一版。

### 第三问:预算与模型

多少钱一次、用哪一个(或两个)模型、哪条 rail。推荐:**一个模型、一条 rail、每臂 3 run**
(10 个 case × 2 臂 × 3 = 60 次 headless),先按用户给的单次上限设 `--max-rounds` 与墙钟超时。
代理与 key 由用户提供,只放在本机 config,不进任何提交文件。

### 第四问:评分怎么定

推荐**只用确定性评分**:0/1(测试过没过、文件对不对),可选附带部分分;评分脚本退出码即结论,
评分器自己崩了记 `unscored`(整对 blocked),不当成 0 分——pi 的原则"低分是数据,基础设施故障
不是"。

### 第五问:第一次 A/B 比什么

**先说一个前提的修正**:原提议是 `request_reduction` 开/关。按 DESIGN.md「Request-time reduction」
与 `request_reduction.rs:15-18`,新 stub 只在缓存已冷时落:这条 history 对该模型的第一个请求
(这时没有旧结果可 stub)、闲置超 TTL(Anthropic 5 分钟、OpenAI 两条 rail 1 小时)、或刚压缩过。
**headless 单 turn 连续跑时几乎不会冷**,DESIGN 自己也写了 "a long unattended run may never go
cold"——两臂发出的请求会基本逐字节相同,量出来一定是 no-lift,而那不是 reduction 的真实效果。
要比它,用例得强制一次压缩(小窗口配置),或走 `--serve` 做带闲置的多轮,每臂还要核 `request_stubs > 0`。

推荐的第一次 A/B 换成**两臂请求确实不同**的开关,例如某条工具描述或系统提示段落的新旧两版,
先验证整条管线(计划→跑→评→配对→报告)产出可信的 blocked/flaky 判定,再回头设计 reduction 的用例。

## 六、测试(实现时必须有,全在 `make check` 里,无网络无 key)

- 计划展开:2 个 case × 3 run → 12 个任务,顺序逐个整对象断言(run 1、3 control 先,run 2 treatment
  先);重复的 case 身份、`runs_per_variant = 0`、不含 `/` 的 model 身份各报错。
- protocol digest:改 variant 配置里的 `auth_header` **不改** digest;改 `request_reduction` 改 digest。
- 配对:缺一臂、同臂两条观测、一臂 errored、一臂 unscored,各产生一个 blocked 对,原因整对象断言;
  有 blocked 时该集 `lift == None` 且退出码非零。
- flags:通过率相等 → no-lift;control 全过 → control-saturated;同 case 同臂跨 run 结果不同 → flaky。
- 遥测:会话 JSONL 有 3 条 `ProviderUsage` → 求和正确;没有会话文件 → 字段为不可得而非 0;
  子 agent 会话计入。
- 端到端(Mock):用 core 的 `Provider::mock` 编排一个会写文件的 turn,跑一对臂,评分器判一臂过
  一臂不过,`report.json` 整对象断言;中途杀掉一臂后续跑,只补缺的那臂。
- 凭据不外泄:跑完后在整个产物目录里搜测试用的假 `auth_header`,除每臂 `HOME/.kloop/config.toml`
  外一处都不许出现。

## 七、完成时要一起做的

- `rust/DESIGN.md`:先读「Verification」(现在只讲 `cargo test` 与 parity)与「Headless mode」
  两节还成不成立,再**新增**一节「Behavior evals」写定稿的决定清单;Verification 里补一句
  "真实模型评测不在门禁内,见该节",不重写原有内容。
- `refs/README.md` Pi 一节第 10 条:把"扣总分"改成"blocked 则不发布该集通过率、非零退出",
  并注明已由本 plan 吸收。
- HANDOFF.md 记教训:**对照开关先确认两臂发出的请求真的不同**(reduction 在不冷的会话里是 no-op)。
- 本 plan 补 ✅ 与提交号;实现另立的 plan 号写在这里。
