// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Key bindings: keys as the config writes them, the actions they run, and
//! the tables for each mode.
//!
//! Keys are normalized to what a terminal can tell apart: a shifted letter
//! is just the capital (`Shift+h`, `Shift+H` and `H` are one key), Ctrl
//! with a letter ignores case, since terminals send both the same, and
//! Ctrl with a symbol is the one control character terminals send for it.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A key and its modifiers (Ctrl, Alt, and Shift for keys other than
/// characters), normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    code: KeyCode,
    mods: KeyModifiers,
}

impl Key {
    fn new(code: KeyCode, mods: KeyModifiers) -> Self {
        let mut mods = mods
            & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        let code = match code {
            KeyCode::Char(c) => {
                // A character says whether Shift was down by its case.
                mods.remove(KeyModifiers::SHIFT);
                if mods.contains(KeyModifiers::CONTROL) {
                    KeyCode::Char(control_char(c))
                } else {
                    KeyCode::Char(c)
                }
            }
            // Shift+Tab arrives as BackTab, Shift and all.
            KeyCode::BackTab => {
                mods.insert(KeyModifiers::SHIFT);
                KeyCode::BackTab
            }
            code => code,
        };
        Self { code, mods }
    }

    /// The key a terminal reported.
    pub fn from_event(event: KeyEvent) -> Self {
        Self::new(event.code, event.modifiers)
    }

    /// The key this would be with Ctrl let go, for typing a binding after
    /// the prefix with Ctrl still held. Terminals send Ctrl+i, Ctrl+m and
    /// Ctrl+[ as Tab, Enter and Escape, so those count as i, m and [.
    pub fn without_ctrl(self) -> Option<Key> {
        if self.mods.contains(KeyModifiers::CONTROL) {
            return Some(Key::new(
                self.code,
                self.mods - KeyModifiers::CONTROL,
            ));
        }
        let c = match self.code {
            KeyCode::Tab => 'i',
            KeyCode::Enter => 'm',
            KeyCode::Esc => '[',
            _ => return None,
        };
        self.mods.is_empty().then(|| Key::new(KeyCode::Char(c), self.mods))
    }

    /// A single character typed with no modifiers but Shift, if that's
    /// what this is: for writing several keys run together, as in "hjkl".
    fn bare_char(self) -> Option<char> {
        match self.code {
            KeyCode::Char(c) if self.mods.is_empty() && c != ' ' => Some(c),
            _ => None,
        }
    }

    /// The key without its modifiers, written compactly for the status bar.
    fn short_name(self) -> String {
        match self.code {
            KeyCode::Char(' ') => "Space".to_owned(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Enter => "⏎".to_owned(),
            KeyCode::Esc => "Esc".to_owned(),
            KeyCode::Tab | KeyCode::BackTab => "Tab".to_owned(),
            KeyCode::Backspace => "⌫".to_owned(),
            KeyCode::Left => "←".to_owned(),
            KeyCode::Right => "→".to_owned(),
            KeyCode::Up => "↑".to_owned(),
            KeyCode::Down => "↓".to_owned(),
            KeyCode::PageUp => "PgUp".to_owned(),
            KeyCode::PageDown => "PgDn".to_owned(),
            KeyCode::F(n) => format!("F{n}"),
            code => name_of(code).unwrap_or("?").to_owned(),
        }
    }

    /// Ctrl, Alt and Shift, written compactly for the status bar.
    fn short_mods(self) -> String {
        let mut out = String::new();
        for (flag, name) in [
            (KeyModifiers::CONTROL, "C-"),
            (KeyModifiers::ALT, "Alt-"),
            (KeyModifiers::SHIFT, "S-"),
        ] {
            if self.mods.contains(flag) {
                out.push_str(name);
            }
        }
        out
    }

    /// The key written compactly for the status bar, as in "C-a" or "Alt-⏎".
    pub fn short(self) -> String {
        self.short_mods() + &self.short_name()
    }
}

/// The character Ctrl+`c` is known by. Terminals send one control
/// character for several keys (0x1F for Ctrl+_, Ctrl+/ and Ctrl+7, say),
/// and crossterm reports 0x1C to 0x1F as Ctrl+4 to Ctrl+7, so each group
/// goes by the name it has in ASCII: `\`, `]`, `^`, `_`, and Space for NUL.
fn control_char(c: char) -> char {
    match c {
        '4' | '\\' => '\\',
        '5' | ']' => ']',
        '6' | '^' => '^',
        '7' | '_' | '/' => '_',
        '2' | '@' | ' ' => ' ',
        c => c.to_ascii_lowercase(),
    }
}

/// Names for keys that aren't characters, as the config writes them.
const NAMED: &[(&str, KeyCode)] = &[
    ("Enter", KeyCode::Enter),
    ("Return", KeyCode::Enter),
    ("Escape", KeyCode::Esc),
    ("Esc", KeyCode::Esc),
    ("Tab", KeyCode::Tab),
    ("Backspace", KeyCode::Backspace),
    ("Space", KeyCode::Char(' ')),
    ("Left", KeyCode::Left),
    ("Right", KeyCode::Right),
    ("Up", KeyCode::Up),
    ("Down", KeyCode::Down),
    ("Home", KeyCode::Home),
    ("End", KeyCode::End),
    ("PageUp", KeyCode::PageUp),
    ("PageDown", KeyCode::PageDown),
    ("Insert", KeyCode::Insert),
    ("Delete", KeyCode::Delete),
];

fn name_of(code: KeyCode) -> Option<&'static str> {
    NAMED.iter().find(|(_, c)| *c == code).map(|(n, _)| *n)
}

impl FromStr for Key {
    type Err = String;

    /// Parses keys like `h`, `Shift+h`, `Ctrl+a`, `Alt+Enter` or `F5`.
    fn from_str(s: &str) -> Result<Self, String> {
        // The key is after the last +, unless it's + itself.
        let (mods_part, key) = match (s, s.strip_suffix("++")) {
            ("+", _) => (None, "+"),
            (_, Some(mods)) => (Some(mods), "+"),
            (_, None) => match s.rsplit_once('+') {
                Some((mods, key)) => (Some(mods), key),
                None => (None, s),
            },
        };
        let mut mods = KeyModifiers::empty();
        for m in mods_part.into_iter().flat_map(|m| m.split('+')) {
            mods |= match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => KeyModifiers::CONTROL,
                "alt" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                _ => {
                    return Err(format!(
                        "{m:?} isn't a modifier; there's Ctrl, Alt and Shift"
                    ));
                }
            };
        }
        let mut chars = key.chars();
        let code = match (chars.next(), chars.next()) {
            (Some(c), None) => {
                if mods.contains(KeyModifiers::SHIFT) && !c.is_alphabetic() {
                    return Err(format!(
                        "write the character Shift+{c} types itself, as in $ for Shift+4"
                    ));
                }
                let c = if mods.contains(KeyModifiers::SHIFT) {
                    let mut upper = c.to_uppercase();
                    match (upper.next(), upper.next()) {
                        (Some(upper), None) => upper,
                        _ => {
                            return Err(format!(
                                "Shift+{c} isn't a single character"
                            ));
                        }
                    }
                } else {
                    c
                };
                if mods.contains(KeyModifiers::CONTROL) {
                    let sent_as = match c.to_ascii_lowercase() {
                        'i' => Some("Tab"),
                        'm' => Some("Enter"),
                        '[' | '3' => Some("Escape"),
                        _ => None,
                    };
                    if let Some(name) = sent_as {
                        return Err(format!(
                            "terminals send Ctrl+{c} as {name}, so bind {name} instead"
                        ));
                    }
                }
                KeyCode::Char(c)
            }
            (None, _) => return Err("no key given".to_owned()),
            _ => {
                let named = NAMED
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(key))
                    .map(|(_, c)| *c);
                let function = (key.strip_prefix(['F', 'f']))
                    .and_then(|n| n.parse::<u8>().ok())
                    .filter(|n| (1..=12).contains(n))
                    .map(KeyCode::F);
                match named.or(function) {
                    Some(KeyCode::Tab)
                        if mods.contains(KeyModifiers::SHIFT) =>
                    {
                        KeyCode::BackTab
                    }
                    Some(code) => code,
                    None => return Err(format!("{key:?} isn't a key name")),
                }
            }
        };
        Ok(Key::new(code, mods))
    }
}

impl fmt::Display for Key {
    /// As the config writes it.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for (flag, name) in [
            (KeyModifiers::CONTROL, "Ctrl+"),
            (KeyModifiers::ALT, "Alt+"),
            (KeyModifiers::SHIFT, "Shift+"),
        ] {
            if self.mods.contains(flag) {
                f.write_str(name)?;
            }
        }
        match self.code {
            KeyCode::Char(' ') => f.write_str("Space"),
            KeyCode::Char(c) if c.is_ascii_uppercase() => {
                write!(f, "Shift+{}", c.to_ascii_lowercase())
            }
            KeyCode::Char(c) => write!(f, "{c}"),
            KeyCode::BackTab => f.write_str("Tab"),
            KeyCode::F(n) => write!(f, "F{n}"),
            code => f.write_str(name_of(code).unwrap_or("?")),
        }
    }
}

/// Everything a key can do, named as niri names them (with panes for
/// niri's windows).
#[derive(knus::Decode, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    NewColumn,
    FocusColumnLeft,
    FocusColumnRight,
    FocusColumnFirst,
    FocusColumnLast,
    MoveColumnLeft,
    MoveColumnRight,
    FocusPaneUp,
    FocusPaneDown,
    MovePaneUp,
    MovePaneDown,
    ConsumeOrExpelPaneLeft,
    ConsumeOrExpelPaneRight,
    ConsumePaneIntoColumn,
    ExpelPaneFromColumn,
    SwitchPresetColumnWidth,
    MaximizeColumn,
    FullscreenPane,
    CenterColumn,
    ClosePane,
    FocusWorkspaceDown,
    FocusWorkspaceUp,
    MoveColumnToWorkspaceDown,
    MoveColumnToWorkspaceUp,
    ToggleOverview,
    CloseOverview,
    ToggleThumbnails,
    Detach,
    KillServer,
}

/// What keys do in each mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bindings {
    /// The key that starts a `prefix` binding. Pressed twice, it's sent on
    /// to the pane.
    pub prefix: Key,
    /// Keys after the prefix.
    pub prefix_binds: Table,
    /// Keys that act straight away, outside the overview and in it.
    pub binds: Table,
    /// Keys while the overview is open, ahead of `binds`.
    pub overview_binds: Table,
}

/// Keys and their actions, in the order they were bound.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Table {
    map: HashMap<Key, Action>,
    order: Vec<Key>,
}

impl Table {
    pub fn get(&self, key: Key) -> Option<Action> {
        self.map.get(&key).copied()
    }

    /// Binds `key`, or with None unbinds it.
    pub fn set(&mut self, key: Key, action: Option<Action>) {
        if let Some(action) = action {
            if self.map.insert(key, action).is_none() {
                self.order.push(key);
            }
        } else {
            self.map.remove(&key);
            self.order.retain(|k| *k != key);
        }
    }

    /// The keys bound to `action`, in the order they were bound.
    pub fn keys_for(&self, action: Action) -> impl Iterator<Item = Key> + '_ {
        (self.order.iter().copied())
            .filter(move |k| self.map.get(k) == Some(&action))
    }

    /// The first key bound to each of `actions`, written together: run
    /// together if they're plain characters ("hjkl"), with slashes if not
    /// ("PgUp/PgDn"). None if any is unbound.
    pub fn hint_keys(&self, actions: &[Action]) -> Option<String> {
        let keys: Vec<Key> = (actions.iter())
            .map(|a| self.keys_for(*a).next())
            .collect::<Option<_>>()?;
        Some(hint_keys(&keys))
    }
}

/// `keys` written together for the status bar: "hjkl", "u/i", "Alt-h/l".
pub fn hint_keys(keys: &[Key]) -> String {
    let Some(first) = keys.first() else {
        return String::new();
    };
    let shared_mods = keys.iter().all(|k| k.mods == first.mods);
    let chars: Option<Vec<char>> = (keys.iter())
        .map(|k| Key::new(k.code, KeyModifiers::empty()).bare_char())
        .collect();
    match chars {
        Some(chars) if shared_mods && keys.len() > 2 => {
            first.short_mods() + &chars.into_iter().collect::<String>()
        }
        _ if shared_mods => {
            let names: Vec<String> =
                keys.iter().map(|k| k.short_name()).collect();
            first.short_mods() + &names.join("/")
        }
        _ => keys.iter().map(|k| k.short()).collect::<Vec<_>>().join("/"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> Key {
        s.parse().unwrap()
    }

    fn event(code: KeyCode, mods: KeyModifiers) -> Key {
        Key::from_event(KeyEvent::new(code, mods))
    }

    #[test]
    fn shifted_letters_are_capitals_however_written() {
        let capital = event(KeyCode::Char('H'), KeyModifiers::SHIFT);
        assert_eq!(key("Shift+h"), capital);
        assert_eq!(key("Shift+H"), capital);
        assert_eq!(key("H"), capital);
        assert_ne!(key("h"), capital);
        assert_eq!(
            key("Alt+Shift+h"),
            event(KeyCode::Char('H'), KeyModifiers::ALT | KeyModifiers::SHIFT)
        );
    }

    #[test]
    fn ctrl_letters_ignore_case() {
        assert_eq!(
            key("Ctrl+a"),
            event(KeyCode::Char('a'), KeyModifiers::CONTROL)
        );
        assert_eq!(key("Ctrl+A"), key("ctrl+a"));
    }

    #[test]
    fn ctrl_symbols_are_the_control_character_terminals_send() {
        // crossterm reports 0x1D, which Ctrl+] sends, as Ctrl+5.
        assert_eq!(
            key("Ctrl+]"),
            event(KeyCode::Char('5'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            key("Ctrl+\\"),
            event(KeyCode::Char('4'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            key("Ctrl+/"),
            event(KeyCode::Char('7'), KeyModifiers::CONTROL)
        );
        assert_eq!(key("Ctrl+/"), key("Ctrl+_"));
        assert_eq!(
            key("Ctrl+Space"),
            event(KeyCode::Char(' '), KeyModifiers::CONTROL)
        );
        assert_eq!(key("Ctrl+@"), key("Ctrl+Space"));
        assert_eq!(key("Ctrl+]").to_string(), "Ctrl+]");
        // These three arrive as other keys entirely.
        let err = |s: &str| s.parse::<Key>().unwrap_err();
        assert!(err("Ctrl+i").contains("bind Tab instead"));
        assert!(err("Ctrl+[").contains("bind Escape instead"));
    }

    #[test]
    fn ctrl_held_after_the_prefix() {
        assert_eq!(key("Ctrl+n").without_ctrl(), Some(key("n")));
        assert_eq!(key("Ctrl+]").without_ctrl(), Some(key("]")));
        assert_eq!(key("Tab").without_ctrl(), Some(key("i")));
        assert_eq!(key("Escape").without_ctrl(), Some(key("[")));
        assert_eq!(key("n").without_ctrl(), None);
        assert_eq!(key("Alt+Enter").without_ctrl(), None);
    }

    #[test]
    fn reads_named_keys_and_symbols() {
        assert_eq!(key("Alt+Enter"), event(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(key("escape"), event(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            key("Shift+Tab"),
            event(KeyCode::BackTab, KeyModifiers::SHIFT)
        );
        assert_eq!(key("$"), event(KeyCode::Char('$'), KeyModifiers::SHIFT));
        assert_eq!(
            key("Ctrl++"),
            event(KeyCode::Char('+'), KeyModifiers::CONTROL)
        );
        assert_eq!(key("F5"), event(KeyCode::F(5), KeyModifiers::NONE));
        assert_eq!(key("+"), event(KeyCode::Char('+'), KeyModifiers::SHIFT));
        assert_eq!(key("+").to_string(), "+");
        // Letters outside ASCII shift too.
        assert_eq!(
            key("Shift+é"),
            event(KeyCode::Char('É'), KeyModifiers::SHIFT)
        );
        assert_ne!(key("Shift+é"), key("é"));
    }

    #[test]
    fn explains_bad_keys() {
        let err = |s: &str| s.parse::<Key>().unwrap_err();
        assert!(err("Super+h").contains("isn't a modifier"));
        assert!(err("Ctrl+Hyper").contains("isn't a key name"));
        assert!(err("Shift+4").contains("$ for Shift+4"));
    }

    #[test]
    fn writes_keys_back_as_the_config_does() {
        for s in [
            "h",
            "Shift+h",
            "Ctrl+a",
            "Alt+Enter",
            "Shift+Tab",
            "Space",
            "F5",
            "[",
        ] {
            assert_eq!(key(s).to_string(), s);
        }
    }

    #[test]
    fn writes_hints_compactly() {
        let keys = |s: &[&str]| s.iter().map(|s| key(s)).collect::<Vec<_>>();
        assert_eq!(hint_keys(&keys(&["h", "j", "k", "l"])), "hjkl");
        assert_eq!(hint_keys(&keys(&["Shift+h", "Shift+l"])), "H/L");
        assert_eq!(hint_keys(&keys(&["Alt+h", "Alt+l"])), "Alt-h/l");
        assert_eq!(hint_keys(&keys(&["PageUp", "PageDown"])), "PgUp/PgDn");
        assert_eq!(hint_keys(&keys(&["Alt+Enter"])), "Alt-⏎");
        assert_eq!(hint_keys(&keys(&["Ctrl+a", "n"])), "C-a/n");
        assert_eq!(hint_keys(&keys(&["Space", "j", "k", "l"])), "Space/j/k/l");
    }

    #[test]
    fn tables_keep_binding_order_and_unbind() {
        let mut table = Table::default();
        table.set(key("n"), Some(Action::NewColumn));
        table.set(key("Enter"), Some(Action::NewColumn));
        table.set(key("x"), Some(Action::ClosePane));
        assert_eq!(
            table.keys_for(Action::NewColumn).collect::<Vec<_>>(),
            [key("n"), key("Enter")]
        );
        table.set(key("n"), None);
        assert_eq!(table.get(key("n")), None);
        assert_eq!(table.hint_keys(&[Action::NewColumn]).as_deref(), Some("⏎"));
        assert_eq!(table.hint_keys(&[Action::Detach]), None);
    }
}
