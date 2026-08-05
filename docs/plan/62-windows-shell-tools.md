# Plan 62 — Windows 原生 Shell：Git Bash、PowerShell 与 Job Object

> 状态：✅ 已完成（2026-08-05；初始提交 `ef61d9c`，审查纠偏提交以本条所在提交为准）
>
> 依赖：Plan 50、Plan 51、Plan 61；与未开工 Plan 63 修改面重叠，收尾前不得并行
>
> 交界：Plan 51 已完成显式后台 lifecycle/通知/回灌与 session cleanup；自动后台化、stall 和逐事件 Monitor 仍为产品边界
>
> 调研/实现基线：kloop `4ab04b5`；Claude Code 固定源码 `<redacted>` 只作 Windows 架构参考，精确 2.1.220 target 是 darwin-arm64，不能充当 Windows 运行证据。

## 背景

Claude Code 在原生 Windows 上不把 Bash 偷换成 PowerShell：Bash 使用 Git for Windows，PowerShell 是独立工具；从 PowerShell 启动程序只表示它是终端宿主。WSL 又是独立的 Linux 运行环境。

Plan 62 开工前 kloop 只有 `bash`，执行器固定为 `sh -lc`；Unix 前后台依赖独立 process group，non-Unix 的 group 操作不能兑现 Plan 50 的无遗留进程树保证。当前实现已把前后台 shell 迁入共享 process-tree façade：Unix 保持 process group，Windows 在 user code 执行前绑定专属 Job Object。本段保留为问题基线，不能拿实现后的 cross-compile 反推 Windows 生命周期已验证。

本计划只闭环 **Windows 原生 shell execution**：Git Bash、独立 PowerShell、Job Object 进程树所有权、权限和原生 CI。Windows 文件 mutation/reparse-point/handle-relative safety 仍由 Plan 61 负责；Windows filesystem/network sandbox 仍未实现；Plan 51 已完成的显式后台 lifecycle 状态机在本计划只复用、不重新设计。

## 固定目标与边界

- Windows 原生：`bash` 只代表经过验证并冻结路径的 Git Bash `bash.exe -lc`；绝不 fallback 到 PowerShell、`cmd.exe`、WSL、Cygwin、BusyBox 或 PATH 中任意 `sh.exe`。
- Windows 新增独立、Windows-only、**foreground-only** 的 `powershell` 工具；有 Git Bash 时两个工具同时注册，没有 Git Bash 时只注册 PowerShell 并给一次启动 warning。
- WSL 按 Linux 环境运行 `/bin/bash`，不进入 Windows shell discovery 或 Job Object 分支。
- Windows Bash 的既有显式后台语义（`run_in_background`、`bash_output`、`kill_bash`）保留；PowerShell v1 不支持后台。
- Bash 与 PowerShell 共享 process-tree、输出、timeout/cancel 和 credential env scrub；两者绝不共享 AST 或 shell 语义分类。
- Job Object 只负责 process-tree containment，不是 filesystem/network sandbox；Windows restricted token/AppContainer 继续明确未实现。
- 不修改 pinned Claude Code 2.1.220 的 PowerShell matrix 结论；没有 Windows target identity、fixture 和 locator 就不得标 `same`/`compatible`。

## 证据与开工闸门

- 固定参考证据：Claude Code 固定源码的 Windows Git Bash discovery、独立 `PowerShellTool`/provider、WSL 分流与 tree-kill 调用链。它们只解释设计收敛，不替代精确 Windows binary fixture。
- 开工前复核 Plan 61 的 Windows filesystem focused tests 与 workspace clippy 持续全绿；Windows 全 workspace baseline 当前为 420 pass / 77 fail，其中 shell/hook 与依赖 Bash 的 parity failures 由本计划关闭，search/permission path 展示和 Git worktree verbatim-path 缺口须在开工时修复或显式拆出，最终恢复 Windows workspace test 门。
- 改行为前建立 Windows 原生 baseline：Git for Windows 路径、`pwsh.exe`/`powershell.exe` 可用性、nested Job 环境、现有 Unix-only test helper 与 Python 命令差异。
- 所有 Windows 生命周期结论必须来自 Windows 原生运行；cross-compile 只能证明可编译，不能证明 Job、handle、pipe 或无 orphan。

## 1. 抽出共享进程树执行层

关键文件：

- 新增 `kloop/crates/core/src/process_tree/{mod.rs,unix.rs,windows.rs}`
- 新增 `kloop/crates/process-spawn`，给所有生产 child creation 共用进程级 gate
- 重构 `kloop/crates/core/src/tools/bash.rs`
- 调整 `kloop/crates/core/src/tools/mod.rs`
- target-specific 依赖：`kloop/Cargo.toml`、`kloop/crates/core/Cargo.toml`

固定设计：

- 引入内部 `ProcessSpec`：受信 executable、argv、cwd、env add/remove、stdin、stdout/stderr pipe 或文件句柄。shell flavor 只负责构造 spec；process layer 不解释命令语言。
- 引入 `ProcessTreeChild` 与 `ProcessTreeKiller`：支持 wait、幂等 terminate-tree、等待树归零和 Drop 兜底。Unix 后端复用现有 `process_group(0)`、rustix group kill/test；Plan 50 输出和生命周期不变。
- Windows 后端使用 target-specific `windows-sys`，集中所有 unsafe 与 RAII handle 逻辑，不把 raw `HANDLE` 泄漏到工具层。
- Windows 严格执行：创建 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` Job → `CreateProcessW(CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT)` → 在任何用户代码执行前 `AssignProcessToJobObject` → 成功后 `ResumeThread`。attach/resume 任一步失败都 terminate + wait + close handles，绝不退化为裸 spawn。
- 使用 `STARTUPINFOEXW` handle list，只继承 stdin/stdout/stderr；环境变量名按 Windows 大小写不敏感规则 scrub provider/search secrets，环境 block 使用 UTF-16 双 NUL。
- Job owner 不依赖“最后一个 handle close”完成正常清理：timeout/cancel/session Drop 都显式 `TerminateJobObject`；kill-on-close 只作异常兜底。monitor/registry 的共享 controller 不得让 owner Drop 变成死代码。
- nested Job 下若 assign 被宿主限制，fail closed 并带 Win32 error；不设置 breakaway 绕开宿主。

### 等待、pipe 与取消不变量

- stdout/stderr 继续同时、有界 drain。Windows 可用受控 blocking waiter/readers，但唯一 Job owner 必须留在可取消的 `ProcessTreeChild`，不能移入不可控的 `spawn_blocking` closure。
- 不再把“direct child wait + 两 pipe EOF”作为可能永久互等的整体：direct child 正常退出后先清 residual group/Job members，再等待 reader EOF；timeout/cancel 先清树、reap root，再收 reader；cleanup drain 有明确短 deadline。
- `disable_sandbox` 只能移除 Seatbelt/未来 sandbox，永远不能关闭 Unix process group 或 Windows Job Object。
- `tools/mod.rs::foreground_bash_started` 泛化为 foreground shell execution barrier：hook/审批阶段取消不 spawn；Bash/PowerShell 一旦 spawn，dispatch 必须等待 executor 完成整树清理后再返回 paired interrupted result。

## 2. 冻结 shell executable 与全局配置

关键文件：

- `kloop/crates/core/src/config.rs`
- `kloop/crates/cli/src/user_config.rs`
- `kloop/crates/cli/src/startup.rs`
- 相关 Config test builders

新增全局配置，不加 CLI flag：

```toml
[shells]
bash = 'C:\Program Files\Git\bin\bash.exe'             # optional
powershell = 'C:\Program Files\PowerShell\7\pwsh.exe'  # optional
```

- CLI 启动时一次解析 `ShellPrograms`，保存 canonical absolute path + flavor。主 agent、server threads、sub-agents、worktree 与 code mode 继承同一 snapshot；thread cwd 变化不重新 discovery。
- 配置路径必须绝对、存在、regular executable，并匹配预期 executable/flavor；字符串不能夹带 argv。
- Git Bash discovery：显式配置优先；否则从可信 `git.exe` 安装布局与 `%ProgramFiles%`/`%LocalAppData%` 标准 Git for Windows 位置推导，并验证 `cmd\git.exe`、`bin\bash.exe` 与 MSYS runtime 结构。不能仅凭文件名接受任意 PATH `bash.exe`。
- PowerShell discovery：显式配置优先；否则选择标准安装位置中最高可用的 PowerShell 7 `pwsh.exe`，再 fallback 到 `%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe`；不 fallback 到其他 shell。最终路径和 flavor 冻结。
- discovery 拆成接受候选 roots/env snapshot 的纯函数，测试不得争用进程全局环境。
- `--mock` 不读 `[shells]`、不扫描 PATH、不运行 `where.exe`，直接注入确定性 `ShellPrograms::test_fixture()`。Windows mock fixture 固定 Git for Windows 与系统 PowerShell 标准路径；CI runner 承诺缺失时由真实 spawn/native gate 硬失败，不另行 discovery 或偷偷换语法。

## 3. Windows Git Bash 接入

关键文件：

- `kloop/crates/core/src/tools/bash.rs`
- `kloop/crates/core/src/tools/mod.rs`
- `kloop/crates/core/src/sandbox/mod.rs`

- Unix/macOS 保持现有 shell/Seatbelt 行为；把 Seatbelt wrapper 从“自己硬编码 `sh -lc`”改为包装已构造的 program + argv，现有 profile 与测试除真实 executable 参数外不漂移。
- Windows 使用冻结的 Git Bash `bash.exe -lc <command>` 和 `current_dir`；不重写用户 command 中的反斜杠或 quoting，工具描述明确 shell 内优先使用正斜杠。
- Windows 找不到 Git Bash 时，不注册 `bash`、`bash_output`、`kill_bash`；幻觉直调返回明确 unavailable error。
- Windows Bash schema 不广告 `disable_sandbox`；直调携该字段时明确报 Windows sandbox unavailable，不静默忽略。
- 前台成功/失败、30k 输出、timeout/cancel 和 no-residual 继续走共享 process layer。
- 显式后台继续使用同一个 `BackgroundShells` registry、output file、1 GiB watchdog 和查询/kill contract，但 entry 保存 Job controller。turn cancel 不杀后台；`kill_bash`、watchdog、registry/session Drop 必须终止完整 Job。
- 本计划复用 Plan 51 已有的显式后台通知、回灌和 session cleanup，不新增前台自动转后台、stall 或逐事件 Monitor。

## 4. 新增独立 PowerShell v1

关键文件：

- 新增 `kloop/crates/core/src/tools/powershell.rs`
- `kloop/crates/core/src/tools/mod.rs`
- `kloop/crates/core/src/skills.rs`
- `kloop/crates/tui/src/toolrow.rs`

Windows-only schema：

```json
{
  "command": "string (required)",
  "timeout_ms": "integer (optional, default 60000)"
}
```

- 无 `run_in_background`、`disable_sandbox`、PTY、stdin、persistent session 或 executable 参数；传入 background/sandbox 字段时明确拒绝。
- 固定 argv：`-NoLogo -NoProfile -NonInteractive -EncodedCommand <UTF-16LE base64>`；不使用临时 `.ps1`、`Invoke-Expression` 或 `-ExecutionPolicy Bypass`。
- encoded payload 设置无 BOM UTF-8 stdout/native output，并在用户 command 后立即快照/传播 PowerShell 与 native command exit status；不得因尾部 `# comment`、multiline 或 Unicode 吞掉 wrapper。用户显式 `exit N` 保持原义。
- 模型/UI/history/hooks 永远显示原始 command，不显示 encoded payload。
- 复用前台 bounded output、timeout/cancel、credential env scrub 与 Job Object；PowerShell v1 不接 `BackgroundShells`。
- hooks、server/headless wire 和 code mode 使用现有 generic tool bus，无新协议版本；`skills::map_tool_name` 增 `PowerShell -> powershell`，TUI 显示 `PowerShell PS> …`。

## 5. PowerShell 权限必须独立且 fail closed

关键文件：

- `kloop/crates/core/src/permissions.rs`
- `kloop/crates/core/src/tools/mod.rs`

将 `CallFacts` 的 Bash-only 事实扩成明确 shell facts，例如 `ShellFacts::Bash(BashAnalysis)` 与 `ShellFacts::PowerShellOpaque`；后者绝不调用 `shell.rs`/tree-sitter-bash。PowerShell v1 不做 cmdlet 黑名单、空格 split、regex pipeline parser 或假的 read-only 分类。

固定真值表：

| 条件 | 结果 |
|---|---|
| plan | 直接拒绝，不询问 |
| manual / accept-edits | 每次询问 |
| bypass | 仍询问，不能自动放行 |
| sandbox auto-allow | 永不适用 |
| concurrency | 恒 false，普通轮与 code mode 都串行 |
| `deny = ["powershell"]` | 最早层硬拒绝 |
| `ask = ["powershell"]` | 每次询问 |
| 用户手工 `allow = ["powershell"]` | 明确整工具信任，可自动放行 |
| `powershell(...)` prefix rule | v1 配置校验拒绝，不伪装参数级规则 |
| 交互 AllowSession/AllowAlways | 本次等价 allow-once，不缓存、不持久化 |
| `--mock`/`allow_all` | 保留现有无门测试特例 |

- `remember_payload(PowerShellOpaque) -> None`；现有 TUI/plain 在 `remember_rules=None` 时只显示 y/n，server approval 也不发送 remember rules。
- approval 描述加 `[unclassified PowerShell]` 并展示原始脚本。
- PowerShell 的 obvious sensitive-path raw matcher覆盖 Windows separator、大小写折叠、`$HOME`/`$env:USERPROFILE` 下 `.kloop/.ssh/.gnupg/.aws/.env*`。这仍不是 PowerShell data-flow 或 Windows sandbox；用户手工 whole-tool allow 是显式承担完整 PowerShell 能力。

## 6. Tool catalog、UI 与平台条件

- `builtin_defs`/`all_tool_defs` 接入 resolved `ShellPrograms`：Windows catalog 按实际 executable availability 注册；非 Windows catalog 字节形状不变。
- 修正 hard-coded tool count/defer tests，使预期显式按平台 catalog 计算。PowerShell 仍是 built-in、不 defer；Windows 多一个工具会按现有总数规则影响 MCP defer threshold。
- agent-type allowlist 可独立允许/隐藏 Bash 与 PowerShell；code mode 自动生成 `tools.powershell({command, timeout_ms?}): Promise<string>` 并重入同一 gate。
- core `Event::ToolCall`、server/headless `item/toolCall` 和 approval wire 保持泛型，不 bump protocol；只补 pass-through regression。

## 7. Windows 原生测试矩阵

### ProcessTree / Job

- suspended root 在 assign 前不能写 marker；强制 assign/resume 失败时 user code 从未执行且 root 被回收。
- timeout、active cancel、future Drop、normal leader exit + residual descendant、repeated terminate 后 Job members 归零。
- child/grandchild 继承 stdout/stderr handle 时不会因等 EOF 卡死。
- nested Job/assign failure fail closed；不允许 breakaway。
- registry Drop 与 monitor 持 controller 时仍显式 kill，不因 `Arc`/handle clone 失效。
- 循环 spawn/wait 的 process/Job/thread/pipe handle 数不线性增长。

### Git Bash foreground/background

- discovery 接受完整 Git for Windows/PortableGit 布局，拒绝 WSL/System32/伪 Bash；覆盖 executable path 带空格。
- cwd（空格、非 ASCII）、stdout/stderr、空输出、Unicode、非零 exit、大双流、secret env scrub。
- timeout/cancel 清 TERM-ignoring child+grandchild；leader 先退与 inherited pipe 均无 survivor。
- background start/query/failure/kill/watchdog/turn cancel/session Drop；output file 运行时可读，结束后 handle 释放。

### PowerShell

- PowerShell 7 与 Windows PowerShell 5.1 两个 flavor：Unicode、multiline、单双引号、here-string、尾 comment、空输出、stdout/stderr、大双流。
- native executable `$LASTEXITCODE`、cmdlet terminating/non-terminating error、用户 `exit 7` 的稳定结果。
- `Read-Host` 在 non-interactive/stdin-null 下不永久挂；profile 不加载。
- timeout/cancel/Drop 杀 `Start-Process` descendant；background/sandbox 字段拒绝；fake PATH executable 不被选。

### Permission/dispatch/ecosystem

- 上述 mode/deny/ask/manual allow/remember/concurrency 真值表逐项测试。
- 两个 PowerShell 不 overlap；PowerShell 不与 read-only Bash 合入 safe batch；code mode `Promise.all` 对它取 exclusive gate。
- pre-hook block 不 spawn，post-hook 原样收到结果；cancel 时所有 tool_use 仍有 paired result。
- TUI label、skill mapping、agent allowlist、code-mode TS、server/headless generic projection。
- macOS/Linux 全部 Plan 50 Bash、Seatbelt、permissions、parity report 回归不变。

## 8. CI、文档与 parity 纪律

关键文件：

- `.github/workflows/ci.yml`
- `kloop/README.md`
- `docs/plan/HANDOFF.md`
- `docs/capability-report.md`
- `refs/README.md`
- `docs/plan/51-background-monitor-parity.md`、`docs/plan/61-file-tool-correctives.md` 只交叉引用边界

- 扩充 Plan 61 已有的 `windows-latest` matrix job，而非首次新增平台：三平台统一 `actions/setup-python`、workspace fmt/clippy/tests、mock 与 corpus-only；Windows 另跑既有 file safety 及 process_tree/Git Bash/PowerShell/permissions focused tests。
- README 增平台表、`[shells]`、Git Bash requirement、PowerShell opaque 权限、Job containment 与 Windows sandbox 未实现说明。
- capability report 只销账 Windows shell process-tree/PowerShell tool；Windows filesystem/network sandbox、Plan 61 文件安全之外的 hooks/MCP child-tree 等继续列缺口。
- HANDOFF 记录 shell identity snapshot、Job owner/explicit terminate、pipe EOF 顺序和 PowerShell permission 不变量。
- hooks/MCP 等其他进程若仍只 kill direct child，必须如实列为后续迁移项，不借 shell primitive 宣称全进程治理已完成。
- `refs/claude-code-2.1.220/{build_matrix.py,tool-matrix.json}` 的 PowerShell Windows 维度保持 `n/a/unknown`；只有取得目标 Windows CC binary identity、raw/normalized fixture 与 exact locator 后才另行扩 parity corpus。

## 实施切片

### 切片 0：Windows baseline 与依赖闸门

1. 复核 Plan 61 完成状态和 Windows workspace baseline。
2. 固定 Git Bash、PowerShell、nested Job 与现有 test blockers。
3. 不改 PowerShell parity matrix。

### 切片 1：只抽进程树，不改 Unix 产品语义

1. 新增 `process_tree` façade 与 Unix backend。
2. 把 Bash 前台、显式后台和 Seatbelt wrapper 迁入新接口。
3. Plan 50 parity report、Bash tests 和 macOS Seatbelt tests 保持全绿。

### 切片 2：Windows Job Object 与 Git Bash

1. 落 direct `CreateProcessW`/RAII handles/suspended assign/resume。
2. 接 Windows foreground、background、kill/watchdog/session Drop。
3. 接 Git for Windows discovery、Config snapshot 和条件注册。
4. Windows Job 与 Git Bash 原生测试全绿。

### 切片 3：独立 foreground PowerShell

1. 接 executable discovery 与 EncodedCommand builder。
2. 复用共享前台 executor，不接后台 registry。
3. 接 `PowerShellOpaque` 权限、dispatch cancel barrier、串行分类。
4. 接 hooks/code mode/skills/TUI 与泛型 wire 回归。

### 切片 4：Windows CI、文档与验收

1. 扩充既有 `windows-latest` job，并恢复 Windows 全 workspace 门。
2. 同步 README、HANDOFF、capability report、refs/README 和 Plan 51/61 边界。
3. 保持 CC PowerShell matrix 未证维度不变。
4. 全量门绿后回填本 plan 完成记录，一次提交。

## 非目标

- PowerShell background、`powershell_output`、`kill_powershell` 或 PowerShell 自动后台化。
- 已存在的 Plan 51 lifecycle/通知/回灌不在本计划重做；auto-background/stall/逐事件 Monitor 不实现。
- Windows restricted token/AppContainer、filesystem/network/registry sandbox。
- PowerShell AST、read-only/dangerous cmdlet classifier、alias 解析、prefix permission 规则。
- cmd.exe、WSL bridge、Cygwin、任意 MSYS、PTY、交互 stdin、persistent shell state。
- PowerShell profile、临时 `.ps1`、ExecutionPolicy 绕过。
- Plan 61 的 reparse-point、handle-relative file mutation、CRLF/有界 I/O 纠偏。
- hooks/MCP/git 辅助子进程的一次性全迁移。
- 无 Windows CC 证据时的 parity 宣称。

## 验证

```bash
# 各平台
cd kloop
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock

# Windows 原生 Plan 61 file-safety focused gates
cargo test -p kloop-core file_io::tests
cargo test -p kloop-core text_edit::tests
cargo test -p kloop-core tools::fs::tests
cargo test -p kloop-core tools::fs::windows::tests
cargo test -p kloop-core diff::tests
cargo test -p kloop-core file_state::tests
cargo test -p kloop-core scheduler::tests

# Windows 原生 Plan 62 focused gates
cargo test -p kloop-process-spawn
cargo test -p kloop-core shell_programs::tests
cargo test -p kloop-core process_tree
cargo test -p kloop-core tools::bash::tests
cargo test -p kloop-core tools::powershell::tests
cargo test -p kloop-core permissions::tests

# corpus 不回退；exact binary 仍只在 pinned darwin host
python -B ../refs/claude-code-2.1.220/verify.py --corpus-only
cd ..
python3 -B refs/claude-code-2.1.220/verify.py
git diff --check
```

## 2026-08-05 实施与原生 Windows 验收记录

- 已实现 `process_tree/{mod,unix,windows}.rs`、前后台 Bash 迁移、Seatbelt program+argv wrapper、启动时 `ShellPrograms` snapshot、Git Bash/PowerShell discovery、条件 catalog/code mode，以及独立 PowerShellOpaque 工具/权限/UI/skills 接线。
- Windows backend 已集中 RAII unsafe：Job/process/thread/attribute/stdin/stdout/stderr handle，absolute application、stdio handle list、Windows ordinal-case UTF-16 environment、suspended assign-before-resume、错误路径 terminate+wait；attribute list 自持 handle-value array，`Child` 复用唯一 blocking waiter，取消/re-poll 不再按 watchdog tick 泄漏。
- 新增无业务依赖的 `kloop-process-spawn` workspace gate：process-tree 从创建 inheritable stdio 到关闭 parent-side inheritable handles 全程持锁，hooks、MCP、worktree Git 与 CLI/TUI 生产 child creation 走同一 gate，避免并发无关进程偷继承 pipe/file handle；gate 自身有 Windows 阻塞并发回归。
- PowerShell discovery 除版本化 MSI roots 外，使用 Windows package API 仅枚举官方 `Microsoft.PowerShell_8wekyb3d8bbwe` / `Microsoft.PowerShell-LTS_8wekyb3d8bbwe` MSIX package roots；候选统一以 `pwsh.exe` 的 `VS_FIXEDFILEINFO` file version 排序，缺 version resource 才退目录/package metadata，避免固定 MSI `PowerShell\7` 被旧 MSIX 错压；不接受任意 PATH `pwsh.exe`。
- Windows process-tree 测试源码新增真实 `AssignProcessToJobObject` active-process-limit failure、raw UTF-16 environment child round-trip、反复取消 waiter handle-count，以及 Windows Bash background session shutdown/registry Drop descendant no-survivor；既有覆盖 assign/resume 前 marker、leader-exit inherited pipes、幂等 terminate、Child Drop 与循环 handle-count。
- 原生 PowerShell 测试源码硬门 PowerShell 7 与 Windows PowerShell 5.1，覆盖 Unicode/multiline/单双引号/here-string/尾 comment、空输出、native/cmdlet/显式 exit、Read-Host 非挂死、大双流与 Start-Process descendant timeout/cancel cleanup；Git Bash 的通用前后台 suite 在 Windows 使用标准 Git for Windows fixture。
- 原生验收机为 Windows 10 build 19045、x64 的本地交互式 Windows workstation；测试宿主本身已位于一个 Job 中，nested Job 路径按 fail-closed 契约原生通过。Rust/Cargo 为 1.92.0 MSVC x64，Python 为 3.14.0。
- Git for Windows 根为 `C:\Program Files\Git`：`cmd\git.exe` 2.52.0.windows.1、`bin\bash.exe` 5.2.37，且 `usr\bin\msys-2.0.dll` 存在。PowerShell 7 为官方 Store/MSIX `Microsoft.PowerShell_7.6.4.0_x64__8wekyb3d8bbwe`，实际 executable 为 `C:\Program Files\WindowsApps\Microsoft.PowerShell_7.6.4.0_x64__8wekyb3d8bbwe\pwsh.exe`，产品版本 7.6.4、file version 7.6.4.500；Windows PowerShell 为 `C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe` 5.1.19041.6456。
- 原生 focused gates 全绿且没有 ignored：Plan 61 七组 file-safety selector 合计 62 tests；`kloop-process-spawn` 1、`shell_programs` 8、`process_tree` 15、Bash 16、PowerShell 9、permissions 42。并发 handle-inheritance gate、raw UTF-16 environment round-trip/ordinal comparison、真实 assign failure、waiter cancellation reuse、普通与 debugged spawn handle-growth 都实际运行。
- 原生全 workspace 全绿：`kloop-core` 538 tests，server 26、CLI 81、codemode 21，其余 crate 与 doc tests 同样通过、0 ignored；workspace clippy `-D warnings`、mock 六轮、corpus-only 与 `git diff --check` 均通过。
- 原生验收发现 PowerShell 7 MSIX 的 `Start-Process` 可产生不留在 root Job 的 descendant。PowerShell spawn 因此启用专用 `DEBUG_PROCESS | CREATE_SUSPENDED` gate：root 仍先 assign 后 resume；每个 descendant 的 create-process debug event 在首线程继续前复核 Job membership，不在 root Job 者先 assign 到第二个 kill-on-close containment Job。open/check/assign/continue 任一步失败都终止 event process 与两组 Job；timeout/cancel/normal completion/Drop 对固定 Job 集显式 terminate，不用 PID 扫描、裸 spawn、无控制 breakaway 或 direct-child fallback。PowerShell 7 与 5.1 的 timeout/cancel `Start-Process` 回归均确认无 survivor。
- 既有 Windows blockers 以最窄修复关闭：Git verbatim path 转普通 Windows path，hooks/agent 使用已验证 Git Bash，search/permission/codemode/task/Plan 50/56/server parity report 只规范化报告或断言中的 Windows separator；waiter cancellation 改为精确比较同一个 Tokio blocking waiter ID，独立 handle-growth 回归仍保留。
- corpus-only 在 Windows 保持 immutable corpus/hash/normalization/tamper、matrix/pair/profile bridge、可跨平台 Rust semantic reports 与敏感信息检查；POSIX descriptor/ctime/symlink/publication/PTY 自检仅在 POSIX 跑，Windows 验证其 case declarations、unsafe path rejection 及 collector/PTY fail-closed。Plan 59 native report 继续只在 Darwin arm64 跑，不伪造 Windows 证据。`.gitattributes` 将 436 个 hash-bound fixture JSON 固定为非 text，避免系统 `core.autocrlf=true` 改写内容；fixture、golden、manifest 和 pinned PowerShell matrix 均未修改。
- 最终修改与本完成记录使用 `git commit --amend --no-edit` 合入当时尚未公开的 Plan 62 提交；该历史步骤产出后来公开的 `ef61d9c`，本次纠偏不再改写它。

## 2026-08-05 `ef61d9c` 审查纠偏

- 对公开提交 `ef61d9c` 的逐项审查确认：debugger 把 initial breakpoint 交给被调试进程、descendant admission 可与 terminate 交错、只给 PowerShell 开 debug gate 使 Git Bash→MSIX PowerShell 可逃出 root Job；因此保留历史并另做一个 follow-up corrective commit，不 amend/rebase/force-push。`ef61d9c` body 中“native Windows execution remains pending”是合并前旧文字，实际原生验收记录仍以上一节为准。
- debugger 现在按 PID 登记 pending initial breakpoint，只消费该进程第一次 first-chance `EXCEPTION_BREAKPOINT`；错误异常、second-chance 与后续 breakpoint 均保持 `DBG_EXCEPTION_NOT_HANDLED`。create-process/load-dll 的 image/DLL file handle 显式关闭；process/thread debug handles 遵守 Win32 契约，由系统在相应 exit event continue 后关闭。
- `JobSet` 用一个 lifecycle mutex 线性化 descendant open/membership/assign 与 termination。terminate 先永久关闭 admission，再终止 descendants/root 两个 Job；只有两者都成功才标记 complete，部分失败可重试但 admission 不重开。所有 Windows Bash 与 PowerShell 都使用此 debug path，late event process 在继续前被终止。
- root reap、Job-empty wait 与 debugger finish 共用一次 absolute deadline；debug thread 只有已结束才 join，timeout 保留 handle 供后续 finish，Drop 只关 admission/terminate、不 sleep/join。Windows regression 源码覆盖 per-process breakpoint、两种 admission/terminate 顺序、bounded finish、官方 MSIX Bash 前台 timeout/cancel 与后台 kill/shutdown no-survivor。
- PowerShell wrapper 在脚本前清 `$LASTEXITCODE`，脚本后立即快照 final `$?`、native code 与新 `$Error`：新 error→1，final PowerShell success→0，final native failure→该 code，其余 failure→1。version probe 改成 `Version`/`MissingResource`/`Invalid`；只有 missing resource 才采信标准 MSI 目录或官方 MSIX metadata，明确 non-v7 与其他 probe 错误拒绝 fallback。Unix/WSL shell discovery 同时要求最终 regular file 具 executable bit。
- `Config` 新增 session-scoped PowerShell exclusive mutex；普通 clone/sub-agent/direct/不同 foreground/background code-mode bridge 共享，不同 server session 独立。锁只在 hook/permission 后、spawn 前获取，等待时 cancellation 不 spawn，executor 后、post-hook 前释放；`run_program`/task/skill/wait 等 orchestration 不持锁。
- public catalog API 删除隐式 `ShellPrograms::native_posix()` wrappers；`all_tool_defs`、`tool_defs`、`defer_active`、`deferred_tool_defs`、`tool_merge_warnings` 均显式接收 frozen snapshot，empty/Bash-only/PowerShell-only/both 的 catalog/defer/warning 共用同一输入。
- 旧 `core.autocrlf=true` Windows worktree 不会因后来加入 `.gitattributes -text` 自动迁移。verifier 只把“实际含 CRLF 且替换为 LF 后 hash 精确命中”分类为 CRLF-only，仍 fail closed，并警告确认 fixture 无需保留后执行 `git restore --source=HEAD --worktree -- refs/claude-code-2.1.220/fixtures`；普通 tamper、bare CR 或 mixed drift 不给 destructive guidance。fixture、manifest、golden、matrix 与 `.gitattributes` 本身不改。
- macOS 本机最终门已通过：`cargo fmt --check`、workspace all-target `clippy -D warnings`、workspace tests、`--mock`、corpus-only 与 pinned darwin full verifier、build-matrix check、`git diff --check`；独立小 crate 对当前 Windows process-tree/discovery 单测做 MSVC target `clippy -D warnings` 也通过。`.gitattributes`、manifest 与 436 个 fixture 文件无 diff。首次并行 workspace run 仅有一个 server test 等待行超时；focused rerun通过，随后无并行负载的完整 workspace rerun全绿。上述新增 native lifecycle/MSIX/session-gate regression 尚待既有 Windows 10 x64 验收机执行，不以 cross-compile 冒充运行证据。

## 完成标准（均已满足）

1. Windows model shell 在 user code 执行前已进入专属 Job；attach/resume 失败 fail closed。
2. timeout/cancel/leader-exit/explicit kill/watchdog/future Drop/session Drop 后无 root 或 descendant，且无线性 handle leak。
3. Windows Bash 只执行已验证 Git for Windows；缺失时不注册且无 fallback。
4. PowerShell 只在 Windows 注册、仅前台、使用固定 encoded invocation，并通过 PowerShell 7/5.1 原生测试。
5. PowerShell 在 plan 阻断、bypass 仍问、恒串行、无自动 remember；仅用户手工 whole-tool allow 可授权。
6. Job containment 不被 sandbox 配置或 `disable_sandbox` 关闭，且文档不冒充 Windows sandbox。
7. macOS/Linux Plan 50 与 Seatbelt 行为、exact/corpus parity gate 无回退。
8. 原生 Windows focused/workspace/mock/corpus 全绿，既有 macOS/Linux 回归与三平台 CI 门保持；README/HANDOFF/capability/refs 与 Plan 51/61 边界同步。
9. 初始实现与审查纠偏各自一个 `plan62` 提交；公开的 `ef61d9c` 不改写，纠偏提交信息与本文件记录验证边界。

## 后续边界

Plan 62 已完成；后续不得把 Job containment 扩写为 filesystem/network sandbox，也不得因 hooks/MCP/Git 共享 process-creation gate 就宣称这些 helper 具有 shell Job ownership。PowerShell background/PTY/stdin/session 与 pinned Darwin PowerShell matrix 结论仍保持原边界。
