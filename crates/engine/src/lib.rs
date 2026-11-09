//! The tokio side of the app: kubeconfig catalog, clients and watches.
//!
//! This crate never depends on GPUI. It talks to the UI only through the
//! plain data types in `model`.
