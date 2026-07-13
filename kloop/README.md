# kloop

k is for keel — a minimal Rust agent MVP (~1200 lines, single crate) built to
validate five architectural bets before committing to a larger agent design.

## The five bets

1. **Append-only history + offload at record time.** History is only ever
   appended. A tool result over 8000 chars is spilled to `.kloop/offload/`
   when recorded; the history keeps a head/tail preview plus a pointer, and a
   `read_offloaded` tool fetches the full output on demand.
2. **Continuation signal = presence of `tool_use` blocks.** Never
   `stop_reason` — it is unreliable across providers.
3. **Concurrency safety decided per call, by name AND input.**
   `is_concurrency_safe(name, input)` parses bash commands for read-onlyness;
   consecutive safe calls run as one concurrent batch, everything else runs
   sequentially.
4. **Sub-agents recurse into the same `run_turn` loop** (depth capped at 1;
   `Pin<Box<dyn Future>>` breaks the type recursion).
5. **Provider seam.** Internals speak Anthropic Messages shape only; adapters
   translate Anthropic native SSE and OpenAI-compat `tool_calls` streaming
   into one `StreamEvent` enum. A Mock provider enables keyless end-to-end
   runs.

Extras that came cheap: streaming retry (3 attempts, exponential backoff with
clock-nanosecond jitter, no `rand` dependency), `CancellationToken`
interruption, and orphan patching — on interrupt every unanswered `tool_use`
gets an `is_error` `tool_result` so history stays legal.

## Compaction (Phase 2, first slice)

Two complementary defenses keep long sessions inside the context window
(`src/compact.rs`), both validated first in a dry run on a production codex
fork:

- **Predictive**: before each sampling round, estimate the current context
  (provider-reported usage anchors a ~4 chars/token heuristic for anything
  recorded after it) plus one round of growth (output cap bounded at 20k +
  15k tool-result reserve); compact BEFORE the request when it would overflow.
  Windows at or below the growth reserve skip prediction — a non-positive
  threshold would mean "always compact".
- **Reactive**: a request rejected as too large (`prompt is too long` /
  `context_length_exceeded`) compacts once per turn and retries; a second
  overflow surfaces as an error instead of looping.

Compaction itself asks the model for a structured handoff summary, keeps a
~2k-token recent tail verbatim (never splitting a tool_use/tool_result pair
at the boundary), and replaces the rest with the summary — the one
sanctioned rewrite of the append-only history.

`AGENT_CONTEXT_WINDOW` sets the usable window in tokens (default 200000,
`off` disables compaction).

## Session persistence (Phase 2, second slice)

Every session is persisted to `.kloop/sessions/{id}.jsonl`
(`crates/core/src/rollout.rs`), one JSON line per recorded message, written
through as the history records — so a killed process loses at most the line
being written. Compaction appends a `compacted` marker line carrying the full
replacement history (the codex rollout pattern): the file stays append-only
and auditable, and replay just swaps in the replacement and keeps reading.

Every line carries an envelope — `id` (`{session}#{seq}`, no rand
dependency), `parent` (previous line's id, linked across resumed runs), `ts`
(unix ms). Within one file the chain is purely sequential; a forked file's
first line carries a cross-file parent (see Fork below). Unknown fields are
ignored on read (locked by test), so the format grows additively.

Resume replays the file, then makes the history legal and consistent again:

- pairing is repaired in both directions (as in claude-code): unanswered
  `tool_use` blocks get the same `is_error` "interrupted" results the live
  interrupt path uses, and stray `tool_result` blocks answering nothing are
  dropped;
- a torn tail (crash mid-append) is truncated to the last intact line —
  physically, before appending resumes, so the partial bytes can't merge
  with the next line and orphan everything after (read-only paths like
  `--list-sessions` never modify the file);
- the process-global offload counter advances past every `off-NNNN.txt`
  already on disk, so new spills never clobber files the resumed history
  points at (usage anchors are not persisted — the estimate re-anchors on
  the first sampled response).

Session ids are UTC timestamps (`YYYYMMDD-HHMMSS`, no rand/chrono
dependency); `--resume` picks the most recently modified session, `--resume
<id>` a specific one, `--list-sessions` shows what's on disk.

### Fork (and rewind)

`--fork <id>#<seq>` branches a new session off an existing one at line
`#<seq>` and continues there (`--fork <id>` forks at the end). This also
covers rewind: fork your own session at an earlier point and take the other
road — cheaper than an in-place rewind mechanism, and codex upstream is
deprecating its rollback API in favor of exactly this.

Mechanics (the shape both references converged on — cc's `/branch` and
codex's `thread/fork` both copy, neither replays across files):

- the kept prefix (`#1..=#<seq>`) is physically copied into a brand-new
  session file, re-enveloped under the new stem with timestamps preserved;
  the source file is never touched;
- the fork's first line carries a cross-file parent — `{source}#{seq}` —
  as lineage metadata only; replay stays single-file, so `--resume` works
  on a fork unchanged and forks can be forked again. `--list-sessions`
  shows the lineage as `[forked from {source}#{seq}]`;
- a cut is legal only where the kept prefix ends a complete exchange (the
  next line must start a fresh user turn — cc's `/rewind` whitelist rule),
  which makes splitting a tool_use/tool_result pair impossible by
  construction; an illegal cut lists the legal points near it;
- a compacted marker inside the prefix replays as usual; a cut at a legal
  point *before* one forks the raw pre-compaction history, which never
  left the file;
- offload files are shared across branches (pointers are copied text; the
  dir-scanning counter already prevents clobbering), and usage anchors are
  not persisted, so a fork re-anchors on its first sampled response.

## Permissions (Phase 2, third slice)

Every tool call passes a layered gate before executing
(`crates/core/src/permissions.rs`), shaped after claude-code's permission
pipeline, with the bash analysis ported from codex's `shell-command` crate:

```
deny rules → safety checks → ask rules → sandbox auto-allow → bypass
→ read-only self-verdict → acceptEdits → allow rules → session cache
→ ask the user
```

Two invariants carried over from claude-code: **deny always beats allow**,
and **safety checks are immune to bypass mode**. The sandbox auto-allow
layer is the sandbox/approval coupling — see OS sandbox below: a bash call
the OS sandbox will contain skips everything beneath this layer, while deny
rules, safety checks and explicit ask rules keep their say (the ask-rule
half is deliberately stricter than cc's autoAllowBashIfSandboxed).

**Bash decisions run on a real parse tree** (`crates/core/src/shell.rs`,
tree-sitter-bash): a script qualifies only when every node is a plain
word-only command joined by `&&`/`||`/`;`/`|`/newline; `bash -c "…"` is
unwrapped and analyzed recursively. Subshells, redirections, command/process
substitution, expansions, and variable-assignment prefixes make the script
*opaque* — never auto-approved, never allow-rule-matchable, never cached; it
always goes to the human. The read-only classifier vets options, not just
names (`find -delete`, `rg --pre`, `git -C`/`--git-dir`/`log --output`,
`base64 -o`, `sed` beyond `-n Np` all disqualify), and the independent
dangerous classifier (`rm -rf`, `sudo …`) forces a confirmation even when an
allow rule or bypass mode would otherwise pass — wrappers (`sudo`, `env`,
`timeout`, `nice`, `xargs`) are stripped before deny/danger matching so they
can't smuggle a command past a rule.

**Rules** live in `.kloop/config.toml` and env vars (comma-separated
`AGENT_ALLOW` / `AGENT_DENY` / `AGENT_ASK` append on top):

```toml
[permissions]
allow = ["bash(cargo *)", "write_file(src/**)", "edit_file"]
deny  = ["bash(git push *)", "read_file(**/*.pem)"]
ask   = ["bash(cargo publish *)"]   # always confirm, even if allowed
```

`tool_name` covers the whole tool; `bash(<tokens>)` matches one command's
leading argv tokens (trailing `*` = any remainder, no `*` = exact), applied
per segment — in a chain every segment must be read-only or allowed, while a
single denied segment poisons the whole chain; `write_file(<glob>)` /
`edit_file(<glob>)` / `read_file(<glob>)` match the lexically-normalized
path (and its cwd-relative form) with `**` globs.

**File writes** get path safety: `.git`/`.kloop`/`.ssh`/`.gnupg`/`.aws`
directories, shell/git rc files, and `.env*` are sensitive — confirmed every
time, immune to allow rules, acceptEdits, and bypass. Writes escaping the
working directory never auto-pass in acceptEdits.

**Asking**: `y` allow once · `a` allow for this session (cached per two-word
bash prefix — approving `git commit` never covers `git rebase` — or per
parent directory for file writes) · `p` allow always (appends the suggested
rule, e.g. `bash(cargo build *)`, to `.kloop/config.toml`) · `n` deny. A
denial is not a turn abort: the model receives an `is_error` `tool_result`
and is told to take another approach. Sub-agents share the parent's rules
and cache and prompt through the same seam, tagged `[sub-agent]`.

**Change previews**: when a `write_file`/`edit_file` reaches the prompt, the
request carries a line-numbered diff (`crates/core/src/diff.rs`, `similar`) so
you approve what you can see, not just a path — approving an invisible edit is
meaningless. Following claude-code and codex (which independently converge on
it), `edit_file` **reads the target, applies the edit, and diffs the whole
file** — the change shown in its real surrounding lines with real line numbers,
not the edit strings in isolation. `write_file` diffs an existing file old→new,
or shows a `(new file)` insert preview for a fresh path. When the file can't be
read, is over 1 MiB, or the `old_string` doesn't uniquely match, it falls back
to diffing the two edit strings (numbered from 1) — claude-code's same
degradation. Each line is `{+/-/space}{line-number}  {content}`; hunks carry
three lines of context separated by `⋮`, big diffs are capped
(`… (N more line(s))`), minified lines clipped. The TUI colors the popup (green
adds, red deletes, dim context), the plain REPL prints the same ANSI, the
server adds a `preview` field to `approval/request`. The popup is not yet
scrollable, hence the caps.

**Modes**: default (ask for anything unvouched-for), `--accept-edits`
(file writes inside the working directory auto-pass), `--yolo` (bypass:
everything passes *except* deny rules and safety checks). `--mock` disables
the gate entirely — nobody is at the keyboard.

## TUI (Phase 2, fourth slice)

The default entry point is a ratatui terminal UI (alternate screen): a
scrolling transcript on top, a one-line status row, and a one-line input at
the bottom. Tool calls collapse to single status rows (`… bash {...}` while
running, `✓`/`✗` when done) — their full output lives in history/offload, not
on screen. Permission prompts appear as a centered y/a/p/n popup; prompts
from a concurrent tool batch queue and are answered in order.

Structure (`crates/tui`): the agent runs on its own tokio task and owns
`History`; `ChannelUi` implements both `Ui` and `Approver` by forwarding
everything as events over an mpsc channel (approval decisions travel back
over a oneshot; a dropped reply means deny). The UI loop `select!`s crossterm
key events against agent events, folds both into pure state (`App`), and
renders via pure cell→line functions — which is what makes the transcript
logic testable without a terminal. Streaming deltas are drained in batches so
a burst of tokens redraws once, not per token.

Keys: Enter sends when idle, or **steers** while a turn runs (see below);
Ctrl+C interrupts the running turn or clears the input when idle, Ctrl+D
quits, Up/Down/PageUp/PageDown scroll the transcript (view pins back to bottom
on send). `--plain` keeps the old line-based REPL.

`--resume` replays the saved session into the transcript (user/assistant
text plus tool status rows re-derived from the recorded tool_use/tool_result
pairs), so a resumed session starts with its conversation visible instead of
a blank screen.

## Server mode (Phase 2, fifth slice)

`kloop --serve` speaks a JSON-RPC-shaped protocol over stdio (after codex's
app-server: JSON-RPC 2.0 envelopes minus the `"jsonrpc"` field, one object
per line) so IDEs and automation can drive multiple sessions concurrently.

Methods: `thread/start`, `thread/resume {threadId}`, `thread/list`,
`turn/start {threadId, input}`, `turn/interrupt {threadId}`. Every thread is
its own tokio task owning a History (persisted to the same
`.kloop/sessions/` files the interactive frontends use — sessions are
interchangeable between the TUI and the server) and its own permission gate,
so approval session caches never leak across threads.

Notifications stream per thread: `turn/started`, `text/delta`, `note`,
`tool/started`, `tool/completed`, `turn/completed {reason}`. Approvals are
server→client requests in an own `srv-{n}` id namespace; the client answers
`{"decision": "allow" | "allowSession" | "allowAlways" | "deny"}`, and a
dropped/never-answered reply denies (interrupt the turn to unblock).

```jsonc
→ {"id":1,"method":"thread/start","params":{}}
← {"id":1,"result":{"threadId":"20260709-135146"}}
→ {"id":2,"method":"turn/start","params":{"threadId":"20260709-135146","input":"create s2.txt"}}
← {"method":"turn/started","params":{"threadId":"20260709-135146"}}
← {"id":"srv-1","method":"approval/request","params":{"threadId":"…","description":"write_file: s2.txt","rememberRules":["write_file(*)"],"preview":"(new file)\n+1  hello"}}
→ {"id":"srv-1","result":{"decision":"allow"}}
← {"method":"tool/completed","params":{"threadId":"…","callId":"…","ok":true}}
← {"method":"turn/completed","params":{"threadId":"…","reason":"completed"}}
```

## MCP client (Phase 2, sixth slice)

kloop connects to external MCP tool servers over stdio (JSON-RPC 2.0,
newline-delimited JSON — one object per line). Declare servers in
`.kloop/config.toml`:

```toml
[mcp.servers.fs]
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "sandbox"]
env = { }                                          # merged onto the process env
readonly = ["read_text_file", "list_directory"]    # eligible for concurrent dispatch
```

Servers are spawned once at startup (killed on exit); the handshake is
`initialize` → `notifications/initialized` → `tools/list` (with `nextCursor`
pagination), and each advertised tool joins the model's tool list as
`{server}__{tool}` with its inputSchema passed through verbatim. A failing
server degrades to a startup warning — MCP never blocks kloop. Name
sanitization folds everything outside `[A-Za-z0-9_]` to `_` (so persisted
allow rules round-trip through the permission-rule grammar); collisions warn
at startup and the colliding later definitions are skipped.

Calls go out with the raw server-side tool name; the result content array is
flattened to text (binary blocks degrade to `[image: …]`-style tags), and
`isError: true` surfaces as an is_error tool_result — same shape as a failing
built-in. MCP tools run serially unless listed in `readonly`, and always ask
for permission unless covered by an allow rule (`memory__create_entities` in
`[permissions].allow`) or the session cache — the `a`/`p` answers work on
whole-tool granularity.

Layering: core only knows the `ToolSource` trait (`tools/mod.rs`); the wire
client is the `kloop-mcp` crate (depends only on protocol); the CLI glues
them (config parsing, namespacing, the adapter).

### Deferred tools + tool_search

Past 30 total tools (`AGENT_DEFER_THRESHOLD` overrides; built-ins never
defer), MCP tool definitions stop being sent to the model. Instead the
request carries the built-ins plus two extra tools, and the synthetic
context message lists the deferred tool names:

- **tool_search** `{query, max_results=5}` — `select:<name>[,<name>...]`
  fetches exact tools; anything else is a keyword search over names (ranked
  first) and descriptions. Matching tools' full definitions (description +
  JSON schema) come back in the result and those tools unlock for the rest
  of the session.
- **call_tool** `{tool_name, params}` — escape hatch for models that refuse
  to emit tool calls for names absent from their declared tool list (some
  OpenAI-compat models). Dispatch unwraps the envelope up front, so
  permissions, hooks, concurrency and the UI all judge the real tool name.

The tool defs array and the injected name list are byte-stable for the whole
session — unlocking only opens the dispatch gate, it never mutates the
request prefix, so the prompt cache survives. Calling a deferred tool before
searching bounces with guidance (and does not unlock); permission rules and
the approval cache keep whole-tool-name granularity throughout. Sub-agents
share the parent's unlock set. Under the threshold nothing changes: all
tools ship inline and neither tool_search nor call_tool exists.

## Hooks (Phase 2, seventh slice)

External command hooks fire at four points: before/after a turn
(`pre_turn` / `post_turn`) and before/after a tool call (`pre_tool` /
`post_tool`). Declare them in `.kloop/config.toml`:

```toml
[[hooks]]
event = "pre_tool"            # pre_turn | post_turn | pre_tool | post_tool
command = ["./guard.sh"]      # argv, not a shell string
matcher = "bash"              # tool events only: exact tool-name filter
timeout_ms = 5000             # optional, default 10000
```

The event arrives as one line of JSON on the hook's stdin: `event` and
`session_id` always, plus `tool_name`/`tool_input` on tool events and
`tool_result`/`is_error` on `post_tool`. Exit code 0 allows; exit code **2
blocks** on the pre_* events (cc's convention — a block must be an explicit
signal): a blocked `pre_tool` call never runs and the model gets an is_error
tool_result (`blocked by hook: …`, the reason read from stderr — stdout is
the context channel), a blocked `pre_turn` means the turn never starts.
Every other outcome fails **open** with a warning: any other exit code, exit
2 on a post_* event, a spawn failure, a timeout — a broken hook script is a
malfunction, not a policy decision (the permission gate is the enforcement
layer). Whatever an allowing hook prints on stdout is injected into history
as a `[{event} hook]`-prefixed user message the model sees.

Ordering with permissions: `pre_tool` hooks run **before** the permission
gate — hooks are automation policy, the permission prompt is the human's
last word; a hook block means there is nothing left to ask about. Hooks run
in config order; the first block short-circuits the rest. Sub-agents inherit
the parent's hook set and session id. `--mock` runs without hooks (hermetic).

## Project context (Phase 2, eighth slice)

At startup kloop assembles what the model knows about where it is:

- **System prompt** = base instructions + an environment block (working
  directory, platform, today's UTC date, whether cwd is a git repo) + a git
  snapshot (current branch, `git status --short` capped at 1000 bytes,
  last 5 commits) labeled as a start-of-session snapshot.
- **Instruction files**: per directory `AGENTS.md` wins, `CLAUDE.md` is the
  compatibility fallback. Layers, in order: `~/.kloop/` (global), then every
  directory from the git root down to cwd (closest to cwd last). Without a
  git root only cwd is consulted. Missing files are simply absent. Total
  budget 32 KiB across all files — over it, the overflowing file is truncated
  and the rest skipped, with startup warnings.
- Following cc and codex, instruction files do **not** go into the system
  prompt: they ride every sampling request as a synthetic first user message
  (`<project-instructions>…</project-instructions>`) that is never recorded
  to history — so `--resume` picks up fresh edits and compaction can't
  swallow the rules. The overflow prediction accounts for it separately.

Assembly is pure functions in `core/src/context.rs` (testable without a
filesystem); the IO — file discovery, git commands — lives in
`cli/src/context.rs`. Sub-agents inherit the same context with the Config;
server threads share one process-wide assembly. `--mock` stays hermetic:
no file reads, no git commands, the pre-assembly hardcoded prompt.

## Search tools (Phase 2, ninth slice)

Dedicated read-only `grep` and `glob` tools (`core/src/tools/search.rs`), built on
ripgrep's own crates (`grep-searcher`/`grep-regex`/`ignore`) — no external
binary, no shell-quoting pain, and both are read-only by verdict: they skip
the approval gate and join concurrent tool batches. Shapes follow cc's
Grep/Glob:

- **grep**: `pattern` (Rust regex) plus `path`, `glob`, `type` filters;
  `output_mode` = `files_with_matches` (default, newest-first) | `content`
  (`path:line:text`, `-n`/`-A`/`-B`/`-C` supported) | `count`; `-i`,
  `multiline`, and `head_limit`/`offset` paging (default 250). Honors
  .gitignore, searches hidden files, never descends into VCS dirs, skips
  binary files, clips matched lines at 500 chars, stops after a 20s budget
  with a partial-results note.
- **glob**: gitignore-style `pattern` under `path`, newest-first, capped
  at 100 with cc's truncation notice.

Two deliberate deviations from cc (documented in plan 14): `glob` honors
.gitignore (cc's does not — a Rust tree would drown in `target/`), and the
newest-first sort happens before the cap so truncation drops the stalest
files, not the freshest. cc has no LS tool anymore; kloop follows (bash `ls`
is already read-only-whitelisted).

## Background bash (Phase 2, tenth slice)

`bash` takes `run_in_background`: the command starts in its own process
group, stdout/stderr interleave straight into a file under `.kloop/offload/`
(`bg-N.out`, fd-level — no reader tasks, no pipe deadlock), and the tool
returns immediately with the ID and the output path. Companions:

- **bash_output** `{bash_id, block=true, timeout_ms=30000}` — blocks until
  the command finishes (or the timeout), or peeks with `block=false`;
  reports `running` / `completed (exit 0)` / `failed (exit N)` /
  `killed (reason)` plus the last 30k bytes of output (read the file with
  `read_file` for more). Read-only: skips the gate, joins concurrent batches.
- **kill_bash** `{bash_id}` — SIGKILLs the whole process group and waits for
  the registry to confirm. Auto-allowed: it can only signal processes this
  agent itself started.

Semantics: permission checks are identical to foreground bash (the command
string is what's judged, not where it runs); `timeout_ms` is ignored in
background mode (cc drops the timer too); interrupting a turn never touches
background shells — only `kill_bash`, a 1 GiB output-file watchdog, and
session exit (process-group kill on registry drop) reap them. IDs are
process-global (`bg-1`, `bg-2`, …) so sub-agents and server threads sharing
one offload directory never collide. cc's auto-backgrounding, completion
notifications, stall detection and Monitor tool are not ported.

## Web tools (Phase 2, eleventh slice)

`web_fetch` and `web_search`, implemented in the `kloop-web` crate and glued
into core's `ToolSource` seam by the CLI (exactly like MCP servers — core
stays network-free, reqwest lives only in provider and web):

- **web_fetch** `{url}` — HTTP upgraded to HTTPS, embedded credentials and
  2000-char URLs rejected, SSRF guard (loopback/private/link-local/CGNAT/
  metadata ranges refused, DNS names resolved and checked), same-host
  redirects followed (max 5), cross-host redirects reported back for an
  explicit re-fetch (cc shape — kills open-redirect laundering), 5MB
  download cap, HTML→text (hand-rolled: scripts/styles/comments dropped,
  block tags to newlines, entities decoded), 50k-char text cap.
- **web_search** `{query, max_results}` — pluggable `SearchBackend` trait
  with two backends: Tavily (default, `TAVILY_API_KEY`; its free tier needs
  no card) and Brave (`BRAVE_API_KEY`). Without the selected backend's key
  the tool is not registered and startup warns. `[web] search_provider`
  in `.kloop/config.toml` selects the backend; adding a provider = one
  trait impl + one match arm.

Both are read-only for concurrency; the permission gate treats them like
any external tool (ask by default, `web_fetch`/`web_search` allow rules or
session approvals apply). Not ported from cc: the `prompt` parameter with
small-model post-processing, the 15-minute fetch cache, turndown-style
HTML→Markdown, and the preapproved-domain list.

## Parallel sub-agents (Phase 2, twelfth slice)

The `task` tool dispatches concurrently (cc shape): `is_concurrency_safe`
marks `task` unconditionally safe, so consecutive task calls in one response
run as parallel sub-agents inside the ordinary concurrent batch. Results stay
paired to their `tool_use_id`s in request order; one sub-agent failing (bad
input, error, panic) becomes its own `is_error` tool_result without sinking
the batch. Sub-agents' own writes are still gated individually — hooks and
the shared permission gate see every inner call, and concurrent approval
prompts serialize (the TUI already queues; the plain REPL takes a mutex so
one prompt owns the terminal at a time).

Every spawn gets a process-global label (`agent-1`, `agent-2`, …) stamped on
its cloned Config, and the `Ui` trait carries it end to end:
`tool_start`/`tool_end` take an `agent` parameter ("" = main agent), and
`agent_start`/`agent_end` bracket the sub-agent's life. Presentation per
frontend:

- **TUI**: one live row per sub-agent (`… agent-1 <task> — 3 tools · bash
  {...}`) that folds its tool calls into a counter plus a latest-call
  preview — parallel agents never interleave rows — and collapses to
  `✓ agent-1 <task> (3 tool uses)` when it ends. A row still Running when
  the turn dies (interrupt drops the task future) is patched to failed.
- **plain**: dim notes, `agent-1 · bash {...}` per call.
- **server**: `agent/started` / `agent/completed` notifications; sub-agent
  `tool/started`/`tool/completed` carry an `"agent"` field (main-agent
  calls keep the old shape exactly).

### Custom agent types

A `task` call can target a named specialized agent via the `agent_type`
parameter. Types are defined in `.kloop/config.toml`:

```toml
[agents.researcher]
description = "Read-only code searcher — locates things in the repo."
system = "You are a code search sub-agent. Use grep/glob, read files, report concisely."
model = "claude-haiku-4-5"      # optional; omitted inherits the parent's model
tools = ["grep", "glob", "read_file"]   # optional; omitted inherits the full set
```

Each field overrides the sub-agent's Config (cc's `.claude/agents`
semantics): `system` **replaces** the system prompt (not concatenated),
`model` swaps the model (the point of a cheap searcher), and `tools` is an
exact allowlist — the sub-agent's tool defs are filtered to it and a call to
anything outside is rejected at dispatch (`read_offloaded` always stays
available, so a restricted agent can still read back a truncated result).
Only `description` is required; it is shown to the model in the task tool's
description so it can pick a type, and an unknown `agent_type` is an
is_error result naming the available ones. Sub-agents still can't spawn
further sub-agents, and they share the parent's permission gate — the human's
last word doesn't loosen inside a sub-agent. `--mock` reads no config, so it
sees no types.

Not yet (deliberate): sub-agent history persistence, async dispatch with a
completion mailbox (codex spawn/wait shape), hook events tagged with the
agent, and per-type effort/max-turns — see `docs/plan/17-subagents.md`.

## OS sandbox (Phase 2, thirteenth slice)

On macOS, bash commands run inside a seatbelt sandbox by default
(`crates/core/src/sandbox/`, executed via `/usr/bin/sandbox-exec` with a
deny-by-default SBPL profile — the shape cc and codex converged on):

- **Writes** are allow-listed: cwd + `/tmp` + `$TMPDIR` + configured extras.
  Inside a writable root, `.git/hooks`, `.git/config` and `.kloop` stay
  read-only (they are privilege-escalation surfaces — hooks and git config
  run code, `.kloop` holds the permission rules; the rest of `.git` stays
  writable so `git commit` works sandboxed).
- **Reads** are full-disk; **network** is off unless configured.
- **Sandboxed = fewer questions** (`auto_allow`, default on): a bash call
  the sandbox will contain skips the asking layers of the permission gate —
  opaque scripts (redirects, subshells) included, since OS containment
  replaces parse-level vetting. Deny rules, safety checks (a visible
  `rm -rf` still confirms) and explicit ask rules stay in force above it.
  The accepted trade-off: a contained command can still modify the workspace
  without a prompt — protected `.git` internals aside, git history is the
  recovery path. `auto_allow = false` reverts to pure containment (approve
  first, then run sandboxed). `--yolo` bypasses approvals but not the
  sandbox.

When a sandboxed command fails and the failure looks like a sandbox denial
(keyword match ported from codex, plus DNS-failure shapes when the sandbox
disables network), there are two escalation paths:

- **Code-level loop** (`escalate`, default on — codex's retry-on-denial):
  the tool asks once ("run this without the sandbox?") and, on approval,
  re-runs the command unsandboxed within the same tool call — one fewer
  model round-trip. `--yolo` auto-approves it; declining keeps the failure
  and steers the model to a different approach. This is the default.
- **Model-driven** (fallback when `escalate = false`, or when there is no
  approver): the result is annotated with guidance and the model retries
  that one call with `disable_sandbox: true`, which faces the permission
  gate like any call and is tagged `[no sandbox]` in the approval prompt.

Either way, escalation is per call — the next command is sandboxed again —
and the command already cleared the permission gate (deny rules and safety
checks sit above the sandbox), so escalation asks only about removing
containment.

```toml
[sandbox]
enabled = true            # default; false turns the sandbox off
allow_network = false     # default; true appends the network allow rules
writable_roots = []       # extra writable directories
auto_allow = true         # default; false = ask first, then run sandboxed
escalate = true           # default; false = model-driven disable_sandbox instead
```

`AGENT_SANDBOX=off` is the env escape hatch. Where sandboxing is unavailable
(Linux/Windows for now — planned as future slices behind the same seam; or a
missing `sandbox-exec`), kloop warns at startup and runs commands bare:
fail-open, because the permission gate remains the enforcement layer.
Sandboxed processes see `KLOOP_SANDBOX=seatbelt` (and
`KLOOP_SANDBOX_NETWORK_DISABLED=1`) as detection hints. `--mock` never
sandboxes.

## Todo list (Phase 2, fourteenth slice)

`todo_write` (`core/src/tools/todo.rs`) is cc's TodoWrite: a single tool the
model uses to keep a structured task list, so multi-step work stays coherent
and its progress is visible. It is **full-table replacement** — the model
sends the entire list every call (each item is `{content, activeForm,
status}`, `status ∈ pending | in_progress | completed`, no ids), so there is
no incremental state to drift. Validation is minimal (non-empty list,
non-empty content/activeForm, valid status); multiple `in_progress` items are
allowed (cc's one-at-a-time is guidance carried in the tool description, not a
hard rule).

The list is **session-scoped process state on the `Config`, not history**: it
survives across turns within a session and starts empty on resume — the model
rebuilds it from its own `todo_write` calls replayed in history (the TUI
replays each historical call as its checklist too). Each **sub-agent gets its
own fresh list** (the `task` tool resets it on the cloned Config), so a
sub-agent's planning never touches the parent's. It has no external side
effect, so the permission gate auto-allows it (read-only self-verdict); it
runs serially (full-table replace has ordering).

Rendering: `todo_write` never shows as a generic tool row — it renders as a
checklist. The TUI keeps one `Cell::Todo` per turn, updated in place as the
list evolves (a new user turn starts a fresh block); the plain REPL prints
the marked list; server mode emits a `todo/updated` notification with the
full list (a sub-agent's carries an `agent` field, like tool notifications).
A sub-agent's list stays internal to the TUI transcript (lesson 3), the way
its text does. Not done (deliberate): dependency graphs, cross-session todo
stores, rollout persistence of the list.

## Steering — mid-turn injection (Phase 2, fifteenth slice)

Type a message while a turn is running and it **steers** instead of being
dropped: it queues, and is delivered to the model as a user message at the
next round boundary — never spliced into an in-flight request. It does not
interrupt the current tools (Ctrl+C stays the hard stop). This is a general
**step-boundary injection queue** (`Config.inbox`, a `Vec<String>` behind a
mutex); the current consumer is user steering, and sub-agent completion
delivery (the mailbox path) plugs into the same queue once async dispatch
exists.

The mechanism is a straight drain of `Config.inbox` at round boundaries in the
agent loop (`core/src/agent.rs`): at the **top of each round** (delivering
steers typed during the previous round's tool execution before the next
sampling), and again in an **end guard** — when the model returns no tool
calls, a steer that landed during that final sampling is absorbed and the turn
continues instead of ending, so a late "wait, also do X" is answered rather
than lost. Each injected message is framed (`The user sent this message while
you were working…`) so the model treats it as a mid-work interjection to fold
in, not a brand-new task. It is recorded to history (and rollout) as a normal
user message, so it survives compaction and replays on resume; because it is
recorded right after the round's `tool_result` blocks (a separate user
message), it never interleaves tool results with regular text — the ordering
constraint both cc and codex call out.

Each **sub-agent gets its own fresh queue** (the `task` tool resets it on the
cloned Config, like the todo list), so a running sub-agent never drains the
parent's steering. TUI enqueues on Enter-while-running (the raw text shows as
a User cell); the plain REPL (blocking stdin) and server (`turn/steer`) do not
enqueue yet — the drain path is live for all three, only the enqueue side is
TUI-only for now. This is the cc/codex convergence: steering is
enqueue-not-interrupt, delivered only between steps (see `refs/README.md`).

## Slash commands (Phase 2, sixteenth slice)

An input line starting with `/` is a **built-in command**, not a message to
the model. The set is small and lives one-file-per-command under
`core/src/commands/` (the directory listing *is* the catalog):

- `/help` — list the commands.
- `/cost` — the current model and context-window usage (`~used / window
  tokens (pct%)`, from the same usage anchor + char/4 estimate compaction
  uses; a cumulative token/dollar total would need per-response accounting the
  history does not yet keep).
- `/compact` — summarize and shrink the conversation now, instead of waiting
  for the predictive/reactive triggers.
- `/clear` — empty the conversation and start fresh (cc/claw semantics: an
  append-only compacted-to-nothing marker that resume replays to empty; it
  does **not** fork a new session file). Process-state (todos, steering queue)
  resets too.

An unknown `/name` lists the available commands (the same discoverable shape
as an unknown `agent_type`). Commands run **only when idle** — they read or
rewrite History, which a running turn is otherwise using; while a turn runs, a
`/`-line is just steering text. The plain REPL runs them inline (it owns
History); the TUI routes them to its worker (which owns History) so a slow
`/compact` shows as busy and Ctrl+C interrupts it like a turn. Only cc has
this feature among the three references (claw has built-in slash but no user
templates; the codex checkout has neither) — the shape follows cc.
**Not done** (deferred to their own plan): user-defined `.kloop/commands/*.md`
prompt templates with `$ARGUMENTS`/`$N` substitution — they plug into this same
seam (a `custom.rs` sibling); server-mode slash; `!bash`/`@file` injection.

## Code mode (Phase 2, seventeenth slice)

The `exec` tool (`core/src/tools/codemode.rs`) is CodeAct: instead of one
`tool_use` per step, the model writes **a JavaScript program** that orchestrates
tools and sub-agents — loops, fan-out, pipelines and filters expressed in code.
Intermediate results stay in program variables; only what the program `return`s
(plus any `log(...)`) comes back, so a 100-item loop is one tool_result instead
of 100. This is the industry's "code mode" pattern (Cloudflare coined it; codex
uses in-process V8; cc's dynamic workflows use Node; Anthropic's
code-execution-with-MCP is the token argument).

The engine is **QuickJS** via `rquickjs`, in the `kloop-codemode` crate. QuickJS
over V8 is deliberate: V8 is ~40MB linked plus a sidecar to move it out of the
binary (codex's shape), against kloop's minimalism; QuickJS is a few hundred KB,
gives real isolation (no fs/network/console/module import — a program's only
reach outside is the tools), and — unlike V8, which keeps process-global engine
state — a fresh runtime is built and fully dropped per program. rquickjs's async
runtime maps Rust futures to JS promises natively, so `await tools.x()` and
`Promise.all` work without the manual promise-resolver plumbing bare V8 forces.

The program calls:

```ts
declare const tools: { read_file(args: { path: string; … }): Promise<string>; … };
declare function agent(prompt: string, opts?: { agent_type?; max_rounds? }): Promise<string>;
declare function log(msg: unknown): void;
declare function parallel<T>(thunks: Array<() => Promise<T>>): Promise<Array<T | null>>;
```

The `tools` API and its TypeScript declarations are generated from the built-in
tool schemas and carried in `exec`'s description (typed declarations markedly
improve how reliably models call tools — the references converge on this).

**The safety story is that every `tools.<name>(...)` and `agent(...)` re-enters
the exact same gated dispatch a direct call takes** — `run_one` (allowlist →
deferred lock → hooks → permission gate → sandbox → execute) and `task_tool`. A
denied tool is refused *inside* the program (the model catches the exception); a
sandboxed command is still sandboxed. The `kloop-codemode` crate is engine-only
and knows nothing of permissions; it calls back through a `HostBridge` trait,
which `core/src/tools/codemode.rs` implements over the gate — that inversion is
why `core` can depend on the engine crate without a cycle. `exec` itself is
auto-allowed (like `task`): it touches nothing directly. `Promise.all` maps to
the same concurrency rule as a normal round (read-only calls batch, writes take
an exclusive lock). Resource limits: per-program QuickJS heap cap and stack cap,
and an interrupt handler that kills a runaway synchronous loop (a CPU-burst
deadline that ignores await-suspended time) or a user Ctrl+C.

**Not done** (deferred): exposing MCP tools to programs (built-ins only for
now); a `pipeline()` primitive and token `budget`; UI progress observation (a
`/workflows` equivalent); background programs with `yield`/`wait`; saving a
program for reuse with journal-based resume. See `docs/plan/24-code-mode.md`.

## Running

```sh
# keyless demo: scripted Mock provider exercises all five bets (plain output)
cargo run -- --mock

# Anthropic (default model claude-sonnet-5; override with AGENT_MODEL)
ANTHROPIC_API_KEY=... cargo run
# prompt caching is on by default (cache_control breakpoints on the last
# tool, the system block, and the last message block — the tool set outlives
# the volatile system prompt, so a restart still reads the tools prefix);
# AGENT_CACHE=off disables it for diagnosing cache behavior
# thinking blocks stream dim in the UI and are replayed verbatim (signature
# included). No AGENT_THINKING = no thinking field sent (current models then
# run adaptive on their own); AGENT_THINKING=off|adaptive|<budget tokens>
# forces a mode (the budget form is for pre-adaptive models and raises
# max_tokens by the budget)

# any OpenAI-compatible endpoint (AGENT_MODEL required)
OPENAI_API_KEY=... AGENT_MODEL=gpt-5.2 cargo run
# OPENAI_BASE_URL defaults to https://api.openai.com/v1
# AGENT_PROVIDER=anthropic|openai|openai-responses forces a provider when
# both keys are set; openai-responses speaks the /responses wire (stateless
# store:false, reasoning replayed via encrypted_content) with the same
# OPENAI_* variables. AGENT_EFFORT=minimal|low|medium|high sends the
# reasoning request field (summary=auto) — some backends emit no reasoning
# items at all without it, so this is also the reasoning-capture switch

# line-based REPL instead of the TUI
cargo run -- --plain

# multi-session JSON-RPC server on stdio (see Server mode)
cargo run -- --serve
cargo run -- --mock --serve   # keyless: scripted provider behind the protocol

# sessions
cargo run -- --list-sessions   # what's on disk, most recent first
cargo run -- --continue        # continue the most recent session
cargo run -- --resume          # pick a session from a numbered list
cargo run -- --resume <id>     # continue a specific session
cargo run -- --fork <id>#<seq> # branch off a session at line #<seq> (rewind)
cargo run -- --fork <id>       # branch off a session at its end

# MCP servers come from .kloop/config.toml — see MCP client above
# AGENT_DEFER_THRESHOLD=<n> tunes when MCP tool defs defer behind tool_search
# (default 30 total tools; lower it to exercise deferral with a small server,
# raise it to effectively disable)

# permissions (rules also live in .kloop/config.toml — see Permissions)
AGENT_ALLOW='write_file,bash(cargo *)' cargo run   # pre-approve rules
AGENT_DENY='bash(git push *)' cargo run            # hard-block rules
cargo run -- --accept-edits                        # auto-allow cwd file writes
cargo run -- --yolo                                # bypass (deny/safety still apply)

# OS sandbox (macOS seatbelt; see OS sandbox above)
AGENT_SANDBOX=off cargo run                        # run bash commands bare
```

Both frontends: Ctrl+C interrupts the running turn (history is patched and
stays legal), Ctrl+D quits (`exit` also works in `--plain`). Every session is
saved and resumable — see Session persistence above.

## Verification

`cargo test` runs 353 tests across the workspace:

- **kloop-protocol** — wire-format contract (exact JSON shapes, `is_error`
  omission rule, role casing, serde round-trip).
- **kloop-provider** — history-translation unit tests plus wiremock HTTP
  contract tests for both adapters: scripted SSE event sequences in,
  `StreamEvent` sequences asserted out — delta accumulation, usage capture,
  overflow-error mapping, malformed-input fallbacks, mid-stream death.
- **kloop-core** — every tool's execute path (output/exit capture, timeout
  kill, line numbering, parent-dir creation, edit ambiguity, offload id
  validation, depth guard), dispatch ordering + orphan patching +
  mid-execution cancel, history offload + usage-anchor math, compaction
  rebuild/failure-leaves-history-untouched/boundary pairing, and agent-loop
  end-to-end over the Mock provider: tool batching, predictive + reactive
  compaction, truncation continuation, retry/fallback, max-rounds,
  pre-cancelled abort, sub-agent round-trip, denied-tool-continues-turn;
  custom agent types (type lookup + unknown-type error listing, tool
  allowlist gating with the read_offloaded exception, agent_type routing the
  sub-agent's system/model/filtered tools through the recorded request,
  dispatch rejecting a tool outside the allowlist, `[agents.<name>]` parsing
  with malformed-field rejection); shell analysis contracts (word-only
  parsing, quote/concatenation
  unwrapping, opaque-construct rejection, `bash -c` unwrap, read-only option
  vetting, git option-injection, dangerous-through-wrappers); permission
  pipeline (deny-beats-allow-and-bypass, wrapper-stripped deny, bypass-immune
  safety checks, sensitive paths never cached, ask-rules-over-allow,
  acceptEdits cwd boundary, glob rules, two-word session cache, AllowAlways
  persistence, opaque never cacheable, `ConfirmRequest.preview` carrying an
  edit/write diff while other calls carry none); change-preview generation
  (line-numbered added/deleted/changed lines, distant-hunk splitting,
  long-line clipping, big-diff capping, write-file existing-vs-new-file
  previews, edit_file reading the file and applying the edit vs the
  two-string fallback); hook execution (all four points fire
  in order around a real turn, exit-2 pre_tool block becomes an is_error
  tool_result and the command never runs, blocked pre_turn prevents sampling,
  block reason stderr>stdout>status, non-2 exits fail open even on pre
  events, stdout-injection shape, stdin event JSON contract,
  timeout/spawn-failure fail open with warnings, matcher filtering, block
  short-circuits later hooks, exit 2 on post events only warns); rollout
  round-trip, envelope
  chain (ids link across restarts, no collisions), compacted marker replay,
  two-way pairing repair on resume, torn-tail physical truncation,
  unknown-field forward compatibility, offload counter sync, and a full
  persist → restart → resume turn over the Mock provider; fork contracts
  (prefix copy with cross-file lineage and preserved timestamps, branches
  append independently, illegal cuts rejected with nearby legal points,
  cuts before/at a compacted marker replay each side, fork-of-a-fork,
  branches share the offload dir without clobbering); sandbox contracts
  (exact SBPL profile assembly and `-D` param list, workspace-root
  computation with canonical/literal dedup, denial-detection table incl. the
  network-off DNS extension, `[no sandbox]` approval tag, auto-allow
  layering: contained calls skip asking while deny/safety/ask-rules
  outrank the sandbox; escalation-consent decision/mode mapping) plus
  macOS-only integration against the real `sandbox-exec` (write
  inside/outside a writable root with the denial hint, protected subpath,
  per-call disable_sandbox escape, network denied where a bare run
  connects, background shell sandboxed with inherited-fd output, contained
  bash running with no approver present, the escalation loop re-running a
  denied command unsandboxed on approval and warning off retry when
  declined).
- **kloop-tui** — Ui/Approver→channel event contract (call order, confirm
  decision round-trip, dropped-reply-means-deny), App state folding (delta
  accumulation and splitting, tool status resolution by id, confirm queueing
  and keyboard capture, interrupt/quit commands, turn-end cleanup), and pure
  rendering (CJK-aware wrap/truncate, per-cell-kind lines, tool-row collapse,
  input window around the cursor, diff-preview coloring by +/- sign).
- **kloop-server** — wire envelope contract (request/response/notification
  shapes, string-or-int ids, request-vs-approval-response disambiguation),
  plus duplex-driven protocol tests against the real serve loop with a
  scripted provider: delta streaming and completion, approval deny/allow
  round-trips (file provably not/created, the change `preview` reaching the
  client), parallel threads with no event
  cross-tagging and no same-second id collisions, busy-thread rejection,
  interrupt-while-pending-approval, protocol-error resilience, and sessions
  surviving a server restart (list/resume/re-run over the same files).
- **kloop-mcp** — wire-contract tests against an in-process mock MCP server
  on a duplex pipe: exact handshake JSON (initialize params + the id-less
  initialized notification), tools/list cursor pagination with whole-object
  schema passthrough, tools/call round-trip with content-block rendering,
  isError→Err and JSON-RPC-error→Err mapping, EOF fails pending requests,
  server-initiated requests refused with -32601 amid noise, concurrent
  calls routed by id.
- **kloop-codemode** — the QuickJS engine in isolation: the async op bridge
  (a tool call returns a JS promise resolved from a Rust future), real
  concurrency proven with a 2-party barrier that a serial engine would
  deadlock, the `parallel` helper turning failures into null, sandboxing
  (no fetch/require/process/console, import rejected), result coercion,
  program-error surfacing, and each resource limit (runaway-loop kill,
  cancellation, memory cap). Its core wiring (`tools::codemode`) tests the
  op layer re-entering the real permission gate (a denied tool refused
  inside the program while a read-only one passes), `agent()` spawning a
  real sub-agent, intermediate results staying off the result, and the
  TypeScript-API generation; an agent-level test drives one `exec` tool_use
  over Mock and asserts the next request carries only the program's return
  value, never the content it read internally.
- **kloop (cli)** — argument parsing, UTC timestamp session ids (epoch,
  known dates, leap day), permission-config round-trip (load/persist/merge,
  unrelated-section preservation, malformed rejection), `[mcp.servers]`
  parsing (round-trip, malformed rejection), rule-safe name sanitization,
  and `[[hooks]]` parsing (round-trip, defaults, malformed rejection).

Beyond the suite: `cargo run -p kloop -- --mock` (six scripted rounds
exercising all five bets), and with a real key both adapters have been
exercised live including offload round-trips, mid-session predictive
compaction, and truncation recovery.

CI (`.github/workflows/ci.yml`, at the repo root) enforces the same gate on
every push/PR: `cargo fmt --check`, `cargo clippy --workspace --all-targets
-- -D warnings`, `cargo test --workspace`, on macOS and Linux.

## Layout

Cargo workspace, eight crates; the dependency graph is a strict line up to
core, then two sibling frontends under the cli, with the MCP wire client as
a protocol-only sibling glued in by the cli and the QuickJS code-mode engine
as a leaf core depends on
(protocol ← provider ← core ← {tui, server} ← cli; protocol ← mcp ← cli;
codemode ← core):

```
crates/protocol/    kloop-protocol — zero-dependency leaf
  src/lib.rs        canonical wire types (Anthropic Messages shape),
                    StreamEvent, Usage, OverflowError, ToolDef

crates/provider/    kloop-provider — the adapter seam; owns reqwest
  src/lib.rs        Provider enum + stream() dispatch; Mock with scripted turns
  src/anthropic.rs  Anthropic native SSE adapter
  src/openai.rs     OpenAI-compat chat/completions translation
  src/sse.rs        incremental SSE parser

crates/core/        kloop-core — the agent, network-free
  src/config.rs     Config (construction is the caller's concern)
  src/history.rs    append-only history, record-time offloading,
                    usage-anchored token estimation
  src/tools/        the tool seam and the built-in tools
    mod.rs          tool defs, concurrency-safety classification, batched
                    dispatch with hook+permission gating; ToolSource seam
                    for external (MCP) tools
    bash.rs         foreground + background shell execution, the
                    BackgroundShells registry, bash_output/kill_bash
    fs.rs           read/write/edit file, read_offloaded
    search.rs       grep/glob on the ripgrep crate family (gitignore-aware
                    walking, output modes, paging, clipping)
    task.rs         sub-agent spawning
    codemode.rs     the exec tool: CoreBridge (re-enters the gate per op),
                    TypeScript API generation; engine is the codemode crate
  src/shell.rs      tree-sitter-bash word-only analysis, read-only and
                    dangerous classifiers, wrapper stripping
  src/permissions.rs the layered execution gate: deny/ask/allow rules,
                    safety checks, modes, session cache, Approver seam
  src/diff.rs       write/edit change previews for the approval prompt
  src/compact.rs    predictive threshold math + compaction rewrite
  src/context.rs    pure prompt assembly: system + env block + git snapshot,
                    instruction-file concatenation under a byte budget
  src/rollout.rs    session persistence: JSONL append, compacted markers,
                    replay + orphan repair on resume
  src/agent.rs      run_turn loop, retry/fallback/truncation recovery, Ui

crates/tui/         kloop-tui — the ratatui frontend; owns the terminal
  src/events.rs     AgentEvent + ChannelUi (Ui/Approver over channels)
  src/app.rs        pure state: transcript cells, input, confirm queue
  src/render.rs     pure cell→line rendering, wrap/truncate, confirm popup
  src/lib.rs        terminal lifecycle, agent worker task, event loop

crates/server/      kloop-server — multi-session JSON-RPC frontend
  src/wire.rs       envelopes (request/response/notification/server request)
  src/lib.rs        serve loop, per-thread workers, approval routing

crates/mcp/         kloop-mcp — MCP stdio wire client (depends on protocol only)
  src/lib.rs        newline-delimited JSON-RPC over child stdio: handshake,
                    tools/list pagination, tools/call, content rendering

crates/web/         kloop-web — web_fetch/web_search (owns reqwest with provider)
  src/fetch.rs      SSRF guard, redirect policy, caps, body handling
  src/html.rs       minimal HTML→text (no extra dependencies)
  src/search.rs     SearchBackend trait + Tavily/Brave implementations

crates/codemode/    kloop-codemode — the QuickJS engine for code mode (owns rquickjs)
  src/lib.rs        run_program: isolated async runtime, HostBridge seam,
                    the tools/agent/log/parallel prelude, resource limits

crates/cli/         kloop — the binary
  src/main.rs       arg parsing + dispatch (TUI default, --plain REPL,
                    --serve), env config, StdoutUi, CliApprover (y/a/p/n
                    prompt), .kloop/config.toml rule load/persist, --mock
  src/web.rs        [web] config + ToolSource adapter over kloop-web
                    demo, session selection (--continue, --resume,
                    --list-sessions)
  src/mcp.rs        [mcp.servers] config, startup connection with
                    degrade-to-warning, {server}__{tool} namespacing,
                    the ToolSource adapter
  src/context.rs    project-context IO: AGENTS.md/CLAUDE.md discovery
                    (global + git root→cwd), env info, git snapshot commands
```
