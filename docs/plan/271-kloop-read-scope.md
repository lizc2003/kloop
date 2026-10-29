# Plan 271 — `.kloop` 的读取只认私有状态根

> 2026-10-29 立并完成。来源：kloop 会话审查 skyrouter 时，
> `cd <repo> && grep -rn "EndpointGeminiNative" --include='*.go' . | grep -v _test.go`
> 被“reading this sensitive path is blocked”硬拒。
> 状态：✅ 已完成（2026-10-29）。

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
