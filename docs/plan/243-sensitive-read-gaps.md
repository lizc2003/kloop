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

## 审查后补（2026-10-29，同日六处）

首版有四条入口仍可绕过、三条新误拦，逐条修掉并补回归：

1. **P1 解析失败丢递归证据**：`rg --no-config -. KEY .` 的 `--no-config` 不在角色表，
   `walk_options` 返回 `None`，旧回退只认 `--hidden` 与簇里的 `u`。回退改为按同一组
   拼写复核：grep 的簇 `r`/`R`、`--recursive`/`--dereference-recursive`、
   `-d|--directories recurse`；rg 的簇 `u`/`.`、`--hidden`/`--unrestricted`。
2. **P1 读重定向的 Unknown 目标**：`cat < .en?` 的目标是 Unknown，读取侧只认 `Literal`。
   读写两侧现在共用同一套复核（`raw_target` 去掉引号后过 `path_argument_is_sensitive`）。
3. **P1 home 后缀的引号**：`echo x > ~/".bashrc"` 实际写 `~/.bashrc`，原实现把引号当
   文件名。`expand_home_spelling` 现在按 shell 的引号删除与反斜杠转义还原后缀。
4. **P2 显式文件根**：`grep -rn KEY notes.txt` 的根过滤成空后退回 cwd，含 `.env` 的 cwd
   会误拦。只有“没有路径参数”才在 cwd 下递归；**解析失败**时角色不明，仍按可能读
   cwd 保守处理。
5. **P2 动态纯文本参数**：`echo "$PWD/.env"` 的参数走到 Unknown 分支，绕过了
   echo/printf 的参数角色豁免；豁免补到 argv 不完整的路径。
6. **P2 glob 交集**：`.??`（长度不足）与 `.e[ab]*`（字符类排除 `n`）没有与 `.env*` 的
   交集却被拦。`could_match_prefix` 改为在模式自动机上吃前缀，前缀吃完才认定有交集；
   花括号/转义等未建模语法仍按可能命中处理。

验证：四条新 gate 回归在旧代码上先红、修后转绿；`explicit_file_roots_do_not_fall_back_to_cwd`、
`text_arguments_stay_text_when_dynamic` 与 sensitive 的 `.??`/`.e[ab]*` 断言覆盖 P2。
外层执行沙箱之外 `make check` 全绿（fmt、clippy、workspace 2096 通过、本机 parity）。
提交 `f980eac`。

## 审查后补（第二轮，同日）

1. **P1 模式文件被当成搜索根**：`rg -uu -f patterns.txt` 的 `-f` 操作数会进
   `path_arguments`（它确实会被打开），但它不是搜索根；位置参数才是。`Walked` 新增
   `positional_paths`，`argv_roles::search_roots` 只返回位置参数，递归根改用它。
   `rg -uu -f patterns.txt` 于是回到“没有路径参数 → 搜 cwd”。
2. **P1 否定字符类被提前截断**：`[!]]` 是“排除 `]`”，否定符后的第一个 `]` 是成员。
   tokenizer 改为记住是否已收成员，只有成员后的 `]` 才结束字符类；`.e[!]]v` 重新拦。
3. **P1 引号删除改变真实路径**：整条目标包在双引号里时，里面的 `'` 是文件名的一部分
   （`"$HOME/O'Brien.txt"`）。`shell_unquote` 改为带引用状态的解码，返回是否出现过
   单引号段；`~`/`$HOME` 是否展开另按原文首字符与单引号段判定。
4. **P2 未建模语法丢前缀证据**：花括号交替不建模，但字面前缀仍能排除
   （`.c{ache,onfig}` 拼不出敏感名）。回退改为 `literal_prefix_may_match`，
   `.env{a,b}` 这类仍有交集的保守拦。

验证：三条 gate 回归先在 `f980eac` 上红、修后转绿；单测补 `pattern_files_are_not_search_roots`、
`home_spelling_keeps_quoted_apostrophes` 与否定类/花括号的 glob 断言。外层执行沙箱之外
`make check` 全绿（fmt、clippy、workspace 2101 通过、本机 parity）。提交 `8820fb3`。

## 审查后补（第三轮，同日）

- **P1 前缀与后缀的引用状态被绑在一起**：`$HOME/'.bashrc'` 的 `$HOME` 未引用、后缀在
  单引号里，实际写入 `~/.bashrc`；`saw_single_quote` 是整条拼写的标记，一见真就放弃
  展开，敏感 hazard 与 `bash_write(**/.bashrc)` 双双漏掉。
- 修法：新增 `split_home_prefix`，按引用状态从开头消费 `~/`、`$HOME/`、`${HOME}/`
  （`~` 只在未引用时展开；`$HOME` 在双引号里仍展开；`"~/…"` 不展开），返回剩余拼写与
  它开头的引用状态；后缀用 `shell_unquote(suffix, quote)` 独立解码，再去掉前导 `/`
  拼到 home 上。
- 顺带把动态词的名字复核改用 `decoded_spelling`（能按 shell 规则还原就还原）：此前
  `cat "$PWD/.env"` 的原文带着引号，最后一段是 `.env"`，同样漏判。引用不能让 glob
  变成字面量（`cat ".en?"` 仍拦），这条保守方向写进 DESIGN。

验证：gate 回归 `plan243_home_prefix_quoting_is_independent_of_the_suffix` 先在 `8820fb3`
上红（`$HOME/'.bashrc'` 无 notice）、修后转绿且 deny 命中；单测补
`home_prefix_quoting_is_independent_of_the_suffix` 与 `dynamic_words_are_checked_after_unquoting`。
外层执行沙箱之外 `make check` 全绿（fmt、clippy、workspace 2104 通过、本机 parity）。
