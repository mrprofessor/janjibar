//! The window both examples open: one terminal in a pane card, drawn with
//! the app's tokens and fonts.

#![allow(dead_code, reason = "shared by two examples; each uses a subset")]

use std::any::Any;

use gpui_kit::{
    App, Bounds, Context, Entity, IntoElement, Render, SharedString, TitlebarOptions, Window,
    WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use term::{Rgb, Terminal, TerminalColors, TerminalConfig, TerminalEvent, TerminalState};
use tracing_subscriber::EnvFilter;
use ui::theme;

/// What happens when the program ends.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OnExit {
    /// Close the window, as a local terminal does.
    Close,
    /// Keep it open so the exit status and any error stay readable.
    Stay,
}

/// The terminal colors from the app's tokens.
pub fn colors() -> TerminalColors {
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

/// Logs to stderr, filtered by `RUST_LOG` (default `info`).
pub fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

/// One terminal pane on the canvas.
struct Pane {
    terminal: Entity<TerminalState>,
    /// Whatever must live as long as the terminal, such as a pinned
    /// kubeconfig.
    _keep: Box<dyn Any>,
}

impl Render for Pane {
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
                    .child(Terminal::new("terminal", &self.terminal)),
            )
    }
}

/// Opens the window with the terminal `build` makes and runs the app until
/// it quits. `keep` lives as long as the window.
pub fn run(
    title: impl Into<SharedString>,
    on_exit: OnExit,
    keep: Box<dyn Any>,
    build: impl FnOnce(TerminalConfig, &mut Window, &mut Context<TerminalState>) -> TerminalState
    + 'static,
) {
    run_with(title, on_exit, keep, |_| {}, build);
}

/// [`run`], with `setup` called on the app before the window opens, for
/// hooks such as `on_app_quit`.
pub fn run_with(
    title: impl Into<SharedString>,
    on_exit: OnExit,
    keep: Box<dyn Any>,
    setup: impl FnOnce(&mut App) + 'static,
    build: impl FnOnce(TerminalConfig, &mut Window, &mut Context<TerminalState>) -> TerminalState
    + 'static,
) {
    let title = title.into();
    gpui_kit::application()
        .with_assets(ui::assets::Assets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            ui::init(cx);
            setup(cx);
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
                    title: Some(title.clone()),
                    ..TitlebarOptions::default()
                }),
                ..WindowOptions::default()
            };
            let opened = gpui_kit::open_window(options, cx, move |window, cx| {
                cx.new(|cx| {
                    let config = TerminalConfig::default().with_colors(colors());
                    let terminal = cx.new(|cx| build(config, window, cx));
                    watch(&terminal, title, on_exit, window, cx);
                    let focus = terminal.read(cx).focus_handle().clone();
                    window.focus(&focus, cx);
                    Pane {
                        terminal,
                        _keep: keep,
                    }
                })
            });
            if let Err(error) = opened {
                tracing::error!("could not open the window: {error:#}");
                cx.quit();
            }
            cx.activate(true);
        });
}

/// Follows the program's title and exit.
fn watch(
    terminal: &Entity<TerminalState>,
    base: SharedString,
    on_exit: OnExit,
    window: &mut Window,
    cx: &mut Context<Pane>,
) {
    cx.subscribe_in(terminal, window, move |_, terminal, event, window, cx| {
        match event {
            TerminalEvent::TitleChanged => {
                let title = terminal.read(cx).title().clone();
                window.set_window_title(&format!("{base}: {title}"));
            }
            TerminalEvent::Exited(status) => {
                tracing::info!(code = ?status.code(), signal = ?status.signal(), "the shell exited");
                if on_exit == OnExit::Close {
                    window.remove_window();
                }
            }
            TerminalEvent::Failed(error) => tracing::error!("the terminal failed: {error}"),
            _ => {}
        }
    })
    .detach();
    cx.observe(terminal, |_, _, cx| cx.notify()).detach();
}
