# AGENTS.md

Guide for coding agents (Claude, Codex and friends) working in this repo.
`CLAUDE.md` imports this file. Keep it short and true.

## What the app is

A native Kubernetes IDE for macOS, written in Rust with GPUI. It aims at the
feature set of Freelens in one small native process: no browser, no local
proxy. macOS first; Linux and Windows come after 1.0.

- `PLAN.md` holds the decisions, MVP, milestones and risks. Read it before
  large changes, and update it when the plan changes.
- `README.md` is for humans.

## Workspace layout

```
crates/
  app/      the binary: startup, shell PATH sync, tokio runtime, window
  engine/   tokio side: kubeconfig catalog, clients, watches, row projection
  model/    plain data that crosses the bridge (rows, catalog, events)
  bridge/   vendored gpui_tokio; tokio <-> GPUI glue
  ui/       GPUI views: shell, sidebar, tabs, tables, theme, fonts
.agents/skills/   skills for agents (.claude/skills links here)
```

Planned crates, added only when they get real code: `kinds` (per-kind specs),
`term` (terminal pane), `editor` (YAML edit, diff, dry-run).

## Commands

Rust is not installed globally. Everything runs in the nix dev shell, which
also sets `DEVELOPER_DIR` to the system Xcode.

```sh
nix develop                                   # interactive shell
nix develop -c cargo build                    # build everything
nix develop -c cargo run -p app               # run the app (binary: janjibar)
nix develop -c cargo run -p app -- --context NAME   # connect to NAME at start
nix develop -c cargo run -p app --release     # release build, for measuring
nix develop -c cargo test --workspace         # all tests (no cluster needed)
nix develop -c cargo clippy --workspace --all-targets -- -D warnings
nix develop -c cargo fmt --all                # format (CI runs --check)
nix develop -c cargo deny check               # licenses, bans, advisories
```

Before you commit, run fmt, clippy with `-D warnings`, and the tests. CI
(`.github/workflows/ci.yml`) runs the same on an Apple-silicon runner, plus
cargo-deny on Linux.

Logs: `RUST_LOG=engine=debug,app=debug nix develop -c cargo run -p app`.

## Architecture rules

- `engine` and `model` never depend on GPUI or gpui-kit. Check with
  `cargo tree -p engine | grep -i gpui` (must print nothing).
- `ui` never depends on kube-rs or k8s-openapi. Only `model` types cross.
- The engine runs on tokio threads that the app starts. GPUI runs on the main
  thread. They talk through `tokio::sync::mpsc` channels carrying `model`
  types. A `Receiver` can be awaited inside `cx.spawn` directly.
- The engine sends snapshots at most every 33 ms and only when something
  changed. The UI swaps the snapshot in and paints visible rows only.
- Kubernetes clients are built on a blocking thread: exec auth plugins run
  synchronously and may wait on a browser login.
- `unsafe` is denied workspace-wide. The one exception is `set_var` during
  startup in `app`, before any thread exists. New unsafe needs a
  `// SAFETY:` comment and a reason in review.

## Version pins

- `gpui-kit =0.9.0`, which re-exports `gpui-pre =0.3.11` and turns on
  `runtime_shaders` and `font-kit`. Import GPUI as `gpui_kit::...`. Do not add
  `gpui-pre` directly unless gpui-kit lacks something; if you must, pin it
  `=0.3.11`.
- `kube 5.0.0`, `k8s-openapi 0.29.0` with `latest`, `tokio 1.54`.
- YAML is `serde-saphyr`. `serde_yml` and `serde_yaml` are banned in
  `deny.toml`.
- Commit `Cargo.lock` and `flake.lock`. Bump GPUI as a deliberate change of
  its own, never as a side effect.

## The name rule

The name "Janjibar" is temporary. It appears only in the README title, the
binary name and `crates/model/src/brand.rs`. Everywhere else, including docs
and skills, say "the app". Crates keep generic names.

## Writing and commits

- Plain English, short sentences. No em dashes anywhere: code, docs, commits.
- Small, logical commits with plain imperative subjects ("Add pod watch").
- Never add Co-Authored-By lines or any AI or agent attribution to commits,
  PRs or docs.
- Third-party code keeps its license header and gets a line in `NOTICE`.

## Skills

Load the skill before working in its area. They live in `.agents/skills/`.

| Skill  | Load it when                                                        |
| ------ | ------------------------------------------------------------------- |
| `rust` | Writing or reviewing any Rust here: errors, types, async, tests.     |
| `gpui` | Touching `ui`, `app` or `bridge`: views, entities, actions, theme.   |
| `kube` | Touching `engine`: kubeconfig, clients, watches, logs, exec, apply.  |

## Testing without a cluster

There is no cluster in CI, and engine tests must not need one.

- Unit tests sit next to the code; kubeconfig fixtures are in
  `crates/engine/tests/fixtures`.
- `crates/engine/tests/watch.rs` runs the engine end to end against a fake API
  server (`crates/engine/tests/support/fake_api.rs`).
- To see the app with data, run the same server as an example:
  `cargo run -p engine --example fake_cluster -- --kubeconfig /tmp/fake.yaml`,
  then start the app with `KUBECONFIG=/tmp/fake.yaml` and `--context fake`.
- The owner's machine has a kind cluster for manual checks. Never change
  anything in a real cluster outside a throwaway namespace you delete after.
- Screenshots: capture only the app's window (`screencapture -l <id>`), never
  the whole screen, and never commit them.
