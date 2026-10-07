// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Translating crossterm key events back into the bytes a program running in
//! a pane expects to read.

use std::io::Write as _;

use crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind,
};

use crate::escape::{CONTROL_MASK, CSI, DEL, ESC, SS3, csi};

pub fn encode_key(key: KeyEvent, application_cursor: bool) -> Vec<u8> {
    let mods = key.modifiers;
    let alt = mods.contains(KeyModifiers::ALT);
    let ctrl = mods.contains(KeyModifiers::CONTROL);

    let mut out = Vec::new();
    match key.code {
        KeyCode::Char(c) => {
            if alt {
                out.push(ESC);
            }
            if ctrl && let Some(b) = ctrl_byte(c) {
                out.push(b);
                return out;
            }
            write!(out, "{c}").expect("writing to memory can't fail");
        }
        // Alt sends Escape first, for these as for characters.
        KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace | KeyCode::Esc => {
            if alt {
                out.push(ESC);
            }
            out.push(match key.code {
                KeyCode::Enter => b'\r',
                KeyCode::Tab => b'\t',
                KeyCode::Backspace => DEL,
                _ => ESC,
            });
        }
        KeyCode::BackTab => out.extend_from_slice(csi!("Z").as_bytes()),
        KeyCode::Up => cursor_key(&mut out, b'A', mods, application_cursor),
        KeyCode::Down => cursor_key(&mut out, b'B', mods, application_cursor),
        KeyCode::Right => cursor_key(&mut out, b'C', mods, application_cursor),
        KeyCode::Left => cursor_key(&mut out, b'D', mods, application_cursor),
        KeyCode::Home => cursor_key(&mut out, b'H', mods, application_cursor),
        KeyCode::End => cursor_key(&mut out, b'F', mods, application_cursor),
        KeyCode::Insert => tilde_key(&mut out, 2, mods),
        KeyCode::Delete => tilde_key(&mut out, 3, mods),
        KeyCode::PageUp => tilde_key(&mut out, 5, mods),
        KeyCode::PageDown => tilde_key(&mut out, 6, mods),
        // F1 to F4 are SS3 P to S, or CSI 1 ; modifier P to S.
        KeyCode::F(n @ 1..=4) => cursor_key(&mut out, b'P' + n - 1, mods, true),
        KeyCode::F(n) => {
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => return out,
            };
            tilde_key(&mut out, code, mods);
        }
        _ => {}
    }
    out
}

/// The control character Ctrl+`c` sends, if any: `c`'s code with all but
/// the low bits cleared, so Ctrl+A is 1 and Ctrl+[ is Escape.
fn ctrl_byte(c: char) -> Option<u8> {
    // Terminals also take a few keys for the punctuation that's hard to
    // type with Ctrl: Ctrl+2 for Ctrl+@, Ctrl+3 for Ctrl+[, and so on.
    let c = match c.to_ascii_lowercase() {
        ' ' | '2' => '@',
        '3' => '[',
        '4' => '\\',
        '5' => ']',
        '6' => '^',
        '-' | '/' | '7' => '_',
        '?' | '8' => return Some(DEL),
        c => c,
    };
    matches!(c, '@' | 'a'..='z' | '[' | '\\' | ']' | '^' | '_')
        .then(|| c as u8 & CONTROL_MASK)
}

/// xterm's modifier parameter for keys: one more than the sum of these.
const KEY_SHIFT: u8 = 1;
const KEY_ALT: u8 = 2;
const KEY_CTRL: u8 = 4;
/// The modifier parameter for no modifiers, which goes unsaid.
const NO_MODIFIERS: u8 = 1;

fn modifier_param(mods: KeyModifiers) -> u8 {
    let has = |m| u8::from(mods.contains(m));
    NO_MODIFIERS
        + KEY_SHIFT * has(KeyModifiers::SHIFT)
        + KEY_ALT * has(KeyModifiers::ALT)
        + KEY_CTRL * has(KeyModifiers::CONTROL)
}

fn cursor_key(
    out: &mut Vec<u8>,
    final_byte: u8,
    mods: KeyModifiers,
    application: bool,
) {
    match modifier_param(mods) {
        NO_MODIFIERS if application => out.extend_from_slice(SS3.as_bytes()),
        NO_MODIFIERS => out.extend_from_slice(CSI.as_bytes()),
        m => write!(out, "{CSI}{NO_MODIFIERS};{m}")
            .expect("writing to memory can't fail"),
    }
    out.push(final_byte);
}

fn tilde_key(out: &mut Vec<u8>, code: u8, mods: KeyModifiers) {
    match modifier_param(mods) {
        NO_MODIFIERS => write!(out, "{CSI}{code}~"),
        m => write!(out, "{CSI}{code};{m}~"),
    }
    .expect("writing to memory can't fail");
}

/// Which mouse events a pane's program asked to be told about, and how.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MouseModes {
    /// Presses and releases (mode 1000).
    pub click: bool,
    /// Movement while a button is held (1002).
    pub drag: bool,
    /// All movement (1003).
    pub motion: bool,
    /// The SGR encoding (1006), which has no coordinate limit.
    pub sgr: bool,
    /// The UTF-8 encoding (1005).
    pub utf8: bool,
}

impl MouseModes {
    pub fn any(self) -> bool {
        self.click || self.drag || self.motion
    }
}

// xterm's mouse protocol: an event is a code, the button's or wheel's plus
// modifier and motion bits, and a position counted from 1.
const LEFT_BUTTON: u32 = 0;
const MIDDLE_BUTTON: u32 = 1;
const RIGHT_BUTTON: u32 = 2;
/// No button: movement with none held, and in the older encodings, which
/// can't say which was let go, any release.
const NO_BUTTON: u32 = 3;
/// The low bits that hold the button, below the modifier and motion bits.
const BUTTON_BITS: u32 = 3;
/// Added for movement rather than a press.
const MOTION: u32 = 32;
/// The wheel's codes, one for each way it turns.
const WHEEL_UP: u32 = 64;
const WHEEL_DOWN: u32 = 65;
const WHEEL_LEFT: u32 = 66;
const WHEEL_RIGHT: u32 = 67;
const MOUSE_SHIFT: u32 = 4;
const MOUSE_ALT: u32 = 8;
const MOUSE_CTRL: u32 = 16;
/// The older encodings add this to every value, keeping them printable.
const LEGACY_OFFSET: u32 = 32;
/// The SGR encoding's final character for a press, and for a release.
const SGR_PRESS: char = 'M';
const SGR_RELEASE: char = 'm';

/// Encodes a mouse event at (`col`, `row`), zero-based within the pane, the
/// way the pane's program asked for. None if it didn't ask for this kind of
/// event, or the position can't be expressed in its encoding.
pub fn encode_mouse(
    kind: MouseEventKind,
    col: u16,
    row: u16,
    mods: KeyModifiers,
    modes: MouseModes,
) -> Option<Vec<u8>> {
    if !modes.any() {
        return None;
    }
    let button = |b: MouseButton| match b {
        MouseButton::Left => LEFT_BUTTON,
        MouseButton::Middle => MIDDLE_BUTTON,
        MouseButton::Right => RIGHT_BUTTON,
    };
    let (mut code, release) = match kind {
        MouseEventKind::Down(b) => (button(b), false),
        MouseEventKind::Up(b) => (button(b), true),
        MouseEventKind::Drag(b) if modes.drag || modes.motion => {
            (button(b) + MOTION, false)
        }
        MouseEventKind::Moved if modes.motion => (NO_BUTTON + MOTION, false),
        MouseEventKind::ScrollUp => (WHEEL_UP, false),
        MouseEventKind::ScrollDown => (WHEEL_DOWN, false),
        MouseEventKind::ScrollLeft => (WHEEL_LEFT, false),
        MouseEventKind::ScrollRight => (WHEEL_RIGHT, false),
        _ => return None,
    };
    let has = |m| u32::from(mods.contains(m));
    code += MOUSE_SHIFT * has(KeyModifiers::SHIFT)
        + MOUSE_ALT * has(KeyModifiers::ALT)
        + MOUSE_CTRL * has(KeyModifiers::CONTROL);
    // Positions count from 1.
    let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);

    if modes.sgr {
        let end = if release { SGR_RELEASE } else { SGR_PRESS };
        return Some(format!("{CSI}<{code};{x};{y}{end}").into_bytes());
    }
    // The older encodings can't say which button was released.
    if release {
        code = NO_BUTTON + (code & !BUTTON_BITS);
    }
    let mut out = csi!("M").as_bytes().to_vec();
    for value in [code, x, y] {
        let value = value + LEGACY_OFFSET;
        if modes.utf8 {
            write!(out, "{}", char::from_u32(value)?)
                .expect("writing to memory can't fail");
        } else {
            out.push(u8::try_from(value).ok()?);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SGR: MouseModes = MouseModes {
        click: true,
        drag: true,
        motion: false,
        sgr: true,
        utf8: false,
    };

    #[test]
    fn mouse_sgr_encoding() {
        let none = KeyModifiers::NONE;
        let left = MouseButton::Left;
        let enc = |kind, mods| {
            encode_mouse(kind, 4, 9, mods, SGR)
                .map(|b| String::from_utf8(b).unwrap())
        };
        assert_eq!(
            enc(MouseEventKind::Down(left), none).unwrap(),
            "\x1b[<0;5;10M"
        );
        assert_eq!(
            enc(MouseEventKind::Up(left), none).unwrap(),
            "\x1b[<0;5;10m"
        );
        assert_eq!(
            enc(MouseEventKind::Drag(left), none).unwrap(),
            "\x1b[<32;5;10M"
        );
        assert_eq!(
            enc(MouseEventKind::ScrollDown, KeyModifiers::CONTROL).unwrap(),
            "\x1b[<81;5;10M"
        );
        // Plain movement only goes to programs that asked for all motion.
        assert_eq!(enc(MouseEventKind::Moved, none), None);
    }

    #[test]
    fn mouse_legacy_encodings() {
        let modes = MouseModes { click: true, ..MouseModes::default() };
        let down = encode_mouse(
            MouseEventKind::Down(MouseButton::Right),
            0,
            0,
            KeyModifiers::NONE,
            modes,
        );
        assert_eq!(down.unwrap(), b"\x1b[M\x22\x21\x21");
        let up = encode_mouse(
            MouseEventKind::Up(MouseButton::Right),
            0,
            0,
            KeyModifiers::NONE,
            modes,
        );
        assert_eq!(up.unwrap(), b"\x1b[M\x23\x21\x21");
        // Without drag tracking, drags aren't reported.
        let drag = encode_mouse(
            MouseEventKind::Drag(MouseButton::Left),
            0,
            0,
            KeyModifiers::NONE,
            modes,
        );
        assert_eq!(drag, None);
        // Past column 223 the classic encoding runs out of bytes; UTF-8 doesn't.
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(MouseButton::Left),
                300,
                0,
                KeyModifiers::NONE,
                modes
            ),
            None
        );
        let utf8 = MouseModes { utf8: true, ..modes };
        let wide = encode_mouse(
            MouseEventKind::Down(MouseButton::Left),
            300,
            0,
            KeyModifiers::NONE,
            utf8,
        )
        .unwrap();
        assert_eq!(String::from_utf8(wide).unwrap(), "\x1b[M \u{14d}!");
    }

    #[test]
    fn nothing_is_sent_to_programs_that_did_not_ask() {
        let kind = MouseEventKind::Down(MouseButton::Left);
        assert_eq!(
            encode_mouse(kind, 1, 1, KeyModifiers::NONE, MouseModes::default()),
            None
        );
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn encodes_common_keys() {
        let none = KeyModifiers::NONE;
        assert_eq!(
            encode_key(key(KeyCode::Char('é'), none), false),
            "é".as_bytes()
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL), false),
            [3]
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('b'), KeyModifiers::ALT), false),
            b"\x1bb"
        );
        assert_eq!(encode_key(key(KeyCode::Up, none), false), b"\x1b[A");
        assert_eq!(encode_key(key(KeyCode::Up, none), true), b"\x1bOA");
        assert_eq!(
            encode_key(key(KeyCode::Left, KeyModifiers::CONTROL), true),
            b"\x1b[1;5D"
        );
        assert_eq!(encode_key(key(KeyCode::Delete, none), false), b"\x1b[3~");
        assert_eq!(encode_key(key(KeyCode::F(1), none), false), b"\x1bOP");
        assert_eq!(encode_key(key(KeyCode::F(5), none), false), b"\x1b[15~");
    }

    #[test]
    fn modifiers_reach_function_keys_tab_and_escape() {
        let (shift, alt) = (KeyModifiers::SHIFT, KeyModifiers::ALT);
        assert_eq!(encode_key(key(KeyCode::F(3), shift), false), b"\x1b[1;2R");
        assert_eq!(
            encode_key(key(KeyCode::F(4), KeyModifiers::CONTROL), false),
            b"\x1b[1;5S"
        );
        assert_eq!(encode_key(key(KeyCode::F(5), shift), false), b"\x1b[15;2~");
        assert_eq!(encode_key(key(KeyCode::Tab, alt), false), b"\x1b\t");
        assert_eq!(encode_key(key(KeyCode::Esc, alt), false), b"\x1b\x1b");
        assert_eq!(encode_key(key(KeyCode::Enter, alt), false), b"\x1b\r");
        assert_eq!(
            encode_key(key(KeyCode::Char('/'), KeyModifiers::CONTROL), false),
            [0x1f]
        );
    }
}
