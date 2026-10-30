# Plan 244 — "总是允许"的作用域收窄：两词前缀、wrapper、文件工具规则

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：✅ 已完成(2026-10-30)。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目
> 开工第一步先读代码确认，不成立就在本文件划掉并写明原因。

## 问题

DESIGN「Asking」一节(约 1143 行)说 `a` 记两词前缀，"批准 `git commit` 不覆盖
`git rebase`"。近两次修复(`ffcbff3`、`7c9a19a`)收窄了 bash_write 的记忆，下面这些
同形状的口子还开着。

1. **两词前缀取的是原始 argv**(已复核)。`core/src/permissions/describe.rs:162`
   `argv.iter().take(2)` 后直接 `format!("{tool}({head} *)")`:
   - `timeout 600 cargo test` → `bash(timeout 600 *)`;`env RUST_LOG=1 cargo run` →
     `bash(env RUST_LOG=1 *)`;`time cargo build` → `bash(time cargo *)`;
   - `git -C ../lib status` → `bash(git -C *)`;`python3 -c '…'` → `bash(python3 -c *)`。
   之后 `timeout 600 <任意命令>`、`git -C ~/x push`、任意 `python3 -c` 都不再询问;
   升级版的 `sandbox_escalate(...)` 规则还会让它们执行前就跳过沙箱。allow 一侧匹配时
   不剥 wrapper(`core/src/shell.rs:633` 注释"allow … never strip, erring conservative"),
   生成一侧却没有对应的保守处理。
2. **可解析命令的记忆不区分 `disable_sandbox`**(待复核)。`describe.rs:228-233`:
   macOS、`trust_sandboxed=false` 时，对沙箱内的 `make check` 选了 `a`/`p`,之后带
   `disable_sandbox:true` 的 `make check …` 命中 `bash(make check *)` 直接在沙箱外跑。
   opaque 分支(`:251`)特意把 `!no-sandbox` 写进了键，这里没有。DESIGN 说"may run 与
   may run uncontained 是不同权限"。
3. **文件工具的 allow 匹配和记忆没跟上 bash_write**(待复核)。
   `core/src/permissions/rule.rs:88-107`、`:222`;`describe.rs:262-275`:
   - allow 走 `matches_path`,候选含 `original` 与 `original_relative`;已存在的符号链接
     父目录会被跟随：在 cwd 外批准 `write_file(/private/tmp/**)` 后，
     `/private/tmp/la -> ~/Library/LaunchAgents` 下的写入免审批。
   - 用户手写的相对 glob(`edit_file(**/*.md)`)会匹配 cwd 外的绝对路径，正是 `ffcbff3`
     修过的形状。
   - 记忆生成不转义 glob 元字符：`app/[slug]/` 生成 `edit_file(/…/app/[slug]/**)`,
     被当成字符类;不成对的 `{` 让规则解析失败，`map_err(|_| denied())` 把用户选的
     Project 变成"用户拒绝"。`bash_prefix_memory` 已拒绝 `*?[]{}\\`,这里没有。
   - `scope` 为空时生成 `*`(现在被 contained-writes 先放行而走不到，但与 `7c9a19a`
     同形状)。
4. **`strip_wrappers` 不丢 wrapper 选项的操作数**(待复核)。`core/src/shell.rs:633-662`:
   `xargs -I {} rm -rf {}`、`timeout -s KILL 60 rm -rf build`、`env -u CI git push`
   剥完以 `{}`、`KILL`、`CI` 开头，deny `bash(git push *)` 与 `[destructive]` 都不触发。
   DESIGN 说"Wrappers … can't smuggle a command past a rule"。
5. **升级的 Project 持久化失败被吞**(待复核)。`core/src/permissions.rs:978` 附近
   `let _ =`;普通权限门同样情况会发 PermissionNotice。DESIGN 说"persistence fails …
   the UI says the grant was not saved"。

## 开工先问用户

- 第 1 条：首词是 wrapper(`timeout`/`env`/`nice`/`nohup`/`time`/`xargs`/`command` 等)、
  解释器带 `-c`/`-e`/`-m`、或第二个词以 `-` 开头或含 `=` 时，(a) 只提供 Once 与逐字
  会话记忆;(b) 剥掉 wrapper 再取两词(但 allow 侧不剥，记下的规则以后匹配不上，
  等于白记)。推荐 (a)。
  用户 2026-10-30：「不知道哪个更好，按合理简洁的方式做」→ 按推荐的 (a)。

## 做法

1. `bash_prefix_memory` 按开工问答识别上述形状，返回 None 或逐字记忆;升级路径同步
   (注意 plan 245 第 2 条：remember 为 None 时不能落进通配升级)。
2. 带 `disable_sandbox` 的调用要求 `sandbox_escalate(...)` 规则，或单独成键;whole-tool
   的 `bash` allow 同样不覆盖它(开工确认 DESIGN 当前说法)。
3. 文件工具 allow 方向复用 `matches_shell_write` 的语义(只看规范目标，相对 glob 只在
   cwd 内匹配);deny/ask 继续兼顾别名。`remember_payload` 复用 `bash_prefix_memory` 的
   父目录与元字符检查;解析失败不再映射成 `denied()`,改为只提供 Once 并给出原因。
4. `strip_wrappers` 为每种 wrapper 建"带操作数的选项"表;遇到不认识的选项时，deny 与
   危险判定在剩余 argv 的每个位置都试一次(宁多勿少，只用于 deny/危险判定，不用于 allow)。
5. 升级持久化失败照普通权限门发 PermissionNotice。

## 验收

- 单测(旧代码先红):第 1 条每种形状生成的记忆;`timeout 600 git push --force` 在记过
  `timeout 600 cargo test` 后仍询问;`disable_sandbox` 调用不命中普通 bash 记忆;
  符号链接父目录、相对 glob、`[slug]` 目录名、不成对 `{` 四种文件规则;
  `xargs -I {} rm -rf {}` 与 `env -u CI git push` 命中 deny/危险判定;升级持久化失败
  有 notice。
- DESIGN 改写 Asking 一节与"文件规则按词法路径匹配"那句;外层执行沙箱之外 `make check`;
  HANDOFF 记进展与教训。

## 完成记录（2026-10-30）

**复核**：第 1、2、3、5 条成立。第 4 条部分成立——`timeout -s KILL 60 …` 与
`env -u CI …` 在旧代码里已能正确剥到命令，真正漏的是 `xargs -I {} …` 这类带操作数的
短选项，以及未知选项时 `risk_wrapper_command` 停在操作数上；修法仍按原计划。

**第 1 条（a）**：`shell::argv_prefix_is_unstable` 判首词是否 wrapper
（`env`/`sudo`/`nohup`/`nice`/`time`/`timeout`/`xargs`/`command`/`builtin`/`exec`），
或第二词是否以 `-` 开头/含 `=`。`bash_prefix_memory` 遇到这样的段改走
`verbatim_remember`：键是整条命令原文，普通门的键带 `!no-sandbox`，升级门的键是
`sandbox_escalate-script:…`；`rules` 为空，所以不提供 `p`、也不落进通配升级
（plan 245 第 2 条的约束在这一点上先满足了）。稳定的两词前缀仍照旧落规则。

**第 2 条**：`CallFacts` 新增 `no_sandbox` 与可解析命令原文。第 8 层沙箱自动放行
不覆盖 `disable_sandbox`；第 11 层 allow 只认 `sandbox_escalate(...)`（普通 `bash(...)`、
whole-tool `bash` 都不算）；第 12 层会话缓存改查升级签名（opaque 脚本保留原有的
`bash-script!no-sandbox` 逐字键）；第 13 层提问也改用 `escalation_remember_payload`，
于是选 `p` 写下的是 `sandbox_escalate(...)` 而不是 `bash(...)`。

**第 3 条**：allow 方向新增 `matches_allow_path`：绝对 glob 只对规范绝对目标，相对 glob
只对 cwd 内规范相对目标；deny/ask 继续用 `matches_path` 兼顾原文与规范路径。文件工具
的 `remember_payload` 复用抽出的 `path_memory_scope`：根部文件锚回规范 cwd，cwd 本身、
文件系统根和含 glob 元字符的目录不再生成模式；`Project` 只在规则能重新解析时出现，
解析失败按普通持久化失败处理（放行一次 + notice），不再变成“用户拒绝”。

**第 4 条**：`strip_wrappers` 为 `nice`/`timeout`/`time`/`xargs` 建了带操作数/无操作数/
粘连三种选项表（`xargs --replace` 与 `--eof` 的空参数形式按无操作数算）；未知选项留给
`risk_wrapper_views`，对剩余 argv 的每个起点各试一次。opaque 脚本的 deny 改从
`CommandFact::risk_argv()`（未知词保留占位）取整条位置，而不是在第一个未知词处截断；
这套视图只给 deny/危险判定，allow 与只读不消费。

**第 5 条**：`escalate_sandbox` 改返回 `Escalation { outcome, notice }`；Project 规则
写入失败（或规则重新解析失败）时仍放行这一次，并带与普通权限门相同的
`PermissionNotice`，`bash` 工具把它发成 `Event::Note`。

**验证**：新增 `permissions/plan244_tests.rs`（14 项）覆盖上述形状；`shell.rs` 补了
选项操作数、未知选项多起点与两词前缀稳定性单测；原有 seatbelt 升级记忆测试把
`mkdir -p`（第二词是选项，已不再稳定）换成 `git init`，继续验证真正的两词前缀。
外层执行沙箱之外 `make check` 全绿：fmt、clippy、workspace 全量测试（core 1249 项等）、
本机 parity 语料 baseline verified。DESIGN 的 Asking、规则匹配段、包装器段与升级段同步改写。
Linux 与 Windows 未实测。实现提交：`b0b38fc`。

## 审查后修（2026-10-30）

审查实现提交 `b0b38fc` 抓出四处，全部复核成立：

1. **`xargs -I` 的替换效果丢失**：替换串会进入后面每个参数（包括交给嵌套 shell 的
   脚本文本），剥离后按字面分析会把 `marker.txt` 当真实目标，而实际写的是输入行。
   现在 `-I`/`-i`/`--replace[=]` 让包装器分析保持不完整，事实层保留未知写效果；
   deny/危险判定仍用多起点视图，`xargs -I {} rm -rf {}` 照样命中。
2. **`xargs --process-slot-var=` 改写子进程环境**：它会让 `$HOME` 等变量变成数值字符串，
   内层路径不能再按父 HOME 求值。现在置 `environment_unknown`，随嵌套 `-c` 传播。
3. **`time -o` 的报告文件写入消失**：`-o`/`--output` 由包装器自己写文件，剥离后没有
   命令承载；现在让分析保持不完整，保留未知写效果，`bash_write` 规则继续生效。
4. **逐字记忆清空 `argvs`，已有精确升级规则失效**：`matches_escalation` 对空 argv 恒
   false，配置了 `sandbox_escalate(timeout 600 cargo test)` 的调用仍先进沙箱再问。
   现在逐字记忆只清空 `rules`，保留 argv 与写入事实供已有规则匹配；新记忆的粒度不变。

新增 4 条回归（xargs 替换、process-slot-var、time -o、精确升级规则）。外层执行沙箱
之外 `make check` 全绿。审查修复提交：`3760912`。

## 第二轮审查后修（2026-10-30）

第一轮把 `xargs -I` / `time -o` 一律标成包装器不完整，又引出两处回归：事实层提前
跳过内层 `-c`，嵌套的 deny、敏感读取和危险命令消失；整条调用成为 opaque 后，升级
记忆从逐字退成了通配。

现在把包装器的选项语法完整性与参数改写/未知写入分开。已定位的内层脚本继续采集风险，
未知效果仍阻止严格授权并保留未知写效果；参数替换还让内层 home/cwd 未知。字面命令
形状完整、仅带这些包装器未知效果的调用由事实层标记为逐字升级记忆（向外层 `-c` 传播），
不进入通配或持久前缀规则；模型主动脱沙箱和程序失败后升级共用逐字键，普通运行批准
不能替代它。

新增 4 条 gate 回归，覆盖四种选项拼写的内层 deny/ask/危险判定、敏感读取、两个升级
入口的逐字复用与不同命令重问、嵌套包装器、普通批准不能授权脱沙箱。前三条在修改前
均失败。原有写效果与精确升级规则回归保留，shell 单测同步拆开的事实契约。
外层执行沙箱之外 `make check` 全绿：fmt、clippy、workspace 全量测试（core 1257 项，
含这次新增的 4 条回归）、本机 parity 语料 baseline verified。

## 第三轮审查后修（2026-10-30）

用户给出 `printf 'rm -rf build\n' | xargs -I marker bash -c 'marker'`：第二轮只保留
原脚本的字面风险，没有跟踪替换串，执行时的命令却可完全不同。`bash(rm *)` deny 在
manual 沙箱自动放行和 bypass 下都漏拦，`cat marker` 的读取目标也被误当字面路径。

包装器现在保留替换串和它实际作用的 argv，同一 xargs 后写的替换选项覆盖前者。被替换
的直接参数标为未知并保留位置；替换进入中间包装器或 shell 参数时，整段执行体记为未知，
不再合并替换前的字面脚本命令与重定向。未知命令保守命中内容级 deny/ask，已知命令的
未知参数按可能匹配处理；未知读取目标在沙箱自动放行和 bypass 前硬拒绝，危险判定保守。
写效果继续保持未知，普通授权和逐字升级的边界不变。

新增 3 条回归，在修复前均失败。覆盖六种替换选项、整段脚本/部分脚本/命令名/参数、
中间包装器及未知选项、嵌套 shell、未知读取和重定向目标，也覆盖 shell 转义、动态参数、
替换选项覆盖与显式 ask；同时保留直接 echo 数据参数的放行对照，以及原有 rm 危险确认
和升级记忆回归。外层执行沙箱之外 `make check` 全绿：fmt、clippy、workspace 全量测试
（core 1260 项，含 21 项 plan244 回归）、本机 parity 语料 baseline verified。
