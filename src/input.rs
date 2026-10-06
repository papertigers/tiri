// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Translating crossterm key events back into the bytes a program running in
//! a pane expects to read.

use std::io::Write as _;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};

pub fn encode_key(key: KeyEvent, application_cursor: bool) -> Vec<u8> {
    let mods = key.modifiers;
    let alt = mods.contains(KeyModifiers::ALT);
    let ctrl = mods.contains(KeyModifiers::CONTROL);

    let mut out = Vec::new();
    match key.code {
        KeyCode::Char(c) => {
            if alt {
                out.push(0x1b);
            }
            if ctrl && let Some(b) = ctrl_byte(c) {
                out.push(b);
                return out;
            }
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
        // Alt sends Escape first, for these as for characters.
        KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace | KeyCode::Esc => {
            if alt {
                out.push(0x1b);
            }
            out.push(match key.code {
                KeyCode::Enter => b'\r',
                KeyCode::Tab => b'\t',
                KeyCode::Backspace => 0x7f,
                _ => 0x1b,
            });
        }
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
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

fn ctrl_byte(c: char) -> Option<u8> {
    match c.to_ascii_lowercase() {
        c @ 'a'..='z' => Some(c as u8 - b'a' + 1),
        ' ' | '@' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' | '/' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

/// xterm's modifier parameter: 1 + shift + 2*alt + 4*ctrl.
fn modifier_param(mods: KeyModifiers) -> u8 {
    1 + u8::from(mods.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(mods.contains(KeyModifiers::ALT))
        + 4 * u8::from(mods.contains(KeyModifiers::CONTROL))
}

fn cursor_key(out: &mut Vec<u8>, final_byte: u8, mods: KeyModifiers, application: bool) {
    match modifier_param(mods) {
        1 if application => out.extend_from_slice(b"\x1bO"),
        1 => out.extend_from_slice(b"\x1b["),
        m => write!(out, "\x1b[1;{m}").expect("writing to memory can't fail"),
    }
    out.push(final_byte);
}

fn tilde_key(out: &mut Vec<u8>, code: u8, mods: KeyModifiers) {
    match modifier_param(mods) {
        1 => write!(out, "\x1b[{code}~"),
        m => write!(out, "\x1b[{code};{m}~"),
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
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (mut code, release) = match kind {
        MouseEventKind::Down(b) => (button(b), false),
        MouseEventKind::Up(b) => (button(b), true),
        MouseEventKind::Drag(b) if modes.drag || modes.motion => (button(b) + 32, false),
        MouseEventKind::Moved if modes.motion => (3 + 32, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
        _ => return None,
    };
    code += 4 * u32::from(mods.contains(KeyModifiers::SHIFT))
        + 8 * u32::from(mods.contains(KeyModifiers::ALT))
        + 16 * u32::from(mods.contains(KeyModifiers::CONTROL));
    let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);

    if modes.sgr {
        let end = if release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{x};{y}{end}").into_bytes());
    }
    // The older encodings can't say which button was released.
    if release {
        code = 3 + (code & !3);
    }
    let mut out = b"\x1b[M".to_vec();
    for value in [code, x, y] {
        let value = value + 32;
        if modes.utf8 {
            let mut buf = [0u8; 4];
            out.extend_from_slice(char::from_u32(value)?.encode_utf8(&mut buf).as_bytes());
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
        let enc =
            |kind, mods| encode_mouse(kind, 4, 9, mods, SGR).map(|b| String::from_utf8(b).unwrap());
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
        let modes = MouseModes {
            click: true,
            ..MouseModes::default()
        };
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
        let utf8 = MouseModes {
            utf8: true,
            ..modes
        };
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
