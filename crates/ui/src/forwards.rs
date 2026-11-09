//! The port-forward manager: a small pane listing every forward, with a
//! field to add one, and the sidebar row that counts them.

use std::sync::Arc;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::{
    AnyElement, App, ClickEvent, Context, Entity, FontWeight, Rgba, SharedString, Subscription,
    Window, div, prelude::*, px, svg,
};
use model::{Command, ContextName, ForwardRow, ForwardSpec, ForwardState};

use crate::assets::Icon;
use crate::theme;
use crate::workspace::CommandSink;

/// Height of the pane under the pod table, about six rows.
pub const PANE_HEIGHT: f32 = 236.0;

/// The namespace new forwards use when the input names none and the context
/// sets none, as with kubectl.
const DEFAULT_NAMESPACE: &str = "default";

/// The manager pane's state.
pub struct ForwardsPane {
    rows: Arc<[ForwardRow]>,
    input: Entity<InputState>,
    /// Why the last input was rejected.
    error: Option<SharedString>,
    /// The cluster new forwards go to.
    context: Option<ContextName>,
    /// The namespace new forwards use when the input names none.
    namespace: SharedString,
    send: CommandSink,
    _subscriptions: Vec<Subscription>,
}

impl ForwardsPane {
    /// Builds an empty pane. New forwards go out through `send`.
    pub fn new(send: CommandSink, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("namespace/svc/name local:port"));
        let subscription =
            cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                InputEvent::PressEnter { .. } => this.submit(window, cx),
                InputEvent::Change if this.error.is_some() => {
                    this.error = None;
                    cx.notify();
                }
                _ => {}
            });
        Self {
            rows: Arc::from([]),
            input,
            error: None,
            context: None,
            namespace: DEFAULT_NAMESPACE.into(),
            send,
            _subscriptions: vec![subscription],
        }
    }

    /// Takes a new snapshot from the engine.
    pub fn set_rows(&mut self, rows: Arc<[ForwardRow]>, cx: &mut Context<Self>) {
        self.rows = rows;
        cx.notify();
    }

    /// Sets the cluster and default namespace for new forwards.
    pub fn set_context(
        &mut self,
        context: Option<ContextName>,
        namespace: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        self.context = context;
        self.namespace = namespace.unwrap_or(DEFAULT_NAMESPACE).to_owned().into();
        cx.notify();
    }

    /// Forwards accepting connections, across all clusters.
    pub fn running(&self) -> usize {
        self.rows.iter().filter(|r| r.is_running()).count()
    }

    /// Every forward the engine knows, running or not.
    pub fn total(&self) -> usize {
        self.rows.len()
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value();
        if text.trim().is_empty() {
            return;
        }
        let Some(context) = self.context.clone() else {
            self.error = Some("Select a cluster first.".into());
            cx.notify();
            return;
        };
        match ForwardSpec::parse(&text, &context, &self.namespace) {
            Ok(spec) => {
                (self.send)(Command::StartForward(spec));
                self.error = None;
                self.input.update(cx, |input, cx| input.clean(window, cx));
            }
            Err(err) => self.error = Some(capitalize(&err.to_string()).into()),
        }
        cx.notify();
    }

    fn stop(&mut self, row: &ForwardRow) {
        (self.send)(Command::StopForward(row.id));
    }
}

impl Render for ForwardsPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows: Vec<AnyElement> = Vec::with_capacity(self.rows.len());
        for row in self.rows.iter() {
            rows.push(Self::render_row(row, cx).into_any_element());
        }
        let body: AnyElement = if rows.is_empty() {
            div()
                .px(px(12.0))
                .py(px(10.0))
                .text_color(theme::DIM)
                .child("No port forwards yet. Add one below, like gateway/svc/gateway 8080:80.")
                .into_any_element()
        } else {
            div()
                .id("forward-rows")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px(px(6.0))
                .py(px(2.0))
                .children(rows)
                .into_any_element()
        };
        let summary = summary(self.running(), self.total());
        div()
            .debug_selector(|| "forwards-pane".to_owned())
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::PANE)
            .border_1()
            .border_color(theme::BORDER)
            .rounded(px(theme::PANE_RADIUS))
            .overflow_hidden()
            .text_size(px(theme::TEXT_BASE))
            .child(
                div()
                    .h(px(theme::PANE_HEADER_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .pl(px(11.0))
                    .pr(px(10.0))
                    .border_b_1()
                    .border_color(theme::BORDER)
                    .text_size(px(12.0))
                    .text_color(theme::MUTED)
                    .child(icon(Icon::Plug, 13.0, theme::MUTED))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::TEXT)
                            .child("Port forwards"),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .font_family(theme::MONO_FONT)
                            .text_color(theme::DIM)
                            .child(summary),
                    ),
            )
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .child(self.render_input())
    }
}

impl ForwardsPane {
    fn render_row(row: &ForwardRow, cx: &mut Context<Self>) -> impl IntoElement {
        let stop_row = row.clone();
        let id = row.id;
        let (detail, detail_color) = detail(row);
        div()
            .debug_selector(move || format!("forward-{id}"))
            .h(px(theme::ROW_HEIGHT))
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(8.0))
            .rounded(px(theme::ROW_RADIUS))
            .hover(|s| s.bg(theme::HOVER))
            .child(dot(state_color(&row.state), 7.0))
            .child(
                div()
                    .w(px(132.0))
                    .flex_none()
                    .font_family(theme::MONO_FONT)
                    .text_size(px(theme::TEXT_MONO))
                    .text_color(theme::TEXT)
                    .child(local_label(row)),
            )
            .child(
                div()
                    .flex_none()
                    .font_family(theme::MONO_FONT)
                    .text_size(px(theme::TEXT_MONO))
                    .text_color(theme::TERM_TEXT)
                    .child(target_label(row)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(12.0))
                    .text_color(detail_color)
                    .child(detail),
            )
            .child(
                div()
                    .flex_none()
                    .font_family(theme::MONO_FONT)
                    .text_size(px(theme::TEXT_LABEL))
                    .text_color(theme::DIM)
                    .child(connections(row)),
            )
            .child(
                div()
                    .id(SharedString::from(format!("forward-stop-{id}")))
                    .debug_selector(move || format!("forward-stop-{id}"))
                    .flex_none()
                    .size(px(22.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::HOVER))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, _| this.stop(&stop_row)))
                    .child(icon(Icon::Unplug, 13.0, theme::MUTED)),
            )
    }

    fn render_input(&self) -> impl IntoElement {
        let hint: SharedString = match (&self.error, &self.context) {
            (Some(error), _) => error.clone(),
            (None, Some(context)) => format!(
                "Enter to forward in {context}, namespace {} by default",
                self.namespace
            )
            .into(),
            (None, None) => "Select a cluster to add forwards".into(),
        };
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .px(px(10.0))
            .pt(px(6.0))
            .pb(px(8.0))
            .border_t_1()
            .border_color(theme::BORDER)
            .child(
                div()
                    .debug_selector(|| "forward-input".to_owned())
                    .font_family(theme::MONO_FONT)
                    .text_size(px(theme::TEXT_MONO))
                    .child(Input::new(&self.input)),
            )
            .child(
                div()
                    .text_size(px(theme::TEXT_LABEL))
                    .text_color(if self.error.is_some() {
                        theme::RED
                    } else {
                        theme::DIM
                    })
                    .child(hint),
            )
    }
}

/// The sidebar row: an icon, a label and how many forwards run.
pub fn sidebar_row(
    running: usize,
    total: usize,
    open: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id("sidebar-forwards")
        .debug_selector(|| "sidebar-forwards".to_owned())
        .h(px(28.0))
        .flex()
        .items_center()
        .gap(px(9.0))
        .px(px(8.0))
        .rounded(px(theme::ROW_RADIUS))
        .cursor_pointer()
        .text_color(if open { theme::TEXT } else { theme::MUTED })
        .when(open, |el| el.bg(theme::SELECTED))
        .when(!open, |el| el.hover(|s| s.bg(theme::HOVER)))
        .on_click(on_click)
        .child(icon(
            Icon::Plug,
            14.0,
            if running > 0 {
                theme::GREEN
            } else {
                theme::DIM
            },
        ))
        .child(div().flex_1().child("Port forwards"))
        .when(total > 0, |el| {
            el.child(
                div()
                    .debug_selector(|| "sidebar-forwards-count".to_owned())
                    .font_family(theme::MONO_FONT)
                    .text_size(px(theme::TEXT_LABEL))
                    .text_color(theme::DIM)
                    .child(running.to_string()),
            )
        })
}

/// `127.0.0.1:8080`, or the asked-for port before it listens.
fn local_label(row: &ForwardRow) -> String {
    match (row.local_addr, row.spec.local_port) {
        (Some(addr), _) => addr.to_string(),
        (None, 0) => "127.0.0.1:auto".to_owned(),
        (None, port) => format!("127.0.0.1:{port}"),
    }
}

/// `gateway/svc/gateway:80`.
fn target_label(row: &ForwardRow) -> String {
    format!(
        "{}/{}:{}",
        row.spec.namespace, row.spec.target, row.spec.port
    )
}

/// The line after the target: what is wrong, or where traffic went.
fn detail(row: &ForwardRow) -> (String, Rgba) {
    match &row.state {
        ForwardState::Waiting => (
            format!("Starts when {} connects", row.spec.context),
            theme::DIM,
        ),
        ForwardState::Starting => ("Starting".to_owned(), theme::AMBER),
        ForwardState::Failed(message) => (capitalize(message), theme::RED),
        ForwardState::Listening => match (&row.last_error, &row.last_pod) {
            (Some(error), _) => (capitalize(error), theme::AMBER),
            (None, Some(pod)) => (format!("Last to {pod}"), theme::DIM),
            (None, None) => (format!("{}", row.spec.context), theme::DIM),
        },
    }
}

/// `2 open, 14 total`, or nothing before the first connection.
fn connections(row: &ForwardRow) -> String {
    if row.total == 0 {
        String::new()
    } else {
        format!("{} open, {} total", row.active, row.total)
    }
}

/// The header count: `2 running`, or `2 of 3 running`.
fn summary(running: usize, total: usize) -> String {
    if total == 0 {
        String::new()
    } else if running == total {
        format!("{running} running")
    } else {
        format!("{running} of {total} running")
    }
}

fn state_color(state: &ForwardState) -> Rgba {
    match state {
        ForwardState::Listening => theme::GREEN,
        ForwardState::Starting => theme::AMBER,
        ForwardState::Waiting => theme::DIM,
        ForwardState::Failed(_) => theme::RED,
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

fn dot(color: Rgba, size: f32) -> impl IntoElement {
    div().flex_none().size(px(size)).rounded_full().bg(color)
}

fn icon(icon: Icon, size: f32, color: Rgba) -> impl IntoElement {
    svg()
        .path(icon.path())
        .flex_none()
        .size(px(size))
        .text_color(color)
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{ForwardId, ForwardTarget};

    fn row(state: ForwardState) -> ForwardRow {
        ForwardRow {
            id: ForwardId::new(1),
            spec: ForwardSpec {
                context: "kind".into(),
                namespace: "gateway".into(),
                target: ForwardTarget::Service("gateway".into()),
                port: 80,
                local_port: 8080,
            },
            local_addr: None,
            state,
            active: 0,
            total: 0,
            last_pod: None,
            last_error: None,
        }
    }

    #[test]
    fn rows_read_like_kubectl() {
        let mut r = row(ForwardState::Listening);
        r.local_addr = Some("127.0.0.1:8080".parse().unwrap());
        assert_eq!(local_label(&r), "127.0.0.1:8080");
        assert_eq!(target_label(&r), "gateway/svc/gateway:80");
        r.local_addr = None;
        r.spec.local_port = 0;
        assert_eq!(local_label(&r), "127.0.0.1:auto");
    }

    #[test]
    fn details_say_what_is_going_on() {
        assert_eq!(
            detail(&row(ForwardState::Waiting)),
            ("Starts when kind connects".to_owned(), theme::DIM)
        );
        assert_eq!(
            detail(&row(ForwardState::Failed(
                "port 8080 is already in use".into()
            ))),
            ("Port 8080 is already in use".to_owned(), theme::RED)
        );
        let mut r = row(ForwardState::Listening);
        r.last_pod = Some("gateway-a".into());
        assert_eq!(detail(&r), ("Last to gateway-a".to_owned(), theme::DIM));
        r.last_error = Some("no ready pod behind service gateway".into());
        assert_eq!(
            detail(&r),
            (
                "No ready pod behind service gateway".to_owned(),
                theme::AMBER
            )
        );
    }

    #[test]
    fn counts_stay_quiet_until_used() {
        let mut r = row(ForwardState::Listening);
        assert_eq!(connections(&r), "");
        r.active = 2;
        r.total = 14;
        assert_eq!(connections(&r), "2 open, 14 total");
        assert_eq!(summary(0, 0), "");
        assert_eq!(summary(2, 2), "2 running");
        assert_eq!(summary(1, 3), "1 of 3 running");
    }
}
