//! A cell buffer for composing a frame, and a renderer that only sends the
//! cells that changed since the last frame.

use std::io::{self, Write};

use crossterm::{QueueableCommand, cursor, style, terminal};

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
}

impl Style {
    pub fn fg(color: Color) -> Self {
        Self {
            fg: color,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cell {
    /// Empty when this cell is covered by the wide character to its left.
    sym: String,
    wide: bool,
    style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            sym: " ".to_owned(),
            wide: false,
            style: Style::default(),
        }
    }
}

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
            cells: vec![Cell::default(); usize::from(width) * usize::from(height)],
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
            self.cells[i] = Cell {
                sym: printable(sym).to_owned(),
                wide: false,
                style,
            };
        }
    }

    /// Before cell `i` is overwritten, blanks the other half of any wide
    /// character it belongs to, as a real terminal would. Otherwise the frame
    /// describes a screen no terminal can show, and diffs against it go wrong.
    fn split_wide(&mut self, i: usize) {
        let row_start = i - i % usize::from(self.width);
        if self.cells[i].wide {
            self.cells[i + 1] = Cell {
                style: self.cells[i + 1].style,
                ..Cell::default()
            };
        } else if self.cells[i].sym.is_empty() && i > row_start {
            self.cells[i - 1] = Cell {
                style: self.cells[i - 1].style,
                ..Cell::default()
            };
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
            self.cells[i] = Cell {
                sym: printable(sym).to_owned(),
                wide: true,
                style,
            };
            self.cells[i + 1] = Cell {
                sym: String::new(),
                wide: false,
                style,
            };
        }
    }

    /// Writes a string of narrow characters left to right.
    pub fn put_str(&mut self, x: i32, y: i32, s: &str, style: Style) {
        for (i, ch) in s.chars().enumerate() {
            let mut buf = [0u8; 4];
            self.put(x + i as i32, y, ch.encode_utf8(&mut buf), style);
        }
    }
}

/// Control characters would move the outer terminal's cursor behind the
/// renderer's back. Emulators can leave them in cells: alacritty marks where
/// a tab started with a literal `\t`.
fn printable(sym: &str) -> &str {
    if sym.chars().any(char::is_control) {
        " "
    } else {
        sym
    }
}

#[derive(Default)]
pub struct Renderer {
    prev: Option<Frame>,
}

impl Renderer {
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
                let i = usize::from(y) * usize::from(frame.width) + usize::from(x);
                let cell = &frame.cells[i];
                if cell.sym.is_empty() || prev.as_ref().is_some_and(|p| p.cells[i] == *cell) {
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
                let advance = if cell.wide { 2 } else { 1 };
                pos = Some((x + advance, y));
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
    use style::{Attribute, SetAttribute, SetBackgroundColor, SetForegroundColor};
    out.queue(SetAttribute(Attribute::Reset))?;
    out.queue(SetForegroundColor(color(s.fg)))?;
    out.queue(SetBackgroundColor(color(s.bg)))?;
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

fn color(c: Color) -> style::Color {
    match c {
        Color::Default => style::Color::Reset,
        Color::Idx(i) => style::Color::AnsiValue(i),
        Color::Rgb(r, g, b) => style::Color::Rgb { r, g, b },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        const GLYPHS: [&str; 9] = ["a", "b", "─", "│", "字", "界", " ", "é", "\t"];

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
        let base: Vec<&str> = (0..200).map(|_| GLYPHS[rand(GLYPHS.len())]).collect();
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

            let expected: Vec<String> = frame.cells.iter().map(|c| c.sym.clone()).collect();
            let mut out = Vec::new();
            renderer.draw(&mut out, &[], frame, None).unwrap();
            parser.advance(&mut term, &out);

            for y in 0..H {
                for x in 0..W {
                    let cell = &term.grid()[Point::new(Line(i32::from(y)), Column(usize::from(x)))];
                    let got = if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        String::new()
                    } else {
                        cell.c.to_string()
                    };
                    let want = &expected[usize::from(y) * usize::from(W) + usize::from(x)];
                    assert_eq!(&got, want, "frame {step}, cell ({x}, {y})");
                }
            }
        }
    }
}
