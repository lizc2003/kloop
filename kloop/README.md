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

## Deliberately out of scope (Phase 2)

Compaction, TUI, MCP, hooks, permission system, multi-session persistence.

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

```
src/types.rs     canonical wire types (Anthropic Messages shape) + StreamEvent
src/sse.rs       incremental SSE parser
src/provider.rs  Anthropic / OpenAI-compat / Mock adapters behind one seam
src/history.rs   append-only history with record-time offloading
src/tools.rs     bash, read/write/edit file, read_offloaded, task (sub-agent);
                 concurrency-safety classification + batched dispatch
src/agent.rs     run_turn loop, sampling with retry, Ui trait
src/main.rs      REPL, Config::from_env, --mock demo script
```
