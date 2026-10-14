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

## Community

Share ideas and discuss kloop with the [Linux.do](https://linux.do/) community.
