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