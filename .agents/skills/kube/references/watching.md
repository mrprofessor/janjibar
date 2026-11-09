# Watching at scale

## The hub (from PLAN.md, milestone 2)

- A pane subscribes to a kind and a scope (all namespaces, a set of
  namespaces, a label selector).
- The hub counts subscribers per (context, kind, scope). The first subscriber
  starts the watch. When the last leaves, the watch stays warm for 45 s, then
  stops (drop the task, the store goes with it).
- Rows are built, filtered and sorted on tokio. The UI gets at most 30
  snapshots a second and paints only visible rows.

This is in place in `crates/engine/src/hub.rs`. The UI subscribes with
`Command::Subscribe` (context, `GroupVersionResource`, namespace) and leaves
with `Command::Unsubscribe`; the inspector holds a reference with
`Command::Inspect`. Snapshots are `model::TableSnapshot`, with the watch
state inside. Tests set a short linger through `engine::Options`.

## Memory

- Strip `managedFields` with `.modify()` before the reflector stores objects.
  They are often the largest part of an object.
- Metadata-only watches (`watcher` over `Api<PartialObjectMeta<K>>`) for kinds
  whose bodies we do not show, and always for Secrets.
- `page_size(500)` bounds each list response. `streaming_lists()` avoids the
  big initial list on servers that support it (1.27+ with the feature on,
  default on in newer releases). Try it in the 5,000-pod load test.
- `store.state()` clones `Arc`s, not objects. Project rows straight from it.

## Correctness

- The watcher re-lists after `410 Gone` and on desync. Do not treat a re-list
  as "all objects deleted and re-added" in the UI; the store handles it and
  the next snapshot is just the new state.
- `watcher::Event::Init` / `InitApply` / `InitDone` bracket a (re)list. The
  reflector swaps the store atomically at `InitDone`.
- Backoff: always `.default_backoff()`. Report repeated errors as a degraded
  connection, but keep the last good rows on screen.
- Bookmarks are on by default and should stay on.

## Access checks

Before watching a kind, ask the server with a `SelfSubjectAccessReview`
(`authorization.k8s.io/v1`) for `list` and `watch` on that resource. Kinds the
user cannot read never appear. Cluster-wide 403: fall back to the configured
accessible namespaces.
