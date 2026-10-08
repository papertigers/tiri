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

/// What decides how a pane's program wants its keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyModes {
    /// Cursor keys as SS3, not CSI (DECCKM).
    pub application_cursor: bool,
    /// The kitty keyboard protocol flags it asked for.
    pub keyboard: u8,
}

/// Kitty keyboard flag: keys that send the same as others otherwise, like
/// Escape and Alt combinations, Shift+Enter and Enter, are sent as
/// `CSI code ; modifiers u` instead.
const DISAMBIGUATE: u8 = 1;
/// Kitty keyboard flag: every key is sent that way, typing included.
const ALL_KEYS_AS_ESCAPES: u8 = 8;

/// The bytes a key sends to a program, as `modes` say it wants them.
pub fn encode_key(key: KeyEvent, modes: KeyModes) -> Vec<u8> {
    if modes.keyboard & (DISAMBIGUATE | ALL_KEYS_AS_ESCAPES) != 0
        && let Some(out) = kitty_key(key, modes.keyboard)
    {
        return out;
    }
    legacy_key(key, modes.application_cursor)
}

/// A key in the kitty keyboard protocol's own form, `CSI code ; modifiers
/// u`: for the keys the legacy encoding can't tell apart, or with
/// [`ALL_KEYS_AS_ESCAPES`] for all that have a code. None for the others,
/// like the arrows and function keys, whose legacy forms already say their
/// modifiers, as the protocol keeps them.
fn kitty_key(key: KeyEvent, flags: u8) -> Option<Vec<u8>> {
    let mut mods = kitty_modifiers(key.modifiers);
    let code = match key.code {
        KeyCode::Esc => ESCAPE_CODE,
        KeyCode::Enter => ENTER_CODE,
        KeyCode::Tab => TAB_CODE,
        KeyCode::BackTab => {
            mods |= KITTY_SHIFT;
            TAB_CODE
        }
        KeyCode::Backspace => BACKSPACE_CODE,
        // Letters by their unshifted form, Shift said in the modifiers.
        KeyCode::Char(c) => {
            if c.is_uppercase() {
                mods |= KITTY_SHIFT;
            }
            u32::from(c.to_lowercase().next().unwrap_or(c))
        }
        _ => return None,
    };
    let ambiguous = match key.code {
        KeyCode::Esc | KeyCode::BackTab => true,
        KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace => mods != 0,
        // Shift alone just types the other character.
        _ => mods & !KITTY_SHIFT != 0,
    };
    if !ambiguous && flags & ALL_KEYS_AS_ESCAPES == 0 {
        return None;
    }
    let mut out = Vec::new();
    if mods == 0 {
        write!(out, "{CSI}{code}{KITTY_FINAL}")
    } else {
        write!(out, "{CSI}{code};{}{KITTY_FINAL}", NO_MODIFIERS + mods)
    }
    .expect("writing to memory can't fail");
    Some(out)
}

/// The kitty keyboard protocol's codes for keys that are control
/// characters, and how its sequences end.
const ESCAPE_CODE: u32 = 27;
const ENTER_CODE: u32 = 13;
const TAB_CODE: u32 = 9;
const BACKSPACE_CODE: u32 = 127;
const KITTY_FINAL: char = 'u';

/// The kitty keyboard protocol's modifier bits: xterm's three, then the
/// keys xterm has no room for.
const KITTY_SHIFT: u8 = 1;
const KITTY_ALT: u8 = 2;
const KITTY_CTRL: u8 = 4;
const KITTY_SUPER: u8 = 8;
const KITTY_HYPER: u8 = 16;
const KITTY_META: u8 = 32;

fn kitty_modifiers(mods: KeyModifiers) -> u8 {
    [
        (KeyModifiers::SHIFT, KITTY_SHIFT),
        (KeyModifiers::ALT, KITTY_ALT),
        (KeyModifiers::CONTROL, KITTY_CTRL),
        (KeyModifiers::SUPER, KITTY_SUPER),
        (KeyModifiers::HYPER, KITTY_HYPER),
        (KeyModifiers::META, KITTY_META),
    ]
    .into_iter()
    .filter(|(m, _)| mods.contains(*m))
    .map(|(_, bit)| bit)
    .sum()
}

/// A key as terminals have always sent it, xterm's way. Nothing for keys
/// with Super, Hyper or Meta, which xterm's way can't say: without them,
/// Cmd+C would type a c.
fn legacy_key(key: KeyEvent, application_cursor: bool) -> Vec<u8> {
    let mods = key.modifiers;
    if mods.intersects(
        KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META,
    ) {
        return Vec::new();
    }
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

    const PLAIN: KeyModes = KeyModes { application_cursor: false, keyboard: 0 };
    const APP_CURSOR: KeyModes =
        KeyModes { application_cursor: true, keyboard: 0 };
    const KITTY: KeyModes =
        KeyModes { application_cursor: false, keyboard: DISAMBIGUATE };
    const KITTY_ALL: KeyModes = KeyModes {
        application_cursor: false,
        keyboard: DISAMBIGUATE | ALL_KEYS_AS_ESCAPES,
    };

    #[test]
    fn keys_legacy_encoding_confuses_are_told_apart_for_programs_that_ask() {
        let (none, shift) = (KeyModifiers::NONE, KeyModifiers::SHIFT);
        let ctrl = KeyModifiers::CONTROL;
        let enc = |code, mods, modes| encode_key(key(code, mods), modes);
        // Shift+Enter, Enter's twin until now.
        assert_eq!(enc(KeyCode::Enter, shift, KITTY), b"\x1b[13;2u");
        assert_eq!(enc(KeyCode::Enter, shift, PLAIN), b"\r");
        assert_eq!(enc(KeyCode::Enter, none, KITTY), b"\r");
        // Ctrl+I and Tab, Ctrl+[ and Escape.
        assert_eq!(enc(KeyCode::Char('i'), ctrl, KITTY), b"\x1b[105;5u");
        assert_eq!(enc(KeyCode::Tab, none, KITTY), b"\t");
        assert_eq!(enc(KeyCode::Esc, none, KITTY), b"\x1b[27u");
        assert_eq!(enc(KeyCode::BackTab, shift, KITTY), b"\x1b[9;2u");
        // Alt and Super with letters; Ctrl+Shift by the letter's lower case.
        let alt = KeyModifiers::ALT;
        assert_eq!(enc(KeyCode::Char('a'), alt, KITTY), b"\x1b[97;3u");
        assert_eq!(
            enc(KeyCode::Char('h'), KeyModifiers::SUPER, KITTY),
            b"\x1b[104;9u"
        );
        assert_eq!(enc(KeyCode::Char('A'), ctrl | shift, KITTY), b"\x1b[97;6u");
        // Typing stays typing, Shift included.
        assert_eq!(enc(KeyCode::Char('a'), none, KITTY), b"a");
        assert_eq!(enc(KeyCode::Char('A'), shift, KITTY), b"A");
        // Keys that already say their modifiers keep their forms.
        assert_eq!(enc(KeyCode::Up, shift, KITTY), b"\x1b[1;2A");
        assert_eq!(enc(KeyCode::F(5), none, KITTY), b"\x1b[15~");
    }

    #[test]
    fn keys_with_super_and_the_like_are_not_sent_without_them() {
        let enc = |code, mods, modes| encode_key(key(code, mods), modes);
        let cmd = KeyModifiers::SUPER;
        // Cmd+C and Cmd+K, which Ghostty passes on when it has nothing to
        // copy or clear, aren't a c and a k.
        assert_eq!(enc(KeyCode::Char('c'), cmd, PLAIN), b"");
        assert_eq!(enc(KeyCode::Char('k'), cmd, APP_CURSOR), b"");
        assert_eq!(enc(KeyCode::Up, cmd, PLAIN), b"");
        assert_eq!(enc(KeyCode::Enter, KeyModifiers::HYPER, PLAIN), b"");
        assert_eq!(
            enc(
                KeyCode::Char('x'),
                KeyModifiers::META | KeyModifiers::CONTROL,
                PLAIN
            ),
            b""
        );
        // Programs that asked for the kitty keyboard protocol get them.
        assert_eq!(enc(KeyCode::Char('c'), cmd, KITTY), b"\x1b[99;9u");
    }

    #[test]
    fn with_all_keys_as_escapes_typing_is_sent_as_codes_too() {
        let (none, shift) = (KeyModifiers::NONE, KeyModifiers::SHIFT);
        let enc = |code, mods| encode_key(key(code, mods), KITTY_ALL);
        assert_eq!(enc(KeyCode::Char('a'), none), b"\x1b[97u");
        assert_eq!(enc(KeyCode::Char('A'), shift), b"\x1b[97;2u");
        assert_eq!(enc(KeyCode::Enter, none), b"\x1b[13u");
        assert_eq!(enc(KeyCode::Backspace, none), b"\x1b[127u");
        assert_eq!(enc(KeyCode::Up, none), b"\x1b[A");
    }

    #[test]
    fn encodes_common_keys() {
        let none = KeyModifiers::NONE;
        assert_eq!(
            encode_key(key(KeyCode::Char('é'), none), PLAIN),
            "é".as_bytes()
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL), PLAIN),
            [3]
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('b'), KeyModifiers::ALT), PLAIN),
            b"\x1bb"
        );
        assert_eq!(encode_key(key(KeyCode::Up, none), PLAIN), b"\x1b[A");
        assert_eq!(encode_key(key(KeyCode::Up, none), APP_CURSOR), b"\x1bOA");
        assert_eq!(
            encode_key(key(KeyCode::Left, KeyModifiers::CONTROL), APP_CURSOR),
            b"\x1b[1;5D"
        );
        assert_eq!(encode_key(key(KeyCode::Delete, none), PLAIN), b"\x1b[3~");
        assert_eq!(encode_key(key(KeyCode::F(1), none), PLAIN), b"\x1bOP");
        assert_eq!(encode_key(key(KeyCode::F(5), none), PLAIN), b"\x1b[15~");
    }

    #[test]
    fn modifiers_reach_function_keys_tab_and_escape() {
        let (shift, alt) = (KeyModifiers::SHIFT, KeyModifiers::ALT);
        assert_eq!(encode_key(key(KeyCode::F(3), shift), PLAIN), b"\x1b[1;2R");
        assert_eq!(
            encode_key(key(KeyCode::F(4), KeyModifiers::CONTROL), PLAIN),
            b"\x1b[1;5S"
        );
        assert_eq!(encode_key(key(KeyCode::F(5), shift), PLAIN), b"\x1b[15;2~");
        assert_eq!(encode_key(key(KeyCode::Tab, alt), PLAIN), b"\x1b\t");
        assert_eq!(encode_key(key(KeyCode::Esc, alt), PLAIN), b"\x1b\x1b");
        assert_eq!(encode_key(key(KeyCode::Enter, alt), PLAIN), b"\x1b\r");
        assert_eq!(
            encode_key(key(KeyCode::Char('/'), KeyModifiers::CONTROL), PLAIN),
            [0x1f]
        );
    }
}
