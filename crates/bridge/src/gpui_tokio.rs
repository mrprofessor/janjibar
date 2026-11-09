// Vendored from Zed, `crates/gpui_tokio/src/gpui_tokio.rs`, at commit
// d2a4fb6ded7c470be25d1541281da3882495b65b.
// https://github.com/zed-industries/zed/tree/main/crates/gpui_tokio
//
// Copyright 2022 - 2025 Zed Industries, Inc.
// Licensed under the Apache License, Version 2.0. See LICENSE and NOTICE at
// the repository root.
//
// Changes from upstream:
// - GPUI comes through `gpui_kit` instead of `gpui`.
// - `gpui_util::defer` is replaced by a local `AbortOnDrop` guard.
// - `init` (which builds its own runtime and panics on failure) and the
//   anyhow-based `spawn_result` are removed. The app owns the runtime.

//! Run tokio futures from GPUI code and get the result back as a GPUI task.

use std::future::Future;

use gpui_kit::{App, AppContext, Global, ReadGlobal, Task};

pub use tokio::task::JoinError;

/// Registers a tokio runtime handle with GPUI so [`Tokio::spawn`] can use it.
///
/// Call this once, at startup, before anything calls [`Tokio::spawn`].
pub fn init_from_handle(cx: &mut App, handle: tokio::runtime::Handle) {
    cx.set_global(GlobalTokio { handle });
}

struct GlobalTokio {
    handle: tokio::runtime::Handle,
}

impl Global for GlobalTokio {}

/// Aborts a tokio task when dropped.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Entry point for spawning tokio work from GPUI.
pub struct Tokio {}

impl Tokio {
    /// Spawns the future on tokio's thread pool and returns it as a GPUI task.
    ///
    /// The tokio task is cancelled if the GPUI task is dropped.
    ///
    /// # Panics
    ///
    /// Panics if [`init_from_handle`] was not called first.
    pub fn spawn<C, Fut, R>(cx: &C, f: Fut) -> Task<Result<R, JoinError>>
    where
        C: AppContext,
        Fut: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        cx.read_global(|tokio: &GlobalTokio, cx| {
            let join_handle = tokio.handle.spawn(f);
            let cancel = AbortOnDrop(join_handle.abort_handle());
            cx.background_spawn(async move {
                let result = join_handle.await;
                drop(cancel);
                result
            })
        })
    }

    /// Returns the registered tokio runtime handle.
    ///
    /// # Panics
    ///
    /// Panics if [`init_from_handle`] was not called first.
    pub fn handle(cx: &App) -> tokio::runtime::Handle {
        GlobalTokio::global(cx).handle.clone()
    }
}
