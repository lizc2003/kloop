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

## Deliberately out of scope (Phase 2 remainder)

TUI, MCP, hooks, permission system.

## Running

```sh
# keyless demo: scripted Mock provider exercises all five bets
cargo run -- --mock

# Anthropic (default model claude-sonnet-5; override with AGENT_MODEL)
ANTHROPIC_API_KEY=... cargo run

# any OpenAI-compatible endpoint (AGENT_MODEL required)
OPENAI_API_KEY=... AGENT_MODEL=gpt-5.2 cargo run
# OPENAI_BASE_URL defaults to https://api.openai.com/v1
# AGENT_PROVIDER=anthropic|openai forces a provider when both keys are set

# sessions
cargo run -- --list-sessions   # what's on disk, most recent first
cargo run -- --resume          # continue the most recent session
cargo run -- --resume <id>     # continue a specific session
```

REPL: type a task; Ctrl+C interrupts the running turn (history is patched and
stays legal); `exit` or Ctrl+D quits. Every session is saved and resumable —
see Session persistence above.

## Verification

`cargo test` runs 71 tests across the workspace:

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
  pre-cancelled abort, sub-agent round-trip; rollout round-trip, envelope
  chain (ids link across restarts, no collisions), compacted marker replay,
  two-way pairing repair on resume, torn-tail physical truncation,
  unknown-field forward compatibility, offload counter sync, and a full
  persist → restart → resume turn over the Mock provider.
- **kloop (cli)** — argument parsing, UTC timestamp session ids (epoch,
  known dates, leap day).

Beyond the suite: `cargo run -p kloop -- --mock` (six scripted rounds
exercising all five bets), and with a real key both adapters have been
exercised live including offload round-trips, mid-session predictive
compaction, and truncation recovery.

CI (`.github/workflows/ci.yml`, at the repo root) enforces the same gate on
every push/PR: `cargo fmt --check`, `cargo clippy --workspace --all-targets
-- -D warnings`, `cargo test --workspace`, on macOS and Linux.

## Layout

Cargo workspace, four crates in a strict dependency line
(protocol ← provider ← core ← cli):

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
  src/compact.rs    predictive threshold math + compaction rewrite
  src/rollout.rs    session persistence: JSONL append, compacted markers,
                    replay + orphan repair on resume
  src/agent.rs      run_turn loop, retry/fallback/truncation recovery, Ui

crates/cli/         kloop — the binary
  src/main.rs       REPL, env config, StdoutUi, --mock demo,
                    session selection (--resume, --list-sessions)
```
