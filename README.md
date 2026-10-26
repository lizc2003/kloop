# kloop

A terminal coding agent built from scratch in Rust. Describe a task in plain
language, and kloop explores your codebase, edits files and runs commands.

## What it does

A coding workflow similar to Claude Code, with permissions, resumable sessions,
sub-agents, MCP and skills. Supports configurable messages / responses / chat
gateways and runs interactively, headlessly or as an agent service.

## Install

Requires Rust 1.98+. Primarily tested on macOS; Linux and Windows also build and
run. Sandboxing is macOS-only.

```sh
make install          # installs to ~/.local/bin
```

Edit the generated `~/.kloop/config.toml` with your provider settings before the
first run. Existing configuration is preserved.

## Use it

```sh
kloop                          # start the TUI; the current directory is the workspace
kloop --mock                   # keyless scripted demo — one run shows you the shape
kloop -c                       # continue the previous session
kloop --headless "fix the flaky test in CI"
kloop --worktree=fix-ci        # work inside an isolated git worktree
kloop --help                   # every flag
```

## Configuration

Configure your provider in `~/.kloop/config.toml`:

```toml
provider = "my-gateway"

[providers.my-gateway]
wire_api       = "responses"
base_url       = "https://gateway.example.com/v1"
auth_header    = { Authorization = "Bearer ..." }
model          = "deepseek-v4.1-flash"
effort         = "xhigh"
context_window = 1000000
```

See [`config/config-demo.toml`](config/config-demo.toml) for all settings.

可在 `~/.kloop/config.toml` 的顶层指定自定义系统提示词文件（放在任何 `[表名]` 之前）：

```toml
system_prompt_file = 'system-prompt.md'
```

把提示词正文直接写入 `~/.kloop/system-prompt.md`，使用 UTF-8，无需 TOML 转义。
相对路径基于配置文件所在目录，也支持绝对路径。文件正文整体替换内置基础提示词；
环境信息和开场 Git 快照仍由程序追加。省略该配置则使用内置正文；文件读不到、非 UTF-8
或正文为空白时启动报错。修改后重启 kloop 生效，包括恢复已有会话。

## Community

Share ideas and discuss kloop with the [Linux.do](https://linux.do/) community.
