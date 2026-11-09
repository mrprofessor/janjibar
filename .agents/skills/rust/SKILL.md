---
name: rust
description: How to write good Rust in this repo. Load before writing, refactoring or reviewing any Rust code here, including error types, newtypes and enums, ownership and cloning, iterators, Arc vs Rc, async and tokio (blocking, spawn_blocking, cancellation, select!), unsafe and FFI, module layout, tests, docs, lints and performance work.
---

# Rust in this repo

This is a GUI app in a Cargo workspace, not a published library. Prefer
plain, concrete code. Rules below come from Microsoft's Pragmatic Rust
Guidelines, the Rust API Guidelines, Clippy's docs, tokio's docs and a few
well-regarded agent skills. Guideline IDs are cited in brackets so you can
look them up; `references/sources.md` has the links and licenses.

## Before you hand back code

```sh
nix develop -c cargo fmt --all
nix develop -c cargo clippy --workspace --all-targets -- -D warnings
nix develop -c cargo test --workspace
```

All three must pass. Never add `#![deny(warnings)]` in source; CI passes
`-D warnings`. Lints are set once in the root `Cargo.toml` (`[workspace.lints]`)
and members inherit them; a member cannot override single lints there.

## Errors

- **Libraries (`engine`, `model`, `bridge`, `ui`) use typed errors** built with
  `thiserror`. **`anyhow` only at the edges**: `app`'s `main`, startup, and
  test helpers. This follows the thiserror and anyhow READMEs [M-APP-ERROR].
- One error type per area or operation (`ShellEnvError`, `ConnectError`), not
  one crate-wide enum of everything [M-ERRORS-CANONICAL-STRUCTS].
- Messages are short, lowercase, no trailing period [C-GOOD-ERR]. Put the
  cause in `#[source]` **or** in the message, not both, or reports repeat it.
- Use `?` with `#[from]` for our own conversions [M-FROM-ERROR]; `map_err`
  for foreign errors or to add context.
- Errors that reach the UI must say what to do. `model::ConnectError` has a
  variant per empty state ("auth plugin `kubelogin` not found on PATH").
- Never swallow an error silently. `let _ = fallible()` needs a comment saying
  why the result does not matter (for example "the receiver is gone, the UI
  closed"). Otherwise log it with `tracing` or return it.

## Panics, unwrap, expect

- A panic means a bug, never a user-facing error [M-PANIC-IS-STOP,
  M-PANIC-ON-BUG]. Bad kubeconfig, a dead cluster or a missing binary are
  errors, not panics.
- `unwrap_used`, `expect_used` and `panic` warn outside tests (`clippy.toml`
  allows them in tests). If an invariant truly holds, state it:

```rust
#[expect(clippy::expect_used, reason = "the closure above always sets `built`")]
let view = built.expect("open_window ran its build closure");
```

- Prefer making the bad state unrepresentable over asserting it [M-PANIC-ON-BUG].
- Suppress lints with `#[expect(lint, reason = "...")]`, not `#[allow]`
  [M-LINT-OVERRIDE-EXPECT]. `allow_attributes_without_reason` is on.

## Types over strings

- Newtypes for identifiers that must not mix: `ContextName`, later
  `Namespace`, `Kind` [C-NEWTYPE]. Keep the inner field private and add
  `as_str()` [C-NEWTYPE-HIDE].
- Enums for closed sets: `Health`, `ConnectionState`, `Command`. Never pass
  a status around as a `String` when code branches on it.
- No boolean parameters with unclear meaning: `Follow::Yes` beats `true`
  [C-CUSTOM-TYPE].
- Use the strongest std type: `PathBuf` for paths, `Duration` for time spans,
  `jiff::Timestamp` for instants [M-STRONG-TYPES].
- Derive the common traits eagerly: `Debug`, `Clone`, `PartialEq`, `Eq`,
  `Hash`, `Default` where they make sense [C-COMMON-TRAITS]. Every public
  type is `Debug` [C-DEBUG]; give secret holders a manual impl that redacts.
- Conversions: `as_` is a cheap borrow, `to_` is costly, `into_` consumes
  [C-CONV]. Getters have no `get_` prefix [C-GETTER].

## Ownership and cloning

- Borrow by default. Take `&str`, `&[T]`, `&Path` in parameters; return
  owned values.
- Data shared between threads or kept in many rows: `Arc<str>`, `Arc<[T]>`.
  Clone them with `Arc::clone(&x)` so the cost is visible (`clone_on_ref_ptr`).
- `Rc` only in single-threaded code; GPUI entities already handle sharing on
  the UI side. Never `Rc<RefCell<_>>` across an `.await`.
- When the borrow checker complains, try in order: a named temporary, a
  narrower borrow scope, `std::mem::take` or `replace`, splitting a struct,
  then a deliberate small clone. Indices into a `Vec` come last. (Adapted
  from BeautifulRust, MIT.)
- Clone before `spawn(async move { ... })` by shadowing in a block, so the
  original stays usable.

## Iterators

- Prefer iterator chains over index loops: `filter_map`, `map`, `collect`,
  `any`, `find`, `sum`.
- Indexing (`v[i]`) can panic; use `get` or iterate unless the index is proven
  in range.
- Do not `collect()` just to iterate again. Do collect once before handing
  data to another thread.
- Pre-size with `Vec::with_capacity` when the length is known.
- Sort with `sort_unstable_by` unless stability matters.

## Async rules

See `references/async.md` for details and sources.

- **Never block in async code.** No `std::thread::sleep`, blocking file I/O,
  `std::process::Command::output`, or heavy CPU loops on a tokio worker.
  Aim for under 100 microseconds between `.await`s.
- Blocking work goes to `tokio::task::spawn_blocking` (it cannot be aborted
  once started). Building kube clients is blocking: exec auth plugins run
  inside `Client::try_from`.
- **Cancellation is by drop.** Dropping a future or an `AbortOnDrop` task
  handle stops it at the next `.await`. Dropping a plain `JoinHandle`
  **detaches** the task: it keeps running. Store an abort guard when the task
  must die with its owner.
- **`select!` branches must be cancel safe.** Safe: `mpsc` `recv`,
  `watch::Receiver::changed`, `StreamExt::next`, `Interval::tick`. Not safe:
  `Notify::notified`, `Mutex::lock`, `read_exact`, `write_all`.
- **No `std::sync::Mutex` guard across `.await`.** Scope the guard in a block.
  Prefer giving state to one task and talking with messages.
- Use bounded channels where a fast producer could outrun the consumer;
  unbounded only when the consumer always keeps up or the producer coalesces.
- tokio code never runs on GPUI's executors, and GPUI code never runs on tokio.
  Cross with channels of `model` types (see the `gpui` skill).

## Unsafe and FFI

- `unsafe_code` is denied workspace-wide. Unsafe is allowed only for FFI,
  platform calls, or a measured performance need [M-UNSAFE].
- FFI lives in its own `-sys` style crate that only translates; safe logic
  stays in normal crates [M-FFI-TRANSLATES]. The future terminal crate will
  follow this.
- Every `unsafe` block gets a `// SAFETY:` comment explaining why the
  preconditions hold (`undocumented_unsafe_blocks` warns).
  `unsafe fn` only when misuse can cause UB [M-UNSAFE-IMPLIES-UB].
- The one unsafe today: `std::env::set_var` in `app` startup, before any
  thread exists. That is why PATH sync runs before the tokio runtime starts.

## Module layout

- One concept per file, named for what it does: `catalog.rs`, `hub.rs`,
  `shell_env.rs`. Group by feature, not by kind (no `types.rs`, `utils.rs`)
  [M-BALANCED-MODULES].
- Use `foo.rs` plus `foo/` for submodules, not `foo/mod.rs`.
- Keep `pub` small. Re-export the main types at the crate root; keep helpers
  private or `pub(crate)`.
- No preludes and no glob re-exports [M-NO-PRELUDE, M-NO-GLOB-REEXPORTS].
  `use gpui_kit::prelude::*` is the accepted exception for GPUI traits.
- No `Manager`, `Service`, `Helper` names [M-WEASEL-WORDS].
- Imports in groups: std, external crates, workspace crates, `crate::`/`super::`.

## Tests

- Unit tests sit next to the code in `#[cfg(test)] mod tests`.
- Engine tests never need a cluster. Test pure functions (projection, ages,
  parsers, catalog merging) with fixtures in `crates/<crate>/tests/fixtures`.
- Tests check behaviour, not a copy of the implementation
  [M-TAUTOLOGICAL-TESTS]. Name them for the behaviour: `first_file_wins_on_duplicates`.
- Port external behaviour faithfully and test against known outputs (for
  example kubectl's age and status strings).
- Async tests: `#[tokio::test]`, and `tokio::time::pause()` for timing logic.
- `clippy.toml`'s test allowances cover `#[test]` fns and `#[cfg(test)]`
  modules only. Helper fns in `tests/*.rs` still trip `unwrap_used`; put one
  crate-level `#![allow(clippy::unwrap_used, ..., reason = "...")]` at the top
  of that test file (see `crates/engine/tests/watch.rs`).
- GPUI view tests: see the `gpui` skill.

## Docs and comments

- Every public item gets a `///` summary of about one line [M-FIRST-DOC-SENTENCE].
  Add `# Errors`, `# Panics` or `# Safety` sections when they apply
  [M-CANONICAL-DOCS]. Each crate and public module has `//!` docs [M-MODULE-DOCS].
- Comments explain **why**, not what. Name magic numbers as consts with a doc
  saying why that value [M-DOCUMENTED-MAGIC] (see `SNAPSHOT_WINDOW`).
- Describe the code as it is, not its history [M-NO-META-DESIGN-DOCUMENTATION].
- Plain English, short sentences, no em dashes.

## Logging

- `tracing` macros with named fields: `tracing::warn!(%context, "watch error: {err}")`
  [M-LOG-STRUCTURED]. No `println!` or `dbg!` (`print_stdout` and `dbg_macro` warn).
- Libraries emit events; only `app` installs a subscriber.
- Never log secrets: tokens, kubeconfig user entries, Secret data.

## Performance

- Measure before optimising [M-HOTPATH]. Use `vmmap --summary <pid>` for
  memory, a release build for timing, and benchmarks for hot paths.
- The known hot paths are row projection and snapshot building on big
  clusters. Keep them allocation-light and off the UI thread.
- No unsafe, no exotic data structures, no extra crates for speed without a
  number that shows the need.

## Review checklist

When reviewing (yours or others'), reject on sight:

- `unwrap`/`expect` in shipping code without a stated invariant.
- An error dropped without a reason, or recovered silently.
- Blocking calls inside async, or kube calls outside tokio.
- GPUI types in `engine` or `model`; kube types in `ui`.
- `#[allow]` without a reason; new lints switched off to make code pass.
- Performance changes without a measurement.
- Comments that restate the code; TODOs without context.

(The severity-and-rejection format is adapted from
apollographql/rust-best-practices, Apache-2.0.)

## References

- `references/async.md`: tokio blocking, cancellation, select!, tasks.
- `references/lints.md`: what each Clippy group means and why our set is what it is.
- `references/sources.md`: every source with its link and license.
