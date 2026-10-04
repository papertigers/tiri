//! Translating crossterm key events back into the bytes a program running in
//! a pane expects to read.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
        KeyCode::Enter => out.extend_from_slice(if alt { b"\x1b\r" } else { b"\r" }),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => out.extend_from_slice(if alt { b"\x1b\x7f" } else { b"\x7f" }),
        KeyCode::Esc => out.push(0x1b),
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
        KeyCode::F(n @ 1..=4) => {
            out.extend_from_slice(b"\x1bO");
            out.push(b'P' + n - 1);
        }
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
        '_' | '-' | '7' => Some(0x1f),
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
        m => out.extend_from_slice(format!("\x1b[1;{m}").as_bytes()),
    }
    out.push(final_byte);
}

fn tilde_key(out: &mut Vec<u8>, code: u8, mods: KeyModifiers) {
    match modifier_param(mods) {
        1 => out.extend_from_slice(format!("\x1b[{code}~").as_bytes()),
        m => out.extend_from_slice(format!("\x1b[{code};{m}~").as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
