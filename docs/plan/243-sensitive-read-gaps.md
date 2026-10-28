# Plan 243 — 敏感路径判定补漏：递归搜索、glob 拼写、`~` 写目标

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：待实现。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
> 先读代码确认，不成立就在本文件划掉并写明原因。

## 问题

DESIGN「Bash 的判定按访问方式 × 路径类别」一节(约 1000–1040 行)说 `.kloop`、`.ssh`、
`.gnupg`、`.aws`、`.env*` 与 rc 文件是凭据，内容访问硬拦。下面几种拼写让这条边界失效，
而命令本身被判为只读，在第 9 层直接放行。

1. **递归搜索漏判**(已复核)。`core/src/permissions/facts.rs:574`
   `recursive_search_covers_sensitive_path`:
   - grep 只认原样的 `-r`/`-R`/`--recursive`,`-rn`、`-Rl` 这类短选项簇不算递归;
     `-d recurse`、`--directories=recurse` 也不认。
   - rg 只认 `--hidden` 和含 `u` 的短选项簇,`-.`、`--unrestricted` 不认。
   - 递归根下的敏感名单是 `[".kloop", ".ssh", ".gnupg", ".aws"]`,**漏了 `.env*`**,
     且只看根的直接子项。
   - 后果：项目里有 `.env` 时 `grep -rn API_KEY .` 免审批读出内容;Linux 或关沙箱时
     `grep -rn x ~` 能读出 `~/.aws/credentials`。
2. **glob 拼写绕过三层检查**(待复核)。`facts.rs:408-417`、`742-806`:`cat .en?`、
   `cat .gi?/config`、`cat ~/.s*/id_rsa` 判为 Clear。严格分析的 `plain_word` 原样返回
   glob 词;`PathFacts::gather` 把 `.en?` 当普通文件名;needle 列表匹配不到;局部事实把
   glob 词标为 Unknown 后整个跳过。项目 `.env`/`.git` 不在 macOS 沙箱拒读范围，默认配置
   也能读出。
3. **`~`/`$HOME` 拼写的输出目标不算敏感重定向**(待复核)。`facts.rs:174-191`,目标判定在
   `core/src/shell.rs:312-318`:`echo … >> ~/.bashrc`、`printf … > $HOME/.gitconfig` 的
   目标被判为 Unknown,不进 risk_writes,没有 `[sensitive redirect path]` hazard,
   `bash_write(**/.bashrc)` 这类 deny 也匹配不到。`facts.rs` 自己的 `expand_shell_path`
   在写入侧没用上。
4. **敏感名单写了五份**(待复核位置):`path_sensitivity`、`mentions_sensitive_needle`、
   `powershell_mentions_sensitive_path`、`recursive_search_covers_sensitive_path`、
   `sandbox::with_credential_roots`。第 1 条漏 `.env` 就是这样来的。

## 开工先问用户

- 第 2 条 glob 词怎么判：(a) 在 cwd 下实际展开，展开结果命中敏感名就拒绝、不命中按
  原判定;(b) 只要有一个以 `.` 开头且含未引用 `*?[` 的路径分量，就保守判为 Unresolved
  走询问。推荐 (b):不访问文件系统，与 bash 的实际展开时机无关，误报只落在少见形状上。
- `facts.rs:245` 用 `starts_with("/dev")` 判设备写入，`> /dev/stderr`、`> /dev/tty`
  每次都询问(沙箱下也问)。要不要把 `/dev/null`、`/dev/stdout`、`/dev/stderr`、
  `/dev/tty` 放进白名单?推荐放，噪音大且这四个没有持久副作用。

## 做法

1. 敏感名单收成一份(建议放 `permissions/facts.rs` 或单独 `permissions/sensitive.rs`),
   提供"名字是否敏感"与"路径是否落在敏感目录下"两个查询;沙箱的凭据根从同一份派生。
   五处调用方改用它。
2. 递归搜索：grep 的短选项改用已有的选项簇解析(`walk_options` 一类),补
   `-d recurse`/`--directories=recurse`;rg 补 `-.`、`--unrestricted`(及 `-uu` 等已有形状);
   递归根下改用第 1 步的名单(含 `.env*`),并检查根本身及其祖先是否已落在敏感目录里。
3. glob 词：按开工问答实现。注意局部事实与严格分析两条路径都要处理，不能只补一处。
4. 写目标：Unknown 目标若以 `~/`、`$HOME/`、`${HOME}/` 开头，展开后参与敏感、设备与
   deny 判断;其余 Unknown 目标至少按文件名套一遍敏感名单。

## 验收

- 单测(全部先在旧代码上红):
  - cwd 含 `.env` 时 `grep -rn KEY .`、`grep -Rl KEY`、`grep -d recurse KEY .` 硬拒;
  - `rg -. KEY`、`rg --unrestricted KEY`、`rg -uu KEY` 在含 `.env`/`.ssh` 的根下硬拒;
  - `cat .en?`、`cat ~/.s*/id_rsa`、`cat .gi?/config` 按开工定的方式询问或拒绝;
  - `echo x >> ~/.bashrc`、`printf x > $HOME/.gitconfig` 带敏感重定向 hazard,
    且被 `bash_write(**/.bashrc)` deny 命中;
  - 一条"名单一致"测试：五个调用方对同一组名字给出相同答案。
- 回归：plan 239 的字面输入用例与 argv_roles 现有用例全部保持。
- 外层执行沙箱之外 `make check`;DESIGN 改写敏感路径一节(先读那段现在还成不成立);
  HANDOFF 记进展与教训。
