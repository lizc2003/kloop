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
   runs. The Responses adapter passes gateway/Codex vendor out-of-band
   telemetry (`codex.*`, e.g. `codex.rate_limits`) through as a no-op both
   before and after the terminal — mirroring Anthropic's `ping` — while every
   other unknown SSE event still fails closed. Responses function-call
   arguments are reconciled by JSON value (the proxy may stream compact deltas
   but echo a pretty-printed `.done`), while text and reasoning stay byte-exact;
   genuinely divergent or non-JSON arguments still fail closed.

Provider sampling is bounded and typed: each attempt has one terminal outcome;
open (45s), chunk-idle (15m), wall-clock (30m), response (10 MiB), and SSE
frame (1 MiB) guards apply on all three wires. Transport/open/read failures,
HTTP 408/429/5xx, and incomplete EOF retry up to 3 total attempts only before
any text, reasoning, or complete tool call arrives; `Retry-After` is honored up
to 60s. Stream-level errors on all three wires are surfaced faithfully by their
real `code`/`type` and classified by a small fatal blacklist (auth, permission,
quota, policy, invalid-request, request-too-large, billing); every other stream
error — transient upstream, overload, and rate-limit conditions included —
defaults to retryable, the inverse of the HTTP-status whitelist and gated by the
same no-semantic-output rule. A named Chat `event: error` frame is recognized
and read rather than rejected as an unknown event name. Core retains provider
outcomes and failures inside typed turn errors;
string rendering happens only at CLI/TUI/native protocol 2.0 boundaries.
Cancellation remains a distinct core terminal, aborts the producer task, and
orphan patching still keeps history legal after interruption.

Every provider-produced assistant message also carries private replay
provenance: provider endpoint identity, API family, and exact final wire model.
Anthropic Messages and OpenAI Responses replay reasoning text plus
signature/encrypted/redacted payloads only on an exact match; legacy reasoning
without provenance, a provider/family switch, or a model switch fails closed
before network I/O and cannot silently fall back. OpenAI Chat keeps its existing
intentional boundary and strips reasoning instead of replaying it. Compaction
keeps the recent tail verbatim, while resume and fork preserve this provenance
with the complete message. Public `thread/read` and event snapshots remove the
private provenance, signatures, encrypted content, and redacted blobs; only the
same display reasoning text already emitted by live `reasoning` items remains.

## Claude Code 2.1.220 parity baseline

Plans 48–59 pin comparison to one exact Claude Code 2.1.220 darwin-arm64 binary and publish the auditable corpus under [`../refs/claude-code-2.1.220/`](../refs/claude-code-2.1.220/). The final generated snapshot contains:

- 14 executed local profiles, 218 captures, and 65 two-capture determinism groups;
- 214 static-evidence records;
- 62 matrix rows × 8 dimensions = 496 cells;
- 24 `same`, 141 `compatible`, 169 `intentional-diff`, 5 `missing`, 133 `unknown`, and 24 `n/a` cells;
- 7 executable pair contracts covering all 24 `same` cells;
- 108 generated per-cell exact-bundle profile bridges.

The verifier requires every bundle record to be byte-anchored, static evidence to cover the cited dimension, dimension-specific typed CC runtime witnesses with explicit required-tool sets for composite rows, bilateral evidence for `compatible`/`intentional-diff`, an exact three-ID kloop-only allowlist, matrix-bound profile-bridge fixtures, byte-deterministic native reports, and fresh generated artifacts. It rejects metadata result spoofing, report/matrix/profile-bridge tampering, and unanchored descriptive locators. The default verifier checks the pinned executable and exact bundle bytes; `--corpus-only` retains corpus, matrix, pair, bridge, native Rust report, sensitive-data, and fail-closed checks without reading the target binary.

Plan 59 also runs five core-dispatch cross-tool scenarios: file/search→Bash→Worktree with isolated Git subprocess configuration; exact-count background Bash→Agent→scheduler reinjection and live shutdown; selective ToolSource/resource→local Web seam with observed approval-before-dispatch and denied-call isolation; Ask→Plan→Workflow; and Notebook→file mutation with LSP negative boundary. The report records `target_entrypoint=local-cli` separately from `kloop_entrypoint=core-dispatch-test-harness`, so it does not claim to execute CLI startup wiring. Seam-only, surface-gated, and negative-boundary branches are labeled as such and are not counted as execution of absent MCP transport, Monitor, PTY, or LSP surfaces.

The accepted scope is only exact 2.1.220, darwin-arm64 local CLI, `team=false`, `remote=false`, and the condition vectors recorded in `manifest.json`. The result is **limited behavioral compatibility**, not full-tool parity, wire/schema/UI identity, or a drop-in replacement claim. The five out-of-scope SendMessage `missing` cells, accepted safety differences, condition-bound `unknown`, profile/surface `n/a`, and three kloop-only rows remain explicit.

See the [exact corpus guide](../refs/claude-code-2.1.220/README.md), [methodology and history](../refs/README.md), [Plan 59 final acceptance](../docs/plan/59-tool-parity-acceptance.md), and [capability report](../docs/capability-report.md).

## Platform shell support

| Runtime | Bash-family tool | PowerShell tool | Process-tree ownership | OS filesystem/network sandbox |
|---|---|---|---|---|
| macOS | frozen executable POSIX `sh -lc` | not registered | dedicated process group | Seatbelt for Bash by default |
| Linux | frozen executable POSIX `sh -lc`; WSL uses an executable `/bin/bash -lc` | not registered | dedicated process group | not implemented |
| Native Windows | validated Git for Windows `bin\bash.exe -lc`, registered only when available | highest trusted PowerShell 7 MSI/MSIX `pwsh.exe`, falling back to Windows PowerShell 5.1; foreground-only | kill-on-close root Job established before user code; every Bash/PowerShell spawn also debug-gates descendants into a fixed containment Job set | not implemented |

Shell executables are resolved once at startup and inherited unchanged by server
threads, sub-agents, worktrees, and code mode. Unix discovery skips non-executable
`sh` candidates and validates the final regular-file executable, including the
fallback and a symlink's target. On Windows, Job containment is mandatory even
though restricted-token/AppContainer filesystem and network sandboxing are not
implemented; no setting or per-call field disables the Job. The debug gate is a
process-tree ownership boundary, not a defense against protected processes or a
child that installs its own debugger. The pinned Claude Code parity target is
darwin-arm64, so this native Windows surface is a kloop contract, not a new
`same`/`compatible` claim for the pinned PowerShell matrix row.

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

Compaction itself uses one internal seam for predictive admission, reactive overflow recovery, and manual `/compact`. It asks the model for a stable handoff summary, canonicalizes the result to one summary marker (replacing an older summary rather than stacking markers), keeps a ~2k-token recent tail verbatim (never splitting a tool_use/tool_result pair at the boundary), and replaces the rest — the one sanctioned rewrite of the append-only history. If an existing summary has no newly foldable messages, compaction is a no-op: it does not call the provider or mutate history, usage, or rollout. A successful summary response with provider usage also enters the durable usage ledger before the compacted marker; compaction rewrites provider history, not the transcript's accumulated provider facts. Context-pressure admission uses the resettable estimate/anchor, while the ledger remains historical accounting; they are separate. Failed or cancelled summary requests leave history untouched.

`KLOOP_CONTEXT_WINDOW` sets the usable window in tokens (default 200000,
`off` disables compaction).

## Session persistence (Phase 2, second slice)

Every session is persisted to `.kloop/sessions/{id}.jsonl`
(`crates/core/src/rollout.rs`), one JSON line per recorded message or
provider-usage record, written through as the history records — so a killed
process loses at most the line being written. A `provider_usage` line preserves
the actual model, operation (`sampling` or `compaction`), and all four canonical
provider-reported categories (input, output, cache-read input, cache-creation
input). It is historical transcript data, separate from the resettable context
estimate and never enters provider replay, public display events, or snapshots.
The same append-only chain also carries recovery-only `session`
records (the canonical cwd and resolved model) and display `turn_terminal`
records (completed/maxRounds/aborted/error, positioned after a message index).
Error terminals add an internal typed provider outcome/failure while preserving
the protocol 2.0 status/error projection; terminals never enter provider replay
or token accounting. If a stream fails or is cancelled after visible text, that
partial assistant block and its provider provenance are recorded before the
terminal, so restart/fork keeps the replay boundary without generating a second
answer. Compaction appends a `compacted` marker line carrying the
full replacement history (the codex rollout pattern): the file stays
append-only and auditable, replay swaps in the replacement and keeps reading,
and superseded terminal indices are discarded with the replaced history.

Every line carries an envelope — `id` (`{session}#{seq}`, no rand
dependency), `parent` (previous line's id, linked across resumed runs), `ts`
(unix ms). Within one file the chain is purely sequential; a forked file's
first line carries a cross-file parent (see Fork below). Unknown fields are
ignored on read (locked by test), so the format grows additively.

Resume replays the file, then makes the history legal and consistent again. Read-only inspection (`thread/read`, `thread/list`, session pickers and event seeds) performs the same normalization only in memory and never changes the JSONL file:

- pairing is repaired in both directions (as in claude-code): unanswered
  `tool_use` blocks get the same `is_error` "interrupted" results the live
  interrupt path uses, and stray `tool_result` blocks answering nothing are
  dropped;
- a torn tail (crash mid-append) is truncated to the last intact line —
  physically, before appending resumes, so the partial bytes can't merge
  with the next line and orphan everything after (read-only paths never modify
  the file, including invalid UTF-8 tail bytes);
- when in-memory pairing repair changes a legacy session, explicit resume
  appends one `repaired` rollout marker containing the canonical messages,
  terminal boundaries and repair statistics. The marker is replay-only metadata:
  it does not enter provider history, public protocol/events or the provider
  usage ledger. A second resume is a no-op and does not append another marker;
- the process-global offload counter advances past every `off-NNNN.txt`
  already on disk, so new spills never clobber files the resumed history
  points at (usage anchors are not persisted — the estimate re-anchors on
  the first sampled response; provider-usage records replay in the same scan,
  with a complete usage line retained even if the following message line tore).


Server and CLI client-supplied session ids are restricted to one safe filename
component; path separators, traversal forms, absolute paths, control characters
and symlinked session leaves are rejected before any session file is read or
written. Server thread creation claims a new JSONL with an atomic create-new
operation, so a timestamp collision cannot truncate an existing transcript.

Session ids are UTC timestamps (`YYYYMMDD-HHMMSS`, no rand/chrono
dependency); `--resume` picks the most recently modified session, `--resume
<id>` a specific one, `--list-sessions` shows what's on disk. Native-protocol
sessions additionally restore the exact canonical cwd and resolved model that
were pinned at `thread/start`; a legacy rollout without this metadata remains
readable but requires its original `cwd` once on `thread/resume` before it is
migrated and safely resumable.

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
  dir-scanning counter already prevents clobbering);
- usage records in the kept raw prefix become the branch's baseline, while
  records after the cut remain only on the source; the branches then accumulate
  independently. Context anchors still reset and re-anchor after the fork.

Server mode exposes the same mechanism as `thread/fork {threadId, cut?}`
(omit `cut` to fork at the end): it copies the prefix, restores the source's
pinned cwd/model, spawns the fork as a live thread, and returns
`{thread:{id,cwd,model,resumable}, messageCount}` so the client can
`turn/start` on it right away. The source may be dormant or active-and-idle;
a fork is rejected while its turn is running so an in-flight exchange cannot
be copied before its terminal record closes the prefix.

In the TUI, **Ctrl+R** (when idle) opens a rewind picker: it lists the turn
boundaries the session can rewind to — each previewed by the user message it
would drop — and Enter forks at the chosen point *in place*. Unlike `--fork`,
you don't leave the session: History swaps onto the branch, the transcript
rebuilds to the earlier state, clears non-durable deferred-tool capability
receipts, and the next message continues the new branch (the old one stays on
disk, forkable/resumable). Esc cancels. The picker's points are exactly the cuts
`fork_session` accepts, so a selection can never be rejected. Rewind is
idle-only — a running turn owns History (Ctrl+C first).

### Sub-agent sessions

A sub-agent the `run_agent` tool spawns (synchronous batch or `background: true`)
writes its own session file next to the parent's, so its full transcript is
auditable — the parent's tool_result keeps only the sub-agent's final text,
while the file records every tool call it made. Both references converged on
this (cc's `subagents/agent-<id>.jsonl` sidechains, codex's one rollout per
child thread); kloop keeps its flat, single-file layout rather than cc's
nested dirs or codex's SQLite `thread_spawn_edges`:

- the child file is `{parent id}-{agent-N}.jsonl` — the name itself shows the
  lineage and stays unique (parent id is unique, the label is process-global);
- its first line carries `subagent_of` = `{parent id}#{seq}` of the parent
  turn's assistant line that made the spawning `run_agent` call — a *line-level*
  back-pointer (finer than either reference's session-level link), independent
  of the fork `parent` field since a sub-agent history is wholly its own (no
  prefix copied);
- lines are written through live as the sub-agent records them, so even a
  `stop_agent`-cancelled child leaves its partial transcript on disk;
- `--list-sessions` shows sub-agent sessions labelled `[sub-agent of …]`, but
  the default `--resume`/`--continue` picker skips them (they are reachable
  only by explicit id) — matching cc hiding sidechains and codex's source
  filter, while still keeping them visible for audit;
- the parent's `run_agent {background:true}` reply names the child's session log so
  a human reading the parent transcript can jump to it. A parent with no
  session (`--mock`, tests) leaves the sub-agent in memory, as before.

## Permissions (Phase 2, third slice)

Every tool call passes a layered gate before executing
(`crates/core/src/permissions.rs`), shaped after claude-code's permission
pipeline, with the bash analysis ported from codex's `shell-command` crate:

```
global deny → sensitive-read hard block → plan-mode read-only gate → safety checks →
global ask → sandbox auto-allow → bypass → read-only self-verdict → acceptEdits →
project allow → WorkspaceId-scoped session cache → ask the user
```

Two invariants carried over from claude-code: **deny always beats allow**,
and **safety checks are immune to bypass mode**. Credential-bearing/read-sensitive
paths (`.kloop`, `.ssh`, `.gnupg`, `.aws`, `.env*`) are an even earlier hard
boundary: `read_file` refuses them, grep/glob filter them before reading, and
Bash checks literal plus canonical paths before sandbox/read-only/bypass. The
macOS sandbox denies model-shell reads and writes across the private `~/.kloop`
state tree, including `config.toml`, `mcp-oauth.json`, and
`projects/v1/<ProjectId>/permissions.json` plus its lock, even through symlinks.
Private-store directory and leaf checks also reject open permissions, symlinks,
and non-regular entries. Provider/search key env vars are removed from model
shell children.
The direct file tools bind canonical descriptors to their permission facts; this
is stronger than a pathname-only check. Unsandboxed Bash remains a policy check,
not a filesystem transaction: a hostile same-UID process can race a checked
pathname before the child opens it. Use the OS sandbox for adversarial process
containment.

The sandbox auto-allow layer is the sandbox/approval coupling — see OS sandbox
below: a bash call the OS sandbox will contain skips everything beneath this
layer, while deny rules, sensitive reads, safety checks and explicit ask rules
keep their say (the ask-rule half is deliberately stricter than cc's
autoAllowBashIfSandboxed).

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

**Rules** are split by lifetime. Global `~/.kloop/config.toml` and the
comma-separated `KLOOP_DENY` / `KLOOP_ASK` env vars provide only process-wide
constraints:

```toml
[permissions]
deny = ["bash(git push *)", "read_file(**/*.pem)"]
ask = ["bash(cargo publish *)"]   # always confirm, even if project-approved
```

Durable allow rules live instead in the user-private, per-project
`~/.kloop/projects/v1/<ProjectId>/permissions.json` store. Legacy
`[permissions].allow` and non-empty `KLOOP_ALLOW` fail startup with a
secret-safe migration error: kloop neither applies, silently ignores, rewrites,
nor automatically migrates them. Remove the legacy entry and approve again in
each project.

`tool_name` covers the whole tool; `bash(<tokens>)` matches one command's
leading argv tokens (trailing `*` = any remainder, no `*` = exact), applied
per segment — in a chain every segment must be read-only or allowed, while a
single denied segment poisons the whole chain; `write_file(<glob>)` /
`edit_file(<glob>)` / `read_file(<glob>)` match the lexically-normalized
`path`, while `notebook_edit(<glob>)` matches `notebook_path` (both also match
their cwd-relative form), with `**` globs.

**File writes** get path safety: `.git`/`.kloop`/`.ssh`/`.gnupg`/`.aws`
directories, shell/git rc files, and `.env*` are sensitive — confirmed every
time, immune to allow rules, acceptEdits, and bypass. Writes escaping the
working directory never auto-pass in acceptEdits.

**Asking**: `y` allows once. `a` allows for this session in the current
workspace; its cache is partitioned by `WorkspaceId` (two-word bash prefix —
approving `git commit` never covers `git rebase` — or parent directory for file
writes). `p` allows for the current `ProjectId` across sessions and linked
worktrees, persisting the suggested rule (for example `bash(cargo build *)`) to
`~/.kloop/projects/v1/<ProjectId>/permissions.json`. If persistence fails, only
the current call runs and the UI says the grant was not saved. `n` denies. A
shared child in the same workspace sees its parent's cache; an isolated
worktree starts with an empty WorkspaceId partition, while the base partition
survives the transition. Sub-agents share the session mode/approver and project
policy. A denial is not a turn abort: the model receives an `is_error`
`tool_result` and is told to take another approach.

**Change previews**: when a `write_file`/`edit_file`/`notebook_edit` reaches
the prompt, the request carries a line-numbered diff (`crates/core/src/diff.rs`,
`similar`) so you approve what you can see, not just a path — approving an
invisible edit is meaningless. Notebook previews identify the edit mode and cell
ID, then diff that cell's source. Following claude-code and codex (which
independently converge on it), `edit_file` **reads the target, applies the edit,
and diffs the whole file** — the change shown in its real surrounding lines with
real line numbers, not the edit strings in isolation. `write_file` diffs an
existing file old→new,
or shows a `(new file)` insert preview for a fresh path. When the file can't be
read, is over 1 MiB, or the `old_string` doesn't uniquely match, it falls back
to diffing the two edit strings (numbered from 1) — claude-code's same
degradation. Each line is `{+/-/space}{line-number}  {content}`; hunks carry
three lines of context separated by `⋮`, minified lines are clipped, and the
whole preview is capped only by a generous 500-line ceiling (a runaway
minified whole-file overwrite can't blow up) — ordinary edits are never cut.
The TUI colors the popup (green adds, red deletes, dim context) and **scrolls
it** (↑/↓/j/k/PageUp/PageDown, with a `↑↓ more` hint in the border and the
y/a/p/n options pinned below the scroll region), the plain REPL prints the
same ANSI, the server adds a `preview` field to `approval/request`.

**Modes**: one flag `--permission-mode <mode>` picks the gate mode — `manual`
(the default when the flag is omitted — ask for anything unvouched-for),
`accept-edits` (file writes inside the working directory auto-pass), `bypass`
(everything passes *except* deny rules and safety checks), or `plan` (read-only
until the plan is approved, below). `--mock` disables the gate entirely — nobody
is at the keyboard. In the TUI, **shift+Tab** cycles the mode live (manual →
accept-edits → plan → manual; the status bar shows the current one), while bypass
stays opt-in via the flag. (cc calls the ask-first mode `default` internally but
labels it "Manual"; kloop drops the `default` name entirely — `manual` is the one
name, value and label alike.)

**Plan mode** (`--permission-mode plan`, cc's `plan`) is read-only exploration
until you sign off on a plan. The gate sits just below deny: every write or
side-effecting command is refused outright — not asked — so a destructive
command is a flat "no", not a `[destructive]` prompt whose yes would break the
promise ("what the gate shows = what runs" is the hardest contract, so the
enforcement is the hard gate, not a prompt the model may ignore — codex's
Plan is a soft prompt; kloop takes cc's hard form). Reads, searches, read-only
bash, and sub-agents (each re-gated per call) still run. A plan-mode reminder
rides every request so the model knows to plan, not act. The top-level model can
call **`enter_plan_mode {}`** from manual, accept-edits, or bypass; entering is
idempotent and remembers the exact previous mode. When ready, it calls
**`exit_plan_mode`** with the plan text; that rides the approval popup a
change-diff does (the plan is the scrollable `preview`). Approve restores the
remembered mode and the model implements; reject leaves the session in Plan.
Sub-agents inherit Plan mode and are read-only, but cannot enter or exit it.
The provider tool array advertises both controls for the whole session, so mode
changes do not invalidate the prompt-cache prefix.

**General questions are not approvals.** The depth-0-only
`ask_user_question` tool uses a separate `Questioner` contract and can ask one
to four bounded single- or multi-select questions, with Other text, option
previews, and notes. An answer is recorded only as the matching tool result;
explicit cancel is a non-error result, while EOF, disconnect, a dropped reply,
or an unsupported client fails closed. Plain and TUI sessions render the
question directly (TUI questions and approvals share one FIFO modal owner), and
the native server uses a separately negotiated `question/request` reverse RPC.
Headless mode never guesses an answer.

## TUI (Phase 2, fourth slice)

The default entry point is a ratatui terminal UI. It renders **inline** (no
alternate screen, plan 38 slice 0): a full-height viewport holds the still-live
tail — the streaming answer, any running tool rows, a one-line status row, and
the input — while every finalized cell scrolls up into the terminal's **native
scrollback**, so the mouse wheel, text selection, and Cmd+F reach history
directly (the UI keeps no scroll of its own). Tool calls render as
human-readable rows (plan 38 slice 2, `crates/tui/src/toolrow.rs`): a
status-marked verb and its key argument — `● Bash $ ls -la` (running, cyan),
`✓ Read src/main.rs`, `✓ Grep TODO in src`, `✗ Write notes.txt` (failed, red),
an MCP `server__tool` verbatim — over a few lines of the result indented under a
`└` gutter (double-limited by lines and chars, control chars sanitized, the rest
left in history/offload). `edit_file` shows a one-line `- old` / `+ new` diff
from its input instead. Permission prompts appear as a centered y/a/p/n popup
over the viewport, its diff carrying a GitHub-style `+N -M` summary (green/red)
above the line-numbered body (plan 38 slice 6); prompts from a concurrent tool
batch queue and are answered in order.

The session opens with a **banner** (plan 38 slice 6, a rounded brand-coloured
box): `>_ kloop` over the model, cwd, git branch, and starting mode, then it
scrolls into scrollback as the conversation grows. Colour uses a restrained
palette: a vivid **cyan-blue** RGB accent (`#4fb3c8`) marks kloop itself (the
banner, working spinner, and mode badge), ANSI **cyan** marks input/selection/
status (the `›` prompt, running tool marks, and approval action bar),
**green**/**red** are success/additions and errors/deletions, and secondary text
is dim. The custom brand colour intentionally stays separate from semantic
cyan, so agent chrome remains recognizable without the previous pink cast.

Assistant messages render as **markdown** (plan 38 slice 1, `crates/tui/src/markdown.rs`
via `pulldown-cmark`): headings and `**bold**`/`*italic*`/`~~strike~~` weight,
inline `code` and fenced code blocks over a dim background, ordered and unordered
lists with a hanging indent, block quotes with a `│` bar, and GitHub-style tables
drawn with box-drawing borders (`┌┬┐ ├┼┤ └┴┘`) and per-column alignment. A single
newline inside a paragraph reflows to a space (CommonMark soft break), so answers
re-wrap to the terminal width. Fenced code blocks that name a supported language
are **syntax-highlighted** (plan 38 slice 7, `synoptic`): a tight theme-safe
palette over the dim background — keywords magenta, strings green, comments dim,
numbers/types/functions cyan, everything else plain (no yellow/blue). While a
message is still streaming, only the part up to the last **stable boundary** (a
blank line, or a closed code fence) is rendered as markdown; the forming tail
shows raw, so a half-written table or fence never reflows mid-stream, and it
snaps to markdown once it completes.

Structure (`crates/tui`): the agent runs on its own tokio task and owns
`History`; `ChannelUi` implements `Ui`, `Approver`, and `Questioner` by
forwarding everything as events over an mpsc channel (approval/question answers
travel back over oneshots; dropped question replies are Unavailable rather than
invented answers). Questions and approvals share one FIFO modal owner so
concurrent interactions never overlap. Keys arrive from a dedicated
poll thread (`poll(200ms)+read`, not crossterm's `EventStream`) so the input
reader never parks holding the lock a resize's cursor-position query needs.
The UI loop `select!`s those key events against agent events, folds both into
pure state (`App`, whose `cells` are the uncommitted tail), and renders via
pure cell→line functions — which is what makes the transcript logic testable
without a terminal. `Terminal::draw` owns autoresize. After a completed frame,
the backend captures one physical size and pins `Backend::size()` through
confirmation, any irreversible overflow commit, and its repaint. If the captured
geometry already differs from the completed frame, the loop redraws first;
repeated resize churn skips commit for that frame. Once finalized cells must
freeze, `insert_before` writes them to native scrollback, clear succeeds, the App
drains that exact prefix, and the loop immediately repaints the tail while the
size fence is still held. The commit only freezes a leading prefix that still
leaves the live tail at least a viewport tall, so a tall final message (e.g. the
last turn on `-c` resume, trailed by a one-line note) is never stranded behind a
full-screen blank pad. Because the inline viewport is the full terminal height,
`insert_before` runs Ratatui's default path (the `scrolling-regions` cargo
feature is deliberately off): it scrolls committed lines into scrollback with
plain line feeds (`append_lines`) at the bottom row, not the DECSTBM one-row
scroll region that iTerm2 smears. Each `insert_before` on a full-height viewport
costs one full-screen scroll + clear regardless of how tall the block is, so a
`-c` resume that commits a long backlog coalesces it into a few tall batches
(capped rows each) rather than one `insert_before` per cell — the difference
between a resume that lands at once and one that visibly scrolls the whole
history for seconds. That default path blits every buffer cell verbatim,
including the `" "` placeholder each wide (CJK/emoji) grapheme reserves for its
second column; `Terminal::draw`'s diff drops those but `insert_before` does not,
so the backend wrapper mirrors that skip for every draw — otherwise
committed-to-scrollback CJK reads `关 键 逻 辑` while the live tail reads `关键逻辑`.
That default path also ends by clearing the whole inline viewport, which is
where the commit could go visibly wrong: ratatui-crossterm flushes each command
on its own, so the clear used to reach the terminal by itself and the screen sat
blank until the repaint arrived — and `Terminal::clear` opens by asking the
terminal where the cursor is (`ESC[6n`), a reply that waits on crossterm's
reader lock, held for up to one 200ms input poll. Measured through the PTY
harness, that left the screen black for 205–335ms per commit. So kloop adds no
clear of its own (`insert_before` already did it), answers cursor queries from
the last position it sent while a commit is in flight, and buffers every byte of
a frame into one write wrapped in synchronized output (DEC mode 2026, ignored by
terminals without it) — clear and repaint are presented as one update instead of
a blank screen followed by a repaint.
A resize that arrives during the transaction
becomes visible only to the next frame, so commit and repaint cannot split across
two geometries. Key handling receives the final repaint's viewport. Composer wrapping,
cursor projection, visible height, and ↑/↓ navigation likewise come from one
canonical visual-row layout.
Streaming deltas are drained in batches so a burst of tokens redraws once, not
per token.

Keys follow Claude Code: Enter sends when idle, or **steers** while a turn runs
(see below); **Esc** interrupts the running turn and clears the input line when
idle; **Ctrl+C** is a two-tap exit (the first press arms a "press Ctrl+C again
to exit" hint, the second quits, any other key disarms) — the same everywhere,
including inside a popup, so it is the single quit path (Ctrl+D is disabled);
Ctrl+R (idle) opens the rewind picker (see [Fork](#fork-and-rewind)). Scrolling
back through history is the terminal's job now (native scrollback). While an
approval popup or the rewind picker is up it captures the keyboard: for
approvals the scroll keys (plus j/k) page through a tall diff and y/a/p/n
answer, Esc denies; for rewind ↑↓/kj move and Enter/Esc select or cancel.
`--plain` keeps the old line-based REPL; `--mock` stays on plain output.

The input is a **multi-line composer** (Plan 38 slice 3, hardened by Plan 76,
`crates/tui/src/composer.rs`) with a `›` prompt over a dim placeholder when
empty. Its document coordinates are strong UTF-8 byte offsets, but the cursor
only lands on extended-grapheme boundaries: **Left/Right, Backspace, and forward
Delete** cross a whole combining sequence, ZWJ emoji, skin-tone emoji, flag, or
wide glyph. Home/End retain hard logical-line semantics; **↑/↓ follow soft-wrapped
visual rows** at the current viewport width, preserve a display-column goal over
short/CJK rows, and enter history only at the first/last visual row. One canonical
layout drives wrapping, cursor row/column, the eight-row window, and composer
height, so Unicode scalar count, grapheme boundaries, display columns, and visual
rows are never treated as interchangeable coordinates. On Unix, kloop
best-effort requests the terminal's keyboard-enhancement disambiguation mode;
when the terminal and any multiplexer/SSH hop pass CSI-u through,
**Shift+Enter** inserts a newline instead of submitting. **Alt+Enter / Ctrl+J**
do the same, with Ctrl+J remaining the reliable fallback for legacy terminals
that encode Shift+Enter as ordinary Enter. Composer newline keys keep this
meaning while a completion menu is open; bare **Tab / Enter** still accepts the
highlighted completion.

A large text paste becomes an indivisible, range-addressed **paste atom** with a
stable ID. Its `[Pasted #N: M chars]` label is only a projection: submission,
history recall, draft restore, and steering expand that exact payload once, while
identical text typed by the user remains literal. Bracketed paste canonicalizes
CRLF and CR line endings to the Composer's LF logical newline at the TUI ingress;
other characters, surrounding spaces, and trailing newlines are preserved (no
trim). The cursor cannot enter an atom; adjacent Backspace/Delete removes it
whole, and a range edit that cuts one materializes its payload before editing.
**Ctrl+V / Alt+V** pastes an image straight off the OS clipboard (a screenshot,
browser copy, or Finder-copied file); a pasted or dragged single-line
**image-file path** attaches too, while ordinary bracketed paste remains text.
Typed `Attachment { label, block }` records replace the old parallel label/image
arrays. Images show on a `📎` line and ride the next fresh turn (steering
preserves them), reusing the `--image` ingestion in `crates/tui/src/clipboard.rs`
via `arboard`.

Copying two lines therefore keeps one logical newline between them even when the
terminal supplies CRLF or CR bytes.

Typing a **`/`** at the start of the line or a **`@`** anywhere opens a
**completion menu** (plan 38 slice 4, `crates/tui/src/menu.rs`) that floats just
above the composer: `/` filters the slash-command catalog (built-ins plus loaded
skills and user commands — the same set `commands::run` dispatches), and `@`
lists files, searched from the project cwd off-thread with the same `ignore`
crate the `grep`/`glob` tools use (`kloop_core::fs_complete`, so it honors
.gitignore and skips VCS dirs). **↑/↓** move the selection, **Tab / Enter**
complete the highlighted entry into the composer (Enter completes rather than
submits while the menu is open), **Esc** dismisses it, and every other key edits
the query and re-filters. At most one menu is open at a time, and a pending
approval prompt or rewind picker takes precedence. The slash menu is suppressed
while a turn runs (a `/` line is then steering text). Trigger detection carries
the exact byte range and cursor identity through file search and popup accept;
late results with the same query at another position are rejected instead of
editing the wrong token.

While a turn runs, an **animated status line** (plan 38 slice 5,
`crates/tui/src/anim.rs`) sits just above the composer: a braille spinner, a
"shimmer" light band sweeping the verb, and `(elapsed · esc to interrupt)`. The
**footer** carries the mode badge and key hints on the left and the **system
status** — model name and a context gauge (`model · N% ctx`, refreshed at the end
of every agent round from the same usage accounting `/cost` reads, so a
long turn's gauge moves while it runs) — flush right (dropped on
a narrow row so the hints win). A **thinking block** shows a CC-style verb and
elapsed rather than its text: `∗ Thinking… (Xs)` while it streams, `∗ Thought for
Xs` once sealed. The animation self-drives — a frame tick wakes the loop only
while a turn runs, so an idle session redraws on nothing and spends no CPU (tokio
plays the FrameRequester role); `KLOOP_NO_ANIM` (or a `dumb` terminal) freezes
the spinner and drops the shimmer for reduced motion. The pure `App` has no
clock — the event loop owns the timing and feeds it in as a `Hud` each frame.

`--resume` replays the saved session into the tail (user/assistant text plus
tool status rows re-derived from the recorded tool_use/tool_result pairs); a
long history scrolls straight into native scrollback, so a resumed session
starts with its recent conversation visible instead of a blank screen.

`kloop app-server` (alias `kloop --serve`) speaks the provider-aware native agent protocol over stdio. The breaking native wire version is `2.0`; model-only `2.0` clients are rejected without downgrade. The core still emits one shared Event stream for every front-end.

**Provider catalog and session route.** `provider/catalog/read {}` returns only configured provider IDs, API families, ordered model allowlists, fallback models, and bounded availability codes. Credentials, endpoints, and private provenance never cross this boundary. `thread/start {cwd?, providerId?, model?}` selects the initial route. `thread/provider/switch {threadId, providerId, model?, expectedRouteRevision}` is idle-only, shares the turn/compact single-flight gate, and appends a typed durable transition before publishing the new route and the sequenced `thread/provider/changed` event. The response, changed event, and route-aware snapshots carry the same bounded `continuity` (`preserved` or `filtered`) and the route's `effort` (absent when no effort field is sent; a route read off disk reports what a session there would start at, since effort is session-local — see **Reasoning effort** below). A switch creates no turn, message, terminal, or usage record; typed busy/CAS, unavailable targets, and persistence failures leave the old route untouched.


**Handshake.** `initialize {clientInfo, protocolVersion, capabilities}` →
`{serverInfo, protocolVersion, capabilities}` accepts exactly protocol `"2.0"`
and gates every other method until it succeeds. Plan 63 changes the scoped
approval schema in place because the native protocol had no external users; it
does not add a compatibility adapter, fallback, alias, or silent downgrade.
Capabilities are structured: `{streaming, subagents, mcp,
images, approvals:{scopes:["once","workspaceSession","project"]}, questions,
events:{sequence:true,sync:true,snapshot:true}, threads:{list,
read,resume,fork}, providers:{catalog:true,switch:true}, config:{read}, skills:{list},
mcpServers:{status}}`. Event recovery is part of the only current protocol contract,
not an opt-in compatibility switch; the Desktop adapter rejects a server that does
not advertise all three event capabilities.

**Methods:** `thread/start {cwd?, providerId?, model?}` → `{thread:{id,route}}`;
`thread/list {limit?, cursor?}` → `{threads, nextCursor}` (newest first,
sub-agent sidechains hidden, `limit` capped at 500); `thread/read {threadId}` →
`{thread:{id,cwd,route,resumable,forkedFrom,messages,terminals}}`;
`thread/resume {threadId}` and `thread/fork {threadId, cut?}` →
`{thread:{id,cwd,route,resumable}, messageCount}`; `turn/start {threadId,
input}` → `{turn:{id}}`, `turn/steer {threadId, input}` → `{turnId}`,
`turn/interrupt {threadId}`. Old rollouts without route timelines are rejected,
not migrated. `input` is a string or an array of content parts
(`{type:"text",text}` / `{type:"image",source:{…}}`).

**Read-only discovery:** `provider/catalog/read {}` returns configured provider IDs, API families, ordered model allowlists, fallback models, and bounded availability codes; `config/read {cwd? | threadId?}` returns an explicit non-sensitive allowlist including the active route; `skills/list {cwd? | threadId?, forceReload?}` returns skill metadata without bodies, allowed-tool rules, or user commands; and `mcpServerStatus/list {}` returns the immutable startup discovery snapshot. Read methods reject unknown parameters, accept at most one scope selector, and canonicalize cwd before invoking their reader.

Every thread is its own tokio task owning a History and a session provider state. Turns, manual compaction, fallback, and child admission freeze a `FrozenProviderRoute`/`FrozenProviderAttempt`; later switches cannot change an in-flight request or a running child. Canonical history is never rewritten by a switch. Only a durable explicit switch may authorize a lossy reasoning request view; exact-compatible A→B→A replay remains byte-preserving. Public snapshots/events never expose endpoints, credentials, route history, signatures, or encrypted/redacted reasoning.
**Events** stream per active thread. Every public notification carries
`threadId`, one opaque `eventGeneration`, and a decimal-string `seq`; `seq`
starts at `"1"`, increases strictly across turns in that generation, and never
uses a JavaScript number. Turn-scoped notifications also carry `turnId` where
applicable. JSON-RPC responses/errors and the `approval/request` /
`question/request` reverse requests are outside this sequence and use the
independent request-id space. The public methods are: `turn/started {turn:{id}}`;
then the turn's items as
`item/started` / `item/delta {itemId, channel, text}` (channel ∈
`text`/`reasoning`/`output`) / `item/completed`, where `item.type` ∈
`assistantMessage` / `reasoning` / `toolCall` / `subAgent` (a tool call
carries its full `input`, and `output` + `agent` label when present); plus
`thread/backgroundTask/updated {task:{id, kind, description, status,
outputPath?, detail?, runId?}}` for session-scoped shell/agent/program/workflow work (`runId` is present for Program `run-*` and Workflow `wf_*`; **no
`turnId`**, because completion may arrive after the launching turn),
`thread/agentMessage/updated {messageId,from,to,summary,status}`
for the separate Local Agent Mailbox lifecycle (`status` ∈ `queued` / `delivered` /
`undeliverable`; **no `turnId` and no message body, context ID, or Task ID**),
`thread/scheduler/updated {task:{id, origin:"cron"|"loopWakeup",
status:"scheduled"|"fired"|"cancelled"|"failed", scheduledForMs?, reason?, detail?}}`
for owner-scoped scheduler lifecycle (**no `turnId`**),
`thread/tokenUsage/updated {tokenUsage:{total}}`, `note {text}`,
`thread/cwd/updated {cwd, branch}`; and `turn/completed {turn:{id, status,
error?}}`. A `turn/start` whose input is a slash command (`/help`, `/cost`, `/compact`, `/clear`, and an inert `/exit`) runs the command instead of the model: its output comes back as a `system` notification, `/clear` also emits `thread/cleared`, and the turn bracket is unchanged. `/provider` is the exception: it uses the idle provider transaction directly, emits the bounded provider result (and `thread/provider/changed` on a real switch), and creates no turn bracket or usage event. `/effort` runs on the ordinary command path but likewise re-freezes the session route, so its new `effort` reaches the next turn and the published route.

**Event recovery.** `thread/events/sync {threadId, eventCursor?}` is the one
atomic recovery entry point for an active thread. The typed cursor is
`{threadId,generation,seq}` and is unrelated to the `thread/list` pagination
cursor. Its thread/generation strings must be non-empty and `seq` must be a
base-10 `u64` string; a foreign thread, malformed/overflow value, or a
same-generation future sequence is `INVALID_PARAMS`. Dormant threads must first
be resumed.

With a retained same-generation cursor, sync returns
`{mode:"replay",generation,highWaterSeq,events,eventCursor}`. `events` is the
continuous interval `(cursor.seq, highWaterSeq]` using the exact live
`{method,params}` envelopes; a cursor already at high water produces an empty
replay. With no cursor, an old generation, or a cursor older than retention, it
returns `{mode:"snapshot",reason:"initial"|"generationChanged"|"cursorExpired",
generation,highWaterSeq,snapshot,eventCursor}`. Snapshot schema v1 contains the
rollout-seeded persisted messages/runtime/terminals plus the current
generation's materialized turns/items/notices and latest thread-scoped state.
It is a read-only display projection, never an instruction to redispatch a tool,
restart a process, redeliver mailbox content, or recreate an approval.

Each active thread retains at most 4,096 public envelopes or 16 MiB of encoded
event data, whichever limit is reached first. Eviction affects replay only; the
materialized snapshot remains complete. The generation, ring, and snapshot tail
are process memory, not a durable public-event journal. Resume or server restart
creates a new generation, seeds persisted history from the rollout, resets
volatile execution state, and resolves an old cursor with a
`generationChanged` full snapshot. Sequence/cursor/envelope data is never
written into rollout history or provider replay.

The client starts sync as soon as `thread/start`, `thread/resume`, or
`thread/fork` has produced an active thread and buffers live notifications while
the request is pending. It installs a snapshot by full replacement, applies a
replay from the existing baseline, drops same-generation duplicates, and only
advances through strictly contiguous sequences. A gap starts another sync;
unknown but well-formed sequenced methods still advance the cursor before the
business adapter ignores them. A missing or malformed generation/sequence is a
hard protocol failure—there is no direct-ingest fallback.

**Interactions use two independent reverse requests.** Permission decisions use
`approval/request {threadId, turnId, kind:"command"|"fileChange",
description, preview?, rememberRules?, approvalScopes}` (server ids are integers
in the server's own counter space), answered `{"decision": "accept" |
"acceptForSession" | "acceptForProject" | "decline"}`. The request is
authoritative: the client must offer only its advertised scopes, and core rejects
a response outside that set. `acceptAlways`, cancel, unknown, missing, late, and
EOF/disconnect replies all fail closed to deny. Approval payloads never contain
the ProjectId, raw identity anchor, state path, policy body, or revision.
General model questions use
`question/request {threadId, turnId, questionIndex, question}` only when the
client advertised `capabilities.questions: true`; each question is answered
`{"outcome":"answered","selected":[...],"other"?,"notes"?}` or
`{"outcome":"cancelled"}`. Unknown, malformed, mismatched, disconnected, or
dropped replies fail closed and pending reverse requests are removed when their
turn is interrupted.

```jsonc
→ {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2.0","capabilities":{"events":{"sequence":true,"sync":true,"snapshot":true},"providers":{"catalog":true,"switch":true}}}}
← {"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"kloop","version":"0.1.0"},"protocolVersion":"2.0","capabilities":{"streaming":true,"subagents":true,"mcp":true,"images":true,"providers":{"catalog":true,"switch":true},"events":{"sequence":true,"sync":true,"snapshot":true},"threads":{"list":true,"read":true,"resume":true,"fork":true},"config":{"read":true},"skills":{"list":true},"mcpServers":{"status":true}}}}
→ {"jsonrpc":"2.0","id":2,"method":"thread/start","params":{"providerId":"anthropic","model":"claude-sonnet-5"}}
← {"jsonrpc":"2.0","id":2,"result":{"thread":{"id":"20260721-135146","route":{"revision":1,"providerId":"anthropic","apiFamily":"anthropic_messages","model":"claude-sonnet-5"}}}}
→ {"jsonrpc":"2.0","id":3,"method":"thread/events/sync","params":{"threadId":"20260721-135146"}}
← {"jsonrpc":"2.0","id":3,"result":{"mode":"snapshot","reason":"initial","generation":"g","highWaterSeq":"0","snapshot":{"schemaVersion":1,"thread":{"id":"20260721-135146","cwd":"…","route":{"revision":1,"providerId":"anthropic","apiFamily":"anthropic_messages","model":"claude-sonnet-5"},"resumable":true},"history":{"messages":[],"runtime":{"cwd":"…"},"terminals":[]},"tail":{"turns":[],"notices":[],"backgroundTasks":[],"agentMessages":[],"scheduledTasks":[],"tokenUsage":null,"cwd":{"path":"…","branch":null}},"recovery":{"source":"fresh","volatileState":"live"}},"eventCursor":{"threadId":"20260721-135146","generation":"g","seq":"0"}}}
→ {"jsonrpc":"2.0","id":4,"method":"turn/start","params":{"threadId":"20260721-135146","input":"create s2.txt"}}
← {"jsonrpc":"2.0","id":4,"result":{"turn":{"id":1}}}
← {"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"…","eventGeneration":"g","seq":"1","turn":{"id":1}}}
← {"jsonrpc":"2.0","id":1,"method":"approval/request","params":{"threadId":"…","turnId":1,"kind":"fileChange","description":"write_file: s2.txt","rememberRules":["write_file(*)"],"approvalScopes":["once","workspaceSession","project"],"preview":"(new file)\n+1  hello"}}
→ {"jsonrpc":"2.0","id":1,"result":{"decision":"acceptForProject"}}
← {"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"…","eventGeneration":"g","seq":"7","turnId":1,"item":{"id":"…","type":"toolCall","name":"write_file","status":"completed"}}}
← {"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"…","eventGeneration":"g","seq":"8","turn":{"id":1,"status":"completed"}}}
```

### Codex Desktop adapter (plan 39 slices 2–4)

The `桌面前端仓库` repository's dedicated `kloop` branch launches this
server through `ENGINE_BIN` and consumes native protocol 2.0 directly (it
does not emulate the old Codex app-server wire). Its initialize request declares
the mandatory event recovery shape and hard-rejects a server missing any of
`events.sequence`, `events.sync`, or `events.snapshot`. The Tauri reader also
rejects and terminates the native child on any malformed public sequence instead
of forwarding an unsequenced event. A per-thread TypeScript coordinator owns
initial attach, replay/gap repair, duplicate suppression, generation replacement,
and generation-scoped UI item identities; snapshots use a dedicated full-replace
normalizer rather than synthetic live events. The adapter also requires the structured
approval capability, validates each `approvalScopes` payload, offers only the
advertised Once / Workspace session / Project actions, sends
`acceptForProject`, and rejects a mismatched handshake or malformed/unknown approval:

```sh
cargo build -p kloop
cd /path/to/桌面前端仓库/app
ENGINE_BIN=/path/to/kloop-repo/kloop/target/debug/kloop bun run app
```

That branch gets provider credentials from the process-global
`~/.kloop/config.toml` (environment variables are optional overrides), so it
deliberately skips Codex SSO/LoginDialog/AccessGuard. The same immutable
startup snapshot supplies permissions, MCP, web, hooks, sandbox, agents, and
codemode; thread cwd only anchors workspace-specific context, skills,
permissions, and sandbox paths. Live text/image turns, Stop,
generic tool cards, all four approval decisions, and native
`thread/list|read|resume|fork` history are connected.
Slice 4 also enables the local model picker, project-scoped skill catalog and
native `/skill ` invocation, the safe config read adapter, and an immutable MCP
status panel. The model picker is locked after a thread starts because its model
is pinned; reasoning controls stay hidden. Connector add/edit/toggle/delete,
skill/plugin lifecycle operations, generic config writes, personalization
settings/reset, and all other mutation methods remain fail-closed at both the
TypeScript and Tauri command boundaries. Personalization controls also disable
when their legacy settings cannot load, so the safe config projection is never
presented as editable legacy state. The adapter never receives raw config,
skill bodies, MCP commands/env/headers, or connection error chains.

Search, name/archive/rollback/compact/goal, plan/review, dynamic tools,
automation/artifacts, and account-only panels remain capability-gated and never
issue legacy RPCs.

## MCP client (Phase 2, sixth slice)

kloop connects to external MCP tool servers over one of two transports: a
local child process over **stdio** (JSON-RPC 2.0, newline-delimited JSON — one
object per line) or a remote endpoint over **streamable HTTP** (plan 34).
Declare servers in global `~/.kloop/config.toml`; `command`
selects stdio, `url` selects HTTP (exactly one, or it's a config error):

```toml
[mcp.servers.fs]                                   # stdio: local child process
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "sandbox"]
env = { }                                          # merged onto the process env
readonly = ["read_text_file", "list_directory"]    # eligible for concurrent dispatch

[mcp.servers.remote]                               # streamable HTTP: static bearer
url = "https://mcp.example.com/mcp"
bearer_token_env_var = "EXAMPLE_MCP_TOKEN"         # env var NAME, never the token itself
http_headers = { X-Tenant = "acme" }               # static extra request headers
readonly = ["search"]

[mcp.servers.github]                               # streamable HTTP: OAuth (plan 34b)
url = "https://api.githubcopilot.com/mcp/"
# oauth_client_id = "..."                          # optional: skip dynamic registration
# oauth_scopes = ["repo", "read:user"]             # optional: override discovered scopes
```

MCP bearer secrets never live inline in the config: `bearer_token_env_var` names an
variable that kloop reads at connect time into `Authorization: Bearer <token>`
(an inline `bearer_token` is refused; a referenced-but-unset var is an error).
Over HTTP, one POST carries each request, the reply comes back as
`application/json` or a short-lived `text/event-stream`, and the server's
`Mcp-Session-Id` header rides every subsequent request; a `404` for a
session-bearing request re-runs the handshake once but does **not** transparently
replay the original operation. The adapter first revalidates capabilities and the
tool catalog under its lifecycle gate; only a later freshly discovered call may
retry. 408/429/5xx and transient network errors retry (250ms, 1s, then a final
try), while 401/403 are terminal.

**OAuth (plan 34b).** A remote server with no `bearer_token_env_var` takes the
OAuth 2.1 authorization-code path — the way the hosted MCP servers (GitHub,
Linear, Notion) authenticate a user. Log in once:

```
kloop mcp login github
```

This runs two-step discovery (RFC 9728 → RFC 8414, from the server's `401
WWW-Authenticate` header), registers a client if none is preconfigured
(RFC 7591 dynamic client registration), opens your browser to authorize with
PKCE (S256) + a CSRF `state`, catches the redirect on a loopback listener, and
saves the token to `~/.kloop/mcp-oauth.json` (mode `0600`, keyed by
`name|hash(url)`, with an **absolute** expiry). Thereafter kloop injects the
bearer per request and refreshes it proactively before expiry (and once more on
a 401); a failed refresh clears the token and asks you to log in again. Startup
never blocks: a server that needs OAuth but has no stored token degrades to a
warning pointing at `kloop mcp login <name>`. Keyring storage, cross-process
refresh locks, the legacy SSE transport, and the manual-paste (no-browser)
fallback are out of scope (see the plan).

Servers are planned and connected once at startup; the handshake remains
`initialize` → `notifications/initialized` → `tools/list` (with bounded
`nextCursor` pagination). A process-owned lifecycle supervisor then owns every
stdio child, transport-health monitor, and refresh worker, performs explicit
shutdown/reap on every frontend exit, and uses Drop only as an error-path abort
fallback. Stdio EOF/read/write failure and request timeout publish a bounded
health transition, fail all pending requests, and close the matching refresh
worker; dropping a cancelled request also removes its pending route immediately.

Each configured server has one source-owned, in-memory readiness receipt. It
binds the server identity and a secret-free endpoint digest to auth availability,
capability summary, catalog generation, monotonic readiness revision, refresh
state, last health result, and a fixed failure class. The HTTP binding hashes
only scheme/host/port (not userinfo, path, query, headers, or credentials); the
stdio binding hashes only executable identity and environment names (not argv or
environment values). Its lifecycle distinguishes planned / starting / ready /
stale / degraded / failed / closed. It never stores or projects raw
command/URL/header/token/error-body data and is neither a permission grant nor a
second registry. A startup-failed server retains an empty source route, so an
exact `{server}__{tool}` request and all three resource helpers report that the
configured server is unavailable—not `unknown tool`, `no deferred tool`, or
`Server not found`—before hooks or approval.

Each advertised tool joins the model catalog as `{server}__{tool}` with its
inputSchema passed through verbatim. A stdio server advertising
`tools.listChanged` drives an atomic catalog refresh: burst notifications
coalesce, receiver lag still marks the catalog dirty, and list failures retry
within a fixed bound. Refresh first marks the source stale; success publishes the
complete replacement catalog and increments both catalog generation and
readiness revision. Exhausted retries retain the last-known catalog internally
but mark it degraded and hide it from discovery/call until a later successful
refresh; recovery therefore requires fresh tool discovery even when a tool name
is unchanged. Streamable HTTP still has no long-lived server-notification stream,
so its catalog is explicitly startup-fixed. A recovered HTTP 404 session emits a
lifecycle event and revalidates the catalog once rather than treating a new wire
session as proof that the old catalog is current; OAuth refresh/relogin state is
likewise reflected in readiness. MCP startup failure never blocks kloop.
Name sanitization folds everything outside `[A-Za-z0-9_]` to `_` (so persisted
allow rules round-trip through the permission-rule grammar); collisions warn at
startup and the colliding later definitions are skipped.

Calls go out with the raw server-side tool name only after core has frozen both
the source's catalog generation and readiness revision. Deferred discovery and a
Program manifest bind the same pair; core checks availability before hooks and
permission, checks the binding again after a pre-tool hook, and the adapter
rechecks under its call/refresh gate immediately before wire I/O. Availability
therefore never authorizes a call: ordinary permission, hooks, sandbox and
concurrency rules remain unchanged once the server is ready. Text content is
flattened; supported image blocks are lifted into canonical model image blocks,
while unsupported binary/audio content degrades to explicit text tags.
`isError: true` surfaces as an is_error tool_result — the same shape as a failing
built-in. MCP tools run serially unless listed in `readonly`, and always ask for
permission unless covered by the current project's durable whole-tool rule or
the WorkspaceId-scoped session cache — the `a`/`p` answers remember ordinary MCP
tools at workspace-session/project granularity. Resource reads are the exception
below.

Layering: core only knows the `ToolSource` trait (`tools/mod.rs`); the wire
client is the `kloop-mcp` crate (protocol layer transport-agnostic behind a
`Transport` trait — stdio and HTTP both implement it; it pulls `reqwest` for
the HTTP transport and the OAuth wire — PKCE/discovery/token exchange/refresh in
`oauth.rs` — but core never depends on it); the CLI glues them (config parsing,
secret resolution, namespacing, the adapter, and the OAuth login command + token
store in `mcp_auth.rs`, so nothing with a terminal/config-file side-effect
leaks into the wire crate).

### MCP resources

Whenever MCP is configured, kloop exposes three global resource helpers. Each
helper routes through the same per-server readiness receipt; a ready server must
also advertise `resources`, while a failed/degraded/stale server reports its
availability state before URI lookup or approval. The helpers are always deferred
behind `tool_search`, even when the ordinary tool count is below the threshold:

- **list_mcp_resources** `{server?}` lists one server or aggregates all
  resources-capable servers concurrently. It is catalog-only and auto-allowed.
  Single-server and all-server failures are errors; a partial aggregate keeps
  successful entries and names the failed servers.
- **read_mcp_resource** `{server, uri}` reads text or a supported image resource.
  It refreshes `resources/list` first and accepts only a URI the server currently
  advertises; the model-selected server/URI goes through normal external-tool
  approval instead of being treated as intrinsically safe because the operation
  is read-only. Dynamic URI requests advertise Once only and deliberately do not
  produce WorkspaceSession or Project grants; use bypass mode only when broad
  server/resource access is intentional.
- **read_mcp_resource_dir** `{server, uri}` lists direct children through the
  `io.modelcontextprotocol/skills` `directoryRead` extension. It has the same
  current-catalog and approval rule and fails clearly when the capability or URI
  is unsuitable.

Both stdio frames and HTTP JSON/SSE response bodies have an 8 MiB wire cap
before JSON parsing. Tool/resource pagination then rejects repeated cursors and
adds page/item/byte budgets; resource reads also cap content count/bytes, while
tools/call validates content count and supported image base64/decoded size before
an image can enter history. Large text within those wire budgets still uses the
ordinary History offload path. Supported
image MIME types become model image blocks; other blobs never inject raw base64.
Resource templates, prompts, sampling/elicitation/roots, and HTTP server-notification
subscription remain outside this slice.

### Deferred tools + tool_search

Past 30 total tools (`KLOOP_DEFER_THRESHOLD` overrides; built-ins never
defer), external source definitions stop being sent to the model. A source may
also force selected helpers to defer below that threshold (the MCP resource
helpers do this). The request carries the built-ins plus two extra tools, and
the synthetic context message lists the deferred names:

- **tool_search** `{query, max_results=5}` — `select:<name>[,<name>...]`
  fetches exact tools case-insensitively and deduplicates one selection;
  otherwise exact/prefix names and keyword terms are ranked over name,
  description and schema. Prefix a required term with `+`. `max_results` must
  be a positive integer. Matching tools' full definitions (description + JSON
  schema) come back in the result and unlock at the exact source-definition
  generation that supplied that schema.
- **call_tool** `{tool_name, params}` — the standard deferred execution path for
  providers that refuse to emit calls to names absent from the original tool
  array (some OpenAI-compat models). Dispatch unwraps the envelope up front, so
  permissions, hooks, concurrency and the UI all judge the real tool name;
  direct calls remain a compatibility optimization for providers that permit
  undeclared names.

`tool_search` is an ordering barrier rather than a read-only concurrent call: in
one assistant response, `tool_search` followed by a deferred source call
publishes the receipt before that call runs; the reverse order deterministically
bounces the still-locked call.

Searching never mutates the current provider tool array or deferred-name notice,
so the request prefix stays cache-friendly. A successful dynamic MCP refresh is
picked up when the next sampling round rebuilds its source snapshot. Unlocks are
capability receipts, not a session-wide `name -> generation` permission: each
receipt binds the immutable source owner slot and definition generation to the
current `WorkspaceId`/effective cwd, worktree transition epoch, permission
policy/session epoch, agent identity/depth, and tool allowlist. If any of those
scopes changes — including entering/leaving a session worktree, a permission
mode or cached approval change, a policy refresh/invalidation, or a child
authority boundary — the old receipt fails closed and the model must search
again. Operations sharing the same live Config and authority share receipts;
same-session compaction keeps them, while child agents and fresh resume/fork
Configs start empty. In-place rewind and `/clear` explicitly clear receipts;
isolated worktrees never inherit them.

The schema and generation are taken from one atomic source snapshot. MCP calls
hold a shared generation gate through the wire request; refresh takes the write
side, so an already-started call completes before publication or a published
replacement rejects the stale call — no check→await race can route through a
new catalog. Calling a deferred tool before searching likewise bounces with
guidance and does not unlock it. Permission rules and the approval cache keep
whole-tool-name granularity; source-forced helpers remain deferred. A
`run_program` bypasses only the deferred discovery lock because its callable
manifest was already exposed in that provider request. The manifest freezes each
source owner/generation and read-only verdict for foreground and background
execution; a post-sampling refresh, same-name owner switch, or newly appearing
tool fails closed instead of changing what old JavaScript can call. Workspace,
permission, sandbox, hooks, and ordinary tool-call gates still apply. Below the
threshold ordinary source tools ship inline.

## Hooks (Phase 2, seventh slice)

External command hooks fire at six points: before/after a turn
(`pre_turn` / `post_turn`), before/after a tool call (`pre_tool` /
`post_tool`), and around a sub-agent's turn (`subagent_start` /
`subagent_stop`). Declare them in global `~/.kloop/config.toml`:

```toml
[[hooks]]
event = "pre_tool"            # pre_turn|post_turn|pre_tool|post_tool|subagent_start|subagent_stop
command = ["./guard.sh"]      # argv, not a shell string
matcher = "bash"              # tool events only: exact tool-name filter
timeout_ms = 5000             # optional, default 10000
```

The event arrives as one line of JSON on the hook's stdin: `event` and
`session_id` always, plus `tool_name`/`tool_input` on tool events and
`tool_result`/`is_error` on `post_tool`. Exit code 0 allows; exit code **2
blocks** on the "start"/pre events — `pre_turn`, `pre_tool`, `subagent_start`
(cc's convention — a block must be an explicit signal): a blocked `pre_tool`
call never runs and the model gets an is_error tool_result (`blocked by hook:
…`, the reason read from stderr — stdout is the context channel), a blocked
`pre_turn`/`subagent_start` means the turn never starts. Every other outcome
fails **open** with a warning: any other exit code, exit 2 on a stop/post
event, a spawn failure, a timeout — a broken hook script is a malfunction,
not a policy decision (the permission gate is the enforcement layer).
Whatever an allowing hook prints on stdout is injected into history as a
`[{event} hook]`-prefixed user message the model sees.

**Sub-agents** (dispatched by the `run_agent` tool) fire `subagent_start` /
`subagent_stop` **instead of** `pre_turn` / `post_turn` — the split both cc
(`Stop`→`SubagentStop`) and codex ("child turns run SubagentStop")
converge on, so a "when the main agent finishes" hook and a "when a
sub-agent finishes" hook are cleanly separable. `subagent_stop` carries the
sub-agent's own `agent` label, its `agent_transcript_path` (the child session
file, omitted for an in-memory sub-agent) and its `last_assistant_message`
(the result) — enough for an audit or notification hook. A sub-agent's
`pre_tool` / `post_tool` additionally carry an `agent` field (`agent-N`);
main-agent tool events omit it, so their payload is byte-identical to before.

Ordering with permissions: `pre_tool` hooks run **before** the permission
gate — hooks are automation policy, the permission prompt is the human's
last word; a hook block means there is nothing left to ask about. Hooks run
in config order; the first block short-circuits the rest. Sub-agents inherit
the parent's hook set and session id. `--mock` runs without hooks (hermetic).

## Project context (Phase 2, eighth slice)

At startup kloop assembles what the model knows about where it is:

- **System prompt** = base instructions (a provider-neutral, sectioned policy
  prompt — identity plus `# System` / `# Doing tasks` / `# Acting with care` /
  `# Using your tools` / `# Communication style`; tool usage lives in each
  tool's own description, not here) + an environment block (working directory,
  platform, today's UTC date, whether cwd is a git repo) + a git snapshot
  (current branch, `git status --short` capped at 1000 bytes, last 5 commits)
  labeled as a start-of-session snapshot.
- **Instruction files**: per directory `AGENTS.md` wins, `CLAUDE.md` is the
  compatibility fallback. Layers, in order: `~/.kloop/` (global), then every
  directory from the git root down to cwd (closest to cwd last). Within each
  project directory the order is the main file, then `.kloop/rules/*.md`
  (modular fragments, loaded sorted), then a private `AGENTS.local.md` /
  `CLAUDE.local.md` override (gitignore it — loaded last, so it wins). Without
  a git root only cwd is consulted. Missing files are simply absent. Total
  budget 32 KiB across all files — over it, the overflowing file is truncated
  and the rest skipped, with startup warnings.
- **`@import`**: any instruction file can pull in another with a line like
  `@./coding-style.md` (also `@../x.md`, `@~/x.md`, `@/abs/x.md`) — resolved
  relative to the importing file, expanded recursively (depth ≤ 5, cycles
  broken by path), with the imported content placed right after the file that
  references it. `@` only triggers at a line start or after whitespace (so
  `a@b.com` and prose `@mentions` are left alone) and imports inside fenced
  code blocks are ignored. Project/local files may only import from within the
  project (git root); a global `~/.kloop/` file may import from anywhere. A
  missing or out-of-project import is skipped with a startup warning, never an
  error.
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

## File tools (Plans 49, 57, and 61)

`read_file`, `write_file`, `edit_file`, and `notebook_edit` share session-scoped
file observations (`core/src/file_state.rs`) rather than trusting a path forever:

- **read_file** resolves and opens a canonical no-follow regular-file descriptor
  before permission, so an approval wait cannot retarget a benign alias into a
  sensitive file. Ordinary text/images are capped at 5 MiB raw input; lowercase
  `.ipynb` keeps its 10 MiB Notebook limit. Each bounded read checks the opened
  handle's length before allocation, reads at most limit + 1, and verifies metadata,
  object identity, and content version afterward. Text lines are scanned without a
  whole-file line index, numbered, and capped at 7,000 model-visible characters;
  `offset`/`limit`, trailing empty lines, PDF/non-UTF-8 errors, and structured
  PNG/JPEG/GIF/WebP blocks retain their prior behavior. CRLF is shown as logical LF,
  while an isolated `\r` remains content. On Unix, regular files with multiple hard
  links are rejected because pathname sensitivity cannot safely classify another
  name for the same inode.
- Only a complete range that reaches the model in a final successful `tool_result`
  qualifies an existing file for mutation. Preview reads, errors, permission
  rejection, post-hook cancellation, restored sessions, sub-agents, and separate
  worktrees do not inherit authority. Observations include stable file identity, so
  delete/recreate cannot inherit old coverage even when bytes and metadata resemble
  the previous object. The bounded table is process-memory only and deterministically
  evicts old entries.
- **write_file** writes the model-provided full content exactly as supplied; it does
  not inherit old line endings and its replacement content is not limited by the
  5 MiB Read/Edit ceiling. For a new leaf it may plan missing parent directories,
  show that plan during approval, and create them only after approval while holding
  the effective-target path lock. Replacing an existing file still requires a
  complete fresh read.
- **edit_file** requires an existing, complete, fresh UTF-8 target of at most 5 MiB.
  Raw exact matching wins. Only when raw matches are absent does LF input match CRLF
  text; the helper maps logical offsets back to raw byte ranges, restores local (or
  dominant) EOLs in replacement text, and leaves all unmatched bytes—including
  mixed EOLs and isolated `\r`—unchanged. Executor and approval preview use this one
  helper, so duplicate/`replace_all` decisions and shown bytes cannot drift.
- Mutation preflight binds either the existing direct parent or the nearest existing
  ancestor after pre-hooks but before permission. Original spelling and the frozen
  effective target both reach the gate; approval itself has no directory side
  effects. After approval, Unix walks missing components with
  `mkdirat` + `openat(O_DIRECTORY|O_NOFOLLOW)`. Windows uses retained directory
  HANDLEs, `NtCreateFile(RootDirectory=...)`, rejects every reparse point, and binds
  volume + 128-bit file ID. No Windows pathname-open/rename fallback is used.
- At the final parent, target reads, streaming fingerprints/equality checks,
  same-directory exclusive temp creation, final freshness/identity checks, rename,
  and cleanup stay capability-relative. Unix uses `renameat`/`unlinkat` plus file and
  directory sync; Windows flushes the file handle and uses
  `NtSetInformationFile(FileRenameInformationEx)` with a retained parent HANDLE for
  handle-relative atomic visibility. Windows does not claim a portable
  POSIX-equivalent directory-entry durability guarantee. On Windows, failed nested
  writes release their retained child handles, reopen each cleanup candidate relative
  to its retained parent, revalidate stable identity, and set disposition only on an
  empty matching handle; existing, competitor-created, replaced, or non-empty
  directories are not deleted. POSIX has no portable atomic handle-bound `rmdir`: an
  inode check followed by `unlinkat(name)` can race with a same-UID name swap. Failed Unix
  nested writes therefore conservatively leave their newly created empty
  directories rather than risk deleting a replacement. Existing modes are
  preserved; new Unix files/directories use 0666/0777 filtered by umask, while
  Windows inherits parent ACLs. Symbolic/reparse leaves, FIFOs, other non-regular
  targets, parent retargets, and leaf-appeared races fail closed.

The capability boundary closes approval-time alias retargeting; it is not a
filesystem transaction against a hostile same-UID process. POSIX still leaves a
small final identity-check-to-`renameat` namespace window, documented in Plan 49.

The complete-read requirement and fail-on-any-drift Edit policy are deliberately
stricter than Claude Code 2.1.220, which accepts partial qualification in some
paths and may stale-recover an unambiguous edit. Plan 49's executor-level hook
fixtures and dependent mutation pair use the same normalized call inputs as the
real-dispatch kloop report; generated contracts in
`refs/claude-code-2.1.220/paired-parity.json` compare call/event/result/order/workspace
projections and require exact-bundle bridges for cross-profile cells. Detailed
policy differences remain in `tool-matrix.json`.

### Notebook cells (Plan 57)

A lowercase `.ipynb` path passed to `read_file` is rendered cell-by-cell rather
than as raw JSON. Markdown, code, and raw source preserve cell order and IDs;
missing IDs are displayed as `cell-N` without modifying the file. Code outputs
preserve text/image/text order, and supported PNG/JPEG/GIF/WebP outputs use the
same structured image blocks as ordinary image reads. Markdown attachments are
not treated as code outputs. Notebook input is capped at 10 MiB, visible text at
7,000 characters, and decoded output images at 16 files / 5 MiB total. Paging a
notebook is rejected instead of presenting partial cells as complete.

A complete, untruncated cell-aware read grants a separate notebook-qualified
observation. The model-visible **`notebook_edit`** tool then accepts an absolute
lowercase `.ipynb` `notebook_path`, required `new_source`, optional `cell_id`,
`cell_type` (`code|markdown`), and `edit_mode` (`replace|insert|delete`, default
`replace`). Replace preserves unknown fields and metadata, while resetting code
outputs/execution count; insert requires a type and creates an 8-hex ID for
nbformat 4.5+; delete removes only the selected cell. The ordered serializer
preserves untouched object order, uses one-space indentation, and emits no
trailing newline.

`notebook_edit` requires the current file to be complete, fresh, and
notebook-qualified. A generic raw read or ordinary `write_file`/`edit_file` does
not grant that authority. It reuses the same retained parent descriptor, keyed
path lock, no-follow opens, final version check, same-directory synced temporary
file, atomic rename, cleanup, and parent sync as other mutations. Failed edits
leave bytes unchanged and conservatively clear qualification. Worktree switches
use a fresh `FileState`, so a read in the main checkout cannot authorize an edit
inside the worktree. Permission rules and approval previews use
`notebook_path` as a first-class canonical path and show a cell-source diff.

The pinned Claude Code 2.1.220 profile did not expose a standalone
`NotebookRead`; its behavior is likewise an internal `Read(.ipynb)` adapter.
Plan 57 also found plugin-backed LSP locators, but `ENABLE_LSP_TOOL=1` alone did
not register LSP and no authoritative hermetic enabled-plugin profile was
available. kloop therefore has no speculative production LSP client; that
matrix row remains `unknown` rather than being called globally missing.

## Search tools (Phase 2, ninth slice; Plan 49 parity pass)

Dedicated read-only `grep` and `glob` tools (`core/src/tools/search.rs`), built on
ripgrep's own crates (`grep-searcher`/`grep-regex`/`ignore`) — no external
binary, no shell-quoting pain, and both are read-only by verdict: they skip
the approval gate and join concurrent tool batches. Shapes follow cc's
Grep/Glob:

- **grep**: `pattern` (Rust regex) plus `path`, `glob`, `type` filters;
  empty JSON strings for `glob`/`type` mean “filter omitted” (whitespace is
  not trimmed); `output_mode` = `files_with_matches` (default,
  newest-first) | `content`
  (`path:line:text`, `-n`/`-A`/`-B`/`-C`, `context`, and `-o` supported) |
  `count`; `-i`, `multiline`, and numeric-or-string `head_limit`/`offset`
  paging (default 250). An explicitly supplied single-file path uses CC's
  basename-free content format. Honors .gitignore, searches hidden files,
  never descends into VCS dirs, skips binary files, binds each candidate to a
  canonical no-follow regular-file descriptor before filtering and searches that
  descriptor (not a later pathname lookup), clips matched lines at 500
  characters, caps model-facing text at 7,000 characters, and stops after a
  20s budget with a partial-results note.
- **glob**: gitignore-style `pattern` under `path` (an empty pattern lists the
  tree), newest-first, capped at exactly 100 paths with an explicit count of
  paths actually shown after the character cap; model-facing text has the same
  code-mode caller retains the full 100-entry array.

Two deliberate deviations from cc (documented in plan 14): `glob` honors
.gitignore (cc's does not — a Rust tree would drown in `target/`), and the
newest-first sort happens before the cap so truncation drops the stalest
files, not the freshest. cc has no LS tool anymore; kloop follows (bash `ls`
is already read-only-whitelisted).

Both read tools honor the read gate's blocklist uniformly (cc's
`getFileReadIgnorePatterns`; `Permissions::read_path_blocked`, plan 31): a file
covered by a `read_file` deny rule (`read_file(**/*.pem)`, or the whole-tool
`read_file` form) or on the sensitive-path list (`.env*`, `.ssh`, `.git`,
`.kloop`, …) is dropped from grep/glob output — before its contents are read —
so a deny meant for reads is not slipped by grep, and secrets do not leak
through a search. Grep also skips files with multiple hard links because the
pathname cannot classify another name for the same inode. The dropped count is reported (`[N path(s) hidden by
deny/sensitive rules]`) rather than silently swallowed. This is an output
filter, not an approval prompt (a tree walk touches too many paths for the
gate's one-path ask); it applies in every mode but `--mock`, `--permission-mode
bypass` included.

## Foreground bash lifecycle (Plan 50 parity pass)

Foreground `bash` runs the frozen shell identity with `-lc`: ordinary Unix resolves
an executable regular-file POSIX `sh` (skipping non-executable PATH entries and
validating the fallback/symlink target), WSL validates `/bin/bash`, and native
Windows uses only a validated Git for Windows `bin\bash.exe`. Windows never substitutes PowerShell,
`cmd.exe`, WSL, Cygwin, BusyBox, or an arbitrary PATH `sh.exe`. stdin is closed
and stdout/stderr use separate pipes. Both pipes are drained concurrently to
EOF, so a child filling one stream cannot deadlock behind an unread other
stream. Each stream retains at most 150,000 bytes while continuing to drain
discarded bytes; the merged model-facing result is UTF-8 lossy, capped at 30,000
characters, and says how many captured characters and additional bytes were
omitted. stdout precedes stderr in the canonical result, followed by
`[exit status N]` or `[killed by signal]`; an empty successful run returns
`(no output)`.

Every spawn owns a complete process tree: a dedicated process group on Unix or
a fixed pair of dedicated Job Objects on native Windows. Windows creates the root
suspended, assigns it to a `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` Job, and resumes it
only after assignment succeeds; create/assign/resume failure is fail-closed and
never falls back to a bare child. Every Windows Bash and PowerShell root also runs
under `DEBUG_PROCESS`: each process gets exactly one first-chance initial
breakpoint consumed by the debugger, while later breakpoints and all other
exceptions remain unhandled. A descendant create event is admitted to the root or
containment Job before its first thread continues. Admission and termination use
one lifecycle mutex, so termination permanently closes admission before killing
both Jobs and a late event process cannot escape. Debug image/DLL file handles are
closed explicitly; process/thread debug handles remain owned by the debugger
contract and are closed by the system after their exit events are continued.

The stdio handle-list owns both its attribute storage and handle-value array until
`CreateProcessW` completes. A workspace-wide child-creation gate serializes that
short inheritable-handle window with hook, MCP, Git, and other kloop process
spawns, so an unrelated child cannot steal a pipe writer. Environment keys retain
raw UTF-16 and use Windows ordinal case-insensitive comparison; a cancellable wait
future re-polls one persistent process waiter instead of leaking a blocking waiter
on each watchdog tick. A timeout or turn cancellation terminates the entire tree,
explicitly waits/reaps the direct shell child, confirms the tree is empty, and
finishes the debugger within one shared absolute cleanup deadline; it never stacks
separate phase timeouts or performs an unbounded thread join. Only then are pipes
drained under their own short bound. If the shell leader exits normally while a
descendant remains, foreground completion terminates that residual tree before
waiting for EOF. A synchronous Drop guard closes admission and terminates the Jobs
without sleeping or joining. The dispatch layer preserves the boundary:
cancellation during hooks or approval cannot spawn the command; once foreground
Bash has spawned, dispatch waits for cleanup rather than dropping the future and
returning early. Concurrent read-only Bash calls each finish their own cleanup
before paired interrupted results are returned.

These are intentional safety differences from the pinned Claude Code 2.1.220 behavior: its
stubborn timeout is promoted to a background task and its running SIGINT path reports a user
rejection/abort; the isolated fixtures observe TERM-ignoring descendants still alive after both
results. kloop keeps no-survivor foreground semantics, its native `timeout_ms`/result envelope,
and the stricter permission/sandbox pipeline. Both implementations do agree on input-dependent
batching: read-only calls may overlap, while opaque/redirection calls execute serially.

## Background bash (Phase 2, tenth slice)

`bash` takes `background`: the command starts in its own owned process
tree (Unix process group or Windows Job), stdout/stderr interleave straight into
a file under `.kloop/offload/`
(`bg-N.out`, fd-level — no reader tasks, no pipe deadlock), and the tool
returns immediately with the ID and the output path. Companions:

- **bash_output** `{bash_id, block=true, timeout_ms=30000}` — blocks until
  the command finishes (or the timeout), or peeks with `block=false`;
  reports `running` / `completed (exit 0)` / `failed (exit N)` /
  `killed (reason)` plus the last 30k bytes of output (read the file with
  `read_file` for more). Read-only: skips the gate, joins concurrent batches.
- **stop_bash** `{bash_id}` — terminates the whole owned process tree and waits
  for the registry to confirm. Auto-allowed: it can only signal processes this
  agent itself started.

Semantics: permission checks are identical to foreground bash (the command
string is what's judged, not where it runs); `timeout_ms` is ignored in
background mode (cc drops the timer too); interrupting a turn never touches
background shells. Every shell publishes a session-scoped lifecycle
(`running` → exactly one `completed` / `failed` / `cancelled`) to all
frontends. On terminal state, the launching agent's step-boundary inbox receives
only the status, summary, and output-file pointer — command output stays in the
file. A running turn sees it at the next sampling boundary; the TUI autowakes
when idle; plain/server deliver it on the next turn. `stop_bash`, a 1 GiB
output-file watchdog, and explicit session shutdown reap the whole process
tree. IDs are process-global (`bg-1`, `bg-2`, …) so sub-agents and server
threads sharing one offload directory never collide.

Session shutdown closes both background registries before worktree teardown,
requests cooperative cancellation, then bounds the wait (worker abort or shell
SIGKILL fallback). Late terminal events are thread/session scoped rather than
being attached to a fake turn id; a terminal transition wins once, so a stop /
natural-completion race cannot reinject or notify twice. The shell registry
remains separate from the agent/program/workflow registry because it owns an output file
and reinjects only a pointer, while the other executions reinject result bodies.

Deliberately not ported: cc's automatic foreground→background promotion, stall
policy, and model-visible `Monitor`. Exact 2.1.220 evidence shows Monitor is a
different, server-flagged (`tengu_amber_sentinel`, default off) tool for
streaming every command stdout line or WebSocket frame; the clean CLI profile
does not expose it. See `docs/plan/51-background-monitor-parity.md`.

## Native Windows PowerShell (Plan 62)

Native Windows conditionally registers a separate foreground-only
`powershell {command, timeout_ms?}` tool. Startup chooses the highest trusted
PowerShell 7 installation: versioned MSI roots plus installed official
`Microsoft.PowerShell[_-LTS]_8wekyb3d8bbwe` MSIX package roots resolved through
the Windows package API. Candidates are ranked by the executable's file-version
resource. A reported v7 resource is authoritative and a reported non-v7 resource
is rejected; only the Win32 "version resource missing" result may fall back to
trusted MSI directory or official package metadata. Access, query, signature, and
format failures are invalid rather than metadata fallbacks. This handles MSI's
fixed `PowerShell\7` directory without letting an older MSIX outrank a newer
binary. Startup never trusts an arbitrary PATH `pwsh.exe`. It then falls back to
Windows PowerShell 5.1. An explicit `[shells].powershell` absolute executable
path takes precedence. PowerShell availability is independent of Git Bash, so a
Windows session may expose one or both shell families. The resolved executable
and flavor are frozen into the runtime snapshot.

The executor passes a UTF-16LE `EncodedCommand` to a fixed
`-NoLogo -NoProfile -NonInteractive -EncodedCommand` argv. It does not use a
temporary script, `Invoke-Expression`, a profile, or `ExecutionPolicy Bypass`.
The payload sets UTF-8 console/native output encoding, clears `$LASTEXITCODE`
before the original script, and runs that script in a `try` inside its own
script block. A `finally` immediately snapshots final `$?` and `$LASTEXITCODE`,
so a top-level `return` cannot bypass status capture; the wrapper then compares
newly-added `$Error` entries. New PowerShell errors return 1; a successful final
PowerShell operation returns 0 even if an earlier native command left a stale
nonzero code; an otherwise-failed final native command propagates its nonzero
code; all other failures return 1. Multiline text, Unicode, here-strings,
trailing comments, `return` output, and explicit `exit N` retain their meaning.
Hooks, permission prompts, events, history, and `PowerShell PS>` TUI rows always
carry the original script, never the encoded payload.

PowerShell v1 has no background mode, stdin/PTY/session channel, executable
override, or sandbox escape field; unknown fields are rejected even if a caller
bypasses the published schema. It reuses the bounded dual-pipe foreground
executor, credential environment scrub, timeout/cancellation cleanup, and the
mandatory Windows Job set. The Jobs are process-tree containment only: Windows
still has no restricted-token/AppContainer filesystem or network sandbox. Every
Windows Bash and PowerShell process uses the fail-closed debug gate because an
MSIX-hosted `Start-Process` descendant may not remain in the root Job. The root
is still assigned before resume; each descendant create event is admitted under
the same lifecycle lock that permanently closes admission during termination,
then any non-member is assigned to the second kill-on-close Job before its first
thread continues. Open, membership, assignment, or debug-continuation failure
terminates the event process and both Jobs. Cleanup owns this fixed Job set and
never falls back to PID scanning, a bare child, uncontrolled breakaway, or
direct-child kill.

All PowerShell execution in one session also shares one exclusive async gate.
Direct calls and foreground/background code-mode programs therefore cannot
overlap even when they use separate dispatch rounds or `CoreBridge` instances;
Config clones and sub-agents share the gate, while independent server sessions do
not. The gate is acquired after hooks and permission approval but before spawn,
so cancellation while waiting creates no process. It is released before
post-tool hooks and is never held around `run_program`, `run_agent`, `skill`,
`wait_for_activity`, or other orchestration wrappers.

Permissions treat every PowerShell script as `PowerShellOpaque`; the Bash AST
and read-only classifier are never applied. Plan mode rejects it without asking;
manual, accept-edits, and bypass ask every time unless the current project policy
already contains a whole-tool `powershell` allow rule. `deny` and `ask` whole-tool rules
retain their usual precedence, `powershell(...)` prefix rules are rejected, and
interactive approvals authorize only that one call — no opaque script is cached or
persisted by a PowerShell prompt. Prompts are labeled
`[unclassified PowerShell]` and an additional raw matcher keeps obvious Windows
secret paths bypass-immune; this matcher is not a PowerShell data-flow analysis.

## Scheduler (Plan 58)

The scheduler is an in-process, depth-zero, owner-scoped surface. Its model-visible
tools are strict `cron_create`, `cron_delete`, `cron_list`, and `schedule_wakeup`;
`/loop` translates fixed intervals into cron instructions and uses dynamic wakeups when
no interval is supplied. `cron_create` requires `cron` and `prompt`, defaults
`recurring` to `true` and `durable` to `false`, and accepts a five-field local-time cron
expression. A recurring job lives for at most seven days: its final due tick is delivered,
then the job is deleted. `schedule_wakeup` normally requires `delay_seconds`, `reason`,
and `prompt`; it rounds, clamps to 60–3600 seconds, aligns to the next minute, and
atomically replaces the owner's previous dynamic wakeup. `stop:true` clears only that
dynamic slot, never fixed recurring cron jobs.

These names are intentionally native. kloop exposes `schedule_wakeup.delay_seconds`,
not Claude Code's `ScheduleWakeup.delaySeconds`, and provides no PascalCase compatibility
aliases. The tools appear only on a scheduler-capable depth-zero owner surface; they are
not available to sub-agents, mocks, or the `run_program` TypeScript API. `cron_list` may
run concurrently; scheduler mutations are serialized.

Session-only jobs exist only in the in-process registry and disappear at shutdown.
Durable jobs are stored at:

```text
~/.kloop/scheduler/<project-key>/scheduled_tasks.json
~/.kloop/scheduler/<project-key>/scheduled_tasks.lock
```

The project key derives from the canonical Git common directory, so a primary checkout and
its worktrees share one base-project identity; a non-Git project uses its canonical cwd.
A durable job is bound to its creating session/thread owner. Other owners cannot list,
delete, or claim it; competing runtimes of the same owner claim under the store lock and
deliver only once. A late durable one-shot remains pending in a headless run or a
server session without question capability until that owner resumes through a
questions-capable interactive frontend.

Due work enters the typed Inbox rather than mutating an in-flight provider request. The
TUI sends one idle `Wake`; the plain frontend selects between stdin and Inbox activity; and
the server allocates a real increasing turn id for a single-flight delivery turn. Headless
stops the scheduler after its main turn and before background task/shell shutdown:
session-only jobs disappear, while durable jobs remain. Scheduler lifecycle is separate
from background Bash and agent registries. It never installs or modifies `crontab`,
`launchd`, `systemd` timers, login items, or any system scheduler.

## Web tools (Phase 2, eleventh slice)

`web_fetch` and `web_search` have their agent-facing contracts (names,
descriptions, and JSON schemas) in `crates/core/src/tools/web.rs`. Their network
operations remain implemented in the `kloop-web` crate and are bound to core's
`ToolSource` seam by the CLI (exactly like MCP servers — core stays
network-free, reqwest lives only in provider and web):

- **web_fetch** `{url}` (strict; unknown fields rejected) — HTTP upgraded to
  HTTPS, embedded credentials and 2000-char URLs rejected, SSRF guard
  (loopback/private/link-local/CGNAT/metadata ranges refused, DNS names resolved
  and checked on every hop), same-site redirects followed (max 5), cross-host
  redirects reported for an explicit re-fetch, 5 MiB download cap, HTML→text,
  and a 50k-character model-text cap. Download and text truncation are reported
  independently.
- **web_search** `{query, allowed_domains?, blocked_domains?}` (strict; query is
  at least two characters; allow/block lists are mutually exclusive) —
  pluggable `SearchBackend` trait with Tavily (default, `TAVILY_API_KEY`) and
  Brave (`BRAVE_API_KEY`). The backend result count is an internal fixed bound,
  result URLs must be HTTP(S), domain matching uses exact-host/subdomain
  boundaries, provider JSON responses are capped at 5 MiB, and formatted model
  output is capped at 50k characters. Without the selected backend's key the
  tool is not registered and startup warns. `[web] search_provider` in global
  `~/.kloop/config.toml` selects the backend.

Both are read-only for concurrency; the permission gate treats them like any
external tool (ask by default, with ordinary durable grants scoped to the
current project and temporary grants scoped to the current WorkspaceId
session). Deliberately not ported from CC 2.1.220: WebFetch's mandatory
`prompt` plus secondary small-model processing, the 15-minute fetch cache,
turndown-style HTML→Markdown, and the preapproved-domain list. Exact Plan 55
evidence proves CC WebSearch success/empty/server-tool error through a hermetic
`ANTHROPIC_BASE_URL` side query, but leaves CC WebFetch transport, WebSearch
timeout/large/dynamic-concurrency/CCR branches, and true remote-agent execution
unknown rather than weakening kloop's safety policy to manufacture fixtures.

## Parallel sub-agents (Phase 2, twelfth slice)

The `run_agent` tool dispatches concurrently (cc shape): `is_concurrency_safe`
marks `run_agent` unconditionally safe, so consecutive run_agent calls in one response
run as parallel sub-agents inside the ordinary concurrent batch. Results stay
paired to their `tool_use_id`s in request order; one sub-agent failing (bad
input, error, panic) becomes its own `is_error` tool_result without sinking
the batch. Sub-agents' own writes are still gated individually — hooks and
the shared permission gate see every inner call, and concurrent approval
prompts serialize (the TUI already queues; the plain REPL takes a mutex so
one prompt owns the terminal at a time).

Every spawn gets a process-global label (`agent-1`, `agent-2`, …) stamped on
its typed session-local identity. While live, the same canonical label is its Local
Agent Mailbox address; `main` is the root address. The address is ephemeral and
resolves only inside that session — it is not a remote endpoint, Agent Card, or
Task ID. Core's `Event` stream (plan 39) carries the display label end to end.
`run_agent.description` is optional display metadata: at most 200 Unicode
characters, nonblank, single-line, and control-character-free. It labels the
launch response and foreground/background lifecycle row but never replaces or
modifies the child prompt; omission falls back to the first-line prompt preview.
Invalid metadata fails before label allocation, worktree creation, registration,
or spawn. A configured `agent_type` remains a display decoration rather than an
instance identity.
**Sampling rounds are unbounded by default**, matching codex's child-thread
turn loop; `max_rounds` is an optional per-call guardrail for callers that
explicitly need one. The recursion-depth and background-concurrency limits stay
separate. An `Item::ToolCall` names the `agent` that made the call ("" = main
agent), and an `Item::SubAgent` (started → completed) brackets the sub-agent's
life.
Presentation per frontend:

- **TUI**: one live row per sub-agent (`… agent-1 <task> — 3 tools · bash
  {...}`) that folds its tool calls into a counter plus a latest-call
  preview — parallel agents never interleave rows — and collapses to
  `✓ agent-1 <task> (3 tool uses)` when it ends. A row still Running when
  the turn dies (interrupt drops the task future) is patched to failed.
- **plain**: dim notes, `agent-1 · bash {...}` per call.
- **server**: a `subAgent` item brackets the sub-agent (`item/started` →
  `item/completed`); its own `toolCall` items carry an `"agent"` field
  (main-agent calls omit it).

### Custom agent types

A `run_agent` call can target a named specialized agent via the `agent_type`
parameter. Types are defined in global `~/.kloop/config.toml`:

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
anything outside is rejected at dispatch (`read_offloaded`, `send_message`, and
`list_agents` always stay available as runtime infrastructure, so a restricted
agent can read back a truncated result and coordinate with live peers).
Only `description` is required; it is shown to the model in the run_agent tool's
description so it can pick a type, and an unknown `agent_type` is an
is_error result naming the available ones. Sub-agents still can't spawn
further sub-agents, and they share the parent's permission gate — the human's
last word doesn't loosen inside a sub-agent. `--mock` reads no config, so it
sees no types.

Sub-agent transcripts are persisted in their own `{parent}-agent-N.jsonl` files with a `subagent_of` back-pointer, synchronous and background dispatch share that audit path, and sub-agent hooks/tool events carry the agent label. `run_agent {"background": true}` plus `wait_for_activity`/`stop_agent` and the completion inbox are described below. Still deliberate: per-type effort/max-turn policy is not exposed; `max_rounds` remains a per-call native guardrail.

### Worktree isolation

kloop has two intentionally separate worktree lifecycles. Both use the shared
provenance-aware Git implementation in `crates/core/src/worktree.rs`, but they
do not share ownership, an active slot, or deletion rights:

- **Agent isolation**: `run_agent {"prompt":"...","isolation":"worktree"}` creates an
  agent-owned checkout from the current HEAD. A clean agent worktree is removed when
  the agent finishes; a worktree with uncommitted changes, commits, or an uncertain
  Git probe is retained and reported. Parallel agent worktree mutations are
  serialized per Git common directory.
- **Session worktrees**: at depth zero, and only when the current frontend
  enables the worktree surface, the model may create or enter a worktree and
  move the whole session into it. `kloop --worktree[=<name>]` is the separate
  CLI startup shortcut; its optional CLI value is not the model-tool schema.

The session tools use strict inputs:

```json
{"name": "feature/parser"}                        // enter_worktree
{"path": "/absolute/registered/worktree"}        // enter_worktree
{}                                                 // generated name
{"action": "keep"}                               // exit_worktree
{"action": "remove", "discard_changes": true}  // exit_worktree
```

`name` and `path` are optional but mutually exclusive. Explicit nulls, wrong
types, and unknown fields are rejected. Names are at most 64 characters and
may use `/`-separated ASCII letter/digit/dot/underscore/dash segments; `/` is
encoded as `+`. Managed trees live at `.claude/worktrees/<encoded-name>` on
`worktree-<encoded-name>`. An existing `path` must canonicalize to a registered
worktree with the same Git common directory. Entering by path grants
**External** custody only.

`exit_worktree` always requires an explicit action. `keep` restores the base cwd
and leaves the checkout and branch intact. `remove` may delete only the
Managed tree created by this session. By default it refuses tracked, staged,
untracked, or ignored changes and commits after the recorded base;
`discard_changes:true` may override only those successfully observed content
changes. It never overrides repository/path/registration/branch/base/owner
provenance failures, and it cannot delete an External, task-owned, or
previous-session tree. A refused/failed removal keeps the active handle and cwd
so the operation can be retried or kept safely.

Entering a session tree changes the complete effective workspace anchor:
Read/Write/Edit, Glob/Grep, Bash, permissions, sandbox writable roots, fresh
file-observation state, and the system prompt's `Working directory:` all follow
the new cwd. Main checkout and linked worktrees with the same `ProjectId` share
one live durable project policy; their `WorkspaceId` session-cache partitions do
not. An isolated worktree starts with an empty partition, and exiting restores
the still-live base partition. Successful enter and exit operations emit
`CwdChanged`, which keeps the TUI header/search root and server cwd projection
synchronized. A shared child agent inherits the active effective cwd; an
isolated task branches from that workspace's current HEAD into its own
task-owned tree.

There is no automatic merge, rebase, commit, push, or discard. Session shutdown
has no implicit remove intent, so an active session tree is retained even when
clean. Task-owned clean trees still use their separate automatic cleanup rule.
Use `git worktree list` to inspect retained trees and merge or remove them
explicitly.

## OS sandbox (Phase 2, thirteenth slice)

On macOS, bash commands run inside a seatbelt sandbox by default
(`crates/core/src/sandbox/`, executed via `/usr/bin/sandbox-run_program` with a
deny-by-default SBPL profile — the shape cc and codex converged on):

- **Writes** are allow-listed: the current workspace-derived root + `/tmp` +
  `$TMPDIR` + configured explicit extras. Entering or creating a worktree
  **replaces** the derived root rather than appending to the old policy; tmp roots
  and explicit extras survive, and the old main checkout remains writable only
  when the user explicitly listed it as an extra. Inside a writable root,
  `.git/hooks`, `.git/config` and `.kloop` stay read-only (they are
  privilege-escalation surfaces — hooks and git config run code, and project
  `.kloop` contains agent instructions/state; the rest of `.git` stays writable
  so `git commit` works sandboxed). The user-private `~/.kloop` state tree is a
  separate recursive deny-read/deny-write boundary.
- **Reads** are full-disk; **network** is off unless configured.
- **Sandboxed = fewer questions** (`auto_allow`, default on): a bash call
  the sandbox will contain skips the asking layers of the permission gate —
  opaque scripts (redirects, subshells) included, since OS containment
  replaces parse-level vetting. Deny rules, safety checks (a visible
  `rm -rf` still confirms) and explicit ask rules stay in force above it.
  The accepted trade-off: a contained command can still modify the workspace
  without a prompt — protected `.git` internals aside, git history is the
  recovery path. `auto_allow = false` reverts to pure containment (approve
  first, then run sandboxed). `--permission-mode bypass` bypasses approvals but
  not the sandbox.

On platforms without an OS shell sandbox, worktree separation still anchors
ordinary relative paths and parallel edits, but it cannot contain a model that
deliberately writes the old checkout by absolute path. Windows process Job
Objects contain process lifetime, not filesystem access.

When a sandboxed command fails and the failure looks like a sandbox denial
(keyword match ported from codex, plus DNS-failure shapes when the sandbox
disables network), there are two escalation paths:

- **Code-level loop** (`escalate`, default on — codex's retry-on-denial):
  the tool asks once ("run this without the sandbox?") and, on approval,
  re-runs the command unsandboxed within the same tool call — one fewer
  model round-trip. `--permission-mode bypass` auto-approves it; declining keeps
  the failure and steers the model to a different approach. This is the default.
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

`KLOOP_SANDBOX=off` is the env escape hatch. Linux currently has no OS
filesystem/network sandbox and runs Bash without that containment, while the
permission gate and Unix process-group ownership remain active. Native Windows
also has no restricted-token/AppContainer filesystem/network sandbox, but every
model-controlled Bash or PowerShell process is still assigned to a mandatory
Job Object before user code runs; `[sandbox]`, `KLOOP_SANDBOX=off`, and
`disable_sandbox` never disable process-tree ownership. A missing
`sandbox-run_program` on macOS warns and falls back to the permission gate plus
process group. Sandboxed processes see `KLOOP_SANDBOX=seatbelt` (and
`KLOOP_SANDBOX_NETWORK_DISABLED=1`) as detection hints. `--mock` never
sandboxes.

## Root-owned session task graph (Plans 71–74)

The Task graph is a **session-scoped structured work graph owned by the
root/main Agent**. Only depth 0 receives or may execute its five native
snake_case Task tools:

- `task_create {subject, description, blocked_by?}` creates a pending task and
  returns an opaque stable ID (`"1"`, `"2"`, …). If the existing non-empty graph
  is entirely completed and the new task has no dependencies, the same write
  transaction atomically rolls to a new epoch containing only that task.
- `task_get {task_id}` returns the full record, including direct `blocked_by`
  dependencies and the computed reverse `blocks` projection.
- `task_update {task_id, ...patch}` atomically changes subject, description,
  status, or the complete `blocked_by` list.
- `task_list {}` returns compact records in numeric-ID order; use `task_get`
  for the full description.
- `task_clear {}` explicitly abandons the graph without clearing conversation
  history or stopping Agent, Program, Workflow, or Bash work. It preserves the
  stable-ID high-water mark.

Statuses are `pending | in_progress | completed`. They move only forward:
`pending` may become in-progress or completed, and in-progress may complete;
completed tasks cannot reopen. A task cannot enter a non-pending state until
all blockers are completed. Missing dependencies, self-dependencies, duplicate
edges, cycles, invalid text, and failed rollover/clear validation fail atomically
without consuming an ID or partially changing the graph. Completed tasks remain
addressable until rollover or clear; there is no single-task delete, filter,
pagination, metadata, or active-form field.

The registry is an `Arc<TaskRegistry>` on `Config`. Child Configs retain the Arc
as an internal session service, but their depth>0 catalogs omit all five tools
and the dispatcher rejects stale or forged calls before allowlists, hooks,
permissions, or registry handlers. Foreground children return through their
`run_agent` tool result; background children return through `SubAgentResult` in
the parent Inbox. Neither path automatically changes a task: root decides when
to call `task_update`. There is no per-child task list, assignment/owner field,
Team claim, or task-to-execution binding.

Independent CLI sessions/native server threads and a resumed process get fresh
empty registries. An in-process TUI fork keeps the same live registry; the graph
is not written to rollout or reconstructed from history. Every panel-visible
mutation publishes a revisioned canonical full snapshot. `/clear` preserves the
ID high-water mark, unconditionally advances the graph revision, and hands the
TUI its exact empty snapshot as a reset fence against late older events. The
registry is bounded to 256 tasks, 256 blockers per task, 200-character
single-line subjects, and 8 KiB descriptions. Task rows, activity, the composer's
canonical visual rows, and overflow commit all consume the same viewport-height
budget; no independently counted string-line total can make live chrome freeze
into scrollback.

Task calls still use the ordinary `toolCall` lifecycle. A successful
panel-visible mutation additionally emits internal `TaskGraphUpdated`; only the
TUI projects it as a read-only live graph immediately above the composer.
`Ctrl+T` toggles that projection without mutating the registry. Plain mode prints
no checklist, and server/headless add no Task notification, native item, or
public wire. The permission gate auto-allows these session-memory operations
(including in plan mode); create/update/clear are serial and get/list are
concurrency-safe. Program/Workflow JavaScript and real child Agents cannot call
these tools. `todo_write` and its old checklist/wire path remain deleted rather
than forming a second writable task model.

## Steering — mid-turn injection (Phase 2, fifteenth slice)

Type a message while a turn is running and it **steers** instead of being
dropped: it queues, and is delivered to the model as a user message at the
next round boundary — never spliced into an in-flight request. It does not
interrupt the current tools (Ctrl+C stays the hard stop). This is a general
**step-boundary injection queue** (`Config.inbox`, a signalling `Inbox` of
typed `InboxItem`s); its producers are user steering, Local Agent Mailbox
messages and delivery failures, background sub-agent/program/workflow results,
scheduled prompts, and background-shell terminal pointers, each with its own
framing.

The mechanism is a straight drain of `Config.inbox` plus its typed Local Agent
Mailbox at round boundaries in the agent loop (`core/src/agent.rs`): at the
**top of each round** (delivering steers and peer messages that arrived during
the previous round before the next sampling), and again in an **end guard** —
when the model returns no tool calls, a late ordinary Inbox item is absorbed and
the turn continues instead of ending. Peer-message pending state is checked by
the same final gate but claimed only at the next round top, so one provider
sampling sees at most one bounded FIFO batch (8 messages / 32 KiB) and no message
is inserted into an in-flight request. Each injected item has producer-specific
framing: user steering says the user interjected; a Local Agent message carries
its `message-N`, sender, summary, and bounded body while explicitly identifying
itself as intermediate peer communication rather than a user instruction or
completion. It is recorded to history (and rollout) as a normal user message, so
it survives compaction and replays on resume; because it is recorded right after
the round's `tool_result` blocks (a separate user message), it never interleaves
tool results with regular text — the ordering constraint both cc and codex call
out.

Each **sub-agent gets its own fresh queue** (the `run_agent` tool resets it on the
cloned Config), so a running sub-agent never drains the parent's steering. TUI enqueues on Enter-while-running (the raw text shows as
a User cell); server mode enqueues via `turn/steer {threadId, input}` (while a
turn runs it folds in at the next round boundary; while idle the thread worker's
inbox-activity branch allocates a delivery turn). The plain REPL cannot accept a
second stdin line while `run_turn` owns the foreground, so it still has no
mid-turn steering input; its idle loop does select inbox activity for background
result/scheduler delivery. All three share the same boundary-safe drain path.
This is the cc/codex convergence: steering is enqueue-not-interrupt, delivered
only between steps (see `refs/README.md`).

## Slash commands (Phase 2, sixteenth slice)

An input line starting with `/` is a **built-in command**, not a message to
the model. The set is small and lives one-file-per-command under
`core/src/commands/` (the directory listing *is* the catalog):

- `/help` — list the commands.
- `/provider` — show the configured providers, or `/provider <provider>
  [model]` to switch the session route (see **Provider catalog and session
  route** above; the TUI opens a picker when called bare).
- `/effort` — show the session reasoning effort, `/effort <level>` to set it
  (`none`|`low`|`medium`|`high`|`xhigh`|`max`), `/effort unset` to send no
  effort field at all (see **Reasoning effort** below).
- `/cost` — the current model and context-window estimate (`~used / window
  tokens (pct%)`, from the resettable usage anchor + char/4 tail estimate), plus
  durable provider-reported usage across all models in the current transcript:
  input, output, cache-read input, cache-creation input, and reported-response
  count. An empty ledger is `unavailable`; a reported all-zero response remains
  available as four zeros. The command reports no prices, billable total,
  quota, or budget, and it does not aggregate child-agent transcripts.
- `/compact` — summarize and shrink the conversation now, instead of waiting
  for the predictive/reactive triggers.
- `/clear` — empty the conversation and start fresh (cc/claw semantics: an
  append-only compacted-to-nothing marker that resume replays to empty; it
  does **not** fork a new session file). The same transcript's durable
  provider-usage ledger remains cumulative. Process state (the root-owned task
  graph, steering queue, and deferred-tool capability receipts) resets too.
- `/exit` — quit. The TUI and plain REPL exit (the TUI with the same clean
  teardown as a two-tap Ctrl+C; plain also exits on one Ctrl+C); in server mode
  it is inert — quitting one thread must not stop a multi-session process, so it
  just relays a note.

**Reasoning effort.** `/effort` is a session-local knob over one bounded
vocabulary — `none`, `low`, `medium`, `high`, `xhigh`, `max` — that each rail
renders into its own request field: Anthropic `output_config.effort`, Responses
`reasoning.effort` (with `summary: "auto"`), Chat `reasoning_effort`. Those six
are the levels live endpoints were measured to accept; a seventh, `minimal`, was
carried in the first draft from a stale prior and removed once every model
measured refused it.

`none` is the one level that is not merely a different field name: on the
Anthropic rail "do no reasoning" is the *thinking* parameter, so `/effort none`
sends `thinking: {"type":"disabled"}` and no `output_config` at all, and it
outranks a profile's own `thinking` setting (it is the later, session-level
instruction). Every other level rides `output_config.effort` and leaves the
configured thinking mode alone.

**Which subset a model accepts is the model's own contract, not the rail's**, so
kloop enforces only its own spelling (`/effort hgih` is refused locally) and lets
the model's error settle the rest — those errors name the supported set, and a
per-rail table was measured wrong in both directions before it was deleted.
`/effort unset` sends no field at all, leaving the provider's own default in
force; that is also the startup state, so the chat rail's field (which only
reasoning models accept) never appears unless asked for. It is deliberately not
spelled `off`, because `none` is a real level meaning "do no reasoning" and the
two would read as synonyms. The initial value comes from `KLOOP_EFFORT` >
the top-level `effort` key > the selected profile's own `effort`.

A change applies from the next turn: the effort rides the frozen provider route,
so child agents and compaction sample at the same value, and it appears in the
TUI footer and in `ActiveProviderRoute.effort`. It is deliberately **not** part
of the durable route timeline — route revision and receipts are route *identity*
(what reasoning replay is matched against), and an effort change leaves recorded
reasoning replayable. A resumed session therefore re-seeds effort from
configuration. A `/provider` switch carries an explicitly set effort along
(including an explicit `unset`); a session that never ran `/effort` follows each
provider's configured value. Changing effort mid-conversation invalidates the
Anthropic prompt cache (the request prefix changes), so the next turn re-pays
cache creation.

An unknown `/name` lists the available commands (the same discoverable shape
as an unknown `agent_type`). Commands run **only when idle** — they read or
rewrite History, which a running turn is otherwise using; while a turn runs, a
`/`-line is just steering text. The plain REPL runs them inline (it owns
History); the TUI routes them to its worker (which owns History) so a slow
`/compact` shows as busy and Ctrl+C interrupts it like a turn. Only cc has
this feature among the three references (claw has built-in slash but no user
templates; the codex checkout has neither) — the shape follows cc.
Server mode runs the same commands: a `turn/start` with a `/`-prefixed input is
dispatched to the command layer in the thread worker (see the server section
above). User-defined `.kloop/commands/*.md` prompt templates plug into this same
seam — see **User commands** under Skills below.

## Code mode (Phase 2, seventeenth slice)

The `run_program` tool (`core/src/tools/codemode.rs`) is CodeAct: instead of one
`tool_use` per step, the model writes **a JavaScript program** that orchestrates
tools and sub-agents — loops, fan-out, pipelines and filters expressed in code.
Intermediate results stay in program variables; only what the program `return`s
comes back to the model, so a 100-item loop is one tool_result instead of 100.
`log(...)` streams live to the user as the program runs — progress narration for
the human, deliberately kept out of the model's context. This is the industry's
"code mode" pattern (Cloudflare coined it; codex
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
type AgentOptions = { agent_type?: string; max_rounds?: number };
type OrchestrationScope = {
  agent(prompt: string, opts?: AgentOptions): Promise<string>;
  parallel<T>(thunks: Array<(scope: OrchestrationScope) => Promise<T> | T>): Promise<Array<T | null>>;
  pipeline(items: any[], ...stages: Array<(prev: any, item: any, index: number, scope: OrchestrationScope) => any>): Promise<any[]>;
};
declare function agent(prompt: string, opts?: AgentOptions): Promise<string>;
declare function log(msg: unknown): void;
declare function parallel<T>(thunks: Array<(scope: OrchestrationScope) => Promise<T> | T>): Promise<Array<T | null>>;
declare function pipeline(items: any[], ...stages: Array<(prev: any, item: any, index: number, scope: OrchestrationScope) => any>): Promise<any[]>;
```

`parallel` is a barrier (all thunks, failures→null); `pipeline` runs each item
through every stage as its own chain with **no barrier between stages** — a fast
item reaches stage 3 while a slow one is still in stage 1. Concurrent callbacks
must use their explicit scope: `parallel([(scope) => scope.agent(...)])`, or the
fourth argument of a pipeline stage. Nested fan-out uses `scope.parallel` /
`scope.pipeline`. Calling global `agent`/`parallel`/`pipeline` from such a
callback fails closed because it has no topology-stable resume identity. Global
`agent()` remains available before/after helpers and for top-level
`Promise.all` calls whose invocation order is explicit.

The `tools` API and its TypeScript declarations are generated from the tool
schemas and carried in `run_program`'s description (typed declarations markedly
improve how reliably models call tools — the references converge on this).
**External source (MCP) tools are exposed too**, not just built-ins: below the
defer threshold they get full typed declarations alongside the built-ins; past
it (too many to type in full) they degrade to a compact name + description
manifest but stay callable — `tools.<server>__<tool>(args)` routes through the
same gate. A program call bypasses only the *deferred-tool lock* (the tool is
already exposed on `tools`, so it is "loaded" for the program's purposes); the
top-level model still `tool_search`es to direct-call, and every other gate
(deny, permission, sandbox, hooks) applies unchanged. This is "code execution
with MCP" — orchestrate several MCP tools + built-ins + `agent()` in one program
with intermediate results off the context window.

An MCP tool resolves to its structured `CallToolResult` object
(`Promise<CallToolResult>` — `.content[]`, `.structuredContent`), not flat text,
so a program reads typed fields without parsing. Built-ins stay `Promise<string>`
— except `glob`, whose result is naturally a list, so a program gets a
`string[]` of paths. Both ride a per-call sink on `ToolCtx`: `execute_tool` drops
the structured value there and the bridge returns it to the program, while the
model-facing tool_result still gets the flattened text. An `isError` MCP result
throws (the program `try/catch`es it), same as a built-in. (This follows
codex, whose built-ins are strings by default and objects only where the data
is inherently structured; `grep` stays a string — its shape is output-mode
dependent, so a program splits its lines per mode.)

**The safety story is enforced twice.** The QuickJS prelude captures its raw
host functions in a private closure and removes `__call_tool`, `__agent`,
`__log`, and `__phase` before model source runs. Independently, core gives
`CoreBridge` the exact per-run callable catalog and rejects any forged/noncatalog
name before hooks, permissions, or dispatch. Every legitimate
`tools.<name>(...)` and `agent(...)` then re-enters the same gated path as a
direct call — `run_one` (catalog → deferred lock → hooks → permission gate →
sandbox → execute) and `run_agent_tool`. A denied tool is refused *inside* the
program (the model catches the exception); a sandboxed command is still
sandboxed. Recursive runners and background control (`run_program`, `workflow`,
`run_agent`, wait/stop) are neither described nor host-callable from Program.
The `kloop-codemode` crate is engine-only and knows nothing of permissions; it
calls back through a `HostBridge` trait, which `core/src/tools/codemode.rs`
implements over the gate — that inversion avoids a crate cycle. `run_program`
itself is auto-allowed (like `run_agent`): it touches nothing directly.
`Promise.all` tool calls retain the normal concurrency rule (read-only calls
batch, writes take an exclusive lock).

**Resource limits** (`Limits`, per Program/Workflow run) are two layers. Engine
limits guard the interpreter: a QuickJS heap cap, a stack cap, and an interrupt
handler that kills a runaway synchronous loop (a CPU-burst deadline that ignores
await-suspended time) or a user Ctrl+C. Orchestration limits are:

- `max_agents` — hard cap on total live-miss `agent()` calls (default 1000); the
  next call throws instead of allowing an unbounded loop.
- `max_concurrency` — live child sampling cap (default 16); excess calls wait on
  a cancellation-aware semaphore, so independent pipeline items keep flowing
  without launching thousands of model requests. Journal hits consume no slot.
- `max_items` — hard cap on one `parallel()`/`pipeline()` input (default 4096);
  over-limit calls throw and never silently truncate.

All six knobs override via `[codemode]` in global `~/.kloop/config.toml`
(`memory_mb`, `stack_kb`, `cpu_secs`, `max_agents`, `max_concurrency`,
`max_items`) or matching `KLOOP_PROGRAM_*` environment variables (environment
wins). Detached Agent/Program/Workflow executions retain their separate,
session-wide cap of 8.

A running program is observable, not a black box: each `tools.<name>(...)` and
`agent(...)` shows as its own tool line (the ops go through `run_one`, which
emits the same UI lifecycle a direct call does) and `log(...)` prints live.

**Background programs**: `run_program {"background": true}` fires and forgets.
Its optional `description` follows the same 200-character display-only contract as
Agent and falls back to a first-line source preview. Validation happens before the
run store is opened; description never enters `source.js`, the manifest, source
identity, or journal replay keys. The launch response has two intentionally
different identities: transient
`program-N` belongs to this session and is the only ID accepted by
`stop_program`; durable `run-*` names the persisted source/journal and is the only
ID accepted by `resume_from_run_id`. The result is delivered to the parent as a
later message, so a long fan-out/migration does not hold up the turn. Oversized
successful results use the same offload store as tool results: history receives
a bounded head/tail preview plus a `read_offloaded` pointer, not an unbounded
user message. Background Program shares `BackgroundExecutions`, inbox activity,
`wait_for_activity`, and idle autodelivery with background Agent and Workflow.
Both Program Running and its unique terminal `BackgroundTaskUpdated` carry the
same durable `run-*`; native wire projects it through the existing optional
`runId` field without adding a turn owner or changing protocol version. The shell registry stays separate because a shell has its own output file and
reinjects only a terminal pointer. Deliberately **not** copied from codex: its
cell/observation-frontier machinery (incremental pull-based output streamed to
the model between `yield`s) — kloop is push-based on completion and `log()`
already streams progress to the user live.

**Journal resume v3**: every new Program run atomically persists the original
`source.js` and a versioned manifest before any worker starts. Resume first
validates the Program namespace/run ID and requires byte-identical source; a
changed source or a run without the source contract fails before opening the
journal or spawning an agent. Each completed live `agent()` is stored as a
structured record containing its topology call ID, complete prompt/options JSON,
result, and its validated private child-execution receipt in
`.kloop/program-runs/<run-id>/journal.jsonl`. Top-level calls use a root ordinal;
scoped helper calls include helper/branch/item/stage and local ordinal. A hit
requires both identity and complete input, so opposite future completion orders
and repeated prompts cannot cross-wire results. Journal v1/v2 and future-version
entries are safe cache misses; there is no migration or dual-write path. A
missing or invalid receipt on an otherwise valid v3 result remains unavailable
audit evidence rather than becoming a cache key or execution checkpoint. Only
successful `agent()` calls are journaled. Replay is best-effort memoization of
that model call — it does not make generated text deterministic, prove unchanged
workspace state, or provide exactly-once semantics for external side effects.

Each Program attempt, including foreground and resume, also gets a fresh private
`program-N` linked to its durable `run-*` in bounded
`.kloop/program-runs/<run-id>/provenance.json`. The append-preserved v1 sidecar
stores at most 32 validated 2 KiB receipts in a 128 KiB file. It is private audit
metadata: no prompt, command, raw path, endpoint, credential, provider/model,
billing field, Event, Inbox body, or server wire field is stored there. Missing
metadata starts with the current attempt; malformed/unknown/oversized history is
left untouched and cannot block execution or authorize/infer a route.

**Not done** (deferred, with reason): a token `budget` primitive — cc's
`budget.total` ships as a hardcoded `null` placeholder (its hard cap never
fires), and kloop has no turn-level budget source, so a budget object would be a
no-op until there's a real source to feed it; a richer progress view (cc's
`/workflows` tree — kloop shows a flat live trace); saving a program as a named,
reusable command (cc does this by writing a file into `.claude/workflows/` — for
kloop that's the same seam as user commands, the User-commands slice under
Skills, not a code-mode one). See `docs/plan/24-code-mode.md` and
`docs/plan/27-codemode-mcp-tools.md`.

## Workflow orchestration (Plan 53)

`workflow` is a separate, depth-0-only orchestration tool; it is not an alias
for `run_program`. It always launches in the background and returns a
`workflow-N` execution id, a stable `wf_*` run id, the managed script path, and
resume guidance before any agent work completes. `stop_workflow` accepts only
the execution id; the durable run id is only for resume. The script must begin
with a pure-literal
`export const meta = {name, description, phases?}` declaration and then has only
these host capabilities. `meta.description` is the sole display source for launch,
lifecycle, manifest, and UI; the legacy top-level `description`/`title` inputs are
accepted-but-ignored and cannot override script metadata:

```ts
declare const args: unknown;
declare const meta: Readonly<unknown>;
declare function agent(prompt, opts?);
declare function log(message);
declare function phase(title);
declare function parallel(thunks: Array<(scope) => unknown>);
declare function pipeline(items, ...stages /* (prev, item, index, scope) */);
```

There is deliberately no `tools`, raw `__*` host bridge, filesystem, network,
process, module import, `Date.now()`, or randomness in this runtime profile. The
raw agent/log/phase functions are captured privately and removed before script
execution. Workflow code controls deterministic topology; each child `agent()`
still re-enters the ordinary sub-agent runner and every child tool call still
passes the normal catalog → hook → permission → sandbox → executor chain.
Concurrent callbacks use `scope.agent` and propagate nested helpers through that
scope, exactly as in Program. `parallel` is a barrier and `pipeline` is
per-item/no-stage-barrier. `phase()` updates display-only background detail and
`log()` emits live notes; neither is a checkpoint, transaction, idempotency, or
exactly-once boundary, and neither changes the return value.

Each run is stored under `.kloop/workflow-runs/<run-id>/`; a versioned
manifest governs its managed script, args, journal, terminal result/error, and
private bounded `provenance.json` attempt history. Every launch/resume receives a
fresh `workflow-N` while retaining the same durable `wf_*`; the Workflow itself
is explicitly not a local mailbox peer. A later call with `resume_from_run_id`
may use an edited managed script: journal v3 reuses only `agent()` results whose
topology ID and complete structured input still match, including native JSON
objects/arrays; moved or changed calls run live, and v1, v2, and future-version
entries are safe cache misses with no migration or dual write. A missing or
invalid receipt in an otherwise complete v3 result means only that historical
audit evidence is unavailable; it does not invalidate the cache result or infer
a live route. The sidecar follows the same 32-attempt/2 KiB-per-receipt/128 KiB
bounds and preserve-on-invalid policy as Program. Both remain best-effort model-
call memoization/audit evidence rather than workspace validation, authorization,
or exactly-once side effects.
`script_path` is accepted only when it resolves to that run's managed script;
arbitrary workspace paths, separators, traversal, and symlink escapes are
rejected. On Unix, namespace/run directories and artifact read/write/rename/
lease operations are descriptor-relative with no-follow; the non-Unix fallback
revalidates paths but does not claim race-hard reparse-point safety until the
future Windows backend lands. Background completion/failure is persisted as
`result.json`/`error.txt`, delivered with a bounded summary at a step boundary,
and observable through global `wait_for_activity`; cancellation uses
`stop_workflow {workflow_id}` with `workflow-N`, never durable `wf_*`.

Passing `schema` in a Workflow `agent()` call activates the internal
**`structured_output`** protocol for that child. The requested JSON Schema is
installed as a one-turn synthetic tool definition, then the host validates the
returned object/array/scalar again. Invalid values receive a paired error result
and bounded retry; a missing call receives a nudge; exhaustion rejects the
Workflow promise rather than falling back to unvalidated text. A valid value
ends the child turn and reaches JavaScript as its native JSON type. This
synthetic tool never appears in the main registry, `run_program`, a frontend
capability, or an ordinary child without `schema`.

Named/nested workflows, token budget, remote execution, and arbitrary file
resolution are intentional first-release omissions.

## Async sub-agents (Phase 2, eighteenth slice)

`run_agent` takes `background: true`: instead of blocking and returning the
sub-agent's final text, it returns a typed `agent-N` execution ID immediately and
delivers the result to the parent Inbox automatically when it finishes. Program
and Workflow use the same delivery boundary while retaining separate durable
identities:

| wire tool | UI product | execution ID (status/stop) | durable ID (resume) |
|---|---|---|---|
| `bash {background:true}` | Shell | `bg-N` | — |
| `run_agent {background:true}` | Agent | `agent-N` | — |
| `run_program {background:true}` | Program | `program-N` | `run-*` |
| `workflow` | Workflow | `workflow-N` | `wf_*` |

Passing `run-*` to any typed stop fails closed and directs the caller to the
launch response's `program-N`; `wf_*` behaves likewise for Workflow. Background
control remains resource specific:

- `wait_for_activity {timeout_ms?}` is a non-draining global session activity
  barrier, not a status or output getter. Call it once only when the current model
  step truly needs to block for any shell, Agent, Program, Workflow, or Inbox
  activity. Results still arrive at the next step/final/idle delivery boundary if
  the tool is never called. A timeout is not a resource failure, consumes nothing,
  and must not become a short-period polling loop.
- `stop_agent {agent_id}` accepts only `agent-N`.
- `stop_program {program_id}` accepts only `program-N`.
- `stop_workflow {workflow_id}` accepts only `workflow-N`, never durable `wf_*`.
- Shells retain `bash_output {bash_id}` for file-backed output and use
  `stop_bash {bash_id}` for `bg-N`.

Passing an ID to the wrong stop tool fails and names the correct tool. The
executor also enforces the declared schemas: `wait_for_activity` rejects every
resource-ID field, and a non-boolean `background` never falls back to foreground
execution. This keeps agent results, code-mode results, Workflow artifacts, and
shell output files from collapsing into a misleading universal task handle.

Internally, every admitted Agent, Program attempt, Workflow attempt, and
background shell mints one immutable, validated execution-provenance receipt.
Its typed transient/durable identities, flat enclosing-execution reference,
Agent-only mailbox route, rollout references, workspace disposition, admission
authority, and terminal/delivery owner are frozen before registration or spawn.
Session/thread are domain-separated opaque references even though both currently
originate from the session ID. Program, Workflow, and Shell receipts cannot claim
mailbox membership; execution, durable run, worktree ownership, rollout, and
root Task identities are never parsed into one another. Receipts are core-private
and bounded to 2 KiB: Events, tool results, provider history, Inbox framing,
native/server wire, CLI/TUI/headless projections, Task ownership, and usage
accounting keep their existing shapes.

Mechanism: the detached sub-agent (its own tokio task, on its **own** cancel
token so a finished parent turn never kills it) reinjects its result into the
parent's `Config.inbox` — the same step-boundary queue as steering — as a framed
`InboxItem::SubAgentResult`, drained into history at the next round boundary
(the drain side was already built for steering; this is the queue's second
consumer). A short success passes through verbatim; an oversized success is
stored in the session offload directory and reinjected as a bounded head/tail
preview plus a `read_offloaded` pointer. Program success uses the same drain-time
rule. A failure is already truncated (~900 tokens, codex's cap) with
re-dispatch guidance; an **interrupted sub-agent reinjects nothing** (codex's
`is_final` — its partial output is noise, and cc diverges here by delivering a
`killed` partial). A `BackgroundExecutions`
registry (`core/src/tools/background_executions.rs`) tracks detached agents,
programs, and Workflows with the admission receipt itself, enforces one shared
concurrency cap (8), and reaps on session end. Its typed registration handle
fences attach/finish against the exact stored receipt, while stop input is parsed
only to locate the entry and provide wrong-tool diagnostics. It remains separate
from the background-shell registry because shell output is file-backed; that
registry stores its own typed shell receipt/registration. Both retain Plan 51's
atomic stop-vs-completion arbitration, so panic/forced abort still publishes
exactly one terminal state. Both registries project through the same session-
scoped `BackgroundTaskUpdated` event; this shared DTO is the compatibility seam,
not a forced internal merge. Agent, Program, and Workflow completion messages
retain their canonical typed IDs as `[Agent agent-N]`, `[Program program-N] run
run-*`, and `[Workflow workflow-N] run wf_*`; the private receipt is never placed
in the message, and only the result body is eligible for offload.

The TUI renders this event as a session-owned lifecycle row, not as a turn-owned
sub-agent row or an uncorrelated Note. Running/phase/terminal updates with the
same execution ID replace one mutable live-tail row. A Running row is normally
kept out of native scrollback; if the hard tail cap forces it into immutable
scrollback, later Running updates are ignored and the unique terminal update is
appended as a linked row with the same typed ID. `/clear` and fork rebuild reset
only the UI indices; a late terminal still starts a fresh identifiable row. This
is an event projection, not a resource manager: there is no list/hydration,
universal stop, status getter, or output panel.

**Autowake** closes the loop when the parent turn has already ended: every
interactive frontend subscribes to inbox activity and starts a delivery turn only
while the session is idle. The TUI dispatches a `Wake`; the plain REPL selects
between stdin and inbox activity; the native server's thread worker selects
between client turns and inbox activity and allocates a fresh monotonic turn ID.
A *running* turn drains at its own round boundary, so autowake never races a
second turn against it. Headless one-shot execution remains bounded and does not
expose this idle session surface; its teardown nevertheless reuses the selected
text/NDJSON UI sink, so a queued mailbox event still receives its shutdown
`undeliverable` terminal on the same stream.
Sub-agents cannot spawn further sub-agents, so background dispatch stays depth-0.

### Local Agent Mailbox (Plan 70)

Every real Agent at every depth has two strict built-ins:

- `send_message {to,message,summary?}` queues one bounded text message for an
  exact live peer address (`main` or `agent-N`). The runtime, not the model,
  supplies sender, `message-N`, opaque local context, and lifecycle state. A
  successful result means **queued**, not read, understood, replied to, or
  completed. The body is at most 8 KiB; the optional one-line summary is at most
  200 Unicode scalars / 1 KiB, or is derived from the first visible body line.
- `list_agents {}` returns the other currently open Agents in that session with
  parent, optional configured type, bounded description, and exact address. It
  is only a momentary local roster; a returned peer can close immediately.

The session directory uses one mutex to linearize address lifecycle, quotas,
enqueue/claim/ack, and close races. Per-target pending peer mail is bounded to 32
messages / 128 KiB; a sender can enqueue 64 messages; the session accepts at
most 256 messages / 512 KiB cumulatively. Quota failure allocates no ID and
emits no lifecycle event. System-generated delivery failures do not consume
model send quotas and coalesce by reason into bounded notifications containing
typed target/message-ID groups. A natural child close first rejects later sends,
then waits for already committed mail to cross a round boundary. Forced close
or session shutdown resolves every unclaimed message exactly once as
`undeliverable` and wakes a still-live sender with message IDs only; already
committed delivery claims are never double-resolved. Final route removal releases
the closed Agent's Inbox and metadata rather than retaining session-long
tombstones. Message lifecycle is an independent core event and never mutates the
Agent's `BackgroundTask` terminal or completion delivery.

The TUI upserts one body-free row per message ID; plain output uses the same
bounded lifecycle note; native server/headless JSON emits the independent
`thread/agentMessage/updated` notification documented above. The sender's tool
event input also contains only target, summary, and byte count, while its
canonical audited tool input and the recipient's framed history retain the body.
Program/Workflow JavaScript bridges cannot call either tool directly, although
real child Agents launched by those runtimes can. Messaging does not transfer
permissions, workspace, sandbox, hooks, or tool catalogs.

Foreground `run_agent` blocks the parent's model loop, so the parent cannot send
a mid-run follow-up from that same loop; use `background:true` when live
parent→child coordination is required. Final foreground/background completion
still travels through the existing tool result / `SubAgentResult` path and is
not inferred from a peer message.

This is an **A2A-aligned local envelope**, not A2A support. The local `to` is an
in-process transport address and is excluded from an A2A Message projection;
`message-N` is not an A2A Task ID, and `agent-N` is not an Agent Card endpoint.
Remote discovery, Agent Cards, HTTP/JSON-RPC/gRPC, streaming/push, authentication,
A2A Task lifecycle, and artifacts require a separate gateway.

Plan 52 fixed the semantic boundary against Claude Code 2.1.220; Plan 66 later
renamed the native surface without adding compatibility aliases:

- kloop exposes `run_agent` and defaults to **synchronous** execution; Claude
  Code `Agent` requires both `description` and `prompt` and defaults to background
  unless `run_in_background:false` is explicit.
- kloop exposes the native snake_case `task_create/get/update/list/clear` graph
  above only to the depth-0 root Agent, not as PascalCase Claude Code adapters
  or a child/Team collaboration surface. Child completion is an execution
  result; root explicitly advances graph state. The TUI-only internal snapshot
  projection is not a public Task wire. There are no `TaskOutput`/`TaskStop`
  aliases: those names belong to execution resources in Claude Code, while
  kloop keeps graph state separate from Agent/Program/Workflow/Shell lifecycle.
- `wait_for_activity` is non-draining and ID-free. Typed `stop_agent`,
  `stop_program`, `stop_workflow`, and `stop_bash` deliberately replace a
  universal TaskStop façade.
- `Config.inbox` remains the typed step-boundary delivery queue. Plan 70 adds
  strict session-local `send_message` / `list_agents` over a separate live Agent
  directory; it does not connect remote/cloud or user team state.
- Consecutive synchronous `run_agent` calls remain dispatcher-parallel; detached
  agent/program/workflow work remains capped at 8 per session.

The Plan 52 executable report now consumes the native run_agent/task-graph/wait
surface while retaining the original Claude Code fixture corpus. Plan 66's
dispatcher tests separately lock all twelve cross-resource stop combinations,
the durable `wf_*` boundary, and strict background/wait parsing. Plan 71 added
Task V2 and removed the old checklist; Plan 72 supersedes only its child-sharing
contract by making the session graph root-owned and child execution result-only.
Plan 74 extends the current native graph to five tools and locks strict clear,
atomic rollover, ID high-water, revisioned full-snapshot ordering, ordinary
ToolCall rows, and the absence of public Task wire without changing any pinned
Claude Code raw/normalized fixture. See
`docs/plan/52-agent-task-team-parity.md`,
`docs/plan/66-background-tool-naming.md`,
`docs/plan/71-task-v2-session-graph.md`,
`docs/plan/72-task-v2-root-owned-session-graph.md`,
`docs/plan/73-task-v2-remove-owner.md`, and
`docs/plan/74-task-graph-tui.md`.

## Skills (Phase 2, nineteenth slice)

A **skill** is a reusable instruction pack the model can pull in on its own when a
task matches it. It follows the public Agent Skills format, so a skill downloaded
from the ecosystem works as-is: a directory `<name>/SKILL.md` with YAML
frontmatter (`name` optional — defaults to the directory; `description` required)
followed by a markdown body:

```markdown
---
name: commit
description: Write a conventional-commit message and commit. Use when the user asks to commit changes.
---

Run `git diff --staged`, then write a Conventional Commits message and commit
with it. Scripts live in ${CLAUDE_SKILL_DIR}/scripts. The change to describe: $ARGUMENTS
```

Discovery (`core/src/skills.rs` parses; the CLI walks the dirs): project
`.kloop/skills/` (cwd-relative), then global `~/.kloop/skills/`. Drop an
ecosystem skill's directory into either and it works as-is — the SKILL.md format
is what matters, so kloop scans only its own `.kloop/`, not cc's `.claude/`. The
project layer wins on a name collision, so it overrides a global skill; a
malformed skill (no `description`, bad frontmatter) is skipped with a startup
warning, never an error.

Two ways to trigger a skill, both expanding the **same** body:

- **The model selects it** by description. Only `name` + `description` ride the
  injected context (progressive disclosure — the body stays out until triggered),
  alongside the deferred-tools notice and session-stable for the prompt cache.
  When a task matches, the model calls the built-in **`skill`** tool
  (`skill({"name": ..., "arguments": ...})`), which — like `run_agent` — exists only
  at depth 0 and only when skills are loaded (a sub-agent gets a focused task,
  not the whole catalog).
- **The user invokes it** as `/name args` — the same slash seam as the built-in
  commands (`core/src/commands/`). An unknown `/name` lists skills alongside the
  built-ins. The expansion runs as a turn (recorded as a user message), in all
  three front-ends; `context: fork` is a model-delegation concern, so a
  user-invoked skill always runs inline in the user's own conversation.

A skill runs one of two ways (its `context` frontmatter field):

- **`inline`** (default): the expanded body becomes the `skill` tool's result,
  so the instructions enter the conversation and the turn continues — kloop
  returns the body as a tool result rather than queueing a separate user message
  like cc's SkillTool.
- **`fork`**: the body runs as an **isolated sub-agent** (reusing the `run_agent`
  machinery), and only its final result comes back — the skill's intermediate
  work (tool calls, scratch output) stays out of the delegating model's context.
  A `model` frontmatter field overrides the sub-agent's model, and
  `allowed-tools` restricts its tool set (a YAML list or space/comma string; cc
  tool names like `Read`/`Bash` are mapped to kloop's, kloop-native and MCP
  names pass through) — a capability limit, not a permission grant, so the tools
  still face the gate. A sub-agent can't spawn one, so a fork skill triggered at
  depth ≥ 1 degrades to inline.

Expansion substitutes `$ARGUMENTS` (the whole argument string), `$N` (the Nth
shell-split word, 0-indexed), and `${CLAUDE_SKILL_DIR}` (the skill's directory,
so a skill can point at its own bundled `scripts/*`). When the body has no
argument placeholder but arguments were given, they are appended as an
`ARGUMENTS:` line. A skill **does not execute anything itself**: to run a bundled
script the model issues a normal `bash` call on the `${CLAUDE_SKILL_DIR}` path,
which faces the permission gate like any command. The `skill` tool is read-only
(activating a skill has no system side effect — cc never prompts for it); a fork
skill's sub-agent and any tool an inline skill's instructions later prompt are
each gated on their own.

Plan 54's exact 2.1.220 fixtures confirm this as the same model-visible,
progressively disclosed capability, with an intentional surface/lifecycle
difference: Claude Code names the tool `Skill {skill,args}` and queues the
expanded body as a companion user block; kloop keeps `skill {name,arguments}`
and returns the inline/fork result through the normal tool-result path. User
commands remain slash-only rather than becoming model-invocable skills.

Skills converge across cc (full system) and claw (archived subsystem); the
codex checkout has neither. **Not done** (deferred): `effort`
frontmatter (a provider-baked reasoning param), bundled files with lazy
extraction, `paths` conditional activation, usage-frequency ranking, remote
(`gs://`/`s3://`) and MCP skills, `` !`cmd` `` frontmatter shell expansion,
`${CLAUDE_SESSION_ID}` substitution. See `docs/plan/28-skills.md`.

### User commands (Phase 2, plan 36)

A **user command** is a `/name` shortcut you drop in as a single markdown file —
the same shape cc converged on when it folded its `.claude/commands/` into the
skills mechanism (`loadedFrom: commands_DEPRECATED`). kloop follows suit: a
command is **not a separate system**, just a second discovery root feeding the
same skill registry. Discovery adds project `.kloop/commands/*.md` (cwd-relative)
then global `~/.kloop/commands/`; each top-level `*.md` becomes one command
(subdirectory namespaces are deferred).

A command file is lighter than a `SKILL.md`:

- The **command name is the file stem** (`deploy.md` → `/deploy`); a frontmatter
  `name` is ignored (matches cc).
- **Frontmatter is optional**, and `description` may be omitted — it then falls
  back to the body's first non-empty line (markdown header stripped, truncated to
  100 chars), so the smallest command is a plain `.md` file with no `---` block.

```markdown
Run the deploy for $ARGUMENTS, then post the release notes.
```

Everything else is **reused** from skills: the same `$ARGUMENTS`/`$N`/
`${CLAUDE_SKILL_DIR}` expansion, and the same `/name args` slash seam (an unknown
`/name` lists commands alongside skills and the built-ins). The one deliberate
difference is **who may invoke it**: a command is a user shortcut, so it is
`/name`-only — it is kept out of the model's catalog and the `skill` tool
(`SkillSource::Command`; cc's legacy `disable-model-invocation` default), whereas
a `SKILL.md` skill is model-selectable. On a name collision a skill wins (the
directory form is fuller), so a command only fills a name no skill claimed. A
malformed command is skipped with a startup warning, never an error; `--mock`
skips discovery entirely.

**Injections** (`` !`cmd` `` and `@file`) run at `/name` expansion time — after
argument substitution, before the turn — so the model sees fresh output, not the
raw markers (`core/src/tools/inject.rs`):

- **`` !`cmd` `` / ```` ```!\n…\n``` ````** — an embedded shell command is
  executed and its output inlined in place. It runs through the **same permission
  gate a real `bash` call faces** (deny → safety checks → ask → approver → cache,
  then the OS sandbox): a `!cmd` is not privileged because a command author wrote
  it. A blocked or failed command aborts the expansion — the error is shown and
  no turn runs — while a command that merely exits non-zero inlines its output
  (with the `[exit N]` tail), like the bash tool. The inline form only fires when
  `!` sits at line start or after whitespace (so `foo!`bar`` and `$!` don't).
- **`@file`** — a `@path` mention that resolves to a readable file has its
  contents appended to the prompt (the mention stays in place). It honors the
  **read-path gate** (`read_path_blocked`: deny rules + the sensitive-path list,
  plan 31) — a blocked file is noted, never inlined — and a mention that isn't an
  existing file is left alone (so `@someone` prose is untouched). Contents are
  truncated to 100 KB.

Both trigger paths expand injections on the same body: the user's `/name` and a
model-activated skill (the `skill` tool). Because a `!cmd` always faces the bash
gate, "the model picked a skill that runs a command" is gated exactly like the
model calling bash directly — a `fork` skill expands before forking, so its
sub-agent sees the resolved output. (kloop has no remote/MCP skills, so cc's
"never run an MCP skill's `!cmd`" carve-out has no analogue.)

**Not done** (deferred): `allowed-tools` frontmatter pre-authorizing a command's
own `!cmd`; `@file#Lstart-end` line ranges and `@~/…` home expansion;
subdirectory `namespace:command`; commands entering the model catalog. See
`docs/plan/36-custom-commands.md`.

## Image input (Phase 2, twentieth slice)

The target models see (sonnet-5, OpenAI vision models); kloop can now feed them
images. `--image <path>` (repeatable) attaches local image files to the first
user turn:

```sh
cargo run -- --image screenshot.png        # then ask "what's in this screenshot?"
cargo run -- --image a.png --image b.jpg   # multiple images ride one turn
```

- **Accepted**: png / jpeg / gif / webp, sniffed from magic bytes (not the
  extension), each ≤ 5 MiB. Oversized images are refused (client-side resizing
  is deferred — shrink and retry). Only local files: a path that looks like a
  remote URL is refused outright, keeping the SSRF surface narrow (matching
  codex, which also takes base64 only).
- **Three rails, one canonical block** (`Image { source: Base64 { media_type,
  data } }`, Anthropic's shape): the anthropic adapter serializes it straight to
  `{type:"image", source:{type:"base64", …}}`; openai-chat translates to an
  `image_url` data URL (`data:{mime};base64,{data}`, `detail:"auto"`); Responses
  to `{type:"input_image", image_url:"data:…", detail:"auto"}`. Anthropic has no
  `detail` field, and kloop does not expose it yet (defaulting the OpenAI rails
  to `auto`).
- **Persistence**: images inline into the rollout as base64 — never offloaded,
  unlike large tool output, because the bytes must reach the model as-is. So
  `--resume` replays them and the model can still refer back to an image from an
  earlier turn. The TUI transcript shows a `[image: {media_type}]` placeholder
  for a resumed image (the base64 is not printed).

### Tool-read images (slice 2)

`read_file` reads images too, not just the user. Point it at an image file and
it returns the picture for the model to see (cc's Read is one tool for both):
the bytes are sniffed from magic bytes, and png/jpeg/gif/webp up to 5 MiB come
back as an image — offset/limit don't apply. A binary that is neither UTF-8 text
nor a supported image is a clean error.

```sh
# "look at logo.png and describe it" → the model calls read_file(logo.png)
```

- **`tool_result` content is `string | array`** (`ToolResultContent`): text
  results stay a bare string (old sessions round-trip unchanged); an image
  result carries a block array — exactly Anthropic's own `tool_result.content`
  shape.
- **Three rails again**: anthropic embeds the image natively in the
  `tool_result` (serde does it for free); Responses carries it natively too, in
  the `function_call_output` output items (`input_image`); openai-chat's `tool`
  role **cannot** hold an image, so the image is **relocated** to a trailing
  `user` message (`[tool output contains image data attached in the following
  message]` placeholder + a `Tool output for call_id …:` user message with the
  data URL) — codex's proven shape, so no information is lost on that rail.
- **MCP tools return images too**: an MCP tool whose result carries an image
  content block (`{type:"image", data, mimeType}`) is lifted into the same
  canonical image block (when the mime is one the models accept — png/jpeg/gif/
  webp; anything else stays an `[image: …]` tag rather than risk an API
  rejection). Surrounding text folds into text blocks around it. So an MCP tool
  that hands back a screenshot is seen, not summarized to a placeholder.

Verified against real keys: sonnet-5 (anthropic, native embed) and a vision
model (openai-chat, relocation) both read a secret token painted into a test PNG
via `read_file`; the image inlines into the rollout (not offloaded) and replays
correctly on `--resume`. MCP image results verified end-to-end on both rails (a
stub MCP tool returned a badge image; sonnet-5 via the native embed and
gpt-5.4-mini via relocation both read its text), with the canonical
`[text, image]` blocks inlined in the rollout. The Responses rail is covered by a
unit-test contract (no official Responses endpoint to hit — same as slice 1 /
plan 15).

**Not done** (deferred): client-side downscaling; a drop-images-when-unsupported
token saver / model-vision capability probe; per-request media-count cap;
PDF/document blocks; remote-URL images; exposing `detail`. TUI bracketed text
paste, single-line image-path paste/drag, and OS clipboard image attachment are
implemented; this dev-only PTY test surface is not a production interactive
process channel. See `docs/plan/29-image-input.md`.

## Headless mode (Phase 2, twenty-first slice)

`kloop --headless` runs one turn without a REPL or TUI and exits — the form
scripts, pipes, and CI need. cc's `--print` and codex's `exec` crate converged
here independently, so kloop takes the intersection (under a self-describing name
rather than cc's output-flavored `--print`; no short alias, headless is not a hot
path).

```sh
# prompt as a positional argument; the final answer is the only thing on stdout
kloop --headless "summarize what CHANGELOG.md says" > summary.txt

# or pipe it on stdin (both sources combine: positional first, then a newline,
# then stdin)
git diff | kloop --headless "review this diff"

# machine-readable event stream (reuses the native protocol's item events)
kloop --headless --json "fix the failing test" | jq -c 'select(.method=="item/started")'

# runaway guardrail for scripts; resume a session and run headless on it
kloop --headless --max-rounds 8 "keep going"
kloop -r 20260716-101500 --headless "and now write the tests"

# hermetic end-to-end demo, no key (great for CI)
kloop --mock --headless --json
```

- **Prompt = positional argument and/or piped stdin** (either alone, or both
  combined). Neither present is an error. `--headless` is a mode switch, not a
  value holder — the prompt never rides the flag itself (cc's shape).
- **Two output contracts.** Default (human): the final answer prints once to
  **stdout**, progress notes go to **stderr**, so `result=$(kloop --headless "…")`
  captures a clean result. `--json` (machine): every event is one NDJSON line on
  stdout — `turn/started`, `item/started|delta|completed`,
  `thread/backgroundTask/updated`, `thread/scheduler/updated`,
  `thread/tokenUsage/updated`, `note`,
  `turn/completed` — the **exact same item vocabulary the native protocol server
  emits** (via one shared `project_event`), `threadId` and all. One event
  vocabulary, two front-ends.
- **Approval defaults to deny.** There is nobody at the keyboard, so any
  permission ask is auto-denied (fail-safe, like server mode's "reply lost =
  deny"). Existing project grants, `--permission-mode accept-edits`/`bypass`,
  and sandbox auto-allow still act before the approver. Non-empty `KLOOP_ALLOW`
  is a startup error, not a headless override.
- **Interactive control surfaces are absent.** Headless installs neither a
  `Questioner` nor detached Workflow lifecycle, so `ask_user_question`,
  `enter_plan_mode`, and `workflow` are not advertised. It never reads stdin
  for a model-generated dialog or manufactures a default answer; use plain,
  TUI, or a questions-capable native-protocol client for those flows.
- **Exit code** is `0` on a clean finish, `1` on error, interruption (Ctrl+C),
  or hitting `--max-rounds`. Without that explicit flag, headless uses the same
  unbounded turn loop as interactive/server mode. In human text mode, a
  refusal/filter/incomplete response or exhausted output-limit recovery still
  prints any usable assistant text once, reports the stable failure on stderr,
  and exits `1`; a valid empty EndTurn writes no placeholder line.
- The session persists to `.kloop/sessions/` like every other mode, so a
  headless run is resumable (`--resume <id>`) and forkable afterward.
- **Scheduler shutdown.** After the main headless turn, kloop stops the scheduler before
  background task/shell shutdown. Session-only scheduled jobs disappear; durable jobs remain
  pending until their owner resumes in a questions-capable interactive frontend.

`--max-rounds` and `--json` are `--headless`-only; a bare prompt without it
is an error (interactive mode takes its input at the prompt). Not done (deferred, server
mode already covers the programmatic side): `--permission-prompt-tool`
delegation, `--input-format stream-json`, budget/goal guardrails,
`--output-schema`. See `docs/plan/33-exec-mode.md`.

`--help` and `--list-sessions` are local fast paths: they return before provider
credential resolution, runtime loading, and MCP connection. They remain usable with a
missing key or malformed provider config. The headless, provider, and compaction
contract regressions added in Plan 84 use only local fixtures; the retired claw-code
snapshot is not a runtime dependency.

## Running

Building the workspace requires Rust 1.96 or newer and uses Rust edition
2024. The repository's `kloop/rust-toolchain.toml` pins local development to
Rust 1.96.1 with rustfmt and clippy; CI's stable matrix explicitly invokes
`+stable` so it remains independent from that local pin. The terminal stack is
Ratatui 0.30.2 with its explicit `crossterm_0_29` integration, Crossterm 0.29.0
(`event-stream` retained), `unicode-width` 0.2.2, and
`unicode-segmentation` 1.13.3. The Unix-only real-binary PTY harness uses
vt100 0.16.2; portable-pty, vt100, tempfile, and its fixture wiremock remain dev
edges rather than production or Windows dependencies.

```sh
# usage summary of every flag
cargo run -- --help

# keyless demo: scripted Mock provider exercises all five bets (plain output)
cargo run -- --mock

# The only automatically discovered TOML config is ~/.kloop/config.toml. It
# contains provider/model plus global permission deny/ask constraints, MCP, web,
# hooks, sandbox, agents, and codemode. Durable allow is separate user-private
# ProjectStore state. A cwd .kloop/config.toml is never read or merged; cwd
# remains the workspace anchor for project instructions, skills, tools,
# permissions, and sandbox paths.
#
# Daily provider route configuration lives in that process-global file. The
# directory must have no group/other access (kloop creates it as 0700), and the
# file must be mode 0600:
#
#   model = "gpt-5.6-sol"             # initial model, must be allowlisted
#   model_provider = "gw_router"     # initial provider profile
#
#   [model_providers.gw_router]
#   wire_api = "responses"             # responses | chat | anthropic
#   base_url = "https://example/v1"
#   http_headers = { Authorization = "Bearer ..." }
#   default_model = "gpt-5.6-sol"
#   models = ["gpt-5.6-sol", "gpt-5.6-mini"]
#   fallback_model = "gpt-5.6-mini"     # optional, in models only
#
# Every configured profile declares a stable id, API family, default model, and
# ordered model allowlist. Unselected profiles may be unavailable because their
# credential is absent; they remain visible with a bounded availability code and
# are checked only when selected. No model discovery or credential editing occurs
# at runtime. kloop appends /v1/messages, /chat/completions, or /responses.

# Environment variables only select a new session's initial catalog route; they
# cannot inject an undeclared provider or model. Provider selection:
# KLOOP_PROVIDER > model_provider. Model order:
# ANTHROPIC_MODEL/OPENAI_MODEL > KLOOP_MODEL > top-level model > profile default,
# but every result must occur in that profile's models allowlist. Credentials and
# base URL env overrides apply to the selected profile only; unselected profiles
# remain bounded-unavailable when their configured credential is absent.
#
# ANTHROPIC_API_KEY / ANTHROPIC_BASE_URL select Messages;
# OPENAI_API_KEY / OPENAI_BASE_URL select Chat or Responses according to the
# selected profile. KLOOP_CACHE and KLOOP_THINKING remain provider-local request
# settings. KLOOP_EFFORT (or the top-level effort key, or a profile's own effort
# key — same word at three scopes, in that precedence, valid on every wire_api)
# seeds the session reasoning effort that /effort then owns. Only the spelling is checked: which
# levels a model takes is the model's own contract, stated in its own error. KLOOP_FALLBACK_MODEL is not a runtime selector.
# Provider/search keys are stripped from model-controlled shell environments.
#
# Stream guards are fixed provider-internal safety defaults, not user config:
# 45s response-header open, 15m per-chunk idle, 30m wall-clock, 10 MiB total
# response, and 1 MiB per unfinished SSE frame. Anthropic requires message_stop;
# Responses requires response.completed/incomplete; Chat requires finish_reason
# ([DONE] only ends the transport). Complete frames and EOF residuals are strict
# UTF-8; malformed JSON, unknown semantic events, unclosed output items/parts,
# missing final tool identity, and non-object arguments fail closed. Responses
# message/function_call item status remains required. A reasoning item may omit
# status on added/done (a production wire shape); when present it must still be
# in_progress/completed respectively, and explicit null or another value fails.
# Optional Agent/Program/Workflow string controls likewise expose the same
# non-empty ID/metadata constraints enforced by their strict runtime parsers;
# nullable options use null for omission, while an empty identity remains invalid.
# run_agent/run_program description likewise accepts omission/null or a bounded,
# single-line string containing at least one non-whitespace character; empty or
# whitespace-only labels still fail before any Agent/Program side effect.
# run_agent advertises agent_type only when custom types exist, as null or an
# enum of the configured names, and does not invent a general-purpose alias.
#
# Adapters publish only Text/Thinking/RedactedThinking/ToolUse blocks plus a
# mandatory typed outcome. EndTurn, ToolUse, output limits, refusal, filtering,
# and incomplete responses are distinct: only output limits enter bounded
# continuation; refusal/filter/incomplete never retry, fallback, or dispatch a
# tool. ToolUse must agree bidirectionally with unique, valid tool blocks. A
# valid empty EndTurn creates no assistant history placeholder. Display items
# with partial text close as failed on stream error, while completed blocks stay
# completed; native protocol 2.0 keeps the existing item/completed method and
# carries that distinction in the item's status field. Core turn errors retain
# AssistantOutcome or ProviderFailure rather than recovering either from text.
# Assistant history binds reasoning to provider endpoint identity + API family +
# exact final wire model. Messages/Responses replay only an exact match; legacy,
# cross-provider/family/model reasoning fails before I/O, while Chat strips it.
# Native thread/read and event snapshots omit replay provenance and opaque bytes.
#
# The ignored native primitive evaluator accepts all three rails. Its Program
# contract uses two explicit client turns: the first returns a Durable Run ID,
# the second supplies that exact ID and byte-identical source. It still requires
# two outer run_program calls and only one journaled child Agent spawn; runtime
# never auto-resumes or promises exactly-once ordinary tool side effects.
# Responses must be selected explicitly and needs KLOOP_EFFORT so it actually
# produces and validates reasoning items. Its real-test watchdog defaults to 900s
# (300s on other rails) and can be overridden with
# KLOOP_REAL_EVALUATOR_TIMEOUT_SECS=60..3600. Provide private OPENAI_* compatibility
# env without printing or committing it:
# KLOOP_PROVIDER=openai-responses KLOOP_EFFORT=high \
#   cargo test -p kloop --test real_agent_program_workflow \
#   real_agent_program_workflow_contract -- --exact --ignored --nocapture
# The cross-rail evaluator keeps both credentials in process memory (it does not
# write a temporary provider config) and runs Anthropic → Chat → Responses →
# Anthropic in one native session while checking route receipts, provenance,
# usage, Chat's no-reasoning history contract and public redaction:
# KLOOP_EFFORT=high cargo test -p kloop-server --test server \
#   real_three_rail_route_switch_contract -- --exact --ignored --nocapture

# line-based REPL instead of the TUI
cargo run -- --plain

# attach local images to the first user turn (repeatable) — see Image input
cargo run -- --image screenshot.png

# native agent protocol server on stdio (see Native agent protocol)
cargo run -- app-server            # alias: cargo run -- --serve
cargo run -- app-server --mock     # keyless: scripted provider behind the protocol

# sessions (-c = --continue, -r = --resume, mirroring cc)
cargo run -- --list-sessions   # what's on disk, most recent first
cargo run -- -c                # continue the most recent session (--continue)
cargo run -- -r                # pick a session from a numbered list (--resume)
cargo run -- -r <id>           # continue a specific session
cargo run -- --fork <id>#<seq> # branch off a session at line #<seq> (rewind)
cargo run -- --fork <id>       # branch off a session at its end

# MCP servers come from global ~/.kloop/config.toml — see MCP client above
# KLOOP_DEFER_THRESHOLD=<n> tunes when MCP tool defs defer behind tool_search
# (default 30 total tools; lower it to exercise deferral with a small server,
# raise it to effectively disable)

# permissions: global constraints come from TOML/KLOOP_DENY/KLOOP_ASK;
# project approvals persist in ~/.kloop/projects/v1/<ProjectId>/permissions.json
KLOOP_DENY='bash(git push *)' cargo run            # hard-block rules
KLOOP_ASK='bash(cargo publish *)' cargo run        # force confirmation
cargo run -- --permission-mode accept-edits        # auto-allow cwd file writes
cargo run -- --permission-mode bypass              # bypass (deny/safety still apply)

# OS sandbox (macOS Seatbelt; see OS sandbox above). This never disables Unix
# process groups or Windows Job Objects.
KLOOP_SANDBOX=off cargo run                        # remove OS fs/network sandbox only
```

Interrupting a running turn patches history so it stays legal either way. In
the TUI, Esc interrupts and Ctrl+C (two taps) exits (see the TUI section). In
`--plain`, one Ctrl+C cancels any running operation, waits for that repair, and
then exits; `exit` and `/exit` also quit. As in a conventional line-based REPL,
an actual stdin EOF ends input, but it is not advertised as an application key.
Every session is saved and resumable — see Session persistence above.

## Verification

`cargo test` runs the full workspace suite:

- **kloop-protocol** — wire-format contract (exact JSON shapes, `is_error`
  omission rule, role casing, serde round-trip).
- **kloop-provider** — history-translation unit tests plus wiremock HTTP
  contracts for Anthropic Messages, OpenAI Chat Completions, and OpenAI
  Responses: strict item/part/order/identity closure, typed semantic outcomes,
  delta/final-value agreement, usage capture, strict UTF-8 terminal tails,
  typed HTTP/timeout/protocol failures, fixed transport and SSE caps,
  Retry-After, fail-closed tool input, producer cancellation, and mid-stream
  death.
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
  acceptEdits cwd boundary, glob rules, WorkspaceId-partitioned session cache,
  ProjectId identity and durable ProjectStore publication/RMW, legacy
  `[permissions].allow`/`KLOOP_ALLOW` rejection, opaque never cacheable, `ConfirmRequest.preview` carrying an
  edit/write diff while other calls carry none); Windows shell contracts
  (Git for Windows layout discovery, conditional catalog, CreateProcessW
  suspended→Job assignment→resume fail-closed ordering, leader-exit/inherited-
  pipe cleanup, idempotent terminate, Drop and handle-count checks, fixed
  PowerShell EncodedCommand argv, PowerShell 7/5.1 native script/exit/error/
  large-output/descendant cases, and opaque permission truth table); change-preview generation
  (line-numbered added/deleted/changed lines, distant-hunk splitting,
  long-line clipping, modest diffs uncut vs the 500-line ceiling marker,
  write-file existing-vs-new-file
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
  (exact SBPL profile assembly and `-D` param list, workspace-derived root
  replacement across worktrees while preserving tmp/explicit extras, recursive
  private-state read/write denial, canonical/literal dedup, denial-detection
  table incl. the
  network-off DNS extension, `[no sandbox]` approval tag, auto-allow
  layering: contained calls skip asking while deny/safety/ask-rules
  outrank the sandbox; escalation-consent decision/mode mapping) plus
  macOS-only integration against the real `sandbox-run_program` (write
  inside/outside a writable root with the denial hint, protected subpath,
  per-call disable_sandbox escape, network denied where a bare run
  connects, background shell sandboxed with inherited-fd output, contained
  bash running with no approver present, the escalation loop re-running a
  denied command unsandboxed on approval and warning off retry when
  declined).
- **kloop-tui** — Ui/Approver→channel event contract (call order, confirm
  decision round-trip, dropped-reply-means-deny), App state folding (delta
  accumulation and splitting, tool status resolution by id, confirm queueing and
  keyboard capture, popup scroll keys with offset reset on advance,
  interrupt/quit commands, turn-end cleanup), and terminal-correct composer
  contracts: strong byte ranges, whole-grapheme cursor/edit/Delete over combining,
  ZWJ/skin-tone/flag/CJK input, one hard+soft visual-row layout for navigation,
  windowing, cursor projection and height, display-column goal preservation, exact
  completion ranges, stable paste-atom expansion/history/draft/steer behavior, and
  typed attachments. Pure rendering additionally covers grapheme-safe
  wrap/truncate, per-cell-kind lines, tool-row collapse, diff colors, live-chrome
  reservation, exact/narrow cursor bounds, and popup windowing through Ratatui
  `TestBackend`. TestBackend proves deterministic layout/composition and synthetic
  scrollback; it does not prove real terminal input or native scrollback retention.
- **kloop-server** — wire envelope contract (request/response/notification
  shapes, string-or-int ids, request-vs-approval-response disambiguation),
  plus duplex-driven exact native protocol 2.0 tests against the real serve loop:
  structured approval scopes, `acceptForProject`, protocol 2.0/`acceptAlways` rejection,
  delta streaming and completion, approval deny/allow
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
  calls routed by id. Streamable HTTP transport (plan 34) has wiremock
  contract tests: HTTP handshake + paginated tools/list echoing the
  `Mcp-Session-Id`/`MCP-Protocol-Version` headers, tools/call over an SSE
  response with an image block, bearer-header injection, 5xx retry then
  success, 401 terminal (no retry), and 404 session-expiry re-handshaking once.
  OAuth (plan 34b) is tested in `oauth.rs`: `WWW-Authenticate` parsing,
  path-aware discovery candidates, two-step discovery + DCR + token exchange
  over wiremock (verifier/resource sent, absolute `expires_at`), a real loopback
  callback (matching state, CSRF rejection, timeout), proactive + refresh-fail
  session behaviour, and full login end-to-end with a mock browser; the CLI's
  `mcp_auth.rs` covers the keyed 0600 token store. The transport's 401→refresh
  →replay is a `http.rs` test.
- **kloop-codemode** — the QuickJS engine in isolation: the async op bridge
  (a tool call returns a JS promise resolved from a Rust future), real
  concurrency proven with a 2-party barrier that a serial engine would
  deadlock, the `parallel` helper turning failures into null, `pipeline`'s
  per-item no-barrier flow (proven with a gate that would deadlock a
  per-stage barrier) plus its failure→null and stage-args contract, sandboxing
  (no fetch/require/process/console, import rejected), result coercion,
  program-error surfacing, and each resource limit (runaway-loop kill,
  cancellation, memory cap). Its core wiring (`tools::codemode`) tests the
  op layer re-entering the real permission gate (a denied tool refused
  inside the program while a read-only one passes), `agent()` spawning a
  real sub-agent, intermediate results staying off the result, `log()`
  streaming live to the UI (with the op's tool line ordered between two logs)
  while staying out of the result, and the TypeScript-API generation; an
  agent-level test drives one `run_program` tool_use over Mock and asserts the next
  request carries only the program's return value, never the content it read
  internally.
- **kloop (cli)** — argument parsing, UTC timestamp session ids (epoch,
  known dates, leap day), strict global deny/ask parsing with legacy allow
  rejection, private config/OAuth/ProjectStore I/O, per-ProjectId schema and
  lock-protected multi-process RMW, `[mcp.servers]`
  parsing (round-trip, malformed rejection), rule-safe name sanitization,
  and `[[hooks]]` parsing (round-trip, defaults, malformed rejection).
- **kloop TUI PTY (Unix only)** — `cargo test -p kloop --test tui_pty --
  --nocapture` launches the real default binary in a sealed `portable-pty`, serves
  deterministic loopback OpenAI-compatible SSE, answers every split/multiple CPR
  query, and feeds raw output incrementally to a zero-history `vt100` parser. Seven
  tests cover boot/bracketed-paste/no alternate screen, shrink/grow resize with
  continued CPR, ordered scroll-region→clear→repaint overflow facts, double-Ctrl+C
  restoration, and exact UTF-8 input through grapheme edits. Captured request
  projections exclude headers/Authorization and raw ANSI is bounded fail-closed.

Beyond the suite: `cargo run -p kloop -- --mock` exercises six scripted
rounds. Plan 63 acceptance also runs the real stdio binary for an exact native
protocol 2.0 handshake and protocol 1.0 refusal, plus the Desktop companion's Bun,
production-build, Rust test/clippy/fmt gates. Those deterministic gates cover
Once/WorkspaceSession/Project wire decisions and persistence-failure behavior;
they do not claim a manual GUI click-through or native Windows runtime where it
was not run. Earlier real-key adapter runs cover offload round-trips,
mid-session predictive compaction, and truncation recovery.

CI (`.github/workflows/ci.yml`, at the repo root) runs macOS, Linux, and native
Windows on every push/PR: `cargo +stable fmt --all -- --check`, locked
all-target/all-feature workspace clippy, locked workspace tests, the keyless
mock smoke, and the corpus-only verifier after an explicit Python setup. A
separate Linux job uses Rust 1.96.1 to record compiler/tool versions and run
`cargo +1.96.1 check --locked --workspace --all-targets --all-features`, keeping
the declared MSRV and final lockfile executable together. Composer/unit/render/
TestBackend correctness is therefore cross-platform. The real-binary TUI PTY
harness is `cfg(unix)` with Unix-only dev
dependencies: it proves CPR, input/resize, terminal mode sequences, and the
captured visible viewport on POSIX; a Windows skip/non-applicable build is not a
ConPTY execution pass. Its `vt100` screen has zero history and is deliberately not
used to claim DECSTBM/native scrollback retention or terminal-specific grapheme
shaping — TestBackend remains the synthetic scrollback oracle, and physical
Terminal.app/iTerm2 behavior is separate manual evidence. This test-only PTY does
not reopen Plan 30's production `write_stdin`/interactive-process decision.
Windows additionally keeps the Plan 61 file-safety gates and focused process-tree,
Bash, PowerShell, and permission selectors. The full exact-binary verifier remains
a pinned darwin-host check; Windows native tests do not create Claude Code Windows
parity evidence. On Windows, corpus-only still checks immutable fixture hashes,
normalization/tamper gates, generated matrix/pairs/bridges, cross-platform native
reports, and sensitive-data rules. POSIX descriptor/ctime/symlink/publication/PTY
self-tests and the Darwin-arm64-only Plan 59 report remain platform-gated rather
than being presented as Windows execution.

## Layout

Cargo workspace, ten crates; the main dependency graph remains a strict line up
to core, then two sibling frontends under the cli, with the MCP wire client, Web
network operations, and the QuickJS code-mode engine kept behind explicit seams.
The small `kloop-process-spawn` utility is shared by core/MCP/CLI/TUI solely for
process-wide child-creation serialization
(protocol ← provider ← core ← {tui, server} ← cli; protocol ← mcp ← cli;
web ← cli; codemode ← core):

```
crates/protocol/    kloop-protocol — zero-dependency leaf
  src/lib.rs        canonical wire types (Anthropic Messages shape),
                    StreamEvent, Usage, ToolDef

crates/provider/    kloop-provider — the adapter seam; owns reqwest
  src/lib.rs        Provider enum + stream() dispatch; Mock with scripted turns
  src/failure.rs    typed provider failures + retry metadata
  src/stream.rs     single-terminal stream owner + open/read/size guards
  src/anthropic.rs  Anthropic native SSE adapter
  src/openai.rs     OpenAI-compat chat/completions translation
  src/responses.rs  OpenAI Responses translation
  src/sse.rs        bounded incremental SSE parser

crates/process-spawn/ process-wide child-creation gate shared across crates

crates/core/        kloop-core — the agent, network-free
  src/config.rs     Config (construction is the caller's concern)
  src/history.rs    append-only history, record-time offloading,
                    context estimation, provider usage ledger ownership
  src/usage.rs      canonical per-response records + checked aggregate
  src/process_tree/ cross-platform owned shell process trees
    mod.rs          ProcessSpec/Child/Killer façade and lifecycle tests
    unix.rs         process-group spawn, kill, reap, and residual checks
    windows.rs      suspended CreateProcessW, stdio handle list, Job Object RAII
  src/shell_programs.rs frozen shell identities and Windows trusted discovery
  src/tools/        the tool seam and the built-in tools
    mod.rs          tool defs, concurrency-safety classification, batched
                    dispatch with hook+permission gating; ToolSource seam
                    for external (MCP) tools
    bash.rs         foreground + background Bash execution, the
                    BackgroundShells registry, bash_output/stop_bash
    powershell.rs   foreground-only fixed EncodedCommand PowerShell executor
    fs.rs           read/write/edit file, read_offloaded
    web.rs          web_fetch/web_search agent contracts: names, descriptions,
                    input schemas (network execution stays in kloop-web)
    search.rs       grep/glob on the ripgrep crate family (gitignore-aware
                    walking, output modes, paging, clipping)
    subagent.rs         sub-agent spawning
    codemode.rs     the run_program tool: CoreBridge (re-enters the gate per op),
                    TypeScript API generation; engine is the codemode crate
  src/shell.rs      tree-sitter-bash word-only analysis, read-only and
                    dangerous classifiers, wrapper stripping
  src/project.rs   machine-local ProjectId/WorkspaceId resolution
  src/permissions.rs the layered execution gate: global deny/ask,
                    ProjectStore durable allow, WorkspaceId session cache,
                    scoped approvals, safety checks, and modes
  src/diff.rs       write/edit change previews for the approval prompt
  src/compact.rs    predictive threshold math + compaction rewrite
  src/context.rs    pure prompt assembly: system + env block + git snapshot,
                    instruction-file concatenation under a byte budget
  src/rollout.rs    append-only session persistence: message/provider-usage/
                    compacted + runtime/turn-terminal records, snapshots,
                    resume/fork
  src/agent.rs      run_turn loop, retry/fallback/truncation recovery, Ui

crates/tui/         kloop-tui — the ratatui frontend; owns the terminal
  src/events.rs     AgentEvent + ChannelUi (Ui/Approver over channels)
  src/app.rs        pure state: transcript cells, interactions, completion routing
  src/composer.rs   byte/grapheme document, paste atoms, visual-row layout/history
  src/text_layout.rs shared grapheme boundaries, display columns, wrap/truncate
  src/clipboard.rs  typed OS clipboard image ingestion
  src/render.rs     pure cell→line/live-chrome layout and modal rendering
  src/lib.rs        terminal lifecycle, effective viewport, worker/event loop

crates/server/      kloop-server — native agent protocol frontend (JSON-RPC 2.0)
  src/wire.rs       envelopes + Event→notification projection (project_event)
  src/lib.rs        serve loop, handshake, per-thread workers, approval routing

crates/mcp/         kloop-mcp — MCP wire client, stdio + streamable HTTP + OAuth (protocol + reqwest)
  src/lib.rs        newline-delimited JSON-RPC over child stdio: handshake,
                    tools/list pagination, tools/call, content rendering

crates/web/         kloop-web — web_fetch/web_search network operations (owns reqwest)
  src/lib.rs        fetch/search operation API + configured backend discovery
  src/fetch.rs      SSRF guard, redirect policy, caps, body handling
  src/html.rs       minimal HTML→text (no extra dependencies)
  src/search.rs     SearchBackend trait + Tavily/Brave implementations

crates/codemode/    kloop-codemode — the QuickJS engine for code mode (owns rquickjs)
  src/lib.rs        run_program: isolated async runtime, HostBridge seam,
                    the tools/agent/log/parallel prelude, resource limits

crates/cli/         kloop — the binary
  src/main.rs       arg parsing + dispatch (TUI default, --plain REPL,
                    app-server/--serve), StdoutUi, CliApprover, --mock demo,
                    session selection (--continue, --resume, --list-sessions)
  src/user_config.rs one global TOML read and strict root schema
  src/private_store.rs descriptor/handle-relative private I/O, atomic replace,
                    directory durability, reparse/symlink rejection, file locks
  src/project_store.rs per-ProjectId allow-only policy schema and RMW
  src/startup.rs    typed runtime policy + frozen ShellPrograms snapshot and
                    cwd-bound Project/Session/Workspace wiring
  src/web.rs        [web] config + ToolSource adapter binding core contracts
                    to kloop-web network operations
  src/mcp.rs        [mcp.servers] config, startup connection with
                    degrade-to-warning, {server}__{tool} namespacing,
                    the ToolSource adapter
  src/mcp_auth.rs   MCP OAuth: `kloop mcp login`, the 0600 token store,
                    connect-time OAuthSession build (plan 34b)
  src/context.rs    project-context IO: instruction-file discovery
                    (global + git root→cwd; main + .kloop/rules/*.md + local
                    override per dir; @import expansion), env, git snapshot
```
