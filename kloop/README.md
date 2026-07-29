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

## Claude Code 2.1.220 parity baseline

Plan 48 pins the comparison target to one exact Claude Code 2.1.220 binary and
commits auditable raw/normalized fixtures, static evidence, and a machine-readable
per-dimension matrix under [`../refs/claude-code-2.1.220/`](../refs/claude-code-2.1.220/).
The methodology, current counts, replay commands, and Plan 49–59 follow-up map
are documented in [`../refs/README.md`](../refs/README.md) and
[`../docs/plan/48-claude-code-2.1.220-tool-parity.md`](../docs/plan/48-claude-code-2.1.220-tool-parity.md).

This baseline is evidence and a roadmap, not a claim that all tools already
match or that kloop can replace Claude Code. Product-level gaps and unknowns
remain for Plans 49–59; kloop-only capabilities stay intentionally separate.

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

`KLOOP_CONTEXT_WINDOW` sets the usable window in tokens (default 200000,
`off` disables compaction).

## Session persistence (Phase 2, second slice)

Every session is persisted to `.kloop/sessions/{id}.jsonl`
(`crates/core/src/rollout.rs`), one JSON line per recorded message, written
through as the history records — so a killed process loses at most the line
being written. The same append-only chain also carries recovery-only `session`
records (the canonical cwd and resolved model) and display-only `turn_terminal`
records (completed/maxRounds/aborted/error, positioned after a message index);
neither enters provider replay or token accounting. If a stream fails or is
cancelled after visible text, that partial assistant block is recorded before
the terminal, so `thread/read` can reconstruct exactly what the user already
saw after a restart. Compaction appends a `compacted` marker line carrying the
full replacement history (the codex rollout pattern): the file stays
append-only and auditable, replay swaps in the replacement and keeps reading,
and superseded terminal indices are discarded with the replaced history.

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
  dir-scanning counter already prevents clobbering), and usage anchors are
  not persisted, so a fork re-anchors on its first sampled response.

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
rebuilds to the earlier state, and the next message continues the new branch
(the old one stays on disk, forkable/resumable). Esc cancels. The picker's
points are exactly the cuts `fork_session` accepts, so a selection can never be
rejected. Rewind is idle-only — a running turn owns History (Ctrl+C first).

### Sub-agent sessions

A sub-agent the `task` tool spawns (synchronous batch or `background: true`)
writes its own session file next to the parent's, so its full transcript is
auditable — the parent's tool_result keeps only the sub-agent's final text,
while the file records every tool call it made. Both references converged on
this (cc's `subagents/agent-<id>.jsonl` sidechains, codex's one rollout per
child thread); kloop keeps its flat, single-file layout rather than cc's
nested dirs or codex's SQLite `thread_spawn_edges`:

- the child file is `{parent id}-{agent-N}.jsonl` — the name itself shows the
  lineage and stays unique (parent id is unique, the label is process-global);
- its first line carries `subagent_of` = `{parent id}#{seq}` of the parent
  turn's assistant line that made the spawning `task` call — a *line-level*
  back-pointer (finer than either reference's session-level link), independent
  of the fork `parent` field since a sub-agent history is wholly its own (no
  prefix copied);
- lines are written through live as the sub-agent records them, so even a
  `stop_agent`-cancelled child leaves its partial transcript on disk;
- `--list-sessions` shows sub-agent sessions labelled `[sub-agent of …]`, but
  the default `--resume`/`--continue` picker skips them (they are reachable
  only by explicit id) — matching cc hiding sidechains and codex's source
  filter, while still keeping them visible for audit;
- the parent's `task {background:true}` reply names the child's session log so
  a human reading the parent transcript can jump to it. A parent with no
  session (`--mock`, tests) leaves the sub-agent in memory, as before.

## Permissions (Phase 2, third slice)

Every tool call passes a layered gate before executing
(`crates/core/src/permissions.rs`), shaped after claude-code's permission
pipeline, with the bash analysis ported from codex's `shell-command` crate:

```
deny rules → sensitive-read hard block → plan-mode read-only gate → safety checks →
ask rules → sandbox auto-allow → bypass → read-only self-verdict → acceptEdits →
allow rules → session cache → ask the user
```

Two invariants carried over from claude-code: **deny always beats allow**,
and **safety checks are immune to bypass mode**. Credential-bearing/read-sensitive
paths (`.kloop`, `.ssh`, `.gnupg`, `.aws`, `.env*`) are an even earlier hard
boundary: `read_file` refuses them, grep/glob filter them before reading, and
Bash checks literal plus canonical paths before sandbox/read-only/bypass. The
macOS sandbox also denies reads of `~/.kloop/config.toml` and
`~/.kloop/mcp-oauth.json`, including through a
symlink, and provider/search key env vars are removed from model shell children.
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

**Rules** live in global `~/.kloop/config.toml` and env vars (comma-separated
`KLOOP_ALLOW` / `KLOOP_DENY` / `KLOOP_ASK` append on top):

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
rule, e.g. `bash(cargo build *)`, to global `~/.kloop/config.toml`, affecting
all workspaces) · `n` deny. A
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
rides every request so the model knows to plan, not act. When ready, the model
calls **`exit_plan_mode`** with the plan text; it rides the same approval popup
a change-diff does (the plan is the scrollable `preview`). Approve → plan mode
turns off, restoring the mode it was entered from (manual at startup), and the
model implements; reject → it stays in plan mode and keeps planning. Sub-agents
inherit plan mode with the session and are read-only in it, but only the
top-level agent can `exit_plan_mode`.

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
`History`; `ChannelUi` implements both `Ui` and `Approver` by forwarding
everything as events over an mpsc channel (approval decisions travel back
over a oneshot; a dropped reply means deny). Keys arrive from a dedicated
poll thread (`poll(200ms)+read`, not crossterm's `EventStream`) so the input
reader never parks holding the lock a resize's cursor-position query needs.
The UI loop `select!`s those key events against agent events, folds both into
pure state (`App`, whose `cells` are the uncommitted tail), and renders via
pure cell→line functions — which is what makes the transcript logic testable
without a terminal. Before each draw it freezes the finalized cells that
overflow the viewport into scrollback with `insert_before`, then clears and
fully redraws the current viewport so terminal scroll-region quirks cannot
leave Ratatui's diff buffer out of sync; native scrollback remains intact.
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

The input is a **multi-line composer** (plan 38 slice 3, `crates/tui/src/composer.rs`),
a `›` prompt over a dim placeholder when empty. **Shift+Enter / Alt+Enter / Ctrl+J**
insert a newline (Ctrl+J is the portable one — many terminals do not distinguish
Shift+Enter); arrow keys move the cursor across lines, and **↑/↓ on the first/last
line** step through the input history instead. A **large paste** collapses to a
`[Pasted N chars]` placeholder that expands to the full text on submit (so the
composer stays readable). **Ctrl+V / Alt+V** pastes an image straight off the OS
clipboard (a screenshot, a browser copy, or a Finder-copied file) — terminals
keep Cmd+V for their own text paste, so a distinct key reads the clipboard, the
same choice Claude Code and codex make; a pasted or dragged **image-file path**
attaches too. The image shows on a `📎` line above the composer and rides the
turn (reusing the `--image` ingestion — `crates/tui/src/clipboard.rs` via
`arboard`), merged with any startup `--image` blocks.

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
while a turn runs (a `/` line is then steering text).

While a turn runs, an **animated status line** (plan 38 slice 5,
`crates/tui/src/anim.rs`) sits just above the composer: a braille spinner, a
"shimmer" light band sweeping the verb, and `(elapsed · esc to interrupt)`. The
**footer** carries the mode badge and key hints on the left and the **system
status** — model name and a context gauge (`model · N% ctx`, refreshed after
each turn from the same usage accounting `/cost` reads) — flush right (dropped on
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

## Native agent protocol (plan 39)

`kloop app-server` (alias `kloop --serve`) speaks kloop's native agent protocol
over stdio — **standard JSON-RPC 2.0** (every envelope carries `"jsonrpc":"2.0"`,
one object per line) — so IDEs and automation can drive multiple sessions
concurrently. The core emits a single [`Event`](crates/core/src/event.rs)
stream that every front-end projects; the server (and the headless `--json`
stream) project it onto the wire below via one shared `project_event`, so both
speak one item vocabulary by construction.

**Handshake.** `initialize {clientInfo, protocolVersion, capabilities}` →
`{serverInfo, protocolVersion, capabilities}` negotiates the version (from
`"1.0"`; a version the engine doesn't speak is a hard error, not a silent
downgrade) and gates every other method until it succeeds. Capabilities are
structured: `{streaming, subagents, mcp, images, approvals, threads:{list,
read,resume,fork}, models:{list}, config:{read}, skills:{list},
mcpServers:{status}}`.

**Methods:** `thread/start {cwd?, model?}` → `{thread:{id}}`;
`thread/list {limit?, cursor?}` → `{threads, nextCursor}` (newest first,
sub-agent sidechains hidden, `limit` capped at 500); `thread/read {threadId}` →
`{thread:{id,cwd,model,resumable,forkedFrom,messages,terminals}}`;
`thread/resume {threadId, cwd?}` and `thread/fork {threadId, cut?, cwd?}` →
`{thread:{id,cwd,model,resumable}, messageCount}`; `turn/start {threadId,
input}` → `{turn:{id}}`, `turn/steer {threadId, input}` → `{turnId}`,
`turn/interrupt {threadId}`. The optional `cwd` on resume/fork is only the
one-time migration input for pre-runtime-metadata rollouts; it cannot override
a pinned session runtime. `input` is a string or an array of content parts
(`{type:"text",text}` / `{type:"image",source:{…}}`).

**Read-only discovery:** `model/list {}` returns only the process's locally
resolved default model (not a fabricated provider catalog); `config/read
{cwd? | threadId?}` returns an explicit non-sensitive allowlist;
`skills/list {cwd? | threadId?, forceReload?}` returns skill metadata without
bodies, allowed-tool rules, or user commands; and `mcpServerStatus/list {}`
returns the immutable startup discovery snapshot (transport, connected /
unavailable state, sanitized message, and model-visible tool names). Config,
skill, plugin, and MCP mutation methods are deliberately absent in this slice.
Read methods reject unknown parameters, accept at most one scope selector, and
canonicalize cwd before invoking their reader.

Every thread is its own tokio task owning a History (persisted to the same
`.kloop/sessions/` files the interactive frontends use — sessions are
interchangeable) and its own permission gate, so approval session caches never
leak across threads. A missing `thread/start.cwd` defaults to the app-server
launch directory; an explicit relative path is resolved from that directory,
canonicalized, and rejected unless it is an accessible directory. The CLI builds project instructions, skills, permissions, sandbox,
hooks, agent types, and program limits from that thread cwd without ever
changing the process cwd. `thread/start.model`, when present, overrides the
provider/env default only for that thread; otherwise the factory-resolved
default is pinned before `thread/start` returns, so a later server restart or
environment change cannot silently switch the resumed thread's model. Provider
state and already-connected MCP tool sources remain process-shared;
session/offload roots remain the app-server's `ServerPaths`, not the thread
project directory.

**Events** stream per thread, tagged with `threadId` (and `turnId` for
turn-scoped ones): `turn/started {turn:{id}}`; then the turn's items as
`item/started` / `item/delta {itemId, channel, text}` (channel ∈
`text`/`reasoning`/`output`) / `item/completed`, where `item.type` ∈
`assistantMessage` / `reasoning` / `toolCall` / `subAgent` / `todo` (a tool
call carries its full `input`, and `output` + `agent` label when present; a
`todo_write` surfaces only as a `todo` item, never a tool row); plus
`thread/tokenUsage/updated {tokenUsage:{total}}`, `note {text}`,
`thread/cwd/updated {cwd, branch}`; and `turn/completed {turn:{id, status,
error?}}`. A `turn/start` whose input is a slash command (`/help`, `/cost`,
`/compact`, `/clear`, and an inert `/exit`) runs the command instead of the
model: its output comes back as a `system` notification, `/clear` also emits
`thread/cleared`, and the turn bracket is unchanged.

**Approvals** are one reverse request — `approval/request {threadId, turnId,
kind:"command"|"fileChange", description, preview?, rememberRules?}` (server
ids are integers in the server's own counter space) — answered
`{"decision": "accept" | "acceptForSession" | "acceptAlways" | "decline"}`. A
dropped/never-answered reply, `cancel`, or anything unrecognized declines
(interrupt the turn to unblock).

```jsonc
→ {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1.0","capabilities":{}}}
← {"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"kloop","version":"0.1.0"},"protocolVersion":"1.0","capabilities":{"streaming":true,"subagents":true,"mcp":true,"images":true,"approvals":true,"threads":{"list":true,"read":true,"resume":true,"fork":true},"models":{"list":true},"config":{"read":true},"skills":{"list":true},"mcpServers":{"status":true}}}}
→ {"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}
← {"jsonrpc":"2.0","id":2,"result":{"thread":{"id":"20260721-135146"}}}
→ {"jsonrpc":"2.0","id":3,"method":"turn/start","params":{"threadId":"20260721-135146","input":"create s2.txt"}}
← {"jsonrpc":"2.0","id":3,"result":{"turn":{"id":1}}}
← {"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"…","turn":{"id":1}}}
← {"jsonrpc":"2.0","id":1,"method":"approval/request","params":{"threadId":"…","turnId":1,"kind":"fileChange","description":"write_file: s2.txt","rememberRules":["write_file(*)"],"preview":"(new file)\n+1  hello"}}
→ {"jsonrpc":"2.0","id":1,"result":{"decision":"accept"}}
← {"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"…","turnId":1,"item":{"id":"…","type":"toolCall","name":"write_file","status":"completed"}}}
← {"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"…","turn":{"id":1,"status":"completed"}}}
```

### Codex Desktop adapter (plan 39 slices 2–4)

The `桌面前端仓库` repository's dedicated `kloop` branch launches this
server through `ENGINE_BIN` and consumes v1 directly (it does not emulate
the old Codex app-server wire):

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
session-bearing request re-runs the handshake once, and 408/429/5xx and
transient network errors retry (250ms, 1s, then a final try) while 401/403 are
terminal.

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

Servers are spawned/connected once at startup (stdio children killed on exit);
the handshake is `initialize` → `notifications/initialized` → `tools/list`
(with `nextCursor` pagination), and each advertised tool joins the model's tool
list as `{server}__{tool}` with its inputSchema passed through verbatim. A
failing server degrades to a startup warning — MCP never blocks kloop. Name
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
client is the `kloop-mcp` crate (protocol layer transport-agnostic behind a
`Transport` trait — stdio and HTTP both implement it; it pulls `reqwest` for
the HTTP transport and the OAuth wire — PKCE/discovery/token exchange/refresh in
`oauth.rs` — but core never depends on it); the CLI glues them (config parsing,
secret resolution, namespacing, the adapter, and the OAuth login command + token
store in `mcp_auth.rs`, so nothing with a terminal/config-file side-effect
leaks into the wire crate).

### Deferred tools + tool_search

Past 30 total tools (`KLOOP_DEFER_THRESHOLD` overrides; built-ins never
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

**Sub-agents** (dispatched by the `task` tool) fire `subagent_start` /
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

- **System prompt** = base instructions + an environment block (working
  directory, platform, today's UTC date, whether cwd is a git repo) + a git
  snapshot (current branch, `git status --short` capped at 1000 bytes,
  last 5 commits) labeled as a start-of-session snapshot.
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

## File tools (Plan 49)

`read_file`, `write_file`, and `edit_file` share session-scoped file observations
(`core/src/file_state.rs`) rather than trusting a path forever:

- **read_file** resolves and opens a canonical no-follow regular-file descriptor
  before permission, so an approval wait cannot retarget a benign alias into a
  sensitive file. It numbers UTF-8 text lines and accepts `offset`/`limit` as whole
  numbers or numeric strings (`0` keeps the documented unbounded/from-start
  behavior). Model-facing text is capped at 7,000 characters without splitting
  UTF-8; empty files, past-EOF offsets, PDFs, and non-image binary data return
  explicit results. PNG/JPEG/GIF/WebP continue as structured image blocks.
  On Unix, regular files with multiple hard links are rejected because pathname
  sensitivity cannot safely classify another name for the same inode.
- Only a complete range that reaches the model in a final successful
  `tool_result` qualifies an existing file for mutation. Preview reads, errors,
  permission rejection, post-hook cancellation, restored sessions, sub-agents,
  and separate worktrees do not inherit that authority. The bounded observation
  table is process-memory only and deterministically evicts old entries.
- **write_file** may create a new file only when its direct parent directory
  already exists; it never creates missing intermediate directories. Replacing an
  existing file, and every **edit_file**, requires a complete fresh read. A
  partial, stale, or externally deleted observation fails closed; a successful
  mutation refreshes the observation, while a failed or uncertain one clears it.
- Mutations canonicalize the existing parent and open it as a directory handle
  after pre-hooks but before permission checks; only the leaf may be absent. The
  gate and approval preview see the canonical target. After an approval wait the
  executor revalidates the parent identity, then performs target reads, temporary
  creation, freshness checks, rename, cleanup, and parent sync relative to the
  retained handle. Replacing a parent path with a symlink to `.git` or another
  sensitive/denied location therefore rejects the call rather than retargeting
  it. Internal parent aliases share one lock key, while a cwd-contained spelling
  cannot escape through an ancestor symlink. Target opens are no-follow/nonblocking;
  the commit preserves existing permissions (new files use conventional 0666
  filtered by umask), checks the temporary name still denotes the opened inode,
  and rejects symbolic-link leaves, FIFOs, or other non-regular targets.

The descriptor boundary closes approval-time alias retargeting; it is not a
filesystem transaction against a hostile same-UID process. POSIX still leaves a
small final identity-check-to-`renameat` namespace window, documented in Plan 49.

The complete-read requirement and fail-on-any-drift Edit policy are deliberately
stricter than Claude Code 2.1.220, which accepts partial qualification in some
paths and may stale-recover an unambiguous edit. The paired evidence and reasons
are recorded in Plan 49 and `refs/claude-code-2.1.220/tool-matrix.json`.

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

`web_fetch` and `web_search` have their agent-facing contracts (names,
descriptions, and JSON schemas) in `crates/core/src/tools/web.rs`. Their network
operations remain implemented in the `kloop-web` crate and are bound to core's
`ToolSource` seam by the CLI (exactly like MCP servers — core stays
network-free, reqwest lives only in provider and web):

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
  in global `~/.kloop/config.toml` selects the backend; adding a provider = one
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
its cloned Config, and core's `Event` stream (plan 39) carries it end to end.
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

A `task` call can target a named specialized agent via the `agent_type`
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

### Worktree isolation

Two entry points put an agent into a private git worktree — a separate checkout
on its own branch — so edits to the *same relative path* don't race on the
shared tree (`crates/core/src/worktree.rs`, plan 35; the shape cc and codex
converged on). Both create `git worktree add --no-track -B
kloop/worktree/<name> .kloop-worktrees/<name> HEAD` (the managed dir is added to
`.git/info/exclude`) and rewire the **cwd anchor** onto the tree.

`Config.cwd` is that anchor: bash's working directory, relative file/search
paths, the permission gate's acceptEdits check, and the OS sandbox's writable
root all key off it (a fifth thing rewrites too — the system prompt's
`Working directory:` line, or the model builds absolute paths from the old cwd
and writes past the tree). The main agent's cwd is the process cwd, so nothing
changes there; only a worktree agent diverges. `offload_dir` and `sessions_dir`
deliberately do **not** follow — offload/session files stay in the main repo.
Not under `.kloop/` (that's a protected sensitive path in both the permission
gate and the sandbox); a sibling `.kloop-worktrees/` dodges both.

- **Sub-agent isolation** (slice 1): a `task` call adds `"isolation":
  "worktree"`, so parallel sub-agents each get a throwaway tree. Parallel `git
  worktree add`/`remove` are serialized under a process lock (concurrent ones
  race on the repo's ref locks and silently lose a tree).
- **Session worktrees** (slice 2): the model calls `enter_worktree {name}` to
  move the *whole session* into a tree (its cwd switches immediately via a
  mutable slot the `Config.effective_*` accessors read) and `exit_worktree` to
  leave; `kloop --worktree[=<name>]` enters one at startup. One tree per
  session; a sub-agent spawned while in it inherits the tree as its base cwd.
  Works in every session but `--mock`, **including server threads** (each
  thread has its own slot; a `thread/worktree` notification — `{cwd, branch,
  active}` — reports each switch so an IDE can follow it, and an un-exited tree
  is torn down when the thread ends). The `--worktree` startup flag stays
  single-session (rejected with `--serve`).

Lifecycle (no auto-merge, both references stop here): an **untouched** tree
(clean, no commits past HEAD) is torn down with its branch; a **changed** tree
is kept, and the result names its branch + path so you can `git merge
kloop/worktree/<name>` (commit first if uncommitted) or discard it.
`exit_worktree {discard_changes:true}` force-removes even a dirty tree. The
change probe is **fail-closed** (git untrusted → tree kept). Creation is
fail-closed too — a non-git cwd, a name collision, or a git error is an error,
never a silent fall back to the shared cwd.

Not yet (挂账): an `origin/HEAD` base ref for CI, and 30-day stale-tree pruning
(`git worktree prune` by hand for now).

## OS sandbox (Phase 2, thirteenth slice)

On macOS, bash commands run inside a seatbelt sandbox by default
(`crates/core/src/sandbox/`, executed via `/usr/bin/sandbox-run_program` with a
deny-by-default SBPL profile — the shape cc and codex converged on):

- **Writes** are allow-listed: cwd + `/tmp` + `$TMPDIR` + configured extras.
  Inside a writable root, `.git/hooks`, `.git/config` and `.kloop` stay
  read-only (they are privilege-escalation surfaces — hooks and git config
  run code, and project `.kloop` contains agent instructions/state; the rest of `.git` stays
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
  first, then run sandboxed). `--permission-mode bypass` bypasses approvals but
  not the sandbox.

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

`KLOOP_SANDBOX=off` is the env escape hatch. Where sandboxing is unavailable
(Linux/Windows for now — planned as future slices behind the same seam; or a
missing `sandbox-run_program`), kloop warns at startup and runs commands bare:
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

Rendering: `todo_write` never shows as a generic tool row — core emits only a
`todo` item for it (no `toolCall`), which renders as a checklist. The TUI keeps
one `Cell::Todo` per turn, updated in place as the list evolves (a new user turn
starts a fresh block); the plain REPL prints the marked list; the native
protocol server emits a `todo` item (`item/completed`) with the full list (a
sub-agent's carries an `agent` field). A sub-agent's list stays internal to the
TUI transcript (lesson 3), the way its text does. Not done (deliberate): dependency graphs, cross-session todo
stores, rollout persistence of the list.

## Steering — mid-turn injection (Phase 2, fifteenth slice)

Type a message while a turn is running and it **steers** instead of being
dropped: it queues, and is delivered to the model as a user message at the
next round boundary — never spliced into an in-flight request. It does not
interrupt the current tools (Ctrl+C stays the hard stop). This is a general
**step-boundary injection queue** (`Config.inbox`, a signalling `Inbox` of
typed `InboxItem`s); its two consumers are user steering and background
sub-agent results (see *Async sub-agents* below), each with its own framing.

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
a User cell); server mode enqueues via `turn/steer {threadId, input}` (pushed
during a running turn it folds in at the next round boundary, pushed while idle
it is delivered at the top of the next `turn/start` — there is no autowake in
client-driven server mode). The plain REPL (blocking stdin) does not enqueue
yet — the drain path is live for all three, so the gap is only the enqueue
side. This is the cc/codex convergence: steering is enqueue-not-interrupt,
delivered only between steps (see `refs/README.md`).

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
- `/exit` — quit. The TUI and plain REPL exit (the TUI with the same clean
  teardown as a two-tap Ctrl+C); in server mode it is inert — quitting one
  thread must not stop a multi-session process, so it just relays a note.

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
declare function agent(prompt: string, opts?: { agent_type?; max_rounds? }): Promise<string>;
declare function log(msg: unknown): void;
declare function parallel<T>(thunks: Array<() => Promise<T>>): Promise<Array<T | null>>;
declare function pipeline(items: any[], ...stages): Promise<any[]>;
```

`parallel` is a barrier (all thunks, failures→null); `pipeline` runs each item
through every stage as its own chain with **no barrier between stages** — a fast
item reaches stage 3 while a slow one is still in stage 1 — matching cc's two
core orchestration primitives.

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

**The safety story is that every `tools.<name>(...)` and `agent(...)` re-enters
the exact same gated dispatch a direct call takes** — `run_one` (allowlist →
deferred lock → hooks → permission gate → sandbox → execute) and `task_tool`. A
denied tool is refused *inside* the program (the model catches the exception); a
sandboxed command is still sandboxed. The `kloop-codemode` crate is engine-only
and knows nothing of permissions; it calls back through a `HostBridge` trait,
which `core/src/tools/codemode.rs` implements over the gate — that inversion is
why `core` can depend on the engine crate without a cycle. `run_program` itself is
auto-allowed (like `task`): it touches nothing directly. `Promise.all` maps to
the same concurrency rule as a normal round (read-only calls batch, writes take
an exclusive lock).

**Resource limits** (`Limits`, per program run) are two layers. Engine limits
guard the interpreter: a QuickJS heap cap, a stack cap, and an interrupt handler
that kills a runaway synchronous loop (a CPU-burst deadline that ignores
await-suspended time) or a user Ctrl+C. Orchestration **caps** are hard ceilings
on fan-out — a model-written program loops and fans out programmatically, so it
needs ceilings a hand-written tool_use batch never hits: `max_agents` (total
`agent()` calls; the (N+1)th throws — the guard against `while(true){agent()}`)
and `max_items` (a single `parallel()`/`pipeline()` array length; over it throws,
never truncates). Both mirror cc's workflow caps (1000 / 4096). Concurrency is
deliberately **not** paced — a program firing N concurrent `agent()` is the same
as a model emitting N concurrent `task` calls, which kloop runs uncapped, so
pacing here would break that precedent; the total ceiling is the guard that
matters. All five knobs override via `[codemode]` in global
`~/.kloop/config.toml`
(`memory_mb`, `stack_kb`, `cpu_secs`, `max_agents`, `max_items`) or
`KLOOP_PROGRAM_*` env (env wins).

A running program is observable, not a black box: each `tools.<name>(...)` and
`agent(...)` shows as its own tool line (the ops go through `run_one`, which
emits the same UI lifecycle a direct call does) and `log(...)` prints live.

**Background programs**: `run_program {"background": true}` fires and forgets —
it returns a `program-N` id immediately and the program's return value is
delivered to the parent as a message when it finishes, so a long fan-out /
migration doesn't hold up the turn. It reuses the async sub-agent machinery
wholesale (see "Async sub-agents" below): the same registry, the same `wait` /
`stop_agent` tools, the same inbox reinjection and TUI autowake — a background
program and a background sub-agent are the same kind of detached task, so they
share one registry (the shell registry stays separate: a shell has an output
file, not a reinjected result). Deliberately **not** copied from codex: its
cell/observation-frontier machinery (incremental pull-based output streamed to
the model between `yield`s) — that is pull-based observation coupled to V8's
synchronous-pause model, whereas kloop is push-based (result reinjected on
completion) and `log()` already streams progress to the user live.

**Journal resume**: a long program that fails partway through an `agent()`
fan-out doesn't have to re-burn the sub-agents that already finished. Every run
has a `run_id` and journals each completed `agent()` call (keyed by its
JS-assigned sequence number + a canonical string of its prompt+params) to
`.kloop/program-runs/<run_id>/journal.jsonl`. On failure the run_id is reported;
calling `run_program` again with the same source and `resume_from_run_id` set
replays each matching `(seq, key)` from the journal — returning the cached
result and skipping the spawn (and the agent-cap charge) — so only the calls
that hadn't finished re-run. Only `agent()` is journaled (it is the expensive
call). The JS-assigned seq makes replay independent of the order sub-agent
futures resolve in; unlike cc's prefix-replay this memoizes each `(seq, key)`
independently, which is safe because a call whose inputs changed has a changed
prompt (so its key changes and it re-runs). A one-shot `run_program` tool_use
maps cleanly onto this — exactly cc's `resumeFromRunId` shape.

**Not done** (deferred, with reason): a token `budget` primitive — cc's
`budget.total` ships as a hardcoded `null` placeholder (its hard cap never
fires), and kloop has no turn-level budget source, so a budget object would be a
no-op until there's a real source to feed it; a richer progress view (cc's
`/workflows` tree — kloop shows a flat live trace); saving a program as a named,
reusable command (cc does this by writing a file into `.claude/workflows/` — for
kloop that's the same seam as user commands, the User-commands slice under
Skills, not a code-mode one). See `docs/plan/24-code-mode.md` and
`docs/plan/27-codemode-mcp-tools.md`.

## Async sub-agents (Phase 2, eighteenth slice)

`task` takes `background: true`: instead of blocking and returning the
sub-agent's final text, it **fires and forgets** — returns an `agent-N` id
immediately and the sub-agent's result is delivered to the parent as a message
when it finishes. So the parent can dispatch a long subtask, keep working, and
collect the result later. Two companion tools manage the in-flight agents:

- `wait` — block until a background sub-agent finishes (or new input arrives, or
  a timeout: default 30s, 10s–1h). It returns a short status line; the finished
  agent's result arrives separately at the next round boundary. Like codex's
  `wait`, it **signals but does not carry** — it never drains the queue itself.
- `stop_agent` — cancel a runaway background sub-agent by id.

Mechanism: the detached sub-agent (its own tokio task, on its **own** cancel
token so a finished parent turn never kills it) reinjects its result into the
parent's `Config.inbox` — the same step-boundary queue as steering — as a framed
`InboxItem::SubAgentResult`, drained into history at the next round boundary
(the drain side was already built for steering; this is the queue's second
consumer). A **success passes through verbatim**; a failure is truncated (~900
tokens, codex's cap) with re-dispatch guidance; an **interrupted sub-agent
reinjects nothing** (codex's `is_final` — its partial output is noise, and cc
diverges here by delivering a `killed` partial). A separate `BackgroundTasks`
registry (`core/src/tools/background_tasks.rs`) tracks the in-flight agents,
enforces a concurrency cap (8), and reaps on session end — kept **separate** from
the background-shell registry, because codex keeps its shell tasks and
sub-agents in distinct mechanisms and cc only unifies the *state* model, not
spawn (a shared `Tasks` abstraction would be pre-abstracting against that).

**Autowake** closes the loop when the parent turn has already ended: in the TUI,
a background sub-agent finishing while the agent sits idle starts a delivery turn
automatically (the idle UI loop notices the non-empty inbox and dispatches a
`Wake` — a turn with no new user text that just drains and responds), so the
result reaches the model without the user having to type. A *running* turn drains
at its own round boundary, so autowake only fires when idle (codex's guard:
idle + pending work). The plain REPL (blocking stdin, no event loop) and the
server (client-driven turns) don't autowake — their reinjection is delivered at
the next user / `turn/start`; only the TUI has the event loop to be woken.
Sub-agents cannot spawn further sub-agents, so background dispatch stays depth-0.
See `docs/plan/26-async-dispatch.md`.

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
  (`skill({"name": ..., "arguments": ...})`), which — like `task` — exists only
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
- **`fork`**: the body runs as an **isolated sub-agent** (reusing the `task`
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

**Not done** (deferred): pasting / drag-drop into the TUI (`--image` is the
entry point for now); client-side downscaling; a drop-images-when-unsupported
token saver / model-vision capability probe; per-request media-count cap;
PDF/document blocks; remote-URL images; exposing `detail`. See
`docs/plan/29-image-input.md`.

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
  `thread/tokenUsage/updated`, `note`, `turn/completed` — the **exact same
  item vocabulary the native protocol server emits** (via one shared
  `project_event`), `threadId` and all. One event vocabulary, two front-ends.
- **Approval defaults to deny.** There is nobody at the keyboard, so any
  permission ask is auto-denied (fail-safe, like server mode's "reply lost =
  deny"). Loosen with `--permission-mode accept-edits`/`bypass` or `KLOOP_ALLOW`
  — these act before the approver, so they still open the gate. (Sandbox
  auto-allow still covers safe bash without asking.)
- **Exit code** is `0` on a clean finish, `1` on error, interruption (Ctrl+C),
  or hitting `--max-rounds`. Without that explicit flag, headless uses the same
  unbounded turn loop as interactive/server mode.
- The session persists to `.kloop/sessions/` like every other mode, so a
  headless run is resumable (`--resume <id>`) and forkable afterward.

`--max-rounds` and `--json` are `--headless`-only; a bare prompt without it
is an error (interactive mode takes its input at the prompt). Not done (deferred, server
mode already covers the programmatic side): `--permission-prompt-tool`
delegation, `--input-format stream-json`, budget/goal guardrails,
`--output-schema`. See `docs/plan/33-exec-mode.md`.

## Running

```sh
# usage summary of every flag
cargo run -- --help

# keyless demo: scripted Mock provider exercises all five bets (plain output)
cargo run -- --mock

# The only automatically discovered TOML config is ~/.kloop/config.toml. It
# contains provider/model plus permissions, MCP, web, hooks, sandbox, agents,
# and codemode. A cwd .kloop/config.toml is never read or merged; cwd remains
# the workspace anchor for project instructions, skills, tools, permissions,
# and sandbox paths.
#
# Daily provider/model configuration lives in that process-global file. The
# directory must have no group/other access (kloop creates it as 0700), and the
# file must be mode 0600:
#
#   model = "gpt-5.6-sol"
#   model_provider = "gw_router"
#   model_reasoning_effort = "xhigh"
#
#   [model_providers.gw_router]
#   name = "gateway"             # optional display name
#   wire_api = "responses"         # responses | chat | anthropic
#   base_url = "https://example/v1"
#   http_headers = { Authorization = "Bearer ..." }
#   model = "gpt-5.6-sol"          # optional profile-specific default
#
# Custom profile names require wire_api. Built-in names anthropic, openai /
# openai-compat, and openai-responses infer it. Anthropic profiles use exactly
# one x-api-key header; chat/responses profiles use Bearer Authorization.
# kloop appends /v1/messages, /chat/completions, or /responses to the base.
chmod 700 ~/.kloop
chmod 600 ~/.kloop/config.toml
cargo run

# Environment variables remain compatibility/CI overrides rather than the
# daily source of truth. Provider selection: KLOOP_PROVIDER > model_provider >
# key auto-detection. Key/base env wins over the selected profile. Model order:
# ANTHROPIC_MODEL/OPENAI_MODEL > KLOOP_MODEL > profile model > top-level model.
# Explicitly selected, incomplete profiles fail; kloop never switches rails.
#
# ANTHROPIC_API_KEY / ANTHROPIC_BASE_URL (base excludes /v1) select Messages;
# OPENAI_API_KEY / OPENAI_BASE_URL (base normally includes /v1) select either
# chat or Responses according to KLOOP_PROVIDER. KLOOP_CACHE and KLOOP_THINKING
# override Anthropic cache/thinking; KLOOP_EFFORT overrides Responses effort.
# Provider/search keys are stripped from model-controlled shell environments.

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

# permissions (persistent rules live in global ~/.kloop/config.toml)
KLOOP_ALLOW='write_file,bash(cargo *)' cargo run   # pre-approve rules
KLOOP_DENY='bash(git push *)' cargo run            # hard-block rules
cargo run -- --permission-mode accept-edits        # auto-allow cwd file writes
cargo run -- --permission-mode bypass              # bypass (deny/safety still apply)

# OS sandbox (macOS seatbelt; see OS sandbox above)
KLOOP_SANDBOX=off cargo run                        # run bash commands bare
```

Interrupting a running turn patches history so it stays legal either way. In
the TUI, Esc interrupts and Ctrl+C (two taps) exits (see the TUI section); in
`--plain`, Ctrl+C interrupts the turn and Ctrl+D (or `exit`) quits. Every
session is saved and resumable — see Session persistence above.

## Verification

`cargo test` runs 358 tests across the workspace:

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
  (exact SBPL profile assembly and `-D` param list, workspace-root
  computation with canonical/literal dedup, denial-detection table incl. the
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
  accumulation and splitting, tool status resolution by id, confirm queueing
  and keyboard capture, popup scroll keys with offset reset on advance,
  interrupt/quit commands, turn-end cleanup), and pure
  rendering (CJK-aware wrap/truncate, per-cell-kind lines, tool-row collapse,
  input window around the cursor, diff-preview coloring by +/- sign, confirm
  popup windowing a tall diff with pinned options and a scroll hint — verified
  end-to-end through a TestBackend frame).
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

Cargo workspace, nine crates; the dependency graph is a strict line up to
core, then two sibling frontends under the cli, with the MCP wire client, Web
network operations, and the QuickJS code-mode engine kept behind explicit
seams
(protocol ← provider ← core ← {tui, server} ← cli; protocol ← mcp ← cli;
web ← cli; codemode ← core):

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
    web.rs          web_fetch/web_search agent contracts: names, descriptions,
                    input schemas (network execution stays in kloop-web)
    search.rs       grep/glob on the ripgrep crate family (gitignore-aware
                    walking, output modes, paging, clipping)
    task.rs         sub-agent spawning
    codemode.rs     the run_program tool: CoreBridge (re-enters the gate per op),
                    TypeScript API generation; engine is the codemode crate
  src/shell.rs      tree-sitter-bash word-only analysis, read-only and
                    dangerous classifiers, wrapper stripping
  src/permissions.rs the layered execution gate: deny/ask/allow rules,
                    safety checks, modes, session cache, Approver seam
  src/diff.rs       write/edit change previews for the approval prompt
  src/compact.rs    predictive threshold math + compaction rewrite
  src/context.rs    pure prompt assembly: system + env block + git snapshot,
                    instruction-file concatenation under a byte budget
  src/rollout.rs    append-only session persistence: message/compacted +
                    runtime/turn-terminal records, snapshot reads, resume/fork
  src/agent.rs      run_turn loop, retry/fallback/truncation recovery, Ui

crates/tui/         kloop-tui — the ratatui frontend; owns the terminal
  src/events.rs     AgentEvent + ChannelUi (Ui/Approver over channels)
  src/app.rs        pure state: transcript cells, input, confirm queue
  src/render.rs     pure cell→line rendering, wrap/truncate, confirm popup
  src/lib.rs        terminal lifecycle, agent worker task, event loop

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
  src/user_config.rs one global TOML read, strict root schema, private atomic writes
  src/startup.rs    typed runtime policy snapshot + cwd-bound session wiring
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
