---
name: rust-review
description: What to look for when the code under review is Rust — ownership, panics, unsafe, shared state, async cancellation, integer and UTF-8 boundaries, macro definitions. Use alongside the code-review skill whenever a diff touches `.rs` or `Cargo.toml` — code-review sets the method, this one sets the checklist. Not for writing new code, and not a style guide.
---

# Rust review checklist

`code-review` owns the method — what counts as a finding, the failure-scenario
bar, the verdict every candidate gets, the excluded list. This skill adds only
the *what to look at*. Run it as a sweep over the diff you have already read;
nothing here lowers that bar, and an item below that you cannot turn into a
concrete failure scenario is still not a finding.

**Everything listed here is something the gate does not already catch.** CI runs
`cargo clippy --workspace --all-targets --all-features -- -D warnings`, so a
default-on lint's own case would have failed CI and is never a finding. Several
items below sit next to a lint that covers part of the ground; those name the
part it misses. Keep it that way when editing this file.

## Ownership and lifetimes

- A `clone()` added to satisfy the borrow checker where a borrow, an iterator,
  `Cow`, or moving ownership would be both clearer and cheaper. `redundant_clone`
  is nursery and off — nobody is checking these but the reviewer.
- `RefCell`, `Cell`, or `Mutex` used to route around ownership when there is no
  real shared-mutability requirement.
- `Rc<RefCell<T>>` / `Arc<Mutex<T>>` graphs that form a cycle where `Weak` is
  what breaks it.

## Panics and error handling

- `unwrap()`, `expect()`, `panic!`, `todo!`, `unimplemented!` on a path whose
  failure is recoverable or propagatable. The restriction lints for these are off
  by design — a deliberate `expect` documenting an invariant is fine, and only a
  reader can tell the two apart.
- An error stringified at the point it is caught instead of at the boundary that
  reports it, so the cause is gone by the time anyone reads it.
- A `Result` or `Option` swallowed, or mapped to a default that makes a failure
  look like an ordinary value.
- A public API that panics on ordinary invalid input instead of returning a typed
  error.

## Unsafe and FFI

- An `unsafe` block wider than the operation that needs it, or covering several
  unrelated invariants at once.
- A safety rationale that is missing, or that is stale relative to the code it
  now sits above — including on `unsafe fn`, `unsafe impl Send`, `unsafe impl Sync`.
- Raw pointer dereferences without validity, alignment, initialization, aliasing,
  and lifetime all established.
- An FFI boundary that does not check null, buffer length, ownership transfer,
  string encoding, or allocator compatibility.
- `static mut`, `transmute`, `MaybeUninit`, `mem::zeroed`, or hand-written drop
  logic without the documented invariant that makes it sound.

## Shared state

- A `Mutex`/`RwLock`/`RefCell` guard held across a call into user code, a
  blocking operation, or anything that can itself take a lock. `await_holding_lock`
  only sees `std` guards crossing an `.await`; a `tokio::sync` guard held over a
  slow call, or a `std` guard held over a long synchronous stretch, is invisible
  to it.
- Check-then-act on shared state: cache initialization, file creation, an
  existence test followed by a write.
- An `Ordering` too weak for what the atomic actually protects — or `SeqCst`
  everywhere, which hides which synchronization was intended.
- An `unsafe impl Send`/`Sync` whose contained state does not actually satisfy it.

## Async and cancellation

- A spawned task whose `JoinHandle` is dropped while its failure, or its
  completion before shutdown, still needs to be observed.
- A future that is not cancellation-safe: a partial write, a half-finished state
  transition, or a cleanup step that does not run when the future is dropped
  mid-flight.
- **Cancellation with no fallback.** Some call paths have no cancel token to
  hand down; on those, dropping the future is the only mechanism there is, and a
  timeout that only sets a flag leaves the work running. Ask what happens to the
  in-flight future, not just to the caller.
- Synchronous filesystem, network, or process calls on an async path that a
  request or a worker loop depends on.
- A retry loop without backoff, an overall timeout, a bounded attempt count, and
  cancellation propagation. All four, not three.

## Numbers, slices, and text

- An integer conversion or length arithmetic that can truncate, overflow, or go
  negative. The `cast_*` lints are pedantic and off.
- Byte slicing or truncation that can land inside a UTF-8 sequence, and width
  arithmetic that counts bytes or `char`s where the terminal counts display
  columns — a CJK or emoji run breaks both.
- An O(n²) lookup from nested loops where a `HashMap`/`HashSet`, a sort, or an
  index would obviously collapse it, on a path whose n is not bounded small.

## Macro definitions

Only when the diff actually defines a `macro_rules!` or a proc macro. Ordinary
macro invocations are not in scope.

- An `$x:expr` fragment interpolated more than once, so the caller's expression
  and its side effects run more than once. Bind it with a `let` in the expansion.
- An exported macro naming items without `$crate::`, which resolves to the wrong
  item — or to nothing — from another crate.
- A `$t:tt` fragment re-emitted without parentheses, silently changing operator
  precedence. This applies to `tt` only: an `:expr` fragment is already one
  complete expression.
- A proc macro that `unwrap`s or panics on malformed input instead of emitting a
  `syn::Error` / `compile_error!` with a useful span.
- Hygiene assumptions that break: generated identifiers relying on call-site
  names, or items that collide when the macro is invoked twice in one module.

## Secrets and untrusted input

- A secret, token, API key, or proxy URL reaching a log line, an error message, a
  session file, or a file the repository would commit.
- A path, URL, or command line built by string concatenation from model output or
  tool results. Classification before a release decision is whitelist-only:
  unparsed means unanalyzable means never automatically allowed.
- A repair or normalization step that *guesses*. Rewriting a malformed input into
  a valid one is fine only while the reading is unique and the rewritten text is
  still handed to the real parser for the verdict; anything ambiguous is
  rejected, not fixed. These arguments become shell commands.

## Contracts with the outside world

The recurring shape in this repository: a stream from a provider, a gateway, or
the model itself, and an assertion about it that was true until it wasn't.

- **A validator that calls a shape impossible.** Check what this repository's own
  code emits before believing it — the producer is often us. And when the same
  validator also runs over persisted sessions, tightening it does not just reject
  a live turn, it makes already-written files unopenable.
- **The same datum arriving twice.** Overwrite, accumulate, or error are three
  different bugs. Name which frame is authoritative under the protocol; where
  there is no basis to judge, accumulating silently doubles a counter that
  budgets and cost displays read.
- **Fail-closed aimed at the wrong party.** Close on invariants we cannot
  guarantee ourselves. A mistake the model or the peer made is fed back to them
  with the offending text included, and the turn continues — an error that
  reports only an offset gets the same broken output sent again.
- **A new variant or field that reaches disk.** The on-disk schema is the hardest
  thing here to change. Ask first whether the variant can be normalized away
  before it is persisted; if it can, its blast radius is one segment of the
  pipeline instead of rollout, replay, and the protocol.

---

Adapted from `internal/config/rules/rule_docs/rust.md` in
[alibaba/open-code-review](https://github.com/alibaba/open-code-review)
(Apache-2.0), at commit `7a571b7`, 2026-09-18. Trimmed to what clippy does not
already report, and extended with this repository's own repeat offenders.
