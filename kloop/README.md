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

## Deliberately out of scope (Phase 2 remainder)

TUI, MCP, hooks, permission system, multi-session persistence.

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
```

REPL: type a task; Ctrl+C interrupts the running turn (history is patched and
stays legal); `exit` or Ctrl+D quits.

## Verification checklist

- `cargo test` — SSE parser, offload spill + pointer, concurrency-safety
  classification, cancelled-dispatch orphan patching, and a Mock end-to-end
  turn asserting the exact history shape (concurrent batch → sequential call →
  final text).
- `cargo run -- --mock` — six scripted rounds: concurrent read-only batch,
  oversized output offloaded, `read_offloaded` round-trip, sub-agent spawn,
  final text.
- With a real key: ask for something requiring a few commands; confirm
  streaming text, tool batching notes on stderr, and Ctrl+C interruption.

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
  src/agent.rs      run_turn loop, retry/fallback/truncation recovery, Ui

crates/cli/         kloop — the binary
  src/main.rs       REPL, env config, StdoutUi, --mock demo
```
