//! The main window: sidebar, tab bar, work area and status bar.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::Size;
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::{
    AnyElement, App, Context, Entity, FontWeight, Rgba, SharedString, Subscription, Task, Window,
    div, prelude::*, px, relative, svg,
};
use model::{
    Catalog, Command, ConnectError, ConnectionState, Containers, ContextInfo, ContextName, Event,
    LogOptions, LogRequest, LogTabId, LogTarget,
};
use tokio::sync::mpsc;

use crate::OpenLogs;
use crate::assets::Icon;
use crate::forwards::{self, ForwardsPane};
use crate::helm::{self, HelmView};
use crate::log_pane::{LogPane, LogPaneEvent};
use crate::pod_table::{self, PodTable};
use crate::theme;

/// Room for the macOS window buttons at the top left of the sidebar.
const TRAFFIC_LIGHTS_WIDTH: f32 = 80.0;

/// Share of the work area's height the log pane takes under the pods.
const LOG_PANE_SHARE: f32 = 0.42;

/// Sends a command to the engine.
pub type CommandSink = Rc<dyn Fn(Command)>;

/// What the work area shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MainView {
    Pods,
    Helm,
}

/// The root view of the main window.
pub struct Workspace {
    app_name: SharedString,
    send: CommandSink,
    /// `None` until the engine has read the kubeconfig files once.
    catalog: Option<Catalog>,
    selected: Option<ContextName>,
    states: HashMap<ContextName, ConnectionState>,
    table: Entity<TableState<PodTable>>,
    /// Whether a snapshot for the selected context has arrived.
    has_rows: bool,
    /// The open log pane, under the pods.
    logs: Option<OpenLogPane>,
    last_log_tab: u64,
    /// The port-forward manager, shown under the pod table when open.
    forwards: Entity<ForwardsPane>,
    show_forwards: bool,
    view: MainView,
    helm: Entity<HelmView>,
    _events: Task<()>,
}

struct OpenLogPane {
    pane: Entity<LogPane>,
    _closed: Subscription,
}

impl Workspace {
    /// Builds the workspace and starts listening to engine events.
    pub fn new(
        app_name: impl Into<SharedString>,
        send: CommandSink,
        mut events: mpsc::UnboundedReceiver<Event>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let table =
            cx.new(|cx| TableState::new(PodTable::default(), window, cx).col_movable(false));
        let forwards = cx.new(|cx| ForwardsPane::new(Rc::clone(&send), window, cx));
        // tokio's receiver is executor-agnostic, so GPUI's foreground
        // executor can await it directly.
        let events_task = cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                if this.update(cx, |this, cx| this.apply(event, cx)).is_err() {
                    break;
                }
            }
        });
        let helm = cx.new(|cx| HelmView::new(Rc::clone(&send), window, cx));
        Self {
            app_name: app_name.into(),
            send,
            catalog: None,
            selected: None,
            states: HashMap::new(),
            table,
            has_rows: false,
            logs: None,
            last_log_tab: 0,
            forwards,
            show_forwards: false,
            view: MainView::Pods,
            helm,
            _events: events_task,
        }
    }

    /// Selects a context and asks the engine to connect to it.
    pub fn select(&mut self, context: ContextName, cx: &mut Context<Self>) {
        if self.selected.as_ref() == Some(&context) {
            return;
        }
        self.selected = Some(context.clone());
        self.has_rows = false;
        self.close_logs(cx);
        let namespace = self
            .catalog
            .as_ref()
            .and_then(|c| c.get(&context))
            .and_then(|c| c.namespace.clone());
        self.forwards.update(cx, |pane, cx| {
            pane.set_context(Some(context.clone()), namespace.as_deref(), cx);
        });
        let helm_context = Some(context.clone());
        self.helm
            .update(cx, |helm, cx| helm.set_context(helm_context, cx));
        pod_table::set_rows(&self.table, Vec::new().into(), cx);
        (self.send)(Command::SelectContext(context));
        cx.notify();
    }

    /// Asks the engine to read the kubeconfig files again.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        (self.send)(Command::ReloadCatalog);
        cx.notify();
    }

    /// Opens a log pane for the pod selected in the table, replacing any
    /// open one.
    pub fn open_logs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((namespace, name)) = pod_table::selected_pod(&self.table, cx) else {
            return;
        };
        self.open_pod_logs(namespace, name, window, cx);
    }

    /// Opens a log pane for a pod in the selected context, replacing any
    /// open one.
    pub fn open_pod_logs(
        &mut self,
        namespace: impl Into<Arc<str>>,
        name: impl Into<Arc<str>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(context) = self.selected.clone() else {
            return;
        };
        let target = LogTarget::Pod {
            namespace: namespace.into(),
            name: name.into(),
        };
        if self
            .logs
            .as_ref()
            .is_some_and(|open| open.pane.read(cx).target() == &target)
        {
            return;
        }
        self.close_logs(cx);
        self.last_log_tab += 1;
        let tab = LogTabId::new(self.last_log_tab);
        (self.send)(Command::OpenLogs(LogRequest {
            tab,
            context,
            target: target.clone(),
            containers: Containers::All,
            options: LogOptions::default(),
        }));
        let pane = cx.new(|cx| LogPane::new(tab, target, window, cx));
        let closed = cx.subscribe(&pane, |this, _, event: &LogPaneEvent, cx| match event {
            LogPaneEvent::Close => this.close_logs(cx),
        });
        self.logs = Some(OpenLogPane {
            pane,
            _closed: closed,
        });
        cx.notify();
    }

    /// Closes the log pane and stops its streams.
    pub fn close_logs(&mut self, cx: &mut Context<Self>) {
        if let Some(open) = self.logs.take() {
            (self.send)(Command::CloseLogs(open.pane.read(cx).tab()));
            cx.notify();
        }
    }

    /// The open log pane, if it shows `tab`.
    fn log_pane(&self, tab: LogTabId, cx: &App) -> Option<Entity<LogPane>> {
        let open = self.logs.as_ref()?;
        (open.pane.read(cx).tab() == tab).then(|| open.pane.clone())
    }

    fn apply(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            // Log events only redraw the log pane.
            Event::Logs(chunk) => {
                if let Some(pane) = self.log_pane(chunk.tab, cx) {
                    pane.update(cx, |pane, cx| pane.push(chunk.lines, cx));
                }
                return;
            }
            Event::LogState { tab, state } => {
                if let Some(pane) = self.log_pane(tab, cx) {
                    pane.update(cx, |pane, cx| pane.set_state(state, cx));
                }
                return;
            }
            Event::Catalog(catalog) => self.catalog = Some(catalog),
            Event::Connection { context, state } => {
                self.states.insert(context, state);
            }
            Event::Pods(snapshot) => {
                if self.selected.as_ref() != Some(&snapshot.context) {
                    return;
                }
                self.has_rows = true;
                pod_table::set_rows(&self.table, snapshot.rows, cx);
            }
            // The kinds sidebar and generic tables arrive with the watch hub.
            Event::Kinds(_) | Event::Table(_) => return,
            Event::Forwards(rows) => {
                self.forwards.update(cx, |pane, cx| pane.set_rows(rows, cx));
            }
            Event::Helm(event) => self.helm.update(cx, |helm, cx| helm.apply(event, cx)),
        }
        cx.notify();
    }

    /// Switches the work area. The Helm watch runs only while it shows.
    fn show(&mut self, view: MainView, cx: &mut Context<Self>) {
        if self.view == view {
            return;
        }
        self.view = view;
        self.helm.update(cx, |helm, cx| match view {
            MainView::Helm => helm.open(cx),
            MainView::Pods => helm.close(cx),
        });
        cx.notify();
    }

    fn selected_state(&self) -> Option<&ConnectionState> {
        self.states.get(self.selected.as_ref()?)
    }

    fn pod_count(&self, cx: &App) -> usize {
        self.table.read(cx).delegate().rows().len()
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .bg(theme::CANVAS)
            .text_color(theme::TEXT)
            .font_family(theme::UI_FONT)
            .text_size(px(theme::TEXT_BASE))
            .child(self.render_sidebar(cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(match self.view {
                        MainView::Pods => tab_bar(Icon::Box.path(), "Pods"),
                        MainView::Helm => tab_bar(helm::HELM_ICON.path(), "Helm releases"),
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .p(px(theme::PANE_GAP))
                            .flex()
                            .flex_col()
                            .gap(px(theme::PANE_GAP))
                            .child(match self.view {
                                MainView::Pods => div()
                                    .flex_1()
                                    .min_h_0()
                                    .key_context("PodTable")
                                    .on_action(cx.listener(|this, _: &OpenLogs, window, cx| {
                                        this.open_logs(window, cx);
                                    }))
                                    .child(self.render_pane(cx))
                                    .into_any_element(),
                                MainView::Helm => div()
                                    .flex_1()
                                    .min_h_0()
                                    .child(self.helm.clone())
                                    .into_any_element(),
                            })
                            // The log pane belongs to the pod list.
                            .children(
                                self.logs
                                    .as_ref()
                                    .filter(|_| self.view == MainView::Pods)
                                    .map(|open| {
                                        div()
                                            .h(relative(LOG_PANE_SHARE))
                                            .flex_none()
                                            .child(open.pane.clone())
                                    }),
                            )
                            // Port-forwards span clusters, so they show in every view.
                            .when(self.show_forwards, |el| {
                                el.child(
                                    div()
                                        .h(px(forwards::PANE_HEIGHT))
                                        .flex_none()
                                        .child(self.forwards.clone()),
                                )
                            }),
                    )
                    .child(self.render_status_bar(cx)),
            )
    }
}

// Sidebar.
impl Workspace {
    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(theme::SIDEBAR_WIDTH))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::CHROME)
            .border_r_1()
            .border_color(theme::BORDER)
            .child(self.render_brand())
            .child(section_label("Clusters"))
            .child(self.render_clusters(cx))
            .child(self.render_kinds(cx))
            .child(self.render_forwards_row(cx))
            .child(div().flex_1())
            .child(self.render_sidebar_footer())
    }

    fn render_brand(&self) -> impl IntoElement {
        // The traffic lights sit at the left of this row; the name follows them.
        div()
            .h(px(46.0))
            .flex_none()
            .flex()
            .items_center()
            .pl(px(TRAFFIC_LIGHTS_WIDTH))
            .pr(px(14.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .font_weight(FontWeight::MEDIUM)
                    .child(
                        div()
                            .size(px(18.0))
                            .rounded(px(5.0))
                            .bg(theme::TEXT)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon(Icon::Box, 11.0, theme::CHROME)),
                    )
                    .child(self.app_name.clone()),
            )
    }

    fn render_clusters(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(catalog) = &self.catalog else {
            return sidebar_note("Reading kubeconfig").into_any_element();
        };
        if catalog.is_empty() {
            return sidebar_note("No contexts found").into_any_element();
        }
        let mut rows = Vec::with_capacity(catalog.contexts.len());
        for info in &catalog.contexts {
            rows.push(
                self.render_cluster_row(info, catalog, cx)
                    .into_any_element(),
            );
        }
        div()
            .flex()
            .flex_col()
            .px(px(8.0))
            .children(rows)
            .into_any_element()
    }

    fn render_cluster_row(
        &self,
        info: &ContextInfo,
        catalog: &Catalog,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let name = info.name.clone();
        let selected = self.selected.as_ref() == Some(&info.name);
        let is_current = catalog.current.as_ref() == Some(&info.name);
        let right: AnyElement = match self.states.get(&info.name) {
            Some(state) => dot(state_color(state), 7.0).into_any_element(),
            None if is_current => div()
                .text_size(px(theme::TEXT_LABEL))
                .text_color(theme::DIM)
                .child("current")
                .into_any_element(),
            None => div().into_any_element(),
        };
        div()
            .id(SharedString::from(format!("cluster-{}", info.name)))
            .debug_selector(|| format!("cluster-{}", info.name))
            .h(px(theme::ROW_HEIGHT))
            .flex()
            .items_center()
            .gap(px(9.0))
            .px(px(8.0))
            .rounded(px(theme::ROW_RADIUS))
            .text_color(if selected {
                theme::TEXT
            } else {
                theme::TERM_TEXT
            })
            .when(selected, |el| el.bg(theme::SELECTED))
            .when(!selected, |el| el.hover(|s| s.bg(theme::HOVER)))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.select(name.clone(), cx)))
            .child(letter_tile(info.name.as_str()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(shared(&info.name)),
            )
            .child(div().flex_none().child(right))
    }

    fn render_kinds(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title: SharedString = self
            .selected
            .as_ref()
            .map_or_else(|| "Resources".into(), shared);
        let connected = self.selected.is_some();
        let pods = connected.then(|| self.pod_count(cx));
        let kinds = [
            (Icon::LayoutDashboard, "Overview"),
            (Icon::Server, "Nodes"),
            (Icon::Box, "Pods"),
            (Icon::Layers, "Deployments"),
            (Icon::Database, "StatefulSets"),
            (Icon::Briefcase, "Jobs and CronJobs"),
            (Icon::Network, "Services"),
            (Icon::Globe, "Ingresses"),
            (Icon::FileText, "ConfigMaps"),
            (Icon::KeyRound, "Secrets"),
            (Icon::Bell, "Events"),
        ];
        div()
            .mt(px(10.0))
            .flex()
            .flex_col()
            .child(section_label(title))
            .child(
                div()
                    .px(px(8.0))
                    .flex()
                    .flex_col()
                    .children(kinds.into_iter().map(|(kind_icon, label)| {
                        let live = label == "Pods";
                        let active = live && connected && self.view == MainView::Pods;
                        let count = if live && self.has_rows {
                            pods.map(|n| n.to_string())
                        } else {
                            None
                        };
                        div()
                            .id(label)
                            .debug_selector(move || format!("sidebar-{label}"))
                            .h(px(28.0))
                            .flex()
                            .items_center()
                            .gap(px(9.0))
                            .px(px(8.0))
                            .rounded(px(theme::ROW_RADIUS))
                            .when(live && connected && !active, |el| {
                                el.cursor_pointer().hover(|s| s.bg(theme::HOVER)).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.show(MainView::Pods, cx);
                                    }),
                                )
                            })
                            .text_color(if active {
                                theme::TEXT
                            } else if live {
                                theme::MUTED
                            } else {
                                theme::DIM
                            })
                            .when(active, |el| el.bg(theme::SELECTED))
                            .child(icon(
                                kind_icon,
                                14.0,
                                if active { theme::TEXT } else { theme::DIM },
                            ))
                            .child(div().flex_1().child(label))
                            .children(count.map(|n| {
                                div()
                                    .font_family(theme::MONO_FONT)
                                    .text_size(px(theme::TEXT_LABEL))
                                    .text_color(theme::DIM)
                                    .child(n)
                            }))
                    }))
                    .child(
                        helm::sidebar_entry(
                            self.view == MainView::Helm,
                            connected,
                            self.helm.read(cx).release_count(),
                        )
                        .when(connected, |el| {
                            el.on_click(cx.listener(|this, _, _, cx| {
                                this.show(MainView::Helm, cx);
                            }))
                        }),
                    ),
            )
    }

    fn render_forwards_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = self.forwards.read(cx);
        div().mt(px(10.0)).px(px(8.0)).child(forwards::sidebar_row(
            pane.running(),
            pane.total(),
            self.show_forwards,
            cx.listener(|this, _, _, cx| {
                this.show_forwards = !this.show_forwards;
                cx.notify();
            }),
        ))
    }

    fn render_sidebar_footer(&self) -> impl IntoElement {
        let (text, color) = match &self.catalog {
            None => (SharedString::from(""), theme::DIM),
            Some(c) if !c.problems.is_empty() => (
                format!(
                    "{} kubeconfig file{} could not be read",
                    c.problems.len(),
                    plural(c.problems.len())
                )
                .into(),
                theme::AMBER,
            ),
            Some(c) => {
                let found = c.sources.len();
                (
                    format!(
                        "{} contexts from {found} file{}",
                        c.contexts.len(),
                        plural(found)
                    )
                    .into(),
                    theme::DIM,
                )
            }
        };
        div()
            .h(px(40.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(16.0))
            .border_t_1()
            .border_color(theme::BORDER)
            .text_size(px(12.0))
            .text_color(color)
            .child(icon(Icon::FolderOpen, 13.0, theme::DIM))
            .child(
                div()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(text),
            )
    }
}

// Tabs, pane and status bar.
impl Workspace {
    fn render_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.pod_count(cx);
        let pod_selected = pod_table::selected_pod(&self.table, cx).is_some();
        let scope: SharedString = if self.selected.is_some() {
            "all namespaces".into()
        } else {
            "".into()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::PANE)
            .border_1()
            .border_color(theme::BORDER)
            .rounded(px(theme::PANE_RADIUS))
            .overflow_hidden()
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
                    .child(icon(Icon::Box, 13.0, theme::MUTED))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::TEXT)
                            .child("Pods"),
                    )
                    .child(scope)
                    .child(div().flex_1())
                    .when(self.has_rows, |el| {
                        el.child(
                            div()
                                .font_family(theme::MONO_FONT)
                                .text_color(theme::DIM)
                                .child(format!("{count} pod{}", plural(count))),
                        )
                    })
                    .when(pod_selected, |el| {
                        el.child(
                            div()
                                .id("open-logs")
                                .debug_selector(|| "open-logs".to_owned())
                                .h(px(22.0))
                                .px(px(7.0))
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .rounded(px(6.0))
                                .cursor_pointer()
                                .text_color(theme::TERM_TEXT)
                                .hover(|s| s.bg(theme::HOVER))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.open_logs(window, cx)),
                                )
                                .child("Logs")
                                .child(
                                    div()
                                        .px(px(4.0))
                                        .rounded(px(4.0))
                                        .border_1()
                                        .border_color(theme::BORDER)
                                        .font_family(theme::MONO_FONT)
                                        .text_size(px(10.5))
                                        .text_color(theme::DIM)
                                        .child("L"),
                                ),
                        )
                    }),
            )
            .child(div().flex_1().min_h_0().child(self.render_pane_body(count)))
    }

    fn render_pane_body(&self, count: usize) -> AnyElement {
        if let Some(empty) = self.empty_state(count) {
            return empty.render();
        }
        div()
            .size_full()
            .px(px(6.0))
            .pt(px(2.0))
            .child(
                DataTable::new(&self.table)
                    .with_size(Size::Small)
                    .header_row_height(px(28.0))
                    .bordered(false)
                    .stripe(false)
                    .accessibility_label("Pods"),
            )
            .into_any_element()
    }

    /// What to show instead of the table, if anything.
    fn empty_state(&self, count: usize) -> Option<Empty> {
        let Some(catalog) = &self.catalog else {
            return Some(Empty::new(Icon::LoaderCircle, "Reading kubeconfig", ""));
        };
        if catalog.is_empty() {
            let looked: Vec<String> = catalog
                .sources
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            let body = std::iter::once(
                "Set KUBECONFIG or create ~/.kube/config, then reload with Cmd-R.".to_owned(),
            )
            .chain(
                catalog
                    .problems
                    .iter()
                    .map(|p| format!("{}: {}", p.path.display(), p.message)),
            )
            .collect::<Vec<_>>()
            .join("\n");
            return Some(
                Empty::new(Icon::FolderOpen, "No kubeconfig found", body)
                    .detail(format!("Looked in: {}", looked.join(", "))),
            );
        }
        let Some(selected) = &self.selected else {
            return Some(Empty::new(
                Icon::Plug,
                "Select a cluster",
                "Pick a context in the sidebar to connect.",
            ));
        };
        let server = catalog
            .get(selected)
            .and_then(|c| c.server.clone())
            .unwrap_or_default();
        match self.selected_state() {
            None | Some(ConnectionState::Connecting) => Some(
                Empty::new(Icon::LoaderCircle, format!("Connecting to {selected}"), "")
                    .detail(server),
            ),
            Some(ConnectionState::Failed(err)) => Some(error_state(err, &server)),
            Some(ConnectionState::Live | ConnectionState::Retrying(_)) if !self.has_rows => {
                Some(Empty::new(Icon::LoaderCircle, "Loading pods", ""))
            }
            Some(ConnectionState::Live | ConnectionState::Retrying(_)) if count == 0 => {
                Some(Empty::new(
                    Icon::Box,
                    "No pods",
                    "This cluster has no pods you can see.",
                ))
            }
            Some(_) => None,
        }
    }

    fn render_status_bar(&self, cx: &App) -> impl IntoElement {
        let mut items: Vec<AnyElement> = Vec::new();
        if let Some(selected) = &self.selected {
            let state = self.selected_state();
            let (label, color) = match state {
                None | Some(ConnectionState::Connecting) => ("connecting", theme::AMBER),
                Some(ConnectionState::Live) => ("live", theme::GREEN),
                Some(ConnectionState::Retrying(_)) => ("reconnecting", theme::AMBER),
                Some(ConnectionState::Failed(_)) => ("not connected", theme::RED),
            };
            items.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .text_color(theme::TERM_TEXT)
                    .child(dot(color, 6.0))
                    .child(shared(selected))
                    .into_any_element(),
            );
            if self.has_rows {
                let count = self.pod_count(cx);
                items.push(
                    div()
                        .child(format!("{count} pod{}", plural(count)))
                        .into_any_element(),
                );
            }
            items.push(div().text_color(color).child(label).into_any_element());
        } else {
            items.push(div().child("No cluster selected").into_any_element());
        }
        div()
            .h(px(theme::STATUS_BAR_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(16.0))
            .px(px(12.0))
            .bg(theme::CHROME)
            .border_t_1()
            .border_color(theme::BORDER)
            .text_size(px(12.0))
            .text_color(theme::MUTED)
            .children(items)
    }
}

/// An empty or error state in the middle of a pane.
struct Empty {
    icon: Icon,
    color: Rgba,
    title: SharedString,
    body: SharedString,
    detail: Option<SharedString>,
}

impl Empty {
    fn new(icon: Icon, title: impl Into<SharedString>, body: impl Into<SharedString>) -> Self {
        Self {
            icon,
            color: theme::DIM,
            title: title.into(),
            body: body.into(),
            detail: None,
        }
    }

    fn error(mut self) -> Self {
        self.color = theme::RED;
        self
    }

    fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        let detail = detail.into();
        if !detail.is_empty() {
            self.detail = Some(detail);
        }
        self
    }

    fn render(self) -> AnyElement {
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

fn error_state(err: &ConnectError, server: &str) -> Empty {
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
            "Not allowed to list pods",
            "Your user cannot list pods across all namespaces.",
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

fn tab_bar(icon_path: &'static str, label: &'static str) -> impl IntoElement {
    div()
        .h(px(theme::TAB_BAR_HEIGHT))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(3.0))
        .px(px(10.0))
        .bg(theme::CHROME)
        .border_b_1()
        .border_color(theme::BORDER)
        .child(
            div()
                .h(px(29.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(7.0))
                .rounded(px(7.0))
                .bg(theme::RAISED)
                .border_1()
                .border_color(theme::BORDER)
                .text_size(px(12.8))
                .text_color(theme::TEXT)
                .child(
                    svg()
                        .path(icon_path)
                        .flex_none()
                        .size(px(13.0))
                        .text_color(theme::MUTED),
                )
                .child(label),
        )
}

fn state_color(state: &ConnectionState) -> Rgba {
    match state {
        ConnectionState::Connecting | ConnectionState::Retrying(_) => theme::AMBER,
        ConnectionState::Live => theme::GREEN,
        ConnectionState::Failed(_) => theme::RED,
    }
}

fn section_label(text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .h(px(30.0))
        .flex_none()
        .flex()
        .items_center()
        .px(px(16.0))
        .text_size(px(theme::TEXT_LABEL))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme::DIM)
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .child(text.into())
}

fn sidebar_note(text: &'static str) -> impl IntoElement {
    div()
        .h(px(theme::ROW_HEIGHT))
        .flex()
        .items_center()
        .px(px(16.0))
        .text_color(theme::DIM)
        .child(text)
}

fn letter_tile(name: &str) -> impl IntoElement {
    let letter = name
        .chars()
        .find(char::is_ascii_alphanumeric)
        .map_or('?', |c| c.to_ascii_uppercase());
    div()
        .size(px(18.0))
        .flex_none()
        .rounded(px(5.0))
        .bg(tile_color(name))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(10.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(gpui_kit::white())
        .child(SharedString::from(letter.to_string()))
}

/// A stable colour per cluster name.
fn tile_color(name: &str) -> Rgba {
    // FNV-1a: tiny, stable across runs and platforms.
    let hash = name.bytes().fold(0x811c_9dc5_u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    });
    let ix = usize::try_from(hash).unwrap_or(0) % theme::TILE_COLORS.len();
    theme::TILE_COLORS[ix]
}

fn shared(name: &ContextName) -> SharedString {
    SharedString::from(Arc::clone(name.as_arc()))
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

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_colour_is_stable() {
        assert_eq!(tile_color("kind-platform"), tile_color("kind-platform"));
    }

    #[test]
    fn plural_suffix() {
        assert_eq!(plural(1), "");
        assert_eq!(plural(0), "s");
        assert_eq!(plural(2), "s");
    }
}
