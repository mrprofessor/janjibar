//! Cursor blink cadence tied to painted frames.
//!
//! Derived from Herdr (Apache-2.0).

use std::time::Duration;

use gpui_kit::{Context, Task};

/// Half period of the cursor blink.
const PHASE: Duration = Duration::from_millis(530);

/// Retained blink state for one terminal.
///
/// The next tick is scheduled from `sync`, which runs during paint, so a
/// terminal that is not painting (hidden, occluded, in an inactive window)
/// stops the cadence on its own instead of holding a free-running timer.
pub(crate) struct Blink {
    pub(crate) visible: bool,
    task: Option<Task<()>>,
}

impl Blink {
    pub(crate) fn new() -> Self {
        Self {
            visible: true,
            task: None,
        }
    }

    pub(crate) fn stop(&mut self) {
        self.task = None;
        self.visible = true;
    }

    /// Called once per painted frame. Starts, continues or stops the cadence
    /// to match whether the cursor should blink.
    pub(crate) fn sync<T: 'static>(
        &mut self,
        blinking: bool,
        cx: &mut Context<T>,
        on_tick: fn(&mut T, &mut Context<T>),
    ) {
        if !blinking {
            self.stop();
            return;
        }
        if self.task.is_some() {
            return;
        }
        self.task = Some(cx.spawn(async move |view, cx| {
            cx.background_executor().timer(PHASE).await;
            let _ = view.update(cx, on_tick);
        }));
    }

    /// Advance the phase. The next tick is scheduled by the following paint.
    pub(crate) fn tick(&mut self) {
        self.task = None;
        self.visible = !self.visible;
    }
}
