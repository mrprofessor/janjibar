# Sources

Fetched on 9 November 2026. Text in this skill is written fresh; where an
idea comes from a specific source, the skill says so.

## Guidelines

- Microsoft Pragmatic Rust Guidelines, version 2026.6. MIT, Copyright (c)
  Microsoft Corporation.
  - Site: <https://microsoft.github.io/rust-guidelines/>
  - Checklist of IDs: <https://microsoft.github.io/rust-guidelines/guidelines/checklist/index.html>
  - Single file for agents: <https://microsoft.github.io/rust-guidelines/agents/all.txt>
  - IDs cited in the skill: M-APP-ERROR, M-ERRORS-CANONICAL-STRUCTS,
    M-FROM-ERROR, M-PANIC-IS-STOP, M-PANIC-ON-BUG, M-LINT-OVERRIDE-EXPECT,
    M-STRONG-TYPES, M-UNSAFE, M-UNSAFE-IMPLIES-UB, M-FFI-TRANSLATES,
    M-BALANCED-MODULES, M-NO-PRELUDE, M-NO-GLOB-REEXPORTS, M-WEASEL-WORDS,
    M-TAUTOLOGICAL-TESTS, M-FIRST-DOC-SENTENCE, M-CANONICAL-DOCS,
    M-MODULE-DOCS, M-DOCUMENTED-MAGIC, M-NO-META-DESIGN-DOCUMENTATION,
    M-LOG-STRUCTURED, M-HOTPATH, M-YIELD-POINTS.
  - Page URL pattern: `https://microsoft.github.io/rust-guidelines/guidelines/<page>/index.html#<ID>`.
- Rust API Guidelines. MIT or Apache-2.0.
  - Checklist: <https://rust-lang.github.io/api-guidelines/checklist.html>
  - IDs cited: C-GOOD-ERR, C-NEWTYPE, C-NEWTYPE-HIDE, C-CUSTOM-TYPE,
    C-COMMON-TRAITS, C-DEBUG, C-CONV, C-GETTER.

## Tooling docs

- Clippy lint list: <https://rust-lang.github.io/rust-clippy/master/index.html>
- Clippy groups: <https://doc.rust-lang.org/clippy/lints.html>
- Clippy configuration: <https://doc.rust-lang.org/clippy/lint_configuration.html>
- Cargo `[lints]`: <https://doc.rust-lang.org/cargo/reference/manifest.html#the-lints-section>
- tokio task docs: <https://docs.rs/tokio/latest/tokio/task/index.html>
- tokio `select!`: <https://docs.rs/tokio/latest/tokio/macro.select.html>
- tokio shared state: <https://tokio.rs/tokio/tutorial/shared-state>
- tokio graceful shutdown: <https://tokio.rs/tokio/topics/shutdown>
- Alice Ryhl, "Async: What is blocking?": <https://ryhl.io/blog/async-what-is-blocking/>
- thiserror vs anyhow: <https://github.com/dtolnay/thiserror#comparison-to-anyhow>
- std `Error` trait: <https://doc.rust-lang.org/std/error/trait.Error.html>

## Agent skills consulted

| Project | License | Used for |
| ------- | ------- | -------- |
| [morganlinton/BeautifulRust](https://github.com/morganlinton/BeautifulRust) | MIT | "before you hand back code" commands, the borrow-checker ladder, app vs library framing |
| [apollographql/rust-best-practices](https://github.com/apollographql/rust-best-practices) | Apache-2.0 | review rejection triggers, "performance changes need numbers" |
| [leonardomso/rust-skills](https://github.com/leonardomso/rust-skills) | MIT | async rule list (no lock across await, cancel safety, bounded channels), log each error once |
| [zed-industries/zed](https://github.com/zed-industries/zed) `.rules` | GPL-3.0 (ideas only, no text) | never discard errors silently, clone-before-spawn, no `mod.rs` |
| [actionbook/rust-skills](https://github.com/actionbook/rust-skills) | none (ideas only, no text) | "list only non-obvious rules" |

Ideas were adapted and rewritten; no text was copied.
