# Plan 243 — 敏感路径判定补漏：递归搜索、glob 拼写、`~` 写目标

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：✅ 已完成（2026-10-29）。行号是 2026-10-28 的读数。

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
2. **glob 拼写绕过三层检查**(已复核)。`facts.rs:408-417`、`742-806`:`cat .en?`、
   `cat .gi?/config`、`cat ~/.s*/id_rsa` 判为 Clear。严格分析的 `plain_word` 原样返回
   glob 词;`PathFacts::gather` 把 `.en?` 当普通文件名;needle 列表匹配不到;局部事实把
   glob 词标为 Unknown 后整个跳过。项目 `.env`/`.git` 不在 macOS 沙箱拒读范围，默认配置
   也能读出。
3. **`~`/`$HOME` 拼写的输出目标不算敏感重定向**(已复核)。`facts.rs:174-191`,目标判定在
   `core/src/shell.rs:312-318`:`echo … >> ~/.bashrc`、`printf … > $HOME/.gitconfig` 的
   目标被判为 Unknown,不进 risk_writes,没有 `[sensitive redirect path]` hazard,
   `bash_write(**/.bashrc)` 这类 deny 也匹配不到。`facts.rs` 自己的 `expand_shell_path`
   在写入侧没用上。
4. **敏感名单写了五份**(已复核：实为六处，`sandbox::protects_credential_path` 也各写了一遍):
   `path_sensitivity`、`mentions_sensitive_needle`、
   `powershell_mentions_sensitive_path`、`recursive_search_covers_sensitive_path`、
   `sandbox::with_credential_roots` 与 `sandbox::protects_credential_path`。
   第 1 条漏 `.env` 就是这样来的。

## 开工先问用户

- 第 2 条 glob 词怎么判：**已定**（2026-10-29，用户要求「尽量不打扰」）：两种都不选。
  不实际展开目录，也不对“所有点开头 glob”保守询问；改为只看这个分量**能否拼出**名单里
  的名字（字面前缀与名单求交，纯拼写判定，不访问文件系统）。`.en?`/`.gi?`/`.s*` 按敏感
  硬拦，`.c*` 与 `*.env` 照旧放行。判定落在已有的硬拒通道上，不新增审批询问。
- `facts.rs:245` 用 `starts_with("/dev")` 判设备写入：**已定**——把 `/dev/null`、
  `/dev/stdout`、`/dev/stderr`、`/dev/tty` 放进白名单（纯流设备，无持久副作用），
  真正的设备目标（`> /dev/sda`、`> /dev/disk0`）仍逐次确认。

## 做法

1. 敏感名单收成一份(`permissions/sensitive.rs`)，提供 `dir_name_is_secret`/
   `file_name_is_secret`/`name_is_secret` 与 `glob_component_sensitivity` 四个谓词;
   沙箱的凭据根从同一份派生。**复核到六处**名单拷贝(多出 `protects_credential_path`),
   全部改用它。
2. 递归搜索：`argv_roles::search_is_recursive` 走 `walk_options` 的选项簇与操作数,
   覆盖 `-rn`/`-Rl`、`-d recurse`、`--directories=recurse`、`-.`、`-uu`、
   `--unrestricted`、`--hidden`;搜索根改取参数角色表;根下用第 1 步的名单(含 `.env*`),
   并检查根本身及其祖先是否已落在敏感目录里。`.git` 不进递归名单(仓库内 `grep -r`
   的日常操作)，内容访问仍由路径参数判定兜底。
3. glob 词：按开工问答实现(拼写求交)。严格分析路径经 `WordFact.raw`、局部事实经
   同一入口都覆盖；`path_argument_is_sensitive` 是唯一落点。
4. 写目标：`RedirectFact.raw_target` 保留源文本;`~/`、`$HOME/`、`${HOME}/` 展开后进
   `risk_writes`(参与敏感、设备与 deny 判断);其余 Unknown 目标按最后一段名字过名单。
   顺带把纯流设备从 hazard 里去掉。

## 验收

- 单测(全部先在旧代码上红):
  - cwd 含 `.env` 时 `grep -rn KEY .`、`grep -Rl KEY`、`grep -d recurse KEY .` 硬拒; ✅
  - `rg -. KEY`、`rg --unrestricted KEY`、`rg -uu KEY` 在含 `.env`/`.ssh` 的根下硬拒; ✅
  - `cat .en?`、`cat ~/.s*/id_rsa`、`cat .gi?/config` 按开工定的方式询问或拒绝; ✅
  - `echo x >> ~/.bashrc`、`printf x > $HOME/.gitconfig` 带敏感重定向 hazard,
    且被 `bash_write(**/.bashrc)` deny 命中; ✅
  - 一条"名单一致"测试：五个调用方对同一组名字给出相同答案。 ✅
    (`sensitive_names_agree_across_every_caller`；`.env*` 走路径/递归/glob 三处一致)
- 回归：plan 239 的字面输入用例与 argv_roles 现有用例全部保持。 ✅
- 外层执行沙箱之外 `make check`;DESIGN 改写敏感路径一节(先读那段现在还成不成立);
  HANDOFF 记进展与教训。 ✅

## 完成记录（2026-10-29，提交 `a624892`）

- 新增 `permissions/sensitive.rs`(名单、大小写折叠、glob 拼写查询)与
  `permissions/plan243_tests.rs`(四条 gate 回归，先在旧代码上确认全红)。
- `shell/facts.rs` 的 `WordFact` 增加 `raw`、`RedirectFact` 增加 `raw_target`:
  解析层照实说“这是动态的”，权限层不再因此丢掉写目标。
- behavior 变化：递归搜索根的直接子项命中凭据名(含 `.env*`)即硬拦；点开头、能拼出
  敏感名的 glob 分量硬拦；`~`/`$HOME` 写目标进 deny 匹配;四个流设备不再按敏感确认。
- 保留的边界：`.env` 不进原文子串扫描(避免 `echo "add .env to .gitignore"` 被当成
  敏感读取);递归根只查直接子项与根/祖先，不做整树遍历;`.git` 不在递归名单里。
- 验证：`make check` 全绿(fmt、clippy、workspace 2090 通过、本机 parity)。Linux 与
  Windows 未实测。
