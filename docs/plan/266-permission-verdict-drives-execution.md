# Plan 266 — 权限判定一次产出，执行照单执行

> 2026-10-28 立。来源：全仓只读审查与随后的架构讨论(HANDOFF「〇⁵」节)。
> 状态：待实现，**先做 243、244、245**——它们修的是这里要统一的那几处分歧，先修后统一，
> 每一步都有测试兜底。行号是 2026-10-28 的读数，开工先自己量。

## 问题

权限门的判定与执行计划分开推导，同一类知识各写一份：

1. **同一命令在一次调用里完整解析 5–7 次**。`concurrency_safe`(`tools/builtin.rs:530`)、权限门的
   `CallFacts`(`permissions/facts.rs:386-397`)、执行前 `bash_protection_requirement`
   (`tools/bash.rs:255-262`)、`sandbox_escalation_remembered`(`permissions/describe.rs:55-57` 一侧)、
   `run_foreground` 里再一次 `bash_protection_requirement`;被沙箱拒绝后 `escalate_sandbox` 与裸跑前
   各一次。`bash_reads_sensitive_path` 里同一条 argv 经 `analysis.commands()` 与 `facts.commands`
   各跑一遍 PathFacts,每个参数各 `canonicalize(cwd)`。
2. **执行层重新决定进不进沙箱**。`bash.rs:261-266` 用 `sandbox_escalation_remembered` 自己判，
   与权限门第 8 层"会进沙箱"的前提脱节——plan 245 第 1 条就是这样来的。
3. **两套 glob 匹配语义并存**:`matches_path`(文件工具)与 `matches_shell_write`(bash_write)。
   plan 244 让文件工具的 allow 方向改用后者;这里把两者收成一个。
4. **tree 的选项知识两份**:`core/src/shell.rs` 的 `tree_writes` 与 `permissions/argv_roles.rs` 的
   `TREE`/`tree_long`,长选项集合已不一致(`--json`、`--ignore` 被前者当写入，`--matchdirs`、
   `--info` 后者不认)。目前分歧落在保守一侧，但 HANDOFF 里连续三条修复都出在 tree 上。

## 开工先问用户

- 判定结果的形状。草案：`Verdict { decision: Allow | Ask(..) | Deny(..), sandbox: Contained |
  Escalated | None, protection: ..., remember: Option<Remember>, notice: Option<..> }`,由权限门
  产出，随调用放进 `ToolCtx`(或作为 `execute_tool` 的参数)传给工具;工具只读不改。要先给用户看
  一版类型定义和调用链图再动手。
- 是否顺带让 `concurrency_safe` 也吃同一份分析(它在权限门之前调用，需要把分析提前到分发入口)。

## 做法(分步提交，每步行为零变更)

1. 分发入口对 bash/PowerShell 输入做一次分析，结果缓存进调用上下文;`concurrency_safe`、
   `CallFacts`、`bash_protection_requirement` 改为读缓存。先加
   `if !raw_mentions_sensitive_path(command) { return Ok(false) }` 快路径，并加一条等价性断言测试。
2. canonical cwd 在 `CallFacts` 里只算一次。
3. 引入 `Verdict`,权限门产出、bash 执行消费;删掉执行层的 `sandbox_escalation_remembered` 判断。
4. 合并两套 glob 匹配为一个函数，deny/ask 兼顾别名、allow 只看规范目标的语义作为参数。
5. tree 选项合成一张表，两处都查它。

## 验收

- 每步：permissions/shell/bash 现有测试与 plan 239、243–245 的回归全部不变;
- 第 1 步补计数测试(测试钩子统计解析次数):一次前台 bash 调用只解析一次;
- 第 3 步补"判定说进沙箱 → 执行一定进沙箱"的性质测试(遍历现有用例表);
- DESIGN 权限管线一节改写为"判定一次、执行照单";外层执行沙箱之外 `make check`;HANDOFF 记进展与
  教训。
