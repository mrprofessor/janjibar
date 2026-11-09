//! Empty and error states shown in the middle of a pane.

use gpui_kit::{AnyElement, FontWeight, Rgba, SharedString, div, prelude::*, px};
use model::ConnectError;

use crate::assets::Icon;
use crate::theme;
use crate::widgets::icon;

/// An empty or error state in the middle of a pane.
pub(crate) struct Empty {
    icon: Icon,
    color: Rgba,
    title: SharedString,
    body: SharedString,
    detail: Option<SharedString>,
}

impl Empty {
    pub(crate) fn new(
        icon: Icon,
        title: impl Into<SharedString>,
        body: impl Into<SharedString>,
    ) -> Self {
        Self {
            icon,
            color: theme::DIM,
            title: title.into(),
            body: body.into(),
            detail: None,
        }
    }

    pub(crate) fn error(mut self) -> Self {
        self.color = theme::RED;
        self
    }

    pub(crate) fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        let detail = detail.into();
        if !detail.is_empty() {
            self.detail = Some(detail);
        }
        self
    }

    pub(crate) fn render(self) -> AnyElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(24.0))
            .child(
                div()
                    .max_w(px(520.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(8.0))
                    .child(icon(self.icon, 22.0, self.color))
                    .child(
                        div()
                            .mt(px(4.0))
                            .text_size(px(14.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::TEXT)
                            .child(self.title),
                    )
                    .when(!self.body.is_empty(), |el| {
                        el.child(
                            div()
                                .text_center()
                                .text_color(theme::MUTED)
                                .child(self.body),
                        )
                    })
                    .children(self.detail.map(|d| {
                        div()
                            .mt(px(4.0))
                            .px(px(10.0))
                            .py(px(6.0))
                            .rounded(px(6.0))
                            .bg(theme::HOVER)
                            .font_family(theme::MONO_FONT)
                            .text_size(px(11.5))
                            .text_color(theme::MUTED)
                            .child(d)
                    })),
            )
            .into_any_element()
    }
}

/// The state for a connection or watch error. `what` names the objects,
/// like `pods`, for permission errors.
pub(crate) fn error_state(err: &ConnectError, server: &str, what: &str) -> Empty {
    match err {
        ConnectError::AuthPluginMissing { command, path } => Empty::new(
            Icon::TriangleAlert,
            "Auth plugin not found",
            format!(
                "This context logs in with `{command}`, which is not on the PATH the app sees. \
                 Install it, or make sure your login shell adds it to PATH."
            ),
        )
        .error()
        .detail(format!("PATH searched: {path}")),
        ConnectError::Unreachable(msg) => Empty::new(
            Icon::Unplug,
            "Cluster unreachable",
            "The API server did not answer. Check the network, VPN or that the cluster is running.",
        )
        .error()
        .detail(format!("{server}\n{msg}")),
        ConnectError::Auth(msg) => Empty::new(
            Icon::KeyRound,
            "Authentication failed",
            "The cluster rejected the credentials.",
        )
        .error()
        .detail(msg.clone()),
        ConnectError::Forbidden(msg) => Empty::new(
            Icon::KeyRound,
            format!("Not allowed to list {what}"),
            format!(
                "Your user cannot list {what} here. Ask for read access, or pick another kind."
            ),
        )
        .error()
        .detail(msg.clone()),
        ConnectError::Config(msg) => Empty::new(
            Icon::CircleAlert,
            "Kubeconfig problem",
            "This context could not be loaded from your kubeconfig.",
        )
        .error()
        .detail(msg.clone()),
        ConnectError::UnknownContext => Empty::new(
            Icon::CircleAlert,
            "Context not found",
            "This context is no longer in your kubeconfig. Reload with Cmd-R.",
        )
        .error(),
        ConnectError::Other(msg) => Empty::new(Icon::CircleAlert, "Could not connect", "")
            .error()
            .detail(msg.clone()),
    }
}
