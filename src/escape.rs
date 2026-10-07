// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Terminal control sequences, named once for the whole crate: the
//! introducers that start them, the modes and codes tiri sets, and the
//! queries it asks and the reports that answer them. Names follow xterm's
//! control sequence documentation.
//!
//! The introducers are macros as well as constants, so that tables of fixed
//! sequences, such as key encodings, can be built from them as literals.

use std::io::Write as _;

/// A control sequence: CSI, then `parts`, as one literal.
macro_rules! csi {
    ($($part:literal),* $(,)?) => { concat!("\x1b[", $($part),*) };
}

/// An operating system command: OSC, then `parts`, as one literal.
macro_rules! osc {
    ($($part:literal),* $(,)?) => { concat!("\x1b]", $($part),*) };
}

/// An application program command: APC, then `parts`, as one literal.
macro_rules! apc {
    ($($part:literal),* $(,)?) => { concat!("\x1b_", $($part),*) };
}

/// A single shift 3 sequence: SS3, then `parts`, as one literal.
macro_rules! ss3 {
    ($($part:literal),* $(,)?) => { concat!("\x1bO", $($part),*) };
}

pub(crate) use {apc, csi};

/// Escape, which starts every sequence; alone, what the Escape key sends.
pub const ESC: u8 = 0x1b;
/// Delete, what the Backspace key sends.
pub const DEL: u8 = 0x7f;
/// Bell, which can end an OSC sequence instead of [`ST`].
pub const BEL: char = '\x07';
/// Control sequence introducer.
pub const CSI: &str = csi!();
/// Operating system command.
pub const OSC: &str = osc!();
/// Single shift 3, which starts cursor keys in application mode.
pub const SS3: &str = ss3!();
/// String terminator, which ends OSC and APC sequences.
pub const ST: &str = "\x1b\\";

/// Ctrl with a key sends that key's code with all but these bits cleared.
pub const CONTROL_MASK: u8 = 0x1f;

/// What a program in bracketed paste mode reads before a paste.
pub const PASTE_START: &str = csi!("200~");
/// What a program in bracketed paste mode reads after a paste. A paste that
/// contains it would end early, and the rest would arrive as if typed.
pub const PASTE_END: &str = csi!("201~");

/// OSC numbers.
pub mod osc_code {
    /// The default foreground color, as `rgb:…`, or `?` to ask for it.
    pub const FOREGROUND: u16 = 10;
    /// The default background color, likewise.
    pub const BACKGROUND: u16 = 11;
    /// A palette color: its index, then the color or `?`.
    pub const PALETTE: u16 = 4;
    /// Sets a selection: which (`c`, the clipboard), then base64 text.
    pub const CLIPBOARD: u16 = 52;
}

/// Sets the clipboard to `base64`-encoded text, with OSC 52.
pub fn set_clipboard(out: &mut Vec<u8>, base64: &str) {
    let clipboard = osc_code::CLIPBOARD;
    write!(out, "{OSC}{clipboard};c;{base64}{BEL}")
        .expect("writing to memory can't fail");
}

/// Asks for the color OSC `code` sets: [`osc_code::FOREGROUND`] or
/// [`osc_code::BACKGROUND`].
pub fn query_color(
    out: &mut impl std::io::Write,
    code: u16,
) -> std::io::Result<()> {
    write!(out, "{OSC}{code};?{ST}")
}

/// Asks for palette color `index`.
pub fn query_palette(
    out: &mut impl std::io::Write,
    index: usize,
) -> std::io::Result<()> {
    let palette = osc_code::PALETTE;
    write!(out, "{OSC}{palette};{index};?{ST}")
}

/// Asks for a cell's size in pixels (XTWINOPS 16), answered by a
/// [`report::CELL_SIZE`] window report.
pub const QUERY_CELL_SIZE: &str = csi!("16t");
/// Asks for the text area's size in pixels (XTWINOPS 14), answered by a
/// [`report::TEXT_AREA_SIZE`] window report.
pub const QUERY_TEXT_AREA_SIZE: &str = csi!("14t");
/// Asks for the primary device attributes, which every terminal answers.
pub const QUERY_DEVICE_ATTRIBUTES: &str = csi!("c");

/// What terminals answer with.
pub mod report {
    /// The final character of a window report: CSI kind ; height ; width t.
    pub const WINDOW_FINAL: char = 't';
    /// A window report's kind: a cell's size in pixels.
    pub const CELL_SIZE: u16 = 6;
    /// A window report's kind: the text area's size in pixels.
    pub const TEXT_AREA_SIZE: u16 = 4;
    /// How the primary device attributes report starts: CSI ? …
    pub const DEVICE_ATTRIBUTES: &str = csi!("?");
    /// How the primary device attributes report ends.
    pub const DEVICE_ATTRIBUTES_FINAL: u8 = b'c';
    /// How a color report gives its color: `rgb:` then hex channels.
    pub const RGB_PREFIX: &str = "rgb:";
}

/// Select graphic rendition: the codes that set how text looks, sent as
/// CSI codes separated by `;`, then [`sgr::FINAL`].
pub mod sgr {
    pub const BOLD: u8 = 1;
    pub const DIM: u8 = 2;
    pub const ITALIC: u8 = 3;
    pub const UNDERLINE: u8 = 4;
    pub const INVERSE: u8 = 7;
    pub const STRIKEOUT: u8 = 9;
    /// Neither bold nor dim: one code turns off both.
    pub const NORMAL_INTENSITY: u8 = 22;
    pub const NOT_ITALIC: u8 = 23;
    pub const NOT_UNDERLINED: u8 = 24;
    pub const NOT_INVERSE: u8 = 27;
    pub const NOT_STRIKEOUT: u8 = 29;
    /// A foreground color follows: [`INDEXED`] or [`RGB`], then its value.
    pub const FOREGROUND: u8 = 38;
    pub const DEFAULT_FOREGROUND: u8 = 39;
    /// A background color follows, likewise.
    pub const BACKGROUND: u8 = 48;
    pub const DEFAULT_BACKGROUND: u8 = 49;
    /// An underline color follows, likewise.
    pub const UNDERLINE_COLOR: u8 = 58;
    pub const DEFAULT_UNDERLINE_COLOR: u8 = 59;
    /// After a color code: a palette index follows.
    pub const INDEXED: u8 = 5;
    /// After a color code: red, green and blue follow.
    pub const RGB: u8 = 2;
    /// The final character of an SGR sequence.
    pub const FINAL: char = 'm';
}
