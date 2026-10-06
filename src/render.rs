// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A cell buffer for composing a frame, and a renderer that only sends the
//! cells that changed since the last frame.

use std::io::{self, Write};

use compact_str::CompactString;
use crossterm::{QueueableCommand, cursor, style, terminal};
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    /// The outer terminal's own foreground or background.
    #[default]
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub strikeout: bool,
    /// The color of underlines, as a program in a pane sets it. In a kitty
    /// placeholder cell, which isn't underlined, it names the image's
    /// placement instead.
    /// <https://sw.kovidgoyal.net/kitty/underlines/>
    pub underline_color: Color,
}

impl Style {
    pub fn fg(color: Color) -> Self {
        Self { fg: color, ..Self::default() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cell {
    /// Empty when this cell is covered by the wide character to its left.
    /// Nearly always one character, which is kept inline: a frame is tens
    /// of thousands of cells, rebuilt for every draw.
    sym: CompactString,
    wide: bool,
    style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            sym: CompactString::const_new(" "),
            wide: false,
            style: Style::default(),
        }
    }
}

#[derive(Clone)]
pub struct Frame {
    width: u16,
    height: u16,
    cells: Vec<Cell>,
}

impl Frame {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            cells: vec![
                Cell::default();
                usize::from(width) * usize::from(height)
            ],
        }
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    fn index(&self, x: i32, y: i32) -> Option<usize> {
        let (x, y) = (u16::try_from(x).ok()?, u16::try_from(y).ok()?);
        (x < self.width && y < self.height)
            .then(|| usize::from(y) * usize::from(self.width) + usize::from(x))
    }

    /// Writes one narrow cell; anything off-frame is clipped.
    pub fn put(&mut self, x: i32, y: i32, sym: &str, style: Style) {
        if let Some(i) = self.index(x, y) {
            self.split_wide(i);
            self.cells[i] =
                Cell { sym: printable(sym).into(), wide: false, style };
        }
    }

    /// Before cell `i` is overwritten, blanks the other half of any wide
    /// character it belongs to, as a real terminal would. Otherwise the frame
    /// describes a screen no terminal can show, and diffs against it go wrong.
    fn split_wide(&mut self, i: usize) {
        let row_start = i - i % usize::from(self.width);
        if self.cells[i].wide {
            self.cells[i + 1] =
                Cell { style: self.cells[i + 1].style, ..Cell::default() };
        } else if self.cells[i].sym.is_empty() && i > row_start {
            self.cells[i - 1] =
                Cell { style: self.cells[i - 1].style, ..Cell::default() };
        }
    }

    /// Writes a double-width character. If it would be cut by the right edge
    /// it is replaced with a blank.
    pub fn put_wide(&mut self, x: i32, y: i32, sym: &str, style: Style) {
        if self.index(x + 1, y).is_none() {
            self.put(x, y, " ", style);
            return;
        }
        if let Some(i) = self.index(x, y) {
            self.split_wide(i);
            self.split_wide(i + 1);
            self.cells[i] =
                Cell { sym: printable(sym).into(), wide: true, style };
            self.cells[i + 1] =
                Cell { sym: CompactString::default(), wide: false, style };
        }
    }

    /// The text at (`x`, `y`), whether it's a wide character, and its style.
    /// Text is empty for the cell a wide character covers.
    pub fn content(&self, x: u16, y: u16) -> (&str, bool, Style) {
        let cell = &self.cells
            [usize::from(y) * usize::from(self.width) + usize::from(x)];
        (&cell.sym, cell.wide, cell.style)
    }

    /// Writes a string left to right, each character taking the columns a
    /// terminal gives it: two for wide ones, and none for combining marks,
    /// which join the character before them.
    pub fn put_str(&mut self, x: i32, y: i32, s: &str, style: Style) {
        let mut col = x;
        // The cell the last character went into, for marks that follow it.
        let mut last: Option<usize> = None;
        for ch in s.chars() {
            let mut buf = [0u8; 4];
            let sym = &*ch.encode_utf8(&mut buf);
            match char_width(ch) {
                0 => {
                    if let Some(i) = last {
                        self.cells[i].sym.push(ch);
                    }
                }
                2 => {
                    self.put_wide(col, y, sym, style);
                    // Clipped by the right edge, it was drawn as a blank.
                    last = self.index(col, y).filter(|&i| self.cells[i].wide);
                    col += 2;
                }
                _ => {
                    self.put(col, y, sym, style);
                    last = self.index(col, y);
                    col += 1;
                }
            }
        }
    }
}

/// Whether printing `cell` moves the cursor exactly one column in any
/// terminal: a single narrow character from before the symbol blocks.
/// Those blocks on (U+2600 up) hold emoji and wide scripts, which terminals
/// size differently, as they do characters with marks or variation
/// selectors attached. Letters, line drawing and block shapes are before.
fn advances_one(cell: &Cell) -> bool {
    let mut chars = cell.sym.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) => {
            !cell.wide && ch < '\u{2600}' && ch.width() == Some(1)
        }
        _ => false,
    }
}

/// The columns `ch` takes in a terminal. Control characters count as one,
/// since they're drawn as a blank.
fn char_width(ch: char) -> usize {
    ch.width().unwrap_or(1)
}

/// The columns `s` takes when drawn with [`Frame::put_str`].
pub fn text_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// As much of the start of `s` as fits in `max` columns.
pub fn fit_width(s: &str, max: usize) -> &str {
    let mut width = 0;
    for (i, ch) in s.char_indices() {
        width += char_width(ch);
        if width > max {
            return &s[..i];
        }
    }
    s
}

/// Control characters would move the outer terminal's cursor behind the
/// renderer's back. Emulators can leave them in cells: alacritty marks where
/// a tab started with a literal `\t`.
fn printable(sym: &str) -> &str {
    // Nor may a cell be empty: that marks the covered half of a wide one.
    if sym.is_empty() || sym.chars().any(char::is_control) { " " } else { sym }
}

#[derive(Default)]
pub struct Renderer {
    prev: Option<Frame>,
}

impl Renderer {
    /// The last frame drawn, as the terminal shows it now.
    pub fn last_frame(&self) -> Option<&Frame> {
        self.prev.as_ref()
    }

    /// Forces the next frame to be drawn in full.
    pub fn invalidate(&mut self) {
        self.prev = None;
    }

    /// Draws `frame` as a diff against the last one. `escapes` (graphics
    /// commands and the like) go out inside the same synchronized update,
    /// so the terminal shows them and the frame's cells together.
    pub fn draw(
        &mut self,
        out: &mut impl Write,
        escapes: &[u8],
        frame: Frame,
        cursor_at: Option<(u16, u16)>,
    ) -> io::Result<()> {
        out.queue(terminal::BeginSynchronizedUpdate)?;
        out.write_all(escapes)?;
        out.queue(cursor::Hide)?;

        let prev = self
            .prev
            .take()
            .filter(|p| p.width == frame.width && p.height == frame.height);
        if prev.is_none() {
            out.queue(style::SetAttribute(style::Attribute::Reset))?;
            out.queue(terminal::Clear(terminal::ClearType::All))?;
        }

        let mut pos: Option<(u16, u16)> = None;
        let mut current: Option<Style> = None;
        for y in 0..frame.height {
            for x in 0..frame.width {
                let i =
                    usize::from(y) * usize::from(frame.width) + usize::from(x);
                let cell = &frame.cells[i];
                if cell.sym.is_empty()
                    || prev.as_ref().is_some_and(|p| p.cells[i] == *cell)
                {
                    continue;
                }
                if pos != Some((x, y)) {
                    out.queue(cursor::MoveTo(x, y))?;
                }
                if current != Some(cell.style) {
                    apply_style(out, cell.style)?;
                    current = Some(cell.style);
                }
                out.queue(style::Print(&cell.sym))?;
                // Where terminals may disagree on how far that moved the
                // cursor, the next cell says where it goes.
                pos = advances_one(cell).then_some((x + 1, y));
            }
        }

        out.queue(style::SetAttribute(style::Attribute::Reset))?;
        if let Some((x, y)) = cursor_at {
            out.queue(cursor::MoveTo(x, y))?;
            out.queue(cursor::Show)?;
        }
        out.queue(terminal::EndSynchronizedUpdate)?;
        out.flush()?;

        self.prev = Some(frame);
        Ok(())
    }
}

fn apply_style(out: &mut impl Write, s: Style) -> io::Result<()> {
    use style::{Attribute, SetAttribute};
    out.queue(SetAttribute(Attribute::Reset))?;
    set_color(out, 38, s.fg)?;
    set_color(out, 48, s.bg)?;
    set_color(out, 58, s.underline_color)?;
    for (on, attr) in [
        (s.bold, Attribute::Bold),
        (s.dim, Attribute::Dim),
        (s.italic, Attribute::Italic),
        (s.underline, Attribute::Underlined),
        (s.inverse, Attribute::Reverse),
        (s.strikeout, Attribute::CrossedOut),
    ] {
        if on {
            out.queue(SetAttribute(attr))?;
        }
    }
    Ok(())
}

/// Sets the foreground (`base` 38), background (48) or underline (58)
/// color, just after a reset. The default needs nothing, as the reset has
/// set it already.
///
/// Written here rather than with crossterm, which leaves colors out when
/// NO_COLOR is set: tiri passes on the colors programs in its panes chose,
/// and kitty placeholders need theirs to name their image.
fn set_color(out: &mut impl Write, base: u8, color: Color) -> io::Result<()> {
    match color {
        Color::Default => Ok(()),
        Color::Idx(i) => write!(out, "\x1b[{base};5;{i}m"),
        Color::Rgb(r, g, b) => write!(out, "\x1b[{base};2;{r};{g};{b}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styled(style: Style) -> String {
        let mut out = Vec::new();
        apply_style(&mut out, style).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn writes_colors_after_the_reset() {
        let out = styled(Style {
            fg: Color::Idx(3),
            bg: Color::Rgb(4, 5, 6),
            underline_color: Color::Rgb(1, 2, 3),
            ..Style::default()
        });
        assert_eq!(out, "\x1b[0m\x1b[38;5;3m\x1b[48;2;4;5;6m\x1b[58;2;1;2;3m");
    }

    #[test]
    fn default_colors_need_only_the_reset() {
        assert_eq!(styled(Style::default()), "\x1b[0m");
    }

    #[test]
    fn put_str_gives_characters_their_width() {
        let mut f = Frame::new(8, 1);
        f.put_str(0, 0, "a字e\u{301}b", Style::default());
        let syms: Vec<&str> = f.cells.iter().map(|c| c.sym.as_str()).collect();
        assert_eq!(syms, ["a", "字", "", "e\u{301}", "b", " ", " ", " "]);
        assert!(f.cells[1].wide);
        assert_eq!(text_width("a字e\u{301}b"), 5);
        assert_eq!(fit_width("a字e\u{301}b", 2), "a");
        assert_eq!(fit_width("a字e\u{301}b", 4), "a字e\u{301}");
        assert_eq!(fit_width("ab", 5), "ab");
    }

    #[test]
    fn the_cursor_is_placed_again_after_uncertain_widths() {
        let mut out = Vec::new();
        let mut f = Frame::new(12, 1);
        f.put_str(0, 0, "a─b☺c字de\u{301}f", Style::default());
        Renderer::default().draw(&mut out, &[], f, None).unwrap();
        let out = String::from_utf8(out).unwrap();
        // Letters and line drawing run together. After a symbol, a wide
        // character or one with a mark, the next cell is placed.
        assert!(
            out.contains("a─b☺\x1b[1;5Hc字\x1b[1;8Hde\u{301}\x1b[1;10Hf"),
            "{out:?}"
        );
    }

    #[test]
    fn put_clips_outside_frame() {
        let mut f = Frame::new(4, 2);
        f.put(-1, 0, "a", Style::default());
        f.put(4, 0, "b", Style::default());
        f.put(0, 2, "c", Style::default());
        assert!(f.cells.iter().all(|c| c.sym == " "));
    }

    #[test]
    fn wide_char_cut_by_right_edge_becomes_blank() {
        let mut f = Frame::new(4, 1);
        f.put_wide(3, 0, "字", Style::default());
        assert_eq!(f.cells[3].sym, " ");
        f.put_wide(1, 0, "字", Style::default());
        assert_eq!(f.cells[1].sym, "字");
        assert_eq!(f.cells[2].sym, "");
    }

    #[test]
    fn overwriting_half_a_wide_char_blanks_the_other_half() {
        let mut f = Frame::new(4, 1);
        f.put_wide(0, 0, "字", Style::default());
        f.put(1, 0, "a", Style::default());
        assert_eq!((f.cells[0].sym.as_str(), f.cells[0].wide), (" ", false));

        f.put_wide(1, 0, "界", Style::default());
        f.put(1, 0, "b", Style::default());
        assert_eq!(f.cells[2].sym, " ");
    }

    /// Renders a series of frames and replays the bytes through a terminal
    /// emulator standing in for the outer terminal. After every frame the
    /// emulator's screen must match the frame exactly; any drift between the
    /// renderer's idea of the screen and the real one shows up here.
    #[test]
    fn diffed_output_reproduces_every_frame() {
        use alacritty_terminal::Term;
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::index::{Column, Line, Point};
        use alacritty_terminal::term::{Config, cell::Flags, test::TermSize};
        use alacritty_terminal::vte::ansi::Processor;

        const W: u16 = 30;
        const H: u16 = 6;
        const GLYPHS: [&str; 9] =
            ["a", "b", "─", "│", "字", "界", " ", "é", "\t"];

        let mut term = Term::new(
            Config::default(),
            &TermSize::new(W.into(), H.into()),
            VoidListener,
        );
        let mut parser: Processor = Processor::new();
        let mut renderer = Renderer::default();

        // Small deterministic PRNG so failures reproduce.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rand = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };

        // A base pattern that shifts sideways each frame, like the strip
        // scrolling, with random overwrites on top.
        let base: Vec<&str> =
            (0..200).map(|_| GLYPHS[rand(GLYPHS.len())]).collect();
        for step in 0..300 {
            let mut frame = Frame::new(W, H);
            let shift = (step * 3) % 100;
            for y in 0..i32::from(H) {
                let mut x = 0;
                let mut i = shift + y as usize * 7;
                while x < i32::from(W) {
                    let g = base[i % base.len()];
                    i += 1;
                    if g == "字" || g == "界" {
                        frame.put_wide(x, y, g, Style::default());
                        x += 2;
                    } else {
                        frame.put(x, y, g, Style::default());
                        x += 1;
                    }
                }
            }
            for _ in 0..rand(6) {
                let (x, y) = (rand(W.into()) as i32, rand(H.into()) as i32);
                let g = GLYPHS[rand(GLYPHS.len())];
                if g == "字" || g == "界" {
                    frame.put_wide(x, y, g, Style::default());
                } else {
                    frame.put(x, y, g, Style::default());
                }
            }

            let expected: Vec<String> =
                (frame.cells.iter()).map(|c| c.sym.to_string()).collect();
            let mut out = Vec::new();
            renderer.draw(&mut out, &[], frame, None).unwrap();
            parser.advance(&mut term, &out);

            for y in 0..H {
                for x in 0..W {
                    let cell = &term.grid()[Point::new(
                        Line(i32::from(y)),
                        Column(usize::from(x)),
                    )];
                    let got = if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        String::new()
                    } else {
                        cell.c.to_string()
                    };
                    let want = &expected
                        [usize::from(y) * usize::from(W) + usize::from(x)];
                    assert_eq!(&got, want, "frame {step}, cell ({x}, {y})");
                }
            }
        }
    }
}
