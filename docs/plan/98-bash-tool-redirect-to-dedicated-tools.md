# Plan 98 — bash 工具描述在决策点重定向到专用工具(软引导)

> 状态:✅ 已完成(2026-08-25;提交见本次 git log,plan98)
>
> 依赖:Plan 94(provider 中立分节 base prompt,已含「优先专用工具」方针)。

## Context(为何要补)

Plan 94 真实回归时,Anthropic rail(claude-sonnet-4-6)一轮里模型用 `bash grep` 去查文件内容,而不是专用 `grep` 工具。功能上不算错(拿到了结果),但偏离 base 方针「优先专用工具」——专用工具有结构化输出、沙箱/权限一致、分页可控等好处。

排查发现杠杆点没打满:软引导只在两处,且都不在模型决策的那一刻——

- base 方针(`context.rs` 的 `BASE_SYSTEM`)说了「prefer the dedicated tool … grep over shell grep」。
- `grep` 工具描述里也有一句 "prefer this over grep/rg in bash"(`tools/mod.rs`)。
- **但 `bash` 工具描述本身只字未提**别拿 bash 干 grep/cat/find/sed。模型恰恰是在**调 bash 的那一刻**做决定的,决策点上没有拦它的话。

参考三引擎(`~/work/claude-code`、`refs/codex` 的 codex-rs)都把这条红线放进 **bash 工具描述**("avoid `find`/`grep`, use Grep; avoid `cat`/`head`/`tail`, use Read"),正是补在决策点。

## 已拍板设计(软方案)

用户拍板「先上软方案,真不够再单开 plan 做硬拦截」。

- **只改 `bash` 工具描述**:在最前面加一句重定向,补在模型决定是否 shell out 的地方:
  > Prefer the dedicated tools over shell equivalents: grep (not grep/rg), glob (not find), read_file (not cat/head/tail), edit_file (not sed); reserve bash for real shell work like builds, tests, installs, and git.
- **不做硬拦截**:不解析/拒绝「纯 grep/cat/find/sed」的 bash 调用(成本高、易误伤管道/heredoc/`find -exec`;留待后续按需另开 plan)。
- **如实边界**:软引导**不保证 100%**——仍是模型概率选择;要确定性只能靠硬拦截。功能上 `bash grep` 本就可用,本改只提升合规率与一致性。

## 关键文件

- `rust/crates/core/src/tools/mod.rs` — 唯一实质改动:`bash` 的 `description` 字符串补重定向句;就近在已有 bash-def 测试里加断言锁定这句(grep/glob/read_file/edit_file/"reserve bash" 五个子串)。
- README:无「优先专用工具」类行为描述,不需同步(改动在工具描述层)。
- HANDOFF:补一条教训(软引导要补在**决策点**——bash 描述,不只在 base 方针与目标工具描述里)。

## 非目标

- 不做硬拦截/命令解析拒绝;不改 bash 执行、沙箱、权限逻辑。
- 不改 base prompt(plan 94)与其它工具描述;不加依赖/工具;不改 wire/protocol。

## 测试 / 验证

- `tools/mod.rs` 已有测试(built-in bash 胜过同名 source 冒充)处补断言:内建 bash 描述必须含五个重定向子串。
- `cargo fmt --all -- --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test`(1 秒、无网络无 key)全绿。
- 可选真实回归:后续 dogfood 时观察 bash-vs-专用工具合规率(软引导,非硬保证,不作硬性验收门槛)。

## 完成标准

- `bash` 工具描述在最前含重定向句;新断言 + 现有测试全绿。
- 不改 bash 执行/沙箱/权限;不做硬拦截;无新增依赖/工具/wire 变更。
- `cargo fmt` + clippy(`-D warnings`) + `cargo test` 全绿。
- HANDOFF 同步;本 plan 标 ✅ 与提交号;一次 commit。

## 完成记录(2026-08-25)

- `rust/crates/core/src/tools/mod.rs`:`bash` 描述句首加入重定向句(grep/glob/read_file/edit_file + "reserve bash for real shell work");就近在 `all_tool_defs` 的 bash-def 测试里加断言,锁定五个重定向子串,防回归。
- 决策:采软方案,不做硬拦截(见非目标)。README 无需同步。
- 验证:`cargo fmt --all -- --check` 干净;`cargo clippy --all-targets -- -D warnings` 全绿;`cargo test` 全绿(含新断言)。真实合规率提升属软引导、未设硬门槛。

## 真实回归补记(2026-08-25)

三 rail 各拿到一轮干净结果,同一内容检索探针(让 agent 说出 `BASE_SYSTEM` 定义在哪个文件),重点看走**专用 `grep`/`glob`** 还是 `bash grep`。plan 98 针对的「拿 bash 干 grep」偏离,在**所有观测到的检索调用里一次都没重现**:

- **Chat rail**:专用 `grep` → 答 `context.rs`。没走 bash。
- **Responses rail**(gpt-5.6-sol):`grep` → `glob` → 答 `context.rs:13`。全程专用工具。
- **Anthropic rail**(claude-sonnet-4-6):首轮被通道 429 打断,但 429 前唯一发出的调用就是专用 `grep`;隔次重试拿到干净一轮,专用 `grep` → 答。

如实边界:软引导是概率行为,单轮成功是正面信号、非 100% 保证或因果证明;要确定性只能靠硬拦截(见非目标,留待后续)。另一如实小注:Anthropic 那轮把引用 `BASE_SYSTEM` 的 `cli/src/context.rs` 也算成「定义」(实际定义仅 `crates/core/src/context.rs:16`)——属模型精确度抖动,非工具描述缺口(base「精确/如实」方针已覆盖),不为单次抖动再改 prompt。secrets/endpoints 未落任何文件。
