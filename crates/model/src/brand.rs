//! The app's name and identifiers.
//!
//! The name is temporary. This module is the only place outside the README
//! and the binary name that spells it, so a rename touches one file.

/// Human-readable app name, shown in the window and the sidebar.
pub const APP_NAME: &str = "Janjibar";

/// macOS bundle identifier.
pub const BUNDLE_ID: &str = "io.github.mrprofessor.janjibar";

/// Field manager name for server-side apply.
pub const FIELD_MANAGER: &str = "janjibar";

/// Folder name for the app's own files, under `~/.config`.
pub const CONFIG_DIR: &str = "janjibar";
