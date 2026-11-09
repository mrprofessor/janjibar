# Where to read the source

GPUI's online docs lag behind the `gpui-pre` snapshot we pin. Read the code.

```sh
REG=$(echo ~/.cargo/registry/src/index.crates.io-*)
ls $REG | grep -E 'gpui-(pre|kit|component|base)'
```

If a crate is missing, run `nix develop -c cargo fetch` first.

| Question                                   | File                                               |
| ------------------------------------------ | -------------------------------------------------- |
| What does `gpui_kit::*` export?            | `gpui-kit-0.9.0/src/lib.rs`                        |
| `Context<T>` methods (spawn, listener...)  | `gpui-pre-0.3.11/src/app/context.rs`               |
| `App` methods, globals, windows            | `gpui-pre-0.3.11/src/app.rs`                       |
| `Window` (focus, frames, dispatch)         | `gpui-pre-0.3.11/src/window.rs`                    |
| Style methods (`flex_1`, `rounded`, `px`)  | `gpui-pre-0.3.11/src/styled.rs`, `geometry.rs`     |
| `div`, events, hover, ids                  | `gpui-pre-0.3.11/src/elements/div.rs`              |
| `uniform_list`, `list`                     | `gpui-pre-0.3.11/src/elements/{uniform_list,list}.rs` |
| Key bindings and contexts                  | `gpui-pre-0.3.11/src/keymap/`                      |
| Tasks, executors, timers                   | `gpui-pre-0.3.11/src/executor.rs`                  |
| Test contexts                              | `gpui-pre-0.3.11/src/app/test_context.rs`          |
| Fonts                                      | `gpui-pre-0.3.11/src/text_system.rs` (`add_fonts`) |
| Kit theme and colour fields                | `gpui-component-0.9.0/src/theme/{mod,theme_color}.rs` |
| Kit table                                  | `gpui-component-0.9.0/src/table/`                  |
| Kit `h_flex`, `v_flex`, `Sizable`          | `gpui-base-0.9.0/src/styled.rs`                    |
| Kit icons (Lucide)                         | `gpui-kit-assets-0.9.0/` and `gpui-component-0.9.0/src/icon.rs` |
| Kit UI tests                               | `gpui-kit-0.9.0/TESTING.md`, `gpui-kit-0.9.0/src/test.rs` |

Other good references:

- Zed itself (`zed-industries/zed`, `crates/`) uses GPUI at scale. Its APIs
  are newer than our snapshot, so check signatures against the registry.
- Herdr (`herdrdev/herdr-app`, Apache-2.0) is a GPUI app on gpui-kit with the
  look we follow. Theme derivation is in `src/settings.rs`.
- gpui-kit's own examples and tests in the registry (`tests/`).

When something does not compile the way an example says, the example is
stale. Do not work around it with a different GPUI version.
