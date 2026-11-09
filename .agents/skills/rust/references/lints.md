# Lints

Configured in the root `Cargo.toml` under `[workspace.lints]`, plus
`clippy.toml` for test allowances. Members opt in with `[lints] workspace = true`
and cannot override single lints there; use `#[expect(..., reason = "...")]` in
code instead.

## Clippy groups

From <https://doc.rust-lang.org/clippy/lints.html>:

| Group       | Default | Meaning                                              | Here      |
| ----------- | ------- | ---------------------------------------------------- | --------- |
| correctness | deny    | Outright wrong or useless code                       | warn, `-D warnings` in CI |
| suspicious  | warn    | Most likely wrong                                    | warn      |
| style       | warn    | More idiomatic code                                  | warn      |
| complexity  | warn    | Simple things done in a complex way                  | warn      |
| perf        | warn    | Code that can run faster                             | warn      |
| pedantic    | allow   | Strict, some false positives                         | warn, minus a few |
| restriction | allow   | Bans features; never enable as a whole               | cherry-picked |
| nursery     | allow   | New or buggy                                         | off       |
| cargo       | allow   | Manifest checks for published crates                 | off       |

`clippy::all` is correctness, suspicious, style, complexity and perf.

Groups get `priority = -1` so single-lint overrides win. Within `[lints]`,
table order is ignored, and equal priorities have no defined order. Clippy's
`lint_groups_priority` flags this.
<https://doc.rust-lang.org/cargo/reference/manifest.html#the-lints-section>

## Pedantic lints we allow, and why

- `module_name_repetitions`: `catalog::Catalog` reads fine. (Newer Clippy
  moved it to restriction; the allow is harmless.)
- `must_use_candidate`: noise on every getter in an app.
- `missing_errors_doc`, `missing_panics_doc`: app crates, not a public API.
  Still write `# Errors` where it helps.
- `similar_names`, `too_many_lines`, `items_after_statements`: style calls the
  reviewer can make.
- `cast_precision_loss`: UI code converts counts to `f32` pixels.

## Restriction lints we turn on

- `unwrap_used`, `expect_used`, `panic`: panics are for bugs, with a stated
  invariant (`#[expect(..., reason)]`). Allowed in tests via `clippy.toml`.
- `todo`, `unimplemented`, `dbg_macro`: no leftovers in shipped code.
- `print_stdout`, `print_stderr`: use `tracing`.
- `undocumented_unsafe_blocks`: every `unsafe` needs `// SAFETY:`.
- `allow_attributes_without_reason`: every suppression explains itself.
- `clone_on_ref_ptr`: write `Arc::clone(&x)` so shared-pointer clones are
  visible.
- `mem_forget`, `exit`: no leaked destructors, no hard exits past cleanup.

## rustc lints

- `unsafe_code = "deny"`: unsafe only with an explicit, reviewed `#[expect]`.
- `rust_2018_idioms` (warn), minus `elided_lifetimes_in_paths`, which would
  force `Context<'_, Self>` all over GPUI code.
- `unused_lifetimes` (warn).

## Adding a lint

Turn it on, run `cargo clippy --workspace --all-targets`, fix or justify every
hit in the same commit. Never land a lint that is mostly `#[expect]`s.
