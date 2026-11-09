//! A local shell in a window, drawn with the app's theme.
//!
//! ```sh
//! nix develop -c cargo run -p term --example shell
//! ```
//!
//! The window closes when the shell exits. Cmd-Q quits.

#[path = "support/pane.rs"]
mod pane;

use pane::OnExit;
use term::{LocalTerminalOptions, TerminalState};

fn main() {
    pane::init_logging();
    pane::run(
        "Shell",
        OnExit::Close,
        Box::new(()),
        |config, window, cx| {
            TerminalState::local(LocalTerminalOptions::default(), config, window, cx)
        },
    );
}
