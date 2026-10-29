# Plan 271 — `.kloop` 的读取按第一层条目判

> 2026-10-29 立并完成。来源：kloop 会话审查 skyrouter 时，
> `cd <repo> && grep -rn "EndpointGeminiNative" --include='*.go' . | grep -v _test.go`
> 被“reading this sensitive path is blocked”硬拒。
> 状态：✅ 已完成（2026-10-29）。首版按位置判（只认 `~/.kloop`，`f02a6b4`），同日审查发现
> 落点能被拼写改写，改为按 `.kloop` 下第一层条目判，见末节「审查后改」。「决定」「做法」
> 「验收」三节记的是首版，已被末节取代。

## 问题

1. **误拦的直接原因**。那个仓库根下有 kloop 自己建的 `.kloop/worktrees/`（空目录）。
   递归搜索检查（`recursive_root_is_sensitive`）看根的直接子项，`.kloop` 在按名字匹配的
   凭据名单里，于是硬拒。同一会话里给了具体子目录的 `grep -rn` 都放行，只有以 `.` 为根的
   这一条被拦。
2. **为什么 plan 243 之后才冒出来**。`.kloop` 早就在递归名单里，但之前 grep 只认原样的
   `-r`/`-R`/`--recursive`，`-rn` 不算递归。plan 243 修好选项簇之后，潜伏的误报被放大：
   用过一次 worktree 的仓库，根上的 `grep -rn X .` 永远被拒。
3. **按名字判 `.kloop` 本身就过宽**。真正要保护的是私有状态根 `~/.kloop`（`config.toml` 的
   provider 凭据、`mcp-oauth.json`、所有项目的 `sessions/`、`permissions.json`），位置固定
   （进程 home 下，CLI 的 `private_state_root`），macOS 沙箱也按这个绝对路径拒读。项目里的
   `.kloop/` 只有 `worktrees/`（checkout）与用户提交的 `rules/`、`skills/`、`commands/`；
   `--mock` 的 `sessions/`、`offload/` 是演示数据且 mock 不过权限门。按名字硬拦让 agent
   读不了项目自己的 skill 与 rules。

## 决定（2026-10-29，用户同意）

- 读取侧：`.kloop` 不再按名字匹配，只认私有状态根（原拼写与规范路径都比，符号链接别名
  落回状态根照拦）。
- 写入侧：保持按名字确认。项目 `.kloop/rules` 一类写进去会引导以后的会话。
- 落点不确定时保守：`cd` 之后的相对拼写、含未知片段的词、PowerShell 原文，`.kloop` 退回
  按名字算。
- 原文扫描：立项时说“继续保守地认 `.kloop` 这个词”，实现时发现原文扫描对**所有** Bash
  命令生效（不只是解析失败的），保留 `/.kloop/` 会让 `cat /repo/.kloop/rules/a.md` 继续被拦，
  与相对拼写放行不一致。改为只认状态根的拼写：`~/.kloop`、`$HOME/.kloop`、`${HOME}/.kloop`
  与状态根的绝对路径。代价：解释器代码里拼法绕开这些的状态根访问
  （`os.environ['HOME'] + '/.kloop'`）不再由门禁原文扫描拦，靠 macOS 沙箱兜底；无沙箱时
  是已知缺口，与 `Path.home() / '.kloop'` 这类原本就扫不到的拼法同级。

## 做法

1. `permissions/sensitive.rs`：`SECRET_DIRS` 去掉 `.kloop`，新增 `KLOOP_DIR` 与
   `glob_component_may_be_kloop`。去掉 `.kloop` 后它与沙箱的 `HOME_CREDENTIAL_DIRS` 相同，
   合并成一份。
2. `permissions/facts.rs`：
   - `KloopScope { Anywhere, PrivateRoot }` 决定 `.kloop` 口径；`Access { Write, Read { cwd_unknown } }`
     由调用点给出，读取时只有“相对拼写且 cwd 未知”才退回 `Anywhere`。
   - `path_sensitivity` 带上 scope，跟踪前缀与私有状态根比较（按文件系统大小写口径）；
     worktree 与溢出文件豁免两种口径共用。
   - `PathFacts` 去掉缓存的 `sensitive`/`sensitivity` 字段，改为 `sensitivity(scope)` 方法：
     写入安全（write_file/edit_file/notebook_edit、Bash 写目标）用 `Anywhere`，
     `read_file`、grep/glob 过滤、Bash 读参数用 `PrivateRoot`。
   - 递归根：根本身按 `PrivateRoot` 判；根把私有状态根包在里面即拦（取代原来只比
     `~/.kloop/config.toml` 的那条）；直接子项名单不再含 `.kloop`。
   - 原文扫描按上面的决定只认状态根拼写；PowerShell 继续按名字。
3. glob：能拼出 `.kloop` 的分量，读取时只在父目录（原拼写或规范路径）是 home 时算
   （`ls ~/.k*` 拦，`ls /repo/.k*` 放行，指向 home 的符号链接下的 `.k*` 也拦）。

## 验收

- 新增 gate 回归 `permissions/plan271_tests.rs`：
  - 仓库根有 `.kloop/worktrees/` 与 `.kloop/rules/` 时，会话里的原命令、`grep -rn X .`、
    `cat`（相对与绝对）、`rg`、`ls .k*`、`read_file`、`read_path_blocked` 在
    manual/plan/bypass 下都放行且不询问——在 `db9cb60` 上先红；✅
  - 项目 `.kloop/rules` 的 `write_file` 与 Bash 重定向仍带 `[sensitive path]` /
    `[sensitive redirect path]` 询问（保护性回归，旧代码同样通过）；✅
  - 私有状态根的绝对路径、`${HOME}` 拼写、包住状态根的递归根、大小写别名（仅折叠大小写的
    文件系统）仍硬拒（保护性回归）。✅
- facts 单测：两种口径的路径判定、符号链接别名落回状态根、原文扫描只认状态根拼写、
  递归根按位置判定、glob `.k*`；sensitive 单测：`.kloop` 不在名单、glob 识别。✅
- 旧用例里拿 `/home/u/.kloop`、项目相对 `.kloop/config.toml` 当状态根的改为真实 home 下的
  路径；`cd ~ && head .kloop/config.toml` 补进硬拒列表（cwd 未知时按名字）。三条 TOCTOU 用例
  （`fs.rs` 两条读绑定、`search.rs` 一条 grep 打开 inode）原先拿临时目录下的
  `.kloop/config.toml` 当秘密目标，改后它不再是读取敏感路径，用例虽然仍过但前提变弱，夹具
  换成 `.ssh/`。✅
- 外层执行沙箱之外 `make check`；DESIGN 改写敏感路径一节、递归搜索、文件写入、grep/glob
  过滤与 `--mock` 存储的说法。✅

## 完成记录（2026-10-29，提交 `f02a6b4`）

验证：新 gate 回归在 `db9cb60` 的临时 worktree 上先红（可读性用例失败，
两条保护性用例照过）；外层执行沙箱之外 `make check` 全绿（fmt、clippy、workspace 2124 项
通过、本机 parity）。Linux 与 Windows 未实测。已知未改：递归根与相对读参数在 `cd` 之后仍按会话 cwd 解析
（`cd ~ && grep -rn token .` 查的是会话 cwd 而不是 home），属于 plan 243 之前就有的定位缺口；
本次只在 `.kloop` 上对“cwd 未知的相对拼写”退回按名字，没有改递归根的定位。

## 审查后改：按第一层条目判（同日）

审查指出首版在读取侧留下同类绕过：位置判要求拼写能词法解析成 `<home>/.kloop`，原文
扫描又撤掉了 `/.kloop/` 这条按名字的退路，两边都认不出的拼写就能读出凭据（Linux、
Windows 与关了沙箱的 macOS 上权限门是唯一防线）：

1. **Windows Git Bash 的 `/c/Users/<u>/.kloop/config.toml`**：有根无盘符，`is_absolute()`
   为假，`lexical_normalize` 按 Windows `join` 拼到 cwd 的盘符下，前缀永远不等于状态根。
2. **花括号挡在 `.kloop` 前面**：`cat ~/{sub,}/.kloop/config.toml`，`{` 让词变成 Pattern，
   前缀 `/Users/u/{sub,}` 与状态根分量数不同；glob 分支只认点开头的分量。

顺着同一根因又复核出五类：`/Users/*/.kloop/config.toml`（glob）；经符号链接的 `..`（词法
归一化先消掉 `..`）；macOS firmlink `/System/Volumes/Data/Users/<u>/.kloop`（本机实测
realpath 不映射回 `/Users`，inode 相同）；未建模命令自带换目录（`tar -C ~ -cf - .kloop`）；
经文本传给 `xargs`（`echo /home/*/.kloop/config.toml | xargs cat`）。再往外还有 `bwrap`、
`proot` 一类连绝对路径都能换落点的工具。证明“不是状态根”的规则补不完。

**决定（用户同意，目标是尽量少打扰）**：按名字判，但细到 `.kloop` 下第一层条目。

- 读取放行 `worktrees/`、`rules/`、`skills/`、`commands/`；其余（`config.toml` 及备份、
  `mcp-oauth.json`、`projects/`、`scheduler/`、以后新加的状态文件）硬拦。列可读条目而不是
  受保护条目，新增状态文件默认受保护。`AGENTS.md` 不进名单：它只在 `~/.kloop` 下，每次
  会话已整篇注入上下文，再读拿不到新东西。
- `.kloop` 目录本身只拦内容访问（与 `.git` 同类，`Sensitivity::Vcs` 改名
  `ContentOnly`）；第一层写成 glob 按受保护处理；可读条目下嵌套的 `.kloop` 再判；`..`
  取消豁免。
- 写入只豁免 `worktrees/`，其余照旧确认。
- 递归根的直接子项是 `.kloop` 时看它实际的条目，只有可读条目就放行；根包住 `~/.kloop`
  的检查保留（子项只看一层）。
- 原文扫描恢复按名字认 `~/.kloop` 与 `/.kloop/`，逐 token 中和 `/.kloop/<可读条目>`（取代
  原来整条命令判 `..` 的 worktree 中和）；PowerShell 同一口径。

**做法**：撤掉首版的 `KloopScope`、带 `cwd_unknown` 的 `Access`、状态根比较与收窄的原文
needle；`Access { Read, Write }` 只决定 `.kloop` 条目的口径，由 `kloop_entry_sensitivity`
一处给出，`path_sensitivity` 与 glob 分支共用。首版改过的旧用例（`/home/u/.kloop`、
`.kloop/config.toml` 作秘密夹具、三条 TOCTOU 用例）恢复原样——按名字判之后它们的前提
重新成立。沙箱凭据根与 `SECRET_DIRS` 的合并保留。

**验收**：

- gate 回归改写：可读条目在任意拼写下放行（含 `cd` 之后、绝对路径、`.k*/rules`、
  `ls .kloop`），在 `f02a6b4` 上先红；上面七类绕过、`cd ~ && cat .kloop/config.toml`、
  `rg KEY <repo>/.kloop`、根下 `.kloop` 含 `config.toml` 时的 `grep -rn api_key .`、包住 home 的递归根
  全部硬拒，同样先红；写入询问照旧。✅
- facts 单测：第一层条目的读写口径、目录本身、glob、嵌套与 `..`、大小写；原文扫描只中和
  可读条目；递归根看 `.kloop` 实际条目。sensitive 单测：可读名单。✅
- 外层执行沙箱之外 `make check`；DESIGN 改写 `.kloop` 一段、递归搜索、文件写入、grep/glob
  过滤与 `--mock` 存储。✅

已知未改：递归根在 `cd` 之后仍按会话 cwd 解析（plan 243 之前就有）。按第一层条目判后，
`cd ~ && grep -rn token .` 在会话 cwd 只有可读 `.kloop` 时会放行——与会话 cwd 没有 `.kloop`
时同一个缺口，修它需要静态跟踪字面 `cd` 目标。
