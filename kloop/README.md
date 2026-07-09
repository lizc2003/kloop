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

## Deliberately out of scope (Phase 2 remainder)

MCP, hooks.

## Running

```sh
# keyless demo: scripted Mock provider exercises all five bets (plain output)
cargo run -- --mock

# Anthropic (default model claude-sonnet-5; override with AGENT_MODEL)
ANTHROPIC_API_KEY=... cargo run

# any OpenAI-compatible endpoint (AGENT_MODEL required)
OPENAI_API_KEY=... AGENT_MODEL=gpt-5.2 cargo run
# OPENAI_BASE_URL defaults to https://api.openai.com/v1
# AGENT_PROVIDER=anthropic|openai forces a provider when both keys are set

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

`cargo test` runs 126 tests across the workspace:

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
  persistence, opaque never cacheable); rollout round-trip, envelope
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
- **kloop (cli)** — argument parsing, UTC timestamp session ids (epoch,
  known dates, leap day), permission-config round-trip (load/persist/merge,
  unrelated-section preservation, malformed rejection).

Beyond the suite: `cargo run -p kloop -- --mock` (six scripted rounds
exercising all five bets), and with a real key both adapters have been
exercised live including offload round-trips, mid-session predictive
compaction, and truncation recovery.

CI (`.github/workflows/ci.yml`, at the repo root) enforces the same gate on
every push/PR: `cargo fmt --check`, `cargo clippy --workspace --all-targets
-- -D warnings`, `cargo test --workspace`, on macOS and Linux.

## Layout

Cargo workspace, six crates; the dependency graph is a strict line up to
core, then two sibling frontends under the cli
(protocol ← provider ← core ← {tui, server} ← cli):

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
  src/tools.rs      bash, read/write/edit file, read_offloaded, task;
                    concurrency-safety classification + batched dispatch
  src/shell.rs      tree-sitter-bash word-only analysis, read-only and
                    dangerous classifiers, wrapper stripping
  src/permissions.rs the layered execution gate: deny/ask/allow rules,
                    safety checks, modes, session cache, Approver seam
  src/compact.rs    predictive threshold math + compaction rewrite
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

crates/cli/         kloop — the binary
  src/main.rs       arg parsing + dispatch (TUI default, --plain REPL,
                    --serve), env config, StdoutUi, CliApprover (y/a/p/n
                    prompt), .kloop/config.toml rule load/persist, --mock
                    demo, session selection (--continue, --resume,
                    --list-sessions)
```
