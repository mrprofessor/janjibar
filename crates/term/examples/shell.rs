//! A local shell in a window, drawn with the app's theme.
//!
//! ```sh
//! nix develop -c cargo run -p term --example shell
//! ```
//!
//! The window closes when the shell exits. Cmd-Q quits.

use gpui_kit::{
    App, Bounds, Context, Entity, IntoElement, Render, TitlebarOptions, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, size,
};
use term::{
    LocalTerminalOptions, Rgb, Terminal, TerminalColors, TerminalConfig, TerminalEvent,
    TerminalState,
};
use tracing_subscriber::EnvFilter;
use ui::theme;

/// The terminal colors from the app's tokens.
fn colors() -> TerminalColors {
    TerminalColors::new(
        theme::TERMINAL_FOREGROUND.into(),
        theme::TERMINAL_BACKGROUND.into(),
    )
    .with_cursor(theme::TERMINAL_CURSOR.into())
    .with_selection(
        theme::TERMINAL_SELECTION.into(),
        theme::TERMINAL_SELECTION_TEXT.into(),
    )
    .with_ansi(theme::TERMINAL_ANSI.map(Rgb::from))
}

/// One terminal pane on the canvas.
struct Shell {
    terminal: Entity<TerminalState>,
}

impl Shell {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let config = TerminalConfig::default().with_colors(colors());
        let terminal =
            cx.new(|cx| TerminalState::local(LocalTerminalOptions::default(), config, window, cx));
        cx.subscribe_in(&terminal, window, |_, terminal, event, window, cx| match event {
            TerminalEvent::TitleChanged => {
                let title = terminal.read(cx).title().clone();
                window.set_window_title(&title);
            }
            TerminalEvent::Exited(status) => {
                tracing::info!(code = ?status.code(), signal = ?status.signal(), "the shell exited");
                window.remove_window();
            }
            TerminalEvent::Failed(error) => {
                tracing::error!("the terminal failed: {error}");
                window.remove_window();
            }
            _ => {}
        })
        .detach();
        cx.observe(&terminal, |_, _, cx| cx.notify()).detach();
        let focus = terminal.read(cx).focus_handle().clone();
        window.focus(&focus, cx);
        Self { terminal }
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.terminal.read(cx).focus_handle().is_focused(window);
        div()
            .size_full()
            .bg(theme::CANVAS)
            .p(px(theme::PANE_GAP))
            .child(
                div()
                    .size_full()
                    .rounded(px(theme::PANE_RADIUS))
                    .border_1()
                    .border_color(if focused {
                        theme::ACCENT
                    } else {
                        theme::BORDER
                    })
                    .bg(theme::TERMINAL_BACKGROUND)
                    .overflow_hidden()
                    .child(Terminal::new("shell", &self.terminal)),
            )
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    gpui_kit::application()
        .with_assets(ui::assets::Assets)
        .run(|cx: &mut App| {
            gpui_kit::init(cx);
            ui::init(cx);
            cx.bind_keys(term::default_key_bindings());
            cx.on_action(|_: &ui::Quit, cx| cx.quit());
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let bounds = Bounds::centered(None, size(px(900.), px(560.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Shell".into()),
                    ..TitlebarOptions::default()
                }),
                ..WindowOptions::default()
            };
            if let Err(error) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Shell::new(window, cx))
            }) {
                tracing::error!("could not open the window: {error:#}");
                cx.quit();
            }
            cx.activate(true);
        });
}
