//! The tokio side of the app: kubeconfig catalog, clients and watches.
//!
//! This crate never depends on GPUI. It talks to the UI only through the
//! plain data types in `model`.

pub mod age;
pub mod catalog;
pub mod shell_env;

pub use catalog::KubeconfigSources;
