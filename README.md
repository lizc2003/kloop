# kloop

A terminal coding agent written in Rust from scratch. Give it a sentence and it
reads the code, edits files, runs commands and looks things up until the job is done.

> Status: in daily use by its author, not a release. Configuration and interfaces
> still change, and nothing here is a compatibility promise.

## What it does

- **Talks to any gateway** — a complete implementation of each of the three wire
  protocols (messages / responses / chat); a provider is one profile in one file.
  Model, effort and context window are stated in that config; nothing is discovered
  at runtime.
- **A full set of tools** — read/write/edit files, bash (interruptible in the
  foreground, pollable in the background), grep/glob, notebooks, web fetch and
  search. On Windows that is native PowerShell, not an emulated bash.
- **Asks before it acts** — every tool call goes through a permission decision:
  reads pass, destructive things ask you, and an approval can be remembered. On
  macOS a seatbelt sandbox sits underneath: writes land only inside an allow-list,
  and the network can be shut off entirely.
- **Sessions you can go back into** — everything is persisted. `-c` continues the
  last one, `-r` picks one from a list, and `--fork <id>#<line>` branches from a
  step in the middle and runs it differently.
- **Not one model doing the work** — sub-agents (dispatched synchronously or run in
  the background), an MCP client (stdio / HTTP / OAuth login), skills, and code mode,
  where the model writes a little JavaScript to orchestrate tools so that a
  hundred-item loop comes back as one result.
- **More than a TUI** — `--headless` runs a single turn for scripts (optionally
  emitting an NDJSON event stream), `--plain` is a line-based REPL, and `app-server`
  turns it into an agent service speaking over stdio.

## Install

Rust 1.96+. macOS is the primary platform (the sandbox has a macOS backend only);
Linux and Windows build and run, and CI covers all three.

```sh
make install          # release build, installed into ~/.local/bin (override PREFIX=)
```

The first install also copies `config/config-demo.toml` to `~/.kloop/config.toml`
(directory 0700, file 0600) and **you have to edit it before the first run** — every
key in the demo says `REPLACE-ME`, and kloop has no fallback. If a config is already
there, it is not touched: only the binary is replaced.

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

One file: `~/.kloop/config.toml`. No environment variable decides which gateway a run
talks to, so the answer cannot change with the shell that started it.

```toml
provider = "my-gateway"

[providers.my-gateway]
wire_api    = "messages"                                # messages | responses | chat
base_url    = "https://gateway.example.com"
auth_header = { Authorization = "Bearer ..." }          # or { x-api-key = "..." }
model       = "claude-opus-4-8"
effort      = "high"
```

Permissions, sandbox, MCP, hooks, skills, sub-agents and code mode are configured in
that same file; the full field reference is
[`config/config-demo.toml`](config/config-demo.toml).

## What is in the repository

| Path | What it is |
|---|---|
| `rust/` | The cargo workspace, 10 crates: `core` is the engine, `cli` the binary entry point, `tui` / `server` two other front ends |
| `rust/DESIGN.md` | Design and behaviour in detail — why each trade-off is what it is. Long, and the only authority |
| `docs/plan/` | One numbered file per development task, including what went wrong; `HANDOFF.md` is the current state (written in Chinese) |
| `config/` | Configuration example |

`make help` lists every build target (`make check` = fmt + clippy + test, the same
three commands CI runs).

## License

[Apache-2.0](LICENSE), Copyright 2026 lizc2003@gmail.com.
