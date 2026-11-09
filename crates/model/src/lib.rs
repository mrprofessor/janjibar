//! Plain data that crosses the bridge between the engine and the UI.
//!
//! Nothing here knows about GPUI or kube-rs. Types are cheap to clone and
//! safe to send between threads.

pub mod brand;
