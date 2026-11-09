---
name: kube
description: kube-rs 5 (kube 5.0.0, k8s-openapi 0.29.0) patterns for the engine crate. Load before writing or changing kubeconfig loading, client creation, watchers, reflectors and stores, metadata-only watches, log streams, exec or attach with resize, port-forwarding, server-side apply or dry-run, custom resources through the server's Table format, or anything touching exec auth plugins and PATH.
---

# kube-rs 5 in this repo

The engine (`crates/engine`) owns every Kubernetes call. It runs on tokio
threads the app starts, never on GPUI's executors, and sends only `model`
types to the UI.

Versions: `kube 5.0.0` (features `runtime`; add `ws` for exec, attach and
port-forward), `k8s-openapi 0.29.0` with `latest`. Times are
`k8s_openapi::jiff::Timestamp` (jiff, not chrono). The source is in
`~/.cargo/registry/src/*/kube-{client,runtime,core}-5.0.0/`; read it when
unsure, docs.rs may describe a different version.

## Kubeconfig: a catalog across many files

Sources follow kubectl: `KUBECONFIG` (a `:`-separated list) when set, else
`~/.kube/config`. We keep **every context from every file**, remembering
which file it came from.

- Read each file with `kube::config::Kubeconfig::read_from(path)`. It handles
  multi-document YAML, BOMs and UTF-16, and rewrites relative cert, key,
  token-file and exec command paths to absolute ones.
- Missing files are skipped, like kubectl. Files that fail to parse become a
  per-file problem in the catalog; the other files still load.
- Duplicate context names: the first file wins (kubectl's rule). Record the
  shadowed ones, do not silently drop them in the UI later.
- To connect, merge all files in the same order with `Kubeconfig::merge`
  (first value wins per key) so a context in one file can use a user or
  cluster from another, exactly as kubectl allows. Then
  `Config::from_custom_kubeconfig(merged, &KubeConfigOptions { context: Some(name), ..Default::default() })`.
- Watch the files with `notify` later (live reload). Re-read on change; never
  hold a parsed kubeconfig across reloads.

## Clients: one per context, built on a blocking thread

- One `kube::Client` per context, created on first use and cached. Clients
  are cheap to clone (`Arc` inside); clone them into tasks.
- **Build it on a blocking thread:**
  `tokio::task::spawn_blocking(move || kube::Client::try_from(config))`.
  `Client::try_from` runs exec auth plugins (`aws`, `gke-gcloud-auth-plugin`,
  `kubelogin`...) synchronously with `std::process::Command`. A plugin can
  take seconds or wait on a browser login. On a tokio worker it stalls every
  other task on that thread. Token refresh later already uses
  `spawn_blocking` inside kube.
- macOS GUI apps get a bare PATH (`/usr/bin:/bin:/usr/sbin:/sbin`). The app
  copies PATH from the login shell before tokio starts. If a plugin is still
  missing, kube returns `Error::Auth(AuthError::AuthExecStart(io))` with
  `io.kind() == NotFound`. Map that to a clear error naming the command and
  the PATH searched.
- Interactive plugins print prompts to stderr (device codes). kube captures
  stderr only on failure. Surfacing prompts needs our own exec runner; until
  then, report the failure text.
- Errors to tell apart for the UI: auth plugin missing, auth failed,
  unreachable (connect error or timeout), forbidden (403 from a list), and
  everything else. `kube::Error::Api(status)` carries `code` and `reason`.

## Watching lists

```rust
use kube::runtime::{reflector, watcher, WatchStreamExt};

let api: Api<Pod> = Api::all(client.clone());
let (store, writer) = reflector::store::<Pod>();
let stream = watcher(api, watcher::Config::default().page_size(500))
    .default_backoff()            // never drop the watch on a transient error
    .modify(|pod| {               // strip what we never show
        pod.managed_fields_mut().clear();
    })
    .reflect(writer)
    .touched_objects();           // Apply and Delete both mean "changed"
```

- `watcher` lists in pages (default 500, same as client-go), then watches
  from the list's resourceVersion and re-lists on `410 Gone`. Events are
  `Init`, `InitApply(obj)`, `InitDone`, `Apply(obj)`, `Delete(obj)`.
- Always add `.default_backoff()` (800 ms doubling to 30 s, reset after
  120 s of health). Without it the first error ends the stream.
- `Store::wait_until_ready()` resolves after the first full list.
  `store.state()` returns `Vec<Arc<K>>`: cheap, but it is a full copy of the
  pointers, so call it once per snapshot, not per row.
- The stream must be polled for the store to update. Drive it in its own task
  and tie the task to the subscription: drop or abort it to stop the watch.
- Errors from the watcher are items in the stream (`Err(watcher::Error)`).
  Log them and report a degraded connection state; backoff keeps retrying.
- `streaming_lists()` uses Kubernetes 1.27+ watch-list and saves memory on
  big clusters, but needs the server feature. Not on by default yet.
- Secrets: watch `Api<PartialObjectMeta<Secret>>` with the normal `watcher`;
  kube 5 sends metadata-only requests for it, so values never sit in memory.
  (`metadata_watcher` is deprecated in kube 5.)

### Testing watches without a cluster

`crates/engine/tests/support/fake_api.rs` is a small HTTP server that serves
`GET /api/v1/pods` (a `PodList`) and the same path with `watch=true`
(chunked watch events, replayed from the requested resourceVersion). Point a
client at it with `kube::Config::new(api.url().parse()?)`. The first
snapshot is sent after the initial list, before the watch request opens, so
wait for `open_watches()` instead of asserting it right away.

### Snapshots for the UI

Project rows on tokio, never in the view. Coalesce: wake on change, then
send at most every 33 ms, and only if something changed. See `run` in
`crates/engine/src/hub.rs` for the pattern (a `watch` counter bumped by the
stream, a loop that sleeps out the rest of the 33 ms window, then builds and
sends). Age strings also change with time; re-project on a slow tick and
send only if the rows differ.

### Typed kinds and server-side tables

Both produce `model::TableSnapshot` (`Event::Table`), keyed by
`GroupVersionResource`, so the UI draws them the same way. Typed specs use
`model::Column` with format hints `name`, `status` (coloured by
`TableRow::health`) and `muted`. Neither sends a namespace column; the UI
adds one when listing across namespaces. Kinds without a spec are served by
`engine::tables::watch_table` once issue #19 routes them through the hub.

### Adding a typed kind

1. Add a variant to `model::resource::Kind` (names, API group and plural,
   actions).
2. Write a `Spec` in `crates/kinds` (columns, `row`, `details`) with tests.
3. Add one arm to `spawn_watch` in `crates/engine/src/hub.rs` and one to
   `owner_refs` in `owners.rs`.
4. Add an icon in `ui::assets::kind_icon` and a place in the sidebar's
   `GROUPS`.

## Logs

- `api.log_stream(name, &LogParams { follow: true, container, timestamps: true, since_seconds, tail_lines, previous, ..Default::default() })`
  returns an `AsyncBufRead`; read it with `.lines()` from
  `futures::AsyncBufReadExt`.
- The stream ends when the container stops. To survive restarts, watch the
  pod, and when `restartCount` changes or the container is running again,
  reopen with `since_time` set to the last timestamp seen. Dedupe the overlap
  by timestamp.

## Exec, attach, terminal resize

Needs the `ws` feature.

```rust
let ap = AttachParams::interactive_tty().container("app"); // stdin + tty
let mut proc = pods.exec("web-0", ["sh", "-c", "exec bash || exec sh"], &ap).await?;
let mut stdin = proc.stdin().expect("stdin requested");
let stdout = proc.stdout().expect("stdout requested");
let mut resize = proc.terminal_size().expect("tty requested");
resize.send(TerminalSize { width: cols, height: rows }).await?;
let status = proc.take_status().expect("status").await; // exit code is in here
```

- With a tty, stderr is merged into stdout; do not ask for both.
- Send a resize right after connecting and on every grid change.
- Bytes go straight into the terminal emulator; keys and terminal query
  replies go to `stdin`.

## Port-forward

- `pods.portforward(name, &[port]).await?` gives a `Portforwarder`;
  `take_stream(port)` returns a duplex stream per connection. Accept on a
  local `TcpListener` and pipe each connection with
  `tokio::io::copy_bidirectional`.
- For a Service, resolve a ready backing pod **per connection** (from the
  service selector or EndpointSlices), so forwards survive pod restarts.

## Writes: server-side apply and dry-run

- Apply: `api.patch(name, &PatchParams::apply(brand::FIELD_MANAGER), &Patch::Apply(&obj))`.
  Add `.force()` only after the user confirms taking over conflicting fields.
- Dry-run first: `PatchParams::apply(...).dry_run()`, or
  `PostParams { dry_run: true, .. }` for create. Show the server's result as a
  diff, then apply for real.
- Replace with optimistic locking: keep `metadata.resourceVersion` from the
  read; a `409 Conflict` means someone else changed it. Surface it.
- Delete: `api.delete(name, &DeleteParams::default())`; `DeleteParams::foreground()`
  or `background()` for propagation.

## Custom resources: the server's Table format

kube-rs has no Table client. We build the request by hand, like kubectl and
k9s, so the server computes printer columns:

```rust
let req = http::Request::get(format!("{path}?limit=500"))
    .header(http::header::ACCEPT,
        "application/json;as=Table;v=v1;g=meta.k8s.io,application/json")
    .body(Vec::new())?;
let table: serde_json::Value = client.request(req).await?;
```

- Columns are in `columnDefinitions` (`name`, `type`, `format`, `priority`);
  rows have `cells` and, with `includeObject=Metadata`, an `object` holding
  `PartialObjectMeta`. Priority 0 columns are the default view.
- Watch with the same header plus `watch=1` to get Table rows as events.
- Discover kinds with `kube::discovery::Discovery::new(client).run()` or
  per-group `discovery::group`. Use access reviews before watching.

## Pitfalls

- No kube calls in `cx.background_spawn` or any non-tokio executor: they
  panic without a reactor.
- Never `block_on` inside the runtime. Never hold a `std::sync::Mutex` guard
  across `.await`.
- `Api::namespaced` vs `Api::all`: a user who cannot list cluster-wide gets
  403 on `Api::all`. Fall back to the context's namespace or the configured
  accessible namespaces.
- `Api::default_namespaced` uses the context's namespace, which may be unset
  in the kubeconfig (kube defaults it to `default`).

## References

- `references/kubeconfig.md`: merge rules, exec-auth details, PATH sync.
- `references/watching.md`: watcher config, memory, the hub design from
  `PLAN.md` (counted watches, 45 s warm period).
