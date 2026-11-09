//! The main window: sidebar, tab bar, work area and status bar.
//!
//! The workspace owns the UI state: which context and kind are selected,
//! the table subscription, and the object the inspector follows. Rendering
//! of the sidebar and the inspector lives in `sidebar.rs` and `inspector.rs`.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::Size;
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::{
    AnyElement, App, Context, Entity, EventEmitter, FontWeight, SharedString, Subscription, Task,
    Window, div, prelude::*, px, relative,
};
use model::resource::{Action, Details, InspectTarget, Kind, ObjectKey, OwnerLink};
use model::{
    Catalog, Command, ConnectionState, Containers, ContextName, Event, LogOptions, LogRequest,
    LogTabId, LogTarget, SubscriptionId,
};
use tokio::sync::mpsc;

use crate::OpenLogs;
use crate::assets::{Icon, kind_icon};
use crate::empty::{Empty, error_state};
use crate::forwards::{self, ForwardsPane};
use crate::log_pane::{LogPane, LogPaneEvent};
use crate::resource_table::{self, ResourceTable};
use crate::sidebar::state_color;
use crate::theme;
use crate::widgets::{dot, icon};

/// Share of the work area's height the log pane takes under the table.
const LOG_PANE_SHARE: f32 = 0.42;

/// Sends a command to the engine.
pub type CommandSink = Rc<dyn Fn(Command)>;

/// Events the workspace emits for the app to act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceEvent {
    /// An inspector action button was pressed.
    Action {
        /// Which action.
        action: Action,
        /// On which object.
        target: InspectTarget,
    },
}

struct OpenLogPane {
    pane: Entity<LogPane>,
    _closed: Subscription,
}

/// The current table subscription.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TableSub {
    id: SubscriptionId,
    context: ContextName,
    kind: Kind,
}

/// The root view of the main window.
pub struct Workspace {
    pub(crate) app_name: SharedString,
    send: CommandSink,
    /// `None` until the engine has read the kubeconfig files once.
    pub(crate) catalog: Option<Catalog>,
    pub(crate) selected: Option<ContextName>,
    pub(crate) states: HashMap<ContextName, ConnectionState>,
    /// The kind the table shows.
    pub(crate) kind: Kind,
    sub: Option<TableSub>,
    next_sub: u64,
    table: Entity<TableState<ResourceTable>>,
    /// Whether a snapshot for the current subscription has arrived.
    has_rows: bool,
    /// Row counts per context and kind, from the latest snapshots.
    pub(crate) counts: HashMap<(ContextName, Kind), usize>,
    /// Watch states per context and kind, from the latest snapshots.
    pub(crate) statuses: HashMap<(ContextName, Kind), ConnectionState>,
    /// What the inspector follows.
    pub(crate) inspected: Option<InspectTarget>,
    pub(crate) details: Option<Box<Details>>,
    /// The inspected object was deleted.
    pub(crate) details_gone: bool,
    pub(crate) owners: Vec<OwnerLink>,
    /// The open log pane, under the table.
    logs: Option<OpenLogPane>,
    last_log_tab: u64,
    /// The port-forward manager, shown under the table when open.
    forwards: Entity<ForwardsPane>,
    pub(crate) show_forwards: bool,
    _events: Task<()>,
    _table_events: Subscription,
}

impl EventEmitter<WorkspaceEvent> for Workspace {}

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
            cx.new(|cx| TableState::new(ResourceTable::default(), window, cx).col_movable(false));
        let table_events = cx.subscribe(&table, |this: &mut Self, table, event, cx| match event {
            TableEvent::SelectRow(ix) => {
                let key = table.read(cx).delegate().key_at(*ix);
                if let Some(key) = key {
                    this.inspect(key, cx);
                }
            }
            TableEvent::ClearSelection => this.close_inspector(cx),
            _ => {}
        });
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
        Self {
            app_name: app_name.into(),
            send,
            catalog: None,
            selected: None,
            states: HashMap::new(),
            kind: Kind::Pod,
            sub: None,
            next_sub: 1,
            table,
            has_rows: false,
            counts: HashMap::new(),
            statuses: HashMap::new(),
            inspected: None,
            details: None,
            details_gone: false,
            owners: Vec::new(),
            logs: None,
            last_log_tab: 0,
            forwards,
            show_forwards: false,
            _events: events_task,
            _table_events: table_events,
        }
    }

    /// Selects a context, connects to it and shows the current kind.
    pub fn select(&mut self, context: ContextName, cx: &mut Context<Self>) {
        if self.selected.as_ref() == Some(&context) {
            return;
        }
        self.selected = Some(context.clone());
        self.close_logs(cx);
        let namespace = self
            .catalog
            .as_ref()
            .and_then(|c| c.get(&context))
            .and_then(|c| c.namespace.clone());
        self.forwards.update(cx, |pane, cx| {
            pane.set_context(Some(context.clone()), namespace.as_deref(), cx);
        });
        (self.send)(Command::SelectContext(context));
        self.resubscribe(cx);
        cx.notify();
    }

    /// Shows another kind in the table.
    pub fn select_kind(&mut self, kind: Kind, cx: &mut Context<Self>) {
        if self.kind == kind {
            return;
        }
        self.kind = kind;
        self.resubscribe(cx);
        cx.notify();
    }

    /// Asks the engine to read the kubeconfig files again.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        (self.send)(Command::ReloadCatalog);
        cx.notify();
    }

    /// Follows an object of the current kind in the inspector.
    pub fn inspect(&mut self, key: ObjectKey, cx: &mut Context<Self>) {
        let Some(context) = self.selected.clone() else {
            return;
        };
        let target = InspectTarget {
            context,
            kind: self.kind,
            namespace: None,
            key,
        };
        if self.inspected.as_ref() == Some(&target) {
            return;
        }
        (self.send)(Command::Inspect(Some(target.clone())));
        self.inspected = Some(target);
        self.details = None;
        self.details_gone = false;
        self.owners.clear();
        cx.notify();
    }

    /// Closes the inspector.
    pub fn close_inspector(&mut self, cx: &mut Context<Self>) {
        if self.inspected.take().is_none() {
            return;
        }
        (self.send)(Command::Inspect(None));
        self.details = None;
        self.details_gone = false;
        self.owners.clear();
        self.table.update(cx, TableState::clear_selection);
        cx.notify();
    }

    /// The pod selected in the table, when the table shows pods.
    fn selected_pod(&self, cx: &App) -> Option<ObjectKey> {
        if self.kind != Kind::Pod {
            return None;
        }
        let state = self.table.read(cx);
        state.delegate().key_at(state.selected_row()?)
    }

    /// Opens a log pane for the pod selected in the table, replacing any
    /// open one.
    pub fn open_logs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ObjectKey {
            namespace: Some(namespace),
            name,
        }) = self.selected_pod(cx)
        else {
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

    /// Runs an inspector action. Logs open here; other actions go to
    /// whoever subscribes to [`WorkspaceEvent`].
    pub(crate) fn run_action(
        &mut self,
        action: Action,
        target: InspectTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if action == Action::Logs
            && target.kind == Kind::Pod
            && let Some(namespace) = target.key.namespace.clone()
        {
            self.open_pod_logs(namespace, Arc::clone(&target.key.name), window, cx);
            return;
        }
        cx.emit(WorkspaceEvent::Action { action, target });
    }

    /// Replaces the table subscription with one for the selected context
    /// and kind. The engine keeps the old watch warm for a while.
    fn resubscribe(&mut self, cx: &mut Context<Self>) {
        self.close_inspector(cx);
        if let Some(old) = self.sub.take() {
            (self.send)(Command::Unsubscribe(old.id));
        }
        self.has_rows = false;
        resource_table::clear(&self.table, cx);
        let Some(context) = self.selected.clone() else {
            return;
        };
        let id = SubscriptionId(self.next_sub);
        self.next_sub += 1;
        (self.send)(Command::Subscribe {
            id,
            context: context.clone(),
            resource: self.kind.resource(),
            namespace: None,
        });
        self.sub = Some(TableSub {
            id,
            context,
            kind: self.kind,
        });
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
            Event::Forwards(rows) => {
                self.forwards.update(cx, |pane, cx| pane.set_rows(rows, cx));
                return;
            }
            Event::Catalog(catalog) => self.catalog = Some(catalog),
            Event::Connection { context, state } => {
                self.states.insert(context, state);
            }
            // The sidebar of discovered kinds, and tables for kinds without
            // a typed spec, come with issue #19.
            Event::Kinds(_) => return,
            Event::Table(snapshot) => {
                let Some(kind) = Kind::from_resource(&snapshot.resource) else {
                    return;
                };
                let key = (snapshot.context.clone(), kind);
                let has_data = matches!(
                    snapshot.state,
                    ConnectionState::Live | ConnectionState::Retrying(_)
                );
                if has_data {
                    self.counts.insert(key.clone(), snapshot.rows.len());
                }
                self.statuses.insert(key, snapshot.state.clone());
                let current = snapshot.namespace.is_none()
                    && self
                        .sub
                        .as_ref()
                        .is_some_and(|s| s.context == snapshot.context && s.kind == kind);
                if current && has_data {
                    self.has_rows = true;
                    resource_table::set_rows(
                        &self.table,
                        snapshot.columns,
                        snapshot.rows,
                        kind.namespaced(),
                        cx,
                    );
                }
            }
            Event::Details { target, details } => {
                if self.inspected.as_ref() == Some(&target) {
                    self.details_gone = details.is_none();
                    self.details = details;
                }
            }
            Event::Owners { target, chain } => {
                if self.inspected.as_ref() == Some(&target) {
                    self.owners = chain;
                }
            }
        }
        cx.notify();
    }

    fn selected_state(&self) -> Option<&ConnectionState> {
        self.states.get(self.selected.as_ref()?)
    }

    fn table_status(&self) -> Option<&ConnectionState> {
        let sub = self.sub.as_ref()?;
        self.statuses.get(&(sub.context.clone(), sub.kind))
    }

    fn row_count(&self, cx: &App) -> usize {
        self.table.read(cx).delegate().rows().len()
    }

    /// The forwards pane, for the sidebar row.
    pub(crate) fn forwards_pane(&self) -> &Entity<ForwardsPane> {
        &self.forwards
    }
}

/// A context name as a UI string, without copying it.
pub(crate) fn shared(name: &ContextName) -> SharedString {
    SharedString::from(Arc::clone(name.as_arc()))
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let inspector = self
            .inspected
            .clone()
            .map(|target| self.render_inspector(&target, cx).into_any_element());
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
                    .child(self.render_tab_bar())
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .gap(px(theme::PANE_GAP))
                            .p(px(theme::PANE_GAP))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .h_full()
                                    .flex()
                                    .flex_col()
                                    .gap(px(theme::PANE_GAP))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_h_0()
                                            .key_context("ResourceTable")
                                            .on_action(cx.listener(
                                                |this, _: &OpenLogs, window, cx| {
                                                    this.open_logs(window, cx);
                                                },
                                            ))
                                            .child(self.render_pane(cx)),
                                    )
                                    .children(self.logs.as_ref().map(|open| {
                                        div()
                                            .h(relative(LOG_PANE_SHARE))
                                            .flex_none()
                                            .child(open.pane.clone())
                                    }))
                                    .when(self.show_forwards, |el| {
                                        el.child(
                                            div()
                                                .h(px(forwards::PANE_HEIGHT))
                                                .flex_none()
                                                .child(self.forwards.clone()),
                                        )
                                    }),
                            )
                            .children(inspector),
                    )
                    .child(self.render_status_bar(cx)),
            )
    }
}

impl Workspace {
    fn render_tab_bar(&self) -> impl IntoElement {
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
                    .child(icon(kind_icon(self.kind), 13.0, theme::MUTED))
                    .child(self.kind.plural()),
            )
    }

    fn render_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.row_count(cx);
        let pod_selected = self.selected_pod(cx).is_some();
        let scope: SharedString = if self.selected.is_none() {
            "".into()
        } else if self.kind.namespaced() {
            "all namespaces".into()
        } else {
            "cluster-wide".into()
        };
        let retrying = matches!(self.table_status(), Some(ConnectionState::Retrying(_)));
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
                    .child(icon(kind_icon(self.kind), 13.0, theme::MUTED))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::TEXT)
                            .child(self.kind.plural()),
                    )
                    .child(scope)
                    .child(div().flex_1())
                    .when(retrying, |el| {
                        el.child(div().text_color(theme::AMBER).child("reconnecting"))
                    })
                    .when(self.has_rows, |el| {
                        el.child(
                            div()
                                .font_family(theme::MONO_FONT)
                                .text_color(theme::DIM)
                                .child(count.to_string()),
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
                    .accessibility_label(self.kind.plural()),
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
        let what = self.kind.plural().to_lowercase();
        if let Some(ConnectionState::Failed(err)) = self.selected_state() {
            return Some(error_state(err, &server, &what));
        }
        if self.has_rows {
            return (count == 0).then(|| {
                Empty::new(
                    kind_icon(self.kind),
                    format!("No {what}"),
                    format!("There are no {what} you can see here."),
                )
            });
        }
        match self.table_status() {
            Some(ConnectionState::Failed(err)) => Some(error_state(err, &server, &what)),
            _ if matches!(
                self.selected_state(),
                None | Some(ConnectionState::Connecting)
            ) =>
            {
                Some(
                    Empty::new(Icon::LoaderCircle, format!("Connecting to {selected}"), "")
                        .detail(server),
                )
            }
            _ => Some(Empty::new(
                Icon::LoaderCircle,
                format!("Loading {what}"),
                "",
            )),
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
                    .child(dot(state.map_or(theme::AMBER, state_color), 6.0))
                    .child(shared(selected))
                    .into_any_element(),
            );
            if self.has_rows {
                let count = self.row_count(cx);
                let noun = if count == 1 {
                    self.kind.singular().to_lowercase()
                } else {
                    self.kind.plural().to_lowercase()
                };
                items.push(div().child(format!("{count} {noun}")).into_any_element());
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
