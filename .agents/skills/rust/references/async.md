# Async Rust with tokio

Sources are linked inline. All tokio docs are MIT.

## What blocking means

- tokio tasks are cooperative. A task runs until it reaches an `.await` that
  is not ready. A blocking call stalls the whole worker thread and every task
  queued on it, including blocking done inside a destructor.
  <https://docs.rs/tokio/latest/tokio/task/index.html>
- Rule of thumb: no more than 10 to 100 microseconds of work between `.await`
  points. <https://ryhl.io/blog/async-what-is-blocking/>
- Microsoft's guideline M-YIELD-POINTS says the same for long CPU loops: call
  `tokio::task::yield_now().await` now and then.

Where blocking work goes:

| Work                                 | Use                                      |
| ------------------------------------ | ---------------------------------------- |
| Blocking I/O, short (files, exec)    | `tokio::task::spawn_blocking`            |
| Heavy CPU                            | `spawn_blocking` for now; rayon if it grows |
| Runs forever                         | a dedicated `std::thread`                |

- `spawn_blocking` runs on a separate pool with a large thread limit. Once
  started it **cannot be aborted**, and runtime shutdown waits for it.
  <https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html>
- `block_in_place` panics on a current-thread runtime and suspends sibling
  futures in the same task. Avoid it.
  <https://docs.rs/tokio/latest/tokio/task/fn.block_in_place.html>
- `tokio::spawn` and `Handle::current()` panic outside a runtime. GPUI
  callbacks are outside one. Pass a `Handle` or use a channel.

In this repo, blocking calls are: reading kubeconfig files, `Client::try_from`
(runs exec auth plugins), running the login shell. All go through
`spawn_blocking`, except the login shell, which runs before tokio exists.

## Locks

- Holding a `std::sync::MutexGuard` across `.await` makes the future `!Send`
  (good: it fails to compile in `tokio::spawn`) or deadlocks with guards that
  are `Send`. End the guard's scope in a block before awaiting.
  <https://tokio.rs/tokio/tutorial/shared-state>
- A std mutex is right for short critical sections on plain data (the client
  cache in `engine::client`). `tokio::sync::Mutex` is only for holding a lock
  across `.await`, and costs more.
  <https://docs.rs/tokio/latest/tokio/sync/struct.Mutex.html>
- Often the better design is one task that owns the state and receives
  messages (the engine loop does this).
- Clippy's `await_holding_lock` (on by default) catches most mistakes.
- Same trap with tracing: do not hold `span.enter()` across `.await`; use
  `.instrument(span)`. <https://docs.rs/tracing/latest/tracing/struct.Span.html#method.enter>

## Cancellation

- Dropping a future cancels it. Code after the pending `.await` never runs;
  local destructors do. <https://tokio.rs/tokio/tutorial/select>
- Dropping a `JoinHandle` **detaches** the task; it keeps running.
  `JoinHandle::abort()` cancels at the next `.await`.
  <https://docs.rs/tokio/latest/tokio/task/struct.JoinHandle.html>
- Dropping an `AbortHandle` does nothing. Dropping a `JoinSet` aborts all its
  tasks. <https://docs.rs/tokio/latest/tokio/task/struct.JoinSet.html>
- To tie a task's life to an owner, keep a guard that aborts on drop. The
  engine has `AbortOnDrop` in `crates/engine/src/task.rs`; tokio-util has
  `AbortOnDropHandle`. GPUI's `Task` already cancels on drop.
- For cooperative shutdown with cleanup, use tokio-util's `CancellationToken`
  (and `TaskTracker` to wait). <https://tokio.rs/tokio/topics/shutdown>

## select! and cancel safety

`select!` drops every branch that did not win. A branch is cancel safe when
dropping it and starting again loses nothing.
<https://docs.rs/tokio/latest/tokio/macro.select.html>

| Cancel safe                                     | Not cancel safe                         |
| ----------------------------------------------- | --------------------------------------- |
| `mpsc::Receiver::recv`, `UnboundedReceiver::recv` | `AsyncReadExt::read_exact`, `read_to_end` |
| `broadcast::Receiver::recv`                     | `AsyncWriteExt::write_all`              |
| `watch::Receiver::changed`                      | `Mutex::lock`, `RwLock::read/write`     |
| `StreamExt::next` (futures and tokio-stream)    | `Semaphore::acquire`                    |
| `TcpListener::accept`, `Interval::tick`         | `Notify::notified`                      |

- To keep one long operation alive across loop turns, create it once, pin it,
  and pass `&mut fut` to `select!` with an `, if !done` guard.
- Document cancel safety on any async fn meant for `select!`, the way tokio
  does with a "Cancel safety" section.

## Coalescing updates

The pod watch shows the pattern for "at most N per second, only on change":

1. The stream task bumps a `watch::Sender<u64>` on every change.
2. The snapshot loop awaits `changed()` (cancel safe), sleeps out the rest of
   the 33 ms window, marks the value seen, builds one snapshot.
3. A slow `Interval` re-checks derived values (ages) and sends only if the
   result differs from the last snapshot.
