//! Glue between the tokio engine and the GPUI UI.
//!
//! Only plain data crosses here. `tokio::sync::mpsc` receivers can be awaited
//! directly inside `cx.spawn` on GPUI's foreground executor; [`gpui_tokio`]
//! covers one-shot tokio work started from the UI.

pub mod gpui_tokio;

pub use gpui_tokio::Tokio;
