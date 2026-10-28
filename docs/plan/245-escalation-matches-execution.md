# Plan 245 — 沙箱升级与实际执行对齐

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：待实现。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
> 先读代码确认，不成立就在本文件划掉并写明原因。

## 问题

权限门第 8 层"沙箱会遏制它"的自动放行，前提是执行时真的进沙箱。下面几处让前提与
实际执行分叉。

1. **通配升级 + 沙箱自动放行 = 不审批也不进沙箱**(已复核代码路径)。
   - 某条脚本被沙箱拒绝后，用户选 `a`(提示是"every sandbox escalation this session"),
     会话里记下 `BLANKET_ESCALATION_SIGNATURE`(`core/src/permissions.rs:962`)。
   - 之后任一前台 bash:第 8 层照常因"会进沙箱"自动放行(`permissions.rs:726`);
     执行时 `sandbox_escalation_remembered`(`:991`)第一步就是通配命中，
     `core/src/tools/bash.rs:261-266` 据此 `effective_sandbox = None`,**事前就不进沙箱**。
   - 结果：除危险命令判定、deny、ask 外，等同 bypass 加关沙箱。DESIGN 约 3285 段的论据
     "闸门仍会对每条新命令询问"不成立。
2. **remember 为 None 时一律落进通配**(已复核 `:962`;形状待复核)。不只 opaque 脚本：
   参数含空白、写入目标父目录是 `/` 或含 glob 字符、目标就是 cwd、Windows 带写入时
   `escalation_remember_payload` 返回 None,`a` 也变成通配。`7c9a19a`、`ffcbff3` 用
   "不生成记忆"来收窄，到升级路径反而落进最宽的一档。
3. **可写根内只读子路径的祖先没有防改名**(待复核)。`core/src/sandbox/mod.rs:422-427`、
   `443-451`、`484-497`:`rename_denies` 只为 `denied_write_paths` 收集;`read_only_subpaths`
   (`.git/hooks`、`.git/config`、`.kloop`)只生成 `require-not literal/subpath`。先把
   `.git` 改名(`mv .gi? .g` 或解释器写法)、写 `.g/hooks/pre-commit`、再改回，即可绕过
   DESIGN「Inside a writable root, .git/hooks, .git/config … stay read-only」。plan 239
   已实测出"改名父目录能绕开子树拒绝",但只修了 denied_write_paths。
4. **解析不可信时局部事实整体清空**(待复核)。`core/src/shell/facts.rs:117-125` →
   `permissions.rs:726`:多行脚本里某行有 tree-sitter 不认识的语法，hazard、deny、ask
   全部失去证据，第 8 层仍对 opaque 放行;bash 逐条执行，出错行之前的 `rm -rf data/`
   照常跑。
5. **两份"最近存在的祖先 + 缺失后缀"规范化已经分叉**(待复核)。权限侧
   `core/src/permissions/facts.rs:658-697` 先按词法消 `..` 再 canonicalize,断链当缺失
   叶子;沙箱侧 `sandbox/mod.rs:314-337` 明确"断链不能退到父目录吞掉"。cwd 里有
   `logs/la -> ~/Library/LaunchAgents` 且记过 `bash_write(logs/**)` 时，
   `> logs/la/../LaunchAgents/x.plist` 被判成 cwd 内。
6. **升级审批不感知 turn 取消**(疑似)。`bash.rs:295-305` 与 `core/src/tools/mod.rs:1371`、
   `1627-1631`:`unless_stopped(&ctx.stop, …)` 只响应 stop,`approver.confirm` 不带取消;
   审批通过后 `run_foreground(.., None, ..)` 不先查 cancel,先 spawn 未沙箱进程再杀。

## 开工先问用户

- 第 1 条两种修法选一：(a) 通配记忆只在**这一次真的被沙箱拒绝之后**才生效(先沙箱内
  跑，被拒再免问升级),不参与"执行前跳过沙箱";(b) 第 8 层对"执行时会跳过沙箱"的调用
  不做自动放行，照常询问。推荐 (a):保留"记住升级"的省事，又不让未审批的命令裸跑;
  代价是被拒的那次要先跑一遍沙箱内尝试(DESIGN 里"跳过注定失败的沙箱尝试"的理由只对
  逐命令记忆保留)。

## 做法

1. 按开工问答改 `sandbox_escalation_remembered` 的调用点：通配只在拒绝之后的升级判断里
   查，逐命令记忆维持现状。
2. 非 opaque 但 remember 为 None 的升级只提供 Once。
3. 对每个 read_only_subpath 在可写根内的祖先(关键是 `<root>/.git`)加 literal
   `file-write-unlink` 拒绝，与 denied_write_paths 同一套生成;用 plan 239 的实测脚本复核
   seatbelt 对改名的语义。
4. `reliable == false` 的脚本不进第 8 层自动放行(改为询问);或按 `;`、`&&`、`||`、换行
   粗分，对每段字面前缀跑 deny 与危险判定。开工时选其一并在本文件记下理由。
5. 两份祖先规范化抽成一个逐分量解析函数，断链与 `..` 的处理以沙箱侧为准;结果为 None
   时不生成也不匹配 allow。
6. 升级审批同时 select `ctx.cancel`;`run_process_foreground` 在 spawn 前检查
   `cancel.is_cancelled()`。

## 验收

- 单测与集成(旧代码先红):记过通配后，一条从未审批的 opaque 脚本先进沙箱;被拒后才免问;
  remember 为 None 的非 opaque 形状不提供 `a`;macOS 真沙箱下改名 `.git` 写 hooks 被拒
  (照 plan 239 的写法放进外层执行沙箱之外跑的测试);含语法错误的多行脚本不被自动放行;
  `..` 穿过符号链接与断链两种目标不被判成 cwd 内;取消后不 spawn。
- DESIGN 改写约 3285 段的升级论据与沙箱只读子路径一节;外层执行沙箱之外 `make check`;
  HANDOFF 记进展与教训。
