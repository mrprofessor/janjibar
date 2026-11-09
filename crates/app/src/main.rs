//! The desktop app binary.

use gpui_kit::{App, Context, Window, WindowOptions, div, prelude::*};

struct Empty;

impl Render for Empty {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(model::brand::APP_NAME)
    }
}

fn main() {
    tracing_subscriber::fmt::init();
    gpui_kit::application().run(|cx: &mut App| {
        gpui_kit::init(cx);
        if let Err(err) =
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|_| Empty))
        {
            tracing::error!("could not open the main window: {err:#}");
            cx.quit();
        }
    });
}
