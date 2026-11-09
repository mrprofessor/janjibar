# Janjibar Implementation Plan

## Product

Janjibar is a native Kubernetes IDE with the feature set of Freelens, built in
Rust with GPUI. It is not a Freelens clone in looks. It matches what Freelens
does, then fixes what Freelens users complain about: memory, freezes on big
clusters, polled logs, charts that need Prometheus, and the local proxy.

## Requirements

- macOS 15 or later, Apple silicon. macOS only until 1.0; Linux and Windows
  come after, tested by hand on real machines.
- Rust stable (1.95 or later); Xcode Command Line Tools
- Zig 0.17 for building libghostty-vt (via nix, not installed globally)
- A kubeconfig with at least one reachable context
- Optional: `helm` on PATH for Helm writes; Prometheus or metrics-server for charts

## Technical Direction

- UI: `gpui-kit =0.9.0`, which pins and re-exports `gpui-pre =0.3.11` (a
  snapshot of Zed's GPUI). Enable `runtime_shaders` and `font-kit`.
- Kubernetes: `kube 5.0.0` and `k8s-openapi 0.29.0` (`latest`), on tokio.
- Bridge: vendor Zed's `gpui_tokio` (about 90 lines, Apache-2.0). Only plain
  data crosses between the tokio engine and the GPUI views.
- No local proxy. Each context gets one kube-rs client with the user's own
  auth. Sync PATH from the login shell before tokio starts.
- Custom resources list through server-side Table format (`as=Table`).
- Helm: read release secrets natively; write through the helm CLI.
- Metrics: Prometheus through the API server service proxy, with
  metrics-server as a fallback that still draws charts.
- Terminal: libghostty-vt pinned to Ghostty's `vt-1.0.0` tag (stable C API),
  drawn in GPUI. Start from a pinned fork of `gpui-cn` 0.8.0's terminal module
  (Apache-2.0, derived from Herdr) until it supports gpui-kit 0.9. Pod exec
  feeds kube-rs `AttachedProcess` bytes into it; local shells use its PTY
  source. Herdr's `src/vt.rs` is the reference for find and inline images.
  Fallback for CI without Zig: `ghostty-vt-rs-sys` (prebuilt vt-1.0.0).
  Ship Ghostty's MIT notices and a NOTICE line for Herdr and gpui-cn code.
  Do not copy Zed's GPL terminal code.
- Look: Herdr's design language. One terminal palette per theme drives the
  UI (chrome and canvas mixed darker, hover and borders as white overlays).
  Geist for UI, JetBrains Mono for names, logs and terminals. 13 px base,
  30 px rows, panes as 10 px-radius cards 8 px apart, sentence-case labels.
- Config: themes, keys and layouts as plain files in `~/.config/janjibar`
  with an `AGENTS.md`, like Herdr.

## MVP

### Clusters

- Sync kubeconfig files and folders, live.
- Clusters list in the sidebar, connect, disconnect, clear connection errors.
- A "Needs you" list: crash loops, stuck pods, node pressure, finished jobs.
- Access reviews decide which kinds and actions appear.

### Resources

- Live tables for core kinds: Pods, Deployments, StatefulSets, DaemonSets,
  ReplicaSets, Jobs, CronJobs, Services, Ingresses, ConfigMaps, Secrets,
  Nodes, Namespaces, Events, PVCs, PVs, StorageClasses.
- Custom resources grouped by API group, columns from the server.
- Namespace filter, search, filter tokens, sort, resize and hide columns.
- Inspector with Overview, YAML, Events.

### Actions

- Edit YAML with diff and server dry-run before apply.
- Delete (single and bulk), scale, restart, suspend and resume, trigger CronJob.
- Production guard: red frame and type-to-confirm for destructive actions.

### Logs

- Streaming pod logs per container, previous container, timestamps, search.
- Reconnect when the container restarts.

## 1.0

- Remaining built-in kinds and overview pages.
- Terminal: pod exec and attach, local cluster shell, node shell.
- Workload logs merged across pods; regex, level filter, JSON mode, download.
- Port-forward manager; services resolve to a ready pod per connection.
- Prometheus providers and PromQL ported from Freelens; external URLs.
- Helm releases, history, values; install, upgrade, rollback, uninstall; repos.
- Node cordon, uncordon and drain; rollout history and undo; create templates.
- Command palette across clusters; rebindable keys; theme files; per-cluster
  accent; preferences; auto-update; signed and notarized macOS build.

## Deferred

- Extensions as Herdr-style JS mods (after 1.0). No Freelens-compatible JS API.
- Linux, then Windows builds.
- Terminal find and inline images (port from Herdr).
- Views across several clusters at once.
- Debug containers and file copy.
- Bundled kubectl downloads.

## Architecture

1. **Engine** (`crates/engine`, tokio, no GPUI): cluster catalog, clients,
   watch hub, operations, streams, metrics, Helm.
2. **Model** (`crates/model`): plain data that crosses the bridge.
3. **Bridge** (`crates/bridge`): vendored `gpui_tokio` and snapshot channels.
4. **Kinds** (`crates/kinds`): per-kind column, detail and action specs.
5. **UI** (`crates/ui`, `crates/term`, `crates/editor`): gpui-kit views.
6. **App** (`crates/app`): windows, menus, keymap, startup.

Watches are shared and counted per kind and scope. A watch starts with its
first subscriber and stays warm for 45 seconds after its last one leaves.
Rows are built, filtered and sorted on tokio. The UI gets at most 30
snapshots a second and paints only visible rows.

## Milestones

0. **Spike** (done, ported and removed; see commit `ff1e9ec` for the code)
   - GPUI window with a live, virtualized pod list from kube-rs.
   - 71 MB footprint, 14 MB stripped binary, 44 s cold release build.
1. **Foundations** (weeks 1 to 2): workspace, Herdr-style shell, bridge,
   catalog, CI on macOS.
   - Done: workspace and lints, nix dev shell, CI, agent guide and skills,
     login-shell PATH sync, kubeconfig catalog across files, one client per
     context, pod watch with batched snapshots, Herdr-style shell with the
     clusters list and a live pod table, empty and error states.
   - Left: live kubeconfig reload with `notify`, disconnect and clear-error
     actions, keyboard focus and navigation, status bar extras (server
     version, watch count, own memory).
2. **Read-only explorer** (weeks 3 to 6): watch hub, tables, inspector, CRDs.
3. **Operate** (weeks 7 to 9): edits, actions, logs, metrics columns. MVP.
4. **Terminals and logs** (weeks 10 to 12): gpui-cn terminal fork, pod exec,
   local and node shells, workload logs, port-forwards.
5. **Metrics and Helm** (weeks 13 to 15).
6. **Polish to 1.0** (weeks 16 to 17).

## Testing

- Unit-test row projection, filter tokens, Helm secret decoding, PromQL
  builders and kubeconfig merging.
- Run the engine headless against a fake API server in CI (`tests/watch.rs`,
  in place today), and against kind once CI can run it: watches, edits, logs,
  exec, port-forward.
- GPUI view tests with `TestAppContext` for keyboard flows and the palette
  (harness in `crates/ui/tests`, first tests in place).
- Load test: a kind cluster with 5,000 pods and 100 CRDs must stay smooth and
  under 150 MB.
- Manual checks: VoiceOver, IME, exec auth plugins (aws, gke, kubelogin).

## Acceptance Criteria

The MVP is complete when a user can:

- Open any context from their kubeconfigs without extra setup.
- Browse every core kind and any custom resource with live updates.
- Edit, delete, scale and restart with a diff and dry-run first.
- Follow logs that survive container restarts.
- Do all of the above on a 5,000-pod cluster without the UI stalling.

No local proxy process. GPUI types must not appear in `engine` or `model`.

## Risks

- GPUI is pre-1.0 and changes weekly. Pin exact versions, keep it out of the
  engine, and budget a migration to official GPUI crates.
- The terminal stack is young: libghostty-vt 1.0 is days old, gpui-cn has one
  maintainer, and Zig joins the toolchain. Fork and pin; keep the prebuilt
  fallback. About a week to a working shell and pod exec.
- Exec auth plugins block and print prompts to stderr. Build clients on
  blocking threads and surface prompts in the UI.
- GPUI accessibility and IME are young. Test early, report upstream.

## Change log

Changes to this plan, newest first.

### 9 November 2026: first slice

- **Fake API server for engine tests.** Engine tests run against a small
  in-repo HTTP server that lists and watches pods
  (`crates/engine/tests/support/fake_api.rs`). The same server runs as
  `cargo run -p engine --example fake_cluster` for UI work without a cluster.
- **No auto-connect.** The app lists contexts and connects only when one is
  picked, so an exec plugin never pops a browser login at launch. `--context
  NAME` connects at start for scripts and development.
- **Ages refresh on a slow tick.** Ages change with time, not with objects.
  The engine re-projects once a second and still sends a snapshot only when
  the rows differ, so a quiet cluster wakes the UI rarely (pods under two
  minutes old show seconds and update each second).
- **GPUI view tests are blocked.** gpui-kit's `test-support` feature makes
  Cargo resolve `gpui-fast`, which pins `taffy =0.15.0` and conflicts with
  the rest of the tree. Retry on the next gpui-kit release.
- **Release builds keep symbols.** Cargo's default release strip breaks
  proc-macro dylibs with this toolchain on macOS 27. Strip at packaging time.
- **Memory needs a window size.** The first slice measured 250 MB physical
  footprint with the window at 3840x2224 pixels (a tiling window manager made
  it full screen). About 207 MB of that is GPU surfaces that scale with window
  pixels; the heap is about 27 MB. The 150 MB load-test budget should name a
  window size, for example 1360x840 points.

