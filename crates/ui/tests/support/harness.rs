//! A headless window holding the workspace, driven by a fake engine.
//!
//! The test pushes engine `Event`s in and reads the `Command`s the view sends
//! back. Elements are found by their `debug_selector` in the last frame.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::{Bounds, Modifiers, Pixels, TestAppContext, VisualTestContext};
use model::{
    Catalog, Command, ContextInfo, ContextName, Event, Health, PodRow, PodSnapshot, Ready,
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

/// A running, ready pod.
pub fn pod(namespace: &str, name: &str) -> PodRow {
    PodRow {
        namespace: namespace.into(),
        name: name.into(),
        ready: Ready { ready: 1, total: 1 },
        status: "Running".into(),
        health: Health::Good,
        restarts: 0,
        age: "3d".into(),
        node: "node-1".into(),
    }
}

/// A pod snapshot for `context`. Rows must already be sorted, as the engine
/// sends them.
pub fn snapshot(context: &str, rows: Vec<PodRow>) -> Event {
    Event::Pods(PodSnapshot {
        context: ContextName::new(context),
        rows: Arc::from(rows),
    })
}
