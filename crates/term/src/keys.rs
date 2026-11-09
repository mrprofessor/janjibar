//! Translate GPUI input into terminal input requests.
//!
//! Key and mouse encoding belongs to the engine, which owns the Ghostty
//! encoders and the terminal modes. This module converts platform input
//! into the typed requests and owns the policy for which events the
//! terminal sees at all.
//!
//! Derived from Herdr (Apache-2.0).

use gpui_kit::{Keystroke, Modifiers as GpuiModifiers, MouseButton as GpuiMouseButton};

use crate::input::{
    KeyAction, KeyInput, Modifiers, MouseAction, MouseButton, MouseInput, ScrollRequest,
};

pub(crate) fn modifiers(modifiers: GpuiModifiers) -> Modifiers {
    Modifiers {
        shift: modifiers.shift,
        control: modifiers.control,
        alt: modifiers.alt,
        super_key: modifiers.platform,
    }
}

/// What a focused terminal does with a key that a binding outside it
/// would take.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Claim {
    /// Let the binding run.
    None,
    /// Send the key to the program instead.
    Send,
    /// Copy the selection, when there is one, instead.
    Copy,
}

/// Which keys `gpui-base`'s `Root` binds that belong to a terminal: Tab
/// and Shift-Tab, which move focus there, and its copy key, which is
/// Cmd-C on macOS and Ctrl-C, the interrupt, elsewhere. A binding an
/// application adds in the terminal's context outranks these, so the
/// terminal never takes it.
pub(crate) fn claim(action: &str, keystroke: &Keystroke) -> Claim {
    match action {
        "input::Copy" if keystroke.modifiers.platform => Claim::Copy,
        "root::Tab" | "root::TabPrev" | "input::Copy" => Claim::Send,
        _ => Claim::None,
    }
}

/// Translate a keystroke into terminal key input.
///
/// Returns `None` for platform command chords so application shortcuts
/// such as Cmd-C keep working; every other chord is the terminal's.
pub(crate) fn key(keystroke: &Keystroke, action: KeyAction) -> Option<KeyInput> {
    if keystroke.modifiers.platform {
        return None;
    }
    Some(KeyInput {
        key: keystroke.key.clone(),
        text: keystroke
            .key_char
            .as_ref()
            .filter(|text| !text.is_empty())
            .cloned(),
        modifiers: modifiers(keystroke.modifiers),
        action,
    })
}

pub(crate) fn key_action(is_held: bool) -> KeyAction {
    if is_held {
        KeyAction::Repeat
    } else {
        KeyAction::Press
    }
}

pub(crate) fn mouse_button(button: GpuiMouseButton) -> Option<MouseButton> {
    match button {
        GpuiMouseButton::Left => Some(MouseButton::Left),
        GpuiMouseButton::Middle => Some(MouseButton::Middle),
        GpuiMouseButton::Right => Some(MouseButton::Right),
        GpuiMouseButton::Navigate(_) => None,
    }
}

/// Whether pointer events go to the program instead of selection. Shift is
/// the conventional bypass so a user can still select text under a
/// full-screen program.
pub(crate) fn reports_mouse(mouse_reporting: bool, modifiers: GpuiModifiers) -> bool {
    mouse_reporting && !modifiers.shift
}

pub(crate) fn mouse(
    action: MouseAction,
    button: Option<MouseButton>,
    modifiers_in: GpuiModifiers,
    x: f32,
    y: f32,
) -> MouseInput {
    MouseInput {
        action,
        button,
        modifiers: modifiers(modifiers_in),
        x,
        y,
    }
}

/// Accumulates fractional wheel deltas into whole scrolled lines.
///
/// A trackpad reports many sub-line deltas; truncating each one would
/// discard every event. The remainder is carried until it adds up.
#[derive(Default)]
pub(crate) struct ScrollAccumulator {
    pending: f32,
}

impl ScrollAccumulator {
    /// Feed a delta in lines. Positive means content moves down, which
    /// scrolls toward earlier output.
    pub(crate) fn push(&mut self, lines: f32) -> Option<ScrollRequest> {
        if !lines.is_finite() {
            return None;
        }
        if self.pending != 0.0 && self.pending.signum() != lines.signum() {
            self.pending = 0.0;
        }
        self.pending += lines;
        let whole = self.pending.trunc();
        if whole == 0.0 {
            return None;
        }
        self.pending -= whole;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "`whole` is a whole number of lines from one scroll event"
        )]
        Some(ScrollRequest::Lines(-(whole as isize)))
    }

    pub(crate) fn reset(&mut self) {
        self.pending = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keystroke(value: &'static str) -> Keystroke {
        Keystroke::parse(value).expect("valid keystroke")
    }

    #[test]
    fn a_terminal_claims_roots_focus_keys_and_its_interrupt() {
        assert_eq!(claim("root::Tab", &keystroke("tab")), Claim::Send);
        assert_eq!(claim("root::TabPrev", &keystroke("shift-tab")), Claim::Send);
        assert_eq!(claim("input::Copy", &keystroke("ctrl-c")), Claim::Send);
        assert_eq!(claim("input::Copy", &keystroke("cmd-c")), Claim::Copy);
        assert_eq!(claim("app::NewTab", &keystroke("cmd-t")), Claim::None);
    }

    #[test]
    fn platform_chords_stay_with_the_application() {
        assert!(key(&keystroke("cmd-c"), KeyAction::Press).is_none());
        let control = key(&keystroke("ctrl-c"), KeyAction::Press).expect("terminal key");
        assert_eq!(control.key, "c");
        assert!(control.modifiers.control);
        let shift_enter = key(&keystroke("shift-enter"), KeyAction::Press).expect("key");
        assert_eq!(shift_enter.key, "enter");
        assert!(shift_enter.modifiers.shift);
        assert_eq!(key_action(true), KeyAction::Repeat);
    }

    #[test]
    fn mouse_reporting_is_bypassed_while_shift_is_held() {
        let shift = GpuiModifiers {
            shift: true,
            ..GpuiModifiers::default()
        };
        assert!(reports_mouse(true, GpuiModifiers::default()));
        assert!(!reports_mouse(true, shift));
        assert!(!reports_mouse(false, GpuiModifiers::default()));
    }

    fn lines(request: Option<ScrollRequest>) -> isize {
        match request {
            Some(ScrollRequest::Lines(lines)) => lines,
            _ => 0,
        }
    }

    #[test]
    fn wheel_deltas_accumulate_into_whole_lines() {
        let mut scroll = ScrollAccumulator::default();
        assert_eq!(lines(scroll.push(1.0)), -1);
        assert_eq!(lines(scroll.push(-3.0)), 3);
        assert!(scroll.push(0.4).is_none());
        assert!(scroll.push(0.4).is_none());
        assert_eq!(lines(scroll.push(0.4)), -1);
        let mut scroll = ScrollAccumulator::default();
        assert!(scroll.push(0.9).is_none());
        assert_eq!(lines(scroll.push(-1.0)), 1);
        assert_eq!(lines(scroll.push(12.5)), -12);
    }
}
