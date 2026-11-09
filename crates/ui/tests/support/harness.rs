//! A headless window holding the workspace, driven by a fake engine.
//!
//! The test pushes engine `Event`s in and reads the `Command`s the view sends
//! back. Elements are found by their `debug_selector` in the last frame.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::{Bounds, Modifiers, Pixels, TestAppContext, VisualTestContext};
use model::resource::Kind;
use model::{
    Catalog, Column, ColumnType, Command, ConnectionState, ContextInfo, ContextName, Event, Health,
    TableRow, TableSnapshot,
};
use tokio::sync::mpsc;
use ui::{CommandSink, Workspace};

/// The workspace in a test window, plus both ends of its engine link.
pub struct Harness<'a> {
    pub cx: &'a mut VisualTestContext,
    events: mpsc::UnboundedSender<Event>,
    commands: Rc<RefCell<Vec<Command>>>,
}

impl<'a> Harness<'a> {
    /// Opens a window with a fresh workspace, set up as the app does it.
    pub fn new(cx: &'a mut TestAppContext) -> Self {
        cx.update(|cx| {
            gpui_kit::init(cx);
            ui::init(cx);
        });
        let commands = Rc::new(RefCell::new(Vec::new()));
        let send: CommandSink = {
            let commands = Rc::clone(&commands);
            Rc::new(move |command| commands.borrow_mut().push(command))
        };
        let (events, rx) = mpsc::unbounded_channel();
        let (_, cx) = cx.add_window_view(|window, cx| Workspace::new("Test", send, rx, window, cx));
        Self {
            cx,
            events,
            commands,
        }
    }

    /// Sends an event as the engine would, then lets the view settle.
    pub fn send(&mut self, event: Event) {
        self.events
            .send(event)
            .expect("the workspace dropped its event receiver");
        self.cx.run_until_parked();
    }

    /// Commands the view sent since the last call, oldest first.
    pub fn take_commands(&self) -> Vec<Command> {
        std::mem::take(&mut *self.commands.borrow_mut())
    }

    /// Where an element tagged with `debug_selector` was drawn in the last
    /// frame, or `None` if it was not drawn.
    pub fn bounds(&mut self, selector: &'static str) -> Option<Bounds<Pixels>> {
        self.cx.debug_bounds(selector)
    }

    /// Clicks the middle of an element, through real hit testing.
    pub fn click(&mut self, selector: &'static str) {
        let bounds = self
            .bounds(selector)
            .unwrap_or_else(|| panic!("`{selector}` was not drawn"));
        self.cx.simulate_click(bounds.center(), Modifiers::none());
        self.cx.run_until_parked();
    }
}

/// A catalog with one context per name, the first one current.
pub fn catalog(names: &[&str]) -> Catalog {
    let source = PathBuf::from("/home/test/.kube/config");
    Catalog {
        contexts: names
            .iter()
            .map(|&name| ContextInfo {
                name: ContextName::new(name),
                cluster: name.to_owned(),
                user: name.to_owned(),
                namespace: None,
                server: Some(format!("https://{name}.example:6443")),
                source: source.clone(),
            })
            .collect(),
        current: names.first().map(|&name| ContextName::new(name)),
        sources: vec![source],
        problems: Vec::new(),
        shadowed: Vec::new(),
    }
}

/// The pod table's columns, as the engine sends them.
pub fn pod_columns() -> Arc<[Column]> {
    let col = |name: &str, ty: ColumnType, format: &str| Column {
        name: name.into(),
        ty,
        format: format.into(),
        priority: 0,
        description: "".into(),
    };
    vec![
        col("Name", ColumnType::String, "name"),
        col("Ready", ColumnType::String, ""),
        col("Status", ColumnType::String, "status"),
        col("Age", ColumnType::Date, ""),
    ]
    .into()
}

/// A running, ready pod row.
pub fn pod(namespace: &str, name: &str) -> TableRow {
    TableRow {
        name: name.into(),
        namespace: Some(namespace.into()),
        uid: format!("uid-{namespace}-{name}").into(),
        created: None,
        cells: Arc::from([name.into(), "1/1".into(), "Running".into(), "3d".into()]),
        details: Arc::from([]),
        health: Health::Good,
    }
}

/// A live pod table snapshot for `context`, listing all namespaces. Rows
/// must already be sorted, as the engine sends them.
pub fn snapshot(context: &str, rows: Vec<TableRow>) -> Event {
    Event::Table(TableSnapshot {
        context: ContextName::new(context),
        resource: Kind::Pod.resource(),
        namespace: None,
        state: ConnectionState::Live,
        columns: pod_columns(),
        detail_columns: Arc::from([]),
        rows: Arc::from(rows),
    })
}
