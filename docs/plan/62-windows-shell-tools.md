# Plan 62 — Windows 原生 Shell：Git Bash、PowerShell 与 Job Object

> 状态：未开工
>
> 依赖：Plan 50、Plan 61
>
> 交界：Plan 51 继续负责自动后台化、stall、Monitor、完成通知与结果回灌
>
> 调研基线：kloop `70eebfc`；Claude Code 固定源码 `<redacted>` 只作 Windows 架构参考，精确 2.1.220 target 是 darwin-arm64，不能充当 Windows 运行证据。

## 背景

Claude Code 在原生 Windows 上不把 Bash 偷换成 PowerShell：Bash 使用 Git for Windows，PowerShell 是独立工具；从 PowerShell 启动程序只表示它是终端宿主。WSL 又是独立的 Linux 运行环境。

kloop 当前只有 `bash`，执行器固定为 `sh -lc`。Unix 前后台路径依赖独立 process group；non-Unix 的 `kill_group` 是 no-op，`process_group_alive` 恒为 false。因此当前代码即使在 Windows 找到某个 `sh.exe`，timeout、cancel、`kill_bash`、watchdog 和 session Drop 也只能可靠处理直接子进程，不能兑现 Plan 50 的无遗留进程树保证。

本计划只闭环 **Windows 原生 shell execution**：Git Bash、独立 PowerShell、Job Object 进程树所有权、权限和原生 CI。Windows 文件 mutation/reparse-point/handle-relative safety 仍由 Plan 61 负责；Windows filesystem/network sandbox 仍未实现；Plan 51 的后台任务状态机不并入本计划。

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
- 开工前复核 Plan 61 已完成，并确认 `windows-latest` 能运行 workspace clippy/tests；文件工具或 reparse-point blocker 回归 Plan 61，不在本计划降级安全边界。
- 改行为前建立 Windows 原生 baseline：Git for Windows 路径、`pwsh.exe`/`powershell.exe` 可用性、nested Job 环境、现有 Unix-only test helper 与 Python 命令差异。
- 所有 Windows 生命周期结论必须来自 Windows 原生运行；cross-compile 只能证明可编译，不能证明 Job、handle、pipe 或无 orphan。

## 1. 抽出共享进程树执行层

关键文件：

- 新增 `kloop/crates/core/src/process_tree/{mod.rs,unix.rs,windows.rs}`
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
- `--mock` 不读 `[shells]`、不扫描 PATH、不运行 `where.exe`；测试直接注入确定性 `ShellPrograms`。Windows keyless demo 只可使用受控标准路径，找不到 Git Bash 时给 actionable error，不能偷偷换语法。

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
- Plan 51 的前台自动转后台、stall、Monitor、通知和回灌不进入本计划。

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

- CI matrix 增 `windows-latest`，使用 `actions/setup-python` 后统一 corpus-only verifier 调用；Windows 原生运行 Job/Git Bash/PowerShell focused tests、workspace fmt/clippy/tests 和 mock。
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

1. 启用 `windows-latest` 全 workspace 门。
2. 同步 README、HANDOFF、capability report、refs/README 和 Plan 51/61 边界。
3. 保持 CC PowerShell matrix 未证维度不变。
4. 全量门绿后回填本 plan 完成记录，一次提交。

## 非目标

- PowerShell background、`powershell_output`、`kill_powershell` 或 PowerShell 自动后台化。
- Plan 51 的 auto-background/stall/Monitor/通知/结果回灌。
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

# Windows 原生额外 focused gates
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

## 完成标准

1. Windows model shell 在 user code 执行前已进入专属 Job；attach/resume 失败 fail closed。
2. timeout/cancel/leader-exit/explicit kill/watchdog/future Drop/session Drop 后无 root 或 descendant，且无线性 handle leak。
3. Windows Bash 只执行已验证 Git for Windows；缺失时不注册且无 fallback。
4. PowerShell 只在 Windows 注册、仅前台、使用固定 encoded invocation，并通过 PowerShell 7/5.1 原生测试。
5. PowerShell 在 plan 阻断、bypass 仍问、恒串行、无自动 remember；仅用户手工 whole-tool allow 可授权。
6. Job containment 不被 sandbox 配置或 `disable_sandbox` 关闭，且文档不冒充 Windows sandbox。
7. macOS/Linux Plan 50 与 Seatbelt 行为、exact/corpus parity gate 无回退。
8. Windows/macOS/Linux CI 全绿，README/HANDOFF/capability/refs 与 Plan 51/61 边界同步。
9. 一次提交，提交信息含 `plan62`，本文件记录实际验证和提交号。

## 开工时定 / 问用户

本计划的安全与产品边界已经固定。开工时只需确认 Plan 61 与 Windows runner 已满足前置闸门；若不满足，先处理 blocker，不通过缩小 Windows 测试或放宽 Job/file safety 继续。
