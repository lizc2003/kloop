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
(unix ms). Replay is linear today; the chain is the schema foundation for
rewind/forking later, laid down now because adding it after files exist would
mean a format migration. Unknown fields are ignored on read (locked by test),
so the format grows additively.

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

## Permissions (Phase 2, third slice)

Every tool call passes a layered gate before executing
(`crates/core/src/permissions.rs`), shaped after claude-code's permission
pipeline, with the bash analysis ported from codex's `shell-command` crate:

```
deny rules → safety checks → ask rules → bypass → read-only self-verdict
→ acceptEdits → allow rules → session cache → ask the user
```

Two invariants carried over from claude-code: **deny always beats allow**,
and **safety checks are immune to bypass mode**.

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

Keys: Enter sends (ignored while a turn runs — no queueing), Ctrl+C
interrupts the running turn or clears the input when idle, Ctrl+D quits,
Up/Down/PageUp/PageDown scroll the transcript (view pins back to bottom on
send). `--plain` keeps the old line-based REPL.

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
← {"id":"srv-1","method":"approval/request","params":{"threadId":"…","description":"write_file: s2.txt","rememberRules":["write_file(*)"]}}
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
allow rules round-trip through the permission-rule grammar); collisions and
oversized tool lists (> 30) warn at startup, colliding later definitions are
skipped.

Calls go out with the raw server-side tool name; the result content array is
flattened to text (binary blocks degrade to `[image: …]`-style tags), and
`isError: true` surfaces as an is_error tool_result — same shape as a failing
built-in. MCP tools run serially unless listed in `readonly`, and always ask
for permission unless covered by an allow rule (`memory__create_entities` in
`[permissions].allow`) or the session cache — the `a`/`p` answers work on
whole-tool granularity.

Layering: core only knows the `ToolSource` trait (`tools/mod.rs`); the wire
client is the `kloop-mcp` crate (depends only on protocol); the CLI glues
them (config parsing, namespacing, the adapter). Deferred-tools + tool_search
for oversized tool lists is future work.

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

# MCP servers come from .kloop/config.toml — see MCP client above

# permissions (rules also live in .kloop/config.toml — see Permissions)
AGENT_ALLOW='write_file,bash(cargo *)' cargo run   # pre-approve rules
AGENT_DENY='bash(git push *)' cargo run            # hard-block rules
cargo run -- --accept-edits                        # auto-allow cwd file writes
cargo run -- --yolo                                # bypass (deny/safety still apply)
```

Both frontends: Ctrl+C interrupts the running turn (history is patched and
stays legal), Ctrl+D quits (`exit` also works in `--plain`). Every session is
saved and resumable — see Session persistence above.

## Verification

`cargo test` runs 159 tests across the workspace:

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
  shell analysis contracts (word-only parsing, quote/concatenation
  unwrapping, opaque-construct rejection, `bash -c` unwrap, read-only option
  vetting, git option-injection, dangerous-through-wrappers); permission
  pipeline (deny-beats-allow-and-bypass, wrapper-stripped deny, bypass-immune
  safety checks, sensitive paths never cached, ask-rules-over-allow,
  acceptEdits cwd boundary, glob rules, two-word session cache, AllowAlways
  persistence, opaque never cacheable); hook execution (all four points fire
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
  persist → restart → resume turn over the Mock provider.
- **kloop-tui** — Ui/Approver→channel event contract (call order, confirm
  decision round-trip, dropped-reply-means-deny), App state folding (delta
  accumulation and splitting, tool status resolution by id, confirm queueing
  and keyboard capture, interrupt/quit commands, turn-end cleanup), and pure
  rendering (CJK-aware wrap/truncate, per-cell-kind lines, tool-row collapse,
  input window around the cursor).
- **kloop-server** — wire envelope contract (request/response/notification
  shapes, string-or-int ids, request-vs-approval-response disambiguation),
  plus duplex-driven protocol tests against the real serve loop with a
  scripted provider: delta streaming and completion, approval deny/allow
  round-trips (file provably not/created), parallel threads with no event
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

Cargo workspace, seven crates; the dependency graph is a strict line up to
core, then two sibling frontends under the cli, with the MCP wire client as
a protocol-only sibling glued in by the cli
(protocol ← provider ← core ← {tui, server} ← cli; protocol ← mcp ← cli):

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
  src/shell.rs      tree-sitter-bash word-only analysis, read-only and
                    dangerous classifiers, wrapper stripping
  src/permissions.rs the layered execution gate: deny/ask/allow rules,
                    safety checks, modes, session cache, Approver seam
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
