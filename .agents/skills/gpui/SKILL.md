---
name: gpui
description: GPUI and gpui-kit at the versions this repo pins (gpui-kit =0.9.0, gpui-pre =0.3.11). Load before touching the ui, app or bridge crates, writing a view or component, wiring actions, key bindings or focus, building a virtualized list or table, styling with the app's theme tokens, bridging tokio into GPUI, or writing GPUI tests.
---

# GPUI in this repo

GPUI is Zed's UI framework. We use it through gpui-kit, which re-exports a
snapshot of GPUI (`gpui-pre =0.3.11`) plus a component library
(`gpui-component 0.9.0`). Online docs and tutorials are usually stale for
this snapshot. **The crate source in `~/.cargo/registry/src/` is the truth.**
See `references/source-map.md` for where to look.

## Imports and setup

- Import GPUI from the kit: `use gpui_kit::{div, px, App, Context, Window, prelude::*};`.
  `prelude::*` brings the traits (`AppContext`, `Styled`, `ParentElement`,
  `InteractiveElement`, `IntoElement`, `Render`...). Missing-method errors on
  `cx` usually mean the prelude is not imported.
- Components live under `gpui_kit::component::*` (table, button, icon,
  `h_flex`, `v_flex`, `ActiveTheme`, `Sizable`...).
- Startup: `gpui_kit::application().run(|cx| { gpui_kit::init(cx); ... })`.
  Open windows with `gpui_kit::open_window(options, cx, |window, cx| ...)`,
  which wraps the view in the kit's `Root` (needed for popovers, dialogs and
  notifications). The run closure is `'static`: you cannot return values out
  of it, so log errors and `cx.quit()`.
- Do not add `gpui-pre` or `gpui-pre-platform` directly. gpui-kit already
  enables `runtime_shaders` (no Metal toolchain needed) and `font-kit`
  (without it, text lays out with no glyphs). If you ever must depend on them,
  pin `=0.3.11` and keep both features.
- Use the `gpui_kit::actions!` macro, not GPUI's: GPUI's derive path assumes a
  crate named `gpui`.

## Entities and contexts

State lives in entities owned by the `App`. You hold handles.

- `cx.new(|cx| State::new(cx))` returns `Entity<State>`.
- `entity.read(cx)` borrows; `entity.update(cx, |state, cx| ...)` mutates.
  Never update an entity from inside its own update: it panics at runtime.
- `Context<T>` is the `App` plus the current entity. `cx.entity()` and
  `cx.weak_entity()` give handles to self.
- `WeakEntity<T>` does not keep the entity alive. Use it in long-lived
  closures and tasks to avoid cycles. `weak.update(cx, ...)` returns a
  `Result`; an `Err` means the entity is gone, so stop.
- `cx.notify()` marks the entity dirty so its views re-render. Call it after
  every state change that affects rendering, and only then. Idle CPU should be
  zero, so never notify on a timer when nothing changed.
- Events: `impl EventEmitter<MyEvent> for T {}`, then `cx.emit(MyEvent)`.
  Listeners use `cx.subscribe(&entity, |this, emitter, event, cx| ...)`.
  `cx.observe(&entity, ...)` fires on `notify`. Both return a `Subscription`
  that unsubscribes on drop: store it in a field (`_subscriptions: Vec<_>`).

## Render

- Views implement `Render`:
  `fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement`.
- Stateless components implement `RenderOnce` and derive `IntoElement`.
- Rendering must be cheap and pure. No I/O, no sorting of big lists, no
  allocation-heavy work. The engine pre-builds rows; the view only lays them out.
- Element ids (`.id("...")`) are needed for hover, click and scroll state.
  Ids must be stable across frames and unique among siblings.
- `flex_grow()` takes no argument in some versions and an `f32` in this one;
  prefer `flex_1()`. When in doubt, read `styled.rs` in the registry.
- Event handlers: `.on_click(cx.listener(|this, event, window, cx| ...))`.
  `cx.listener` gives `&mut Self` inside the closure without manual updates.
- Conditionals: `.when(cond, |el| el.bg(...))` and `.when_some(opt, ...)`.

## Async: spawn, background_spawn, tasks

- `cx.spawn(async move |this, cx| { ... })` runs on the **main thread**
  (foreground executor). `this` is a `WeakEntity<Self>`, `cx` an `&mut AsyncApp`.
  Use it to await channels and then update state.
- `cx.background_spawn(fut)` runs on GPUI's thread pool. It is **not tokio**.
  Anything that needs a tokio reactor (kube-rs, hyper, `tokio::time`) panics
  or hangs there. Use the engine or `bridge::Tokio::spawn` instead.
- A `Task<T>` cancels its future when dropped. Store it in a field to tie its
  life to the entity, or call `.detach()` to let it run to completion.
  Dropping it by accident (`let _ = cx.spawn(...)`) cancels it immediately.
- `tokio::sync::mpsc` receivers can be awaited inside `cx.spawn` as is. This is
  how engine events reach views:

```rust
let task = cx.spawn(async move |this, cx| {
    while let Some(event) = events.recv().await {
        if this.update(cx, |this, cx| this.apply(event, cx)).is_err() {
            break; // view is gone
        }
    }
});
self._events = Some(task);
```

## The tokio bridge

`crates/bridge` vendors Zed's `gpui_tokio` (Apache-2.0, see `NOTICE`).

- The app builds the tokio runtime and calls
  `bridge::gpui_tokio::init_from_handle(cx, handle)` once at startup.
- `bridge::Tokio::spawn(cx, fut)` runs `fut` on tokio and returns a GPUI
  `Task<Result<R, JoinError>>`. Dropping the GPUI task aborts the tokio task.
- Long-lived engine work does not go through the bridge. The engine owns its
  tasks and talks over channels of `model` types.

## Actions, key bindings, focus

```rust
gpui_kit::actions!(pods, [SelectNext, SelectPrev, OpenLogs]);

cx.bind_keys([
    KeyBinding::new("j", SelectNext, Some("PodTable")),
    KeyBinding::new("k", SelectPrev, Some("PodTable")),
]);

div()
    .key_context("PodTable")
    .track_focus(&self.focus_handle)
    .on_action(cx.listener(|this, _: &SelectNext, _window, cx| this.select_next(cx)))
```

- Bindings only fire when an element with a matching `key_context` is on the
  focus path. Global actions use `cx.on_action(|_: &Quit, cx| cx.quit())`.
- `cx.focus_handle()` creates a handle; keep it in the view. Implement
  `Focusable` for views that take focus. Focus with
  `window.focus(&handle, cx)` or `handle.focus(window, cx)`.
- Keys are spelled like `"cmd-k"`, `"cmd-shift-p"`, `"escape"`, `"enter"`.

## Lists and tables

- Never render every row. For uniform rows use
  `uniform_list(id, count, cx.processor(|this, range, window, cx| ...))`;
  it calls you only for the visible range. Add `.track_scroll(&handle)` with a
  `UniformListScrollHandle` to scroll programmatically.
- For variable heights use `list()` with a `ListState`.
- Resource tables use the kit's `DataTable` with a `TableDelegate`
  (`gpui_kit::component::table`). It is virtualized and handles selection,
  keyboard movement, column resize and sort. Size `Small` gives 30 px rows.
  See `references/gpui-kit.md` for the delegate shape.
- When a snapshot arrives, replace the delegate's rows and call
  `table.refresh(cx)` plus `cx.notify()`. Keep selection by key (namespace and
  name), not by index.

## Theme and look

The look is Herdr's graphite theme. Tokens live in `crates/ui/src/theme.rs`;
use them, never raw hex in views.

| Token      | Value       | Use                                        |
| ---------- | ----------- | ------------------------------------------ |
| `chrome`   | `#101114`   | sidebar, tab bar, status bar               |
| `canvas`   | `#131518`   | work area behind panes                     |
| `pane`     | `#17191d`   | pane cards                                 |
| `popover`  | `#25272b`   | menus, palette                             |
| `raised`   | `#2c2e31`   | active tab                                 |
| `hover`    | `#ffffff0e` | hover overlay                              |
| `selected` | `#ffffff17` | selected row overlay                       |
| `border`   | `#ffffff18` | 1 px borders                               |
| `input`    | `#ffffff24` | input borders                              |
| `text`     | `#eeeeef`   | primary text                               |
| `term_text`| `#d2d4d9`   | table cells, terminal text                 |
| `muted`    | `#98999b`   | secondary text                             |
| `dim`      | `#696a6d`   | labels, headers, hints                     |
| `accent`   | `#96b2d7`   | focused pane border, links                 |
| `green`    | `#a6bd91`   | healthy state                              |
| `amber`    | `#e5c07b`   | pending, warnings                          |
| `red`      | `#f0705f`   | failing, production                        |
| `blue`     | `#76a8ff`   | info                                       |

Rules:

- Fonts: Geist for UI, JetBrains Mono for names and numbers. Both are bundled
  in `crates/ui/assets/fonts` (OFL, licenses alongside) and registered with
  `cx.text_system().add_fonts(...)` at startup.
- 13 px base text. Section labels 11.5 px, medium weight, sentence case,
  `dim` colour. **No uppercase labels.**
- Rows are 30 px with an 8 px rounded highlight. Panes are cards with a 10 px
  radius, 1 px `border`, a 30 px header, 8 px apart on the canvas.
  **No shadows on panes.**
- Sidebar about 252 px on `chrome`. Tab bar 42 px. Status bar 28 px.
- Colour only for state: a dot or a tinted chip. Everything else is greys.
- The kit's `Theme` global is overridden from the tokens at startup so kit
  components (table, scrollbars, buttons) match.

## Testing

- Add `gpui-kit = { workspace = true, features = ["test-support"] }` under
  `[dev-dependencies]` of the crate under test.
- Use `#[gpui_kit::test]` with `cx: &mut TestAppContext`. Import kit types
  explicitly in test modules so GPUI's `test` macro does not shadow `#[test]`.
- `cx.add_window_view(|window, cx| View::new(window, cx))` returns the view and
  a `VisualTestContext`. `cx.simulate_keystrokes("j j")` drives key bindings.
  `cx.run_until_parked()` drains pending tasks;
  `cx.executor().advance_clock(d)` moves fake time.
- Keep logic out of views so most tests are plain `#[test]`s on `model` and
  `engine` code. GPUI tests are for focus, keys and view state.

## Gotchas from the spike

- No Metal toolchain on the dev Mac: `runtime_shaders` is required (the kit
  enables it).
- No glyphs means `font-kit` is off somewhere.
- `gpui_platform::application()` (re-exported as `gpui_kit::application()`)
  is the entry point. Old examples use `App::new()` or `Application::new()`.
- Building needs `DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer`;
  the nix dev shell sets it.
- kube-rs inside `cx.background_spawn` panics: no tokio reactor.

## References

- `references/gpui-kit.md`: kit components we use, `TableDelegate` shape,
  theme override, fonts.
- `references/source-map.md`: where to read the source when docs are stale.
