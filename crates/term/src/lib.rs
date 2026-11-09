//! A terminal pane for GPUI on libghostty-vt, the terminal emulation
//! library from [Ghostty](https://ghostty.org).
//!
//! The pieces:
//!
//! - [`TerminalState`] is the entity: it holds the latest [`TerminalFrame`],
//!   the program's [`TerminalStatus`], the selection and the focus, and it
//!   sends [`TerminalInput`] to its source.
//! - [`Terminal`] is the element that paints a state and routes keys, the
//!   pointer and the input method to it.
//! - A [`FrameSource`] produces frames. [`Engine`] runs a [`ByteSource`]
//!   through a Ghostty terminal on an owner thread. [`LocalPty`] is the byte
//!   source for a local program. [`SessionSource`] serves a shell the
//!   engine runs, `exec` into a pod or a node shell, over a
//!   `model::ShellSession`. [`StreamSource`] is one the application feeds
//!   itself: bytes in, bytes out and the grid size. [`FixtureSource`]
//!   parses canned bytes for tests.
//! - [`shell`] resolves the user's shell and its login command the way
//!   Ghostty does.
//!
//! A view holds a [`TerminalState`] and renders a [`Terminal`] for it:
//!
//! ```no_run
//! use gpui_kit::{Context, Entity, IntoElement, Render, Window, div, prelude::*};
//! use term::{LocalTerminalOptions, Terminal, TerminalConfig, TerminalState};
//!
//! struct Shell {
//!     terminal: Entity<TerminalState>,
//! }
//!
//! impl Shell {
//!     fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
//!         let terminal = cx.new(|cx| {
//!             TerminalState::local(
//!                 LocalTerminalOptions::default(),
//!                 TerminalConfig::default(),
//!                 window,
//!                 cx,
//!             )
//!         });
//!         cx.observe(&terminal, |_, _, cx| cx.notify()).detach();
//!         Self { terminal }
//!     }
//! }
//!
//! impl Render for Shell {
//!     fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
//!         div().size_full().child(Terminal::new("shell", &self.terminal))
//!     }
//! }
//! ```
//!
//! A remote session feeds a [`StreamSource`] instead of a local pty. The
//! program's output goes in through the [`StreamPeer`]; keys, query replies
//! and resizes come back as [`StreamEvent`]s:
//!
//! ```no_run
//! # use term::*;
//! # fn remote(window: &mut gpui_kit::Window, cx: &mut gpui_kit::Context<TerminalState>) -> TerminalState {
//! let (source, peer) = StreamSource::new();
//! peer.output(b"$ ");
//! TerminalState::new(Engine::new(source, EngineOptions::default()), TerminalConfig::default(), window, cx)
//! # }
//! ```
//!
//! # Key bindings
//!
//! A focused terminal takes every key it can encode, Tab and Ctrl-C
//! included. Escape drops a selection first, when there is one. The rest
//! are [`actions`] an application binds in the [`KEY_CONTEXT`] key context;
//! [`default_key_bindings`] holds Ghostty's keys for them: copy and paste,
//! select all, clear, Shift with Page Up, Page Down, Home, End, Up and Down
//! to scroll, and the font zoom.
//!
//! # Look
//!
//! The grid's colors come from [`TerminalColors`], which the application
//! builds from its own palette. The font and its size follow gpui-kit's
//! `Theme` (its mono font) unless the [`TerminalAppearance`] names others.
//!
//! # Threads
//!
//! The engine never blocks the UI: frames arrive through a latest-wins slot
//! and an async wake, input goes through a bounded queue that Ctrl-C jumps,
//! a hidden terminal keeps parsing but publishes nothing, and synchronized
//! output (mode 2026) holds frames for at most one second. The engine never
//! waits on the program either: a write the pty cannot take yet resumes
//! when it has room, and at most a megabyte of output waits for the parser
//! before the pty holds the program back.
//!
//! Forked from the terminal module of gpui-cn 0.8.0 (Apache-2.0), which is
//! derived from Herdr (Apache-2.0); its block, decoration and element
//! structure are adapted from Muxy v2 (MIT). See `NOTICE`.

mod appearance;
mod block;
mod colors;
mod core;
mod cursor;
mod decoration;
mod element;
mod engine;
pub mod frame;
mod geometry;
mod host_theme;
mod ime;
pub mod input;
mod keys;
mod links;
mod options;
pub mod park;
mod process;
mod pty;
mod pty_io;
mod selection;
mod session;
pub mod shell;
mod source;
mod state;
mod stream;
mod theme;

pub use appearance::TerminalAppearance;
pub use element::{KEY_CONTEXT, Terminal};
pub use engine::{
    ByteHandle, ByteSink, ByteSource, Engine, ExitStatus, ForegroundProcess, InputRejected,
    TerminalSnapshot, TerminalStatus,
};
pub use frame::{Rgb, RowId, TerminalColors, TerminalFrame, Viewport};
pub use geometry::Geometry;
pub use input::TerminalInput;
pub use options::{EngineOptions, LocalTerminalOptions, ShellIntegration, WorkingDirectory};
pub use pty::LocalPty;
pub use session::SessionSource;
pub use source::{FixtureSource, FrameHandle, FrameSink, FrameSource, StartOptions};
pub use state::{TerminalConfig, TerminalEvent, TerminalState};
pub use stream::{StreamEvent, StreamPeer, StreamSource};
pub use theme::ThemeError;

/// Actions dispatched in the [`KEY_CONTEXT`] key context.
pub mod actions {
    gpui_kit::actions!(
        terminal,
        [
            /// Copy the selection.
            Copy,
            /// Paste the clipboard.
            Paste,
            /// Select the viewport.
            SelectAll,
            /// Clear the screen through the shell.
            Clear,
            /// Scroll to the top of scrollback.
            ScrollToTop,
            /// Scroll to the bottom.
            ScrollToBottom,
            /// Scroll one page toward older output.
            ScrollPageUp,
            /// Scroll one page toward newer output.
            ScrollPageDown,
            /// Scroll one line toward older output.
            ScrollLineUp,
            /// Scroll one line toward newer output.
            ScrollLineDown,
            /// Zoom the font in one point, as Ghostty's `increase_font_size:1`.
            IncreaseFontSize,
            /// Zoom the font out one point, as Ghostty's `decrease_font_size:1`.
            DecreaseFontSize,
            /// Go back to the configured font size, as Ghostty's
            /// `reset_font_size`.
            ResetFontSize,
        ]
    );
}

/// Ghostty's keys for the terminal [`actions`], in the [`KEY_CONTEXT`] key
/// context, for an application to install with `cx.bind_keys(..)`. A
/// terminal works without them; see [Key bindings](self#key-bindings).
pub fn default_key_bindings() -> Vec<gpui_kit::KeyBinding> {
    use actions::{
        Clear, Copy, DecreaseFontSize, IncreaseFontSize, Paste, ResetFontSize, ScrollLineDown,
        ScrollLineUp, ScrollPageDown, ScrollPageUp, ScrollToBottom, ScrollToTop, SelectAll,
    };
    use gpui_kit::KeyBinding;
    let context = Some(KEY_CONTEXT);
    // Ghostty binds the font size to Cmd on macOS and Ctrl elsewhere, with
    // `+` beside `=` for keyboards that have a plus key.
    let [copy, paste, select_all, clear, larger, plus, smaller, reset] =
        if cfg!(target_os = "macos") {
            [
                "cmd-c", "cmd-v", "cmd-a", "cmd-k", "cmd-=", "cmd-+", "cmd--", "cmd-0",
            ]
        } else {
            [
                "ctrl-shift-c",
                "ctrl-shift-v",
                "ctrl-shift-a",
                "ctrl-shift-k",
                "ctrl-=",
                "ctrl-+",
                "ctrl--",
                "ctrl-0",
            ]
        };
    vec![
        KeyBinding::new(copy, Copy, context),
        KeyBinding::new(paste, Paste, context),
        KeyBinding::new(select_all, SelectAll, context),
        KeyBinding::new(clear, Clear, context),
        KeyBinding::new("shift-pageup", ScrollPageUp, context),
        KeyBinding::new("shift-pagedown", ScrollPageDown, context),
        KeyBinding::new("shift-home", ScrollToTop, context),
        KeyBinding::new("shift-end", ScrollToBottom, context),
        KeyBinding::new("shift-up", ScrollLineUp, context),
        KeyBinding::new("shift-down", ScrollLineDown, context),
        KeyBinding::new(larger, IncreaseFontSize, context),
        KeyBinding::new(plus, IncreaseFontSize, context),
        KeyBinding::new(smaller, DecreaseFontSize, context),
        KeyBinding::new(reset, ResetFontSize, context),
    ]
}
