//! Input the UI sends to the engine.
//!
//! The engine owns the Ghostty encoders and the terminal modes, so the UI
//! sends typed requests and the owner thread encodes them.
//!
//! Derived from Herdr (Apache-2.0).

/// Modifier keys held during an input event.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent modifier key"
)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Modifiers {
    /// Shift.
    pub shift: bool,
    /// Control.
    pub control: bool,
    /// Alt or Option.
    pub alt: bool,
    /// Super, Command or the Windows key.
    pub super_key: bool,
}

/// What happened to a key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyAction {
    /// Pressed.
    #[default]
    Press,
    /// Held and repeating.
    Repeat,
    /// Released. Only encoded under the Kitty keyboard protocol.
    Release,
}

/// A key event in GPUI terms.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeyInput {
    /// GPUI's key name, such as `enter`, `left`, `f1` or `a`.
    pub key: String,
    /// The text the key produces on the current layout, if any.
    pub text: Option<String>,
    /// Modifiers held.
    pub modifiers: Modifiers,
    /// Press, repeat or release.
    pub action: KeyAction,
}

impl KeyInput {
    /// A key press with no modifiers and no text.
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            ..Self::default()
        }
    }

    /// Set the produced text.
    #[must_use]
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    /// Set the modifiers.
    #[must_use]
    pub fn with_modifiers(mut self, modifiers: Modifiers) -> Self {
        self.modifiers = modifiers;
        self
    }

    /// Set the action.
    #[must_use]
    pub fn with_action(mut self, action: KeyAction) -> Self {
        self.action = action;
        self
    }
}

/// A pointer action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    /// A button went down.
    Press,
    /// A button went up.
    Release,
    /// The pointer moved.
    Move,
    /// The wheel scrolled up.
    ScrollUp,
    /// The wheel scrolled down.
    ScrollDown,
}

/// A pointer button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    /// Left.
    Left,
    /// Middle.
    Middle,
    /// Right.
    Right,
}

/// A pointer event for a program that reports the mouse.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct MouseInput {
    /// What happened.
    pub action: MouseAction,
    /// The button, when one is involved.
    pub button: Option<MouseButton>,
    /// Modifiers held.
    pub modifiers: Modifiers,
    /// X in device pixels from the grid origin.
    pub x: f32,
    /// Y in device pixels from the grid origin.
    pub y: f32,
}

impl MouseInput {
    /// A pointer event at a grid position.
    #[must_use]
    pub fn new(action: MouseAction, x: f32, y: f32) -> Self {
        Self {
            action,
            button: None,
            modifiers: Modifiers::default(),
            x,
            y,
        }
    }

    /// Set the button.
    #[must_use]
    pub fn with_button(mut self, button: MouseButton) -> Self {
        self.button = Some(button);
        self
    }

    /// Set the modifiers.
    #[must_use]
    pub fn with_modifiers(mut self, modifiers: Modifiers) -> Self {
        self.modifiers = modifiers;
        self
    }
}

/// A viewport scroll request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollRequest {
    /// Scroll by rows; negative is toward older output.
    Lines(isize),
    /// Scroll to the top of the scrollback.
    Top,
    /// Scroll to the bottom, the active area.
    Bottom,
}

/// Everything the UI can send to the engine.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum TerminalInput {
    /// Raw text for the program, such as an IME commit.
    Text(String),
    /// Pasted text, bracketed when the program asked for it.
    Paste(String),
    /// A key.
    Key(KeyInput),
    /// A pointer event.
    Mouse(MouseInput),
    /// A viewport scroll.
    Scroll(ScrollRequest),
    /// Focus gained or lost.
    Focus(bool),
}

impl TerminalInput {
    /// Whether this input interrupts the program (Ctrl+C), which jumps the
    /// queue and discards pending input.
    #[must_use]
    pub fn is_interrupt(&self) -> bool {
        match self {
            Self::Key(key) => {
                key.key.eq_ignore_ascii_case("c")
                    && key.modifiers.control
                    && !key.modifiers.alt
                    && !key.modifiers.super_key
                    && key.action != KeyAction::Release
            }
            Self::Text(text) => text == "\x03",
            _ => false,
        }
    }

    /// The bytes this input accounts for in the bounded queue.
    #[must_use]
    pub fn budget(&self) -> usize {
        match self {
            Self::Text(s) | Self::Paste(s) => s.len(),
            Self::Key(k) => k.key.len() + k.text.as_ref().map_or(0, String::len),
            _ => 0,
        }
    }
}
