//! GPUI views for the app: the window shell, sidebar, tabs and tables.
//!
//! This crate knows nothing about kube-rs. It renders `model` types and
//! sends `model::Command`s back through a [`CommandSink`].

pub mod assets;
mod empty;
mod fonts;
mod forwards;
mod inspector;
mod log_pane;
mod log_search;
mod resource_table;
mod sidebar;
pub mod theme;
mod widgets;
mod workspace;

use gpui_kit::{App, KeyBinding};

pub use workspace::{CommandSink, Workspace, WorkspaceEvent};

gpui_kit::actions!(app, [Quit, ReloadKubeconfig]);
gpui_kit::actions!(
    logs,
    [
        /// Open a log pane for the pod selected in the table.
        OpenLogs,
        /// Move focus to the log pane's search field.
        FocusLogSearch
    ]
);

/// Registers fonts, the theme and key bindings. Call once, after
/// `gpui_kit::init`.
pub fn init(cx: &mut App) {
    if let Err(err) = fonts::register(cx) {
        tracing::warn!("could not load bundled fonts, using system fonts: {err:#}");
    }
    theme::apply_to_kit(cx);
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-r", ReloadKubeconfig, None),
        KeyBinding::new("l", OpenLogs, Some("ResourceTable")),
        KeyBinding::new("cmd-f", FocusLogSearch, Some("LogPane")),
    ]);
}
