# Plan 223 — 用户自己的 shell + 登录环境只抓一次

> 状态：✅ 已完成（提交 SHA 以本条所在提交为准）
>
> 依赖：Plan 172（`[env]` 优先级）、Plan 50（前台 bash 生命周期）、Plan 62（Windows shell 工具）。

## 起因

用户报「工具里的 `python3` 是 3.9.6，不是 homebrew 的 3.14.4」。

定位（有实证）：kloop 的 shell 是冻结的 `/bin/sh -lc`。`-l` 让 `/etc/profile`
执行 `path_helper`，它把 `/etc/paths`（这台机器的头两行是 Yunshu 路径）连同
`/usr/local/bin`、`/usr/bin` 整体挪到最前、其余条目追加——于是 homebrew 从用户
zsh 里的第 5 位掉到第 19 位。用户自己的 zsh 登录时同样跑 path_helper，但之后还读
`~/.zprofile`，里面的 `brew shellenv` 又把 homebrew 前置回来；`sh -lc` 永远不读
`~/.zprofile`，所以没人补这一刀。`ps -Eww` 证实 kloop 进程继承到的 PATH 是对的
（homebrew 在前），坏在 kloop 起的那层登录 shell。

参考实现对照（`refs/README.md` 的五个固定快照）：codex 用 `$SHELL` 检测到的用户
shell 跑 `-lc` 并另存一份登录环境快照；claude-code 抓一次快照后每条命令改非登录；
grok 两者都有且可配置；codewhale 的任务路径与 kloop 同形；pi 干脆不用 `-l`。

## 裁决与实现

1. **Unix 用用户自己的 shell**（`shell_programs.rs`）：`[shells].bash` 显式 pin →
   `$SHELL`（须为绝对路径、可执行的普通文件，basename ∈ `sh|bash|zsh`）→ 现有
   `sh` on PATH → `/bin/sh`；WSL 仍是校验过的 `/bin/bash`。分类不了的 shell（fish 等）
   不被信任，直接落到冻结回退。`[shells].bash` 从「仅原生 Windows」放开到 Unix。
2. **登录环境捕获**（新模块 `shell_env.rs`，`ShellLoginEnv`）：Unix 下跑一次
   `<shell> -lc`，脚本用 `\001` 包裹 `command env -0`，5s 超时、1 MiB 上限，
   按 NUL 切 `NAME=VALUE`。过滤三类名字：现有密钥黑名单（`MODEL_SHELL_SECRET_ENV`，
   常量从 `bash.rs` 移到这里，只有一处定义）、启动钩子 `BASH_ENV/ENV`、shell 每次自己
   派生的 `PWD/OLDPWD/SHLVL/_`。再合并配置 `[env]` 的实际值（配置赢，Plan 172；
   同样过滤密钥、钩子与派生变量）。捕获失败/超时/空捕获/关闭开关一律
   返回 `none()`，不是错误。
3. **命令改非登录**（`bash.rs::shell_spec`）：有捕获时使用固定 prelude 与非登录 `-c`，
   逐条 `spec.env(name, value)` 覆盖注入（不 `env_clear`，与 `[env]`「没提的名字
   原样继承」一致，也保住 Windows 的环境块），启动文件后 source 私有临时文件恢复环境，
   再执行作为位置参数传入的原命令；没有捕获或回放文件暂不可用时维持 `["-lc", command]`。
   文件丢失时从捕获表重建；重建失败仍保留捕获、逐项注入环境，后续命令继续重试。
   捕获若仍走登录 shell，`/etc/profile` 的 path_helper 会把注入的 PATH 再重排一遍，
   所以「非登录」是捕获生效的必要条件，不是风格选择。
4. **接线**：`Config` 增 `shell_login_env: Arc<ShellLoginEnv>`（进程级，与
   `shell_programs` 同生命周期，`fresh_session` 保持同一 `Arc`）；`RuntimeSettings`
   在解析 shell 之后捕获一次（`--mock` 跳过），失败写进 `shell_warnings`；
   `[shells]` 新增 `login_env`（bool，默认 true，`shells.login_env` 非布尔即报错）。
5. **工具描述**：`builtin.rs` 的 bash 描述从 `sh -lc` 改成「用户的 shell」。

## 非目标

- 不 source 交互式 rc（grok 会显式 source `$ZDOTDIR` 的 rc；codex/claude-code 只靠
  登录 shell）。登录 profile 已覆盖本症状，交互 rc 有把 oh-my-zsh 拉进非交互启动的
  风险。要加是后续独立一件事。
- Windows 不变（Git Bash 路径与 `-lc` 原样）；hooks 不变（hook 命令是用户自己的 argv，
  不经 kloop 的冻结 shell）。

## 复审修正（四条，来自代码审查）

1. **[P1] 非登录 zsh 仍读 `~/.zshenv`。** 只把参数改成 `-c` 不够:zsh 即使非登录、
   非交互也会读 `~/.zshenv`。本机实测确认——`zsh -c` 会把 `~/.zshenv` 里导出、而捕获
   已按黑名单剔除的 `TAVILY_API_KEY` 重新导出（`-f` 之后为 unset）。修法:捕获生效时
   zsh 加 `-f`（抑制全部启动文件）,并在 `apply` 里移除 `BASH_ENV`/`ENV`（非交互 shell
   仍会展开的启动钩子）。新增 `ShellFlavor::Zsh`,由 shell basename 判定（与命令分析同
   一套规则）。
2. **[P2] 快照只记录新增,不记录删除。** profile `unset PYTHONHOME` 后,父进程那个错误
   值会沿继承回来。修法:`ShellLoginEnv` 增加 `removed`——捕获时取「kloop 环境里存在、
   但不在过滤后捕获里的名字」,排除配置 `[env]` 的名字与 kloop 自己注入的
   `KLOOP_SANDBOX*`;回放时 `env_remove` 这些名字再 `env` 写入捕获值。
3. **[P2] 超时只杀根进程。** 探针改用独立进程组（`process_group(0)`）,超时/错误路径
   用 `rustix::process::kill_process_group` 杀整组,读取线程随管道写端全部消失而结束。
4. **[P2] 捕获辅助项缺 `#[cfg(unix)]`。** Windows 下 `VOLATILE_ENV`、`CAPTURE_*`、
   `unavailable`、`parse_env0`、`removals`、`kill_group` 等会变成 dead_code,在
   `-D warnings` 门禁下报错。已逐个补齐。验证方式见下。

## 复审修正（第二轮，四条）

1. **[P1] `-f` 挡不住 `/etc/zshenv`。** 官方文档写明「Commands are first read from
   /etc/zshenv; this cannot be overridden」——`-f` 只能让该文件自己用 `if [[ -o rcs ]]`
   自我跳过。所以「启动文件已全部抑制」是错的：一个 root 拥有的 `/etc/zshenv` 仍可在回放
   之后重新导出被过滤的密钥、改写 PATH 或代理。修法:回放改由 **shell 自己在启动文件之后**
   执行——当时 `bash.rs` 用一个固定 prelude(`eval "${KLOOP_SHELL_ENV_REPLAY-}"` → `unset` 载体
   → `eval "$1"`),载体 `KLOOP_SHELL_ENV_REPLAY` 里是要执行的 replay 脚本(`unset` 掉
   删除集与密钥,`export` 回捕获值,单引号转义,非标识符名不进脚本)。命令作为 `$1` 传入,
   所以不需要把它引号进脚本,也不会被二次解析。保留 `-f`(压掉其余启动文件与副作用)与
   `BASH_ENV`/`ENV` 移除(bash 非交互仍会展开它们)。载体值本身就是子进程环境的一部分,
   不新增暴露面;prelude 在跑命令前把它 unset。该载体与命令保存方式已被第三轮修正替代。
2. **[P2] 非 UTF-8 值被误判成「profile 删除了」。** `parse_env0` 现在返回 `ParsedEnv
   { pairs, names }`:值不可解码时仍记名字,`removals` 以 **names** 判断存在性,于是
   PATH 里含非 UTF-8 目录时不会被误删(那个变量仍继承父进程的值,只是无法回放)。
3. **[P2] Windows 测试模块的 `ShellFlavor` 导入未受 cfg 保护。** 该导入只被 `#[cfg(unix)]`
   的测试用到,Windows 的 `clippy --all-targets … -D warnings` 会因 unused_imports 报错
   ——已补 `#[cfg(unix)]`。
4. **[P2] 捕获失败路径仍会留下后代。** 原来只有 `try_wait` 报错和主循环超时会清进程组;
   非零退出、以及根进程成功退出但后台后代占着 stdout 导致读取超时,都会直接返回。现在循环
   只负责得出结果,`kill_group` 在**所有**退出路径之后无条件执行。

## 复审修正（第三轮，五条，针对 `0d633d2`；修复提交 `45ac229`）

1. **启动钩子被回放重新导出。** `BASH_ENV`/`ENV` 从捕获与 `[env]` 合并中剔除，
   进程注入与脚本回放也过滤它们，避免命令中的嵌套 bash 再次读取钩子。
2. **普通变量不能保存已获准命令。** 去掉 `__kloop_cmd`；原命令一直保留在位置参数，
   回放不写位置参数。执行原命令前清空参数，命令仍看到 `$# = 0`。
3. **内建只读变量不做运行时赋值。** `SHELLOPTS`、`BASHOPTS`、`UID` 等仅通过进程环境
   继承，脚本不 export/unset，避免 `errexit` 下执行原命令之前退出。
4. **回放载体不能重复整份启动环境。** 用已有随机资源 ID 与排他创建逻辑生成临时脚本，
   创建时 0600、写完 0400，`Arc` 持有到最后一次正常释放并删除。argv 只带路径，环境仍
   逐项注入；既避开单个变量的长度限制，也不再重复占用 exec 的总启动空间。准备失败沿用
   `-lc` 回退；文件失败的重试与诊断已在第四轮细化。
5. **配置优先级必须在启动文件之后兑现。** 捕获接口接收 `[env]` 的键和值，合并为最终
   快照，进程环境和回放使用同一组值，PATH 与代理不再被启动文件覆盖。

## 复审修正（第四轮，两条，✅；修复提交 `2853c2b`）

1. **临时文件不能成为不可恢复的会话依赖。** 0400 不阻止属主在可写目录里 unlink。
   回放文件改为共享锁保护的可替换缓存，每次构造命令参数时检查，丢失后从内存中的
   捕获表重建。重建失败仅本次退回 `-lc`，仍注入捕获环境，并保留数据供后续重试。
   并发 clone 共用一次重建，最后一个持有者释放时删除当前文件。临时目录在检查后
   被外部清理仍可能使本次 source 失败，但不会再造成后续全部 bash 永久失败。
2. **文件准备失败必须指出文件系统原因。** 启动警告单独说明回放文件准备失败，保留
   临时目录路径与原始 I/O 错误；不再冒充登录环境捕获失败。成功的捕获也不再丢弃，
   临时目录恢复后下次命令即可重新准备文件。

## 运行时降级告警补充（✅；提交 SHA 以本节所在提交为准）

第四轮保留了启动失败原因，但运行时重建失败仍静默退回 `-lc`。现将失败路径接到
已有 `Ui::emit(Event::Note)`，每次失败都告警，包含临时目录与 I/O 原因、本次登录 shell
降级及后续重试。前台、后台与沙箱升级后的重跑都经过同一处参数准备，TUI 与普通终端
沿现有通知路径显示；成功重建与正常捕获不增加告警。

## 验证

- 运行时告警补充：回放相关 20 项测试通过，默认并行 `make check` 全绿（fmt、clippy、
  workspace debug 全量测试与本机 parity）。真实前台、后台调用重建失败时各收到一条
  含完整原因的 UI 告警，命令仍成功执行；恢复后两种调用均不再增加告警，成功重建也不告警。
- 第四轮门禁：回放相关 19 项测试通过；默认并行 `make check` 全绿，包含 fmt、clippy、
  workspace debug 全量测试与本机 parity。未自动追加 release 检查。
- 第四轮回归：真实 bash 工具清理自己的回放目录后，下一次调用重建并成功执行；
  四个并发 clone 只生成一个新文件，内容、0400 权限与最后释放时删除均保持；
  写入失败时真实 shell 按 `-lc` 执行原命令、仍携带捕获环境，目录恢复后回到 `-c`；
  启动准备失败警告包含实际路径和 I/O 原因，且成功捕获仍可用于重试。
- 第三轮默认并行 `make check`（fmt + clippy + workspace debug 全量测试 + parity）全绿；
  PTY 整组 18 项也单独按默认并行运行通过。release 检查按用户要求只手动运行。
- 第三轮门禁发现并按用户要求修复 PTY 测试时序：两轮溢出测试先检查中间帧上的
  `[manual]` 数量、后等待静默，默认并行运行两次失败而单跑/串行通过。完整屏幕断言
  改在 `wait_for_quiescent` 后与快照检查同一帧，缩放测试的同类断言一并修正。
- 新断言：`shell_env::tests::*`（标记解析、密钥/钩子/易变名过滤、`[env]` 合并、非零退出与
  超时降级）、`shell_programs::tests::{the_users_own_shell_is_preferred_when_it_is_posix,
  a_shell_kloop_cannot_classify_falls_through,
  an_explicit_shells_bash_pin_beats_the_user_shell,
  without_a_usable_user_shell_the_frozen_lookup_still_answers}`、
  `bash::tests::{an_active_login_environment_runs_the_shell_non_login,
  a_captured_login_environment_reaches_the_command}`、
  `config` 的 `fresh_session` 同一 `Arc` 断言、`startup` 的 `login_env` 解析与默认值。
- 本机真实捕获（不进自动门禁）：`/bin/zsh -lc` 抓回的 PATH 里 homebrew 在第 2 位，
  用它跑非登录 zsh，`python3` = `/opt/homebrew/bin/python3`（3.14.4）。
- 复审修正的断言：`shell_env::tests::{removals_are_what_the_profile_dropped,
  replay_writes_the_removals_and_the_additions, kill_group_ends_the_whole_group}`、
  `bash::tests::an_active_login_environment_runs_the_shell_non_login`（含 zsh 的
  `-f -c` 形状）。`kill_group_ends_the_whole_group` 做过判别力验证：临时去掉 killpg
  时它会失败（后代留在组里），恢复后通过。
- P1 的本机验证：`zsh -c` 下 `TAVILY_API_KEY` 为 SET，`zsh -f -c` 下为 unset。
- P2（cfg）的验证：本机装了 `x86_64-pc-windows-msvc` target，但交叉编译被
  `aws-lc-sys` 缺 Windows SDK 头挡住；改为把 `shell_env.rs` 的 unix cfg 临时翻转成
  非 unix 形状跑 `cargo check -p kloop-core --lib`，该模块零警告，随后恢复原文件并重跑
  测试。
- 第二轮新增断言：`shell_env::tests::{an_undecodable_value_keeps_its_name_but_no_value,
  replay_unsets_what_it_dropped_and_quotes_what_it_keeps,
  both_failure_paths_leave_nothing_behind}`、
  `bash::tests::the_replay_prelude_beats_the_inherited_environment`（用真 `/bin/sh` 跑
  prelude，且故意把 PATH 从 `env_remove` 里放回去，证明是 prelude 而不是进程环境在起作用）。
  `both_failure_paths_leave_nothing_behind` 做过判别力验证：把退出路径上的 `kill_group`
  停掉时它会红（后代活过捕获），恢复后通过。
- 第二轮本机验证：真 zsh 上跑 prelude——不带 prelude 时 `TAVILY_API_KEY` 为 SET，带上
  之后为清空、捕获值生效、载体 `KLOOP_SHELL_ENV_REPLAY` 在命令里已 unset。
- 第三轮回归断言：捕获同时过滤 profile 与配置中的钩子/密钥，配置实际值覆盖 PATH；
  真 sh/zsh 上验证回放 export/unset `__kloop_cmd` 都不能改变原命令，命令看到零个
  位置参数，嵌套 bash 不继承启动钩子；真 bash 导入 `errexit:nounset:pipefail` 后仍能
  执行命令；三个各 50,000 字节的变量可以启动并完整恢复；模拟启动文件改写 PATH/代理后
  配置值恢复；临时文件权限为 0400、共享快照释放到最后一次才删除；空捕获即使有配置
  覆盖也降级，不把配置本身误当作成功捕获。
- 排查记录（教训）：新写的可执行脚本在 macOS 上**首次 exec 约 200ms**，第一版失败用例用的
  是 300ms 超时，于是测试把「起步慢」误报成产品缺陷。凡是「写文件 + 立刻执行 + 短超时」的
  测试都要留出这段首次执行开销，或轮询等待就绪信号而不是假设立即就绪。
