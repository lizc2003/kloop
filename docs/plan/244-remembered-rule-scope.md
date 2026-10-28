# Plan 244 — "总是允许"的作用域收窄：两词前缀、wrapper、文件工具规则

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：待实现。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
> 先读代码确认，不成立就在本文件划掉并写明原因。

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
