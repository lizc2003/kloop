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
   常量从 `bash.rs` 移到这里，只有一处定义）、配置 `[env]` 会设的名字（配置赢，
   Plan 172）、shell 每次自己派生的 `PWD/OLDPWD/SHLVL/_`。失败/超时/关闭开关一律
   返回 `none()`，不是错误。
3. **命令改非登录**（`bash.rs::shell_spec`）：有捕获时参数用 `["-c", command]`，
   逐条 `spec.env(name, value)` 覆盖注入（不 `env_clear`，与 `[env]`「没提的名字
   原样继承」一致，也保住 Windows 的环境块）；没有捕获时维持 `["-lc", command]`。
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

## 验证

- `make check`（fmt + clippy + workspace test + release 测试 + parity）全绿。
- 新断言：`shell_env::tests::*`（标记解析、密钥/`[env]`/易变名被剔除、非零退出与
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