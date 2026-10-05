//! Rasterizing a terminal grid into a small RGBA image for the overview,
//! using a built-in 8x8 bitmap font. Thumbnails are viewed shrunk, so the
//! goal is the right shapes and colors rather than pretty text.

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color as TermColor, NamedColor};
use font8x8::UnicodeFonts as _;

/// Pixels per terminal cell. 1:2 is close to most terminals' cell shape, so
/// the terminal scales the image about evenly in both directions.
pub const CELL_WIDTH: usize = 8;
pub const CELL_HEIGHT: usize = 16;

/// Used for text in the default color. The default background is left
/// transparent so the outer terminal's own background shows through.
const DEFAULT_FG: [u8; 3] = [0xd8, 0xd8, 0xd8];
const DEFAULT_BG: [u8; 3] = [0x00, 0x00, 0x00];

/// Opacity of the smudge drawn for characters the font doesn't have.
const UNKNOWN_GLYPH_ALPHA: u8 = 0x90;

pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Image {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            rgba: vec![0; width as usize * height as usize * 4],
        }
    }

    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, rgb: [u8; 3], alpha: u8) {
        for py in y..(y + h).min(self.height as usize) {
            for px in x..(x + w).min(self.width as usize) {
                let i = (py * self.width as usize + px) * 4;
                self.rgba[i..i + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], alpha]);
            }
        }
    }
}

pub fn rasterize<T>(term: &Term<T>) -> Image {
    let (rows, cols) = (term.screen_lines(), term.columns());
    let mut image = Image::new((cols * CELL_WIDTH) as u32, (rows * CELL_HEIGHT) as u32);
    let colors = term.colors();
    for row in 0..rows {
        for col in 0..cols {
            let cell = &term.grid()[Point::new(Line(row as i32), Column(col))];
            draw_cell(
                &mut image,
                cell,
                colors,
                col * CELL_WIDTH,
                row * CELL_HEIGHT,
            );
        }
    }
    image
}

fn draw_cell(image: &mut Image, cell: &Cell, colors: &Colors, x: usize, y: usize) {
    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
        return;
    }
    let width = if cell.flags.contains(Flags::WIDE_CHAR) {
        2 * CELL_WIDTH
    } else {
        CELL_WIDTH
    };

    let mut fg = resolve(cell.fg, colors).unwrap_or(DEFAULT_FG);
    let mut bg = resolve(cell.bg, colors);
    if cell.flags.contains(Flags::INVERSE) {
        (fg, bg) = (bg.unwrap_or(DEFAULT_BG), Some(fg));
    }
    if cell.flags.contains(Flags::DIM) {
        fg = fg.map(|c| c / 2);
    }
    if let Some(bg) = bg {
        image.fill(x, y, width, CELL_HEIGHT, bg, 0xff);
    }
    if cell.flags.contains(Flags::HIDDEN) || cell.c.is_whitespace() || cell.c.is_control() {
        return;
    }

    if let Some(dots) = braille(cell.c) {
        // Braille is a 2x4 grid of dots; btop draws its graphs with it.
        for (bit, (dx, dy)) in BRAILLE_DOTS.iter().enumerate() {
            if dots & (1 << bit) != 0 {
                image.fill(x + 1 + dx * 4, y + 1 + dy * 4, 2, 2, fg, 0xff);
            }
        }
    } else if let Some(glyph) = glyph(cell.c) {
        // Stretch the 8x8 bitmap to fill the cell. Bit 0 is the leftmost pixel.
        // Strokes are drawn a pixel wider than the font has them: one-pixel
        // vertical lines vanish when the terminal shrinks the image.
        let sx = width / 8;
        let sy = CELL_HEIGHT / 8;
        for (gy, bits) in glyph.iter().enumerate() {
            for gx in 0..8 {
                if bits & (1 << gx) != 0 {
                    let w = (sx + 1).min(width - gx * sx);
                    image.fill(x + gx * sx, y + gy * sy, w, sy, fg, 0xff);
                }
            }
        }
    } else {
        image.fill(
            x + 1,
            y + 5,
            width - 2,
            CELL_HEIGHT - 8,
            fg,
            UNKNOWN_GLYPH_ALPHA,
        );
    }
}

/// Offsets (in 4px steps) of braille dots 1-8, indexed by bit.
const BRAILLE_DOTS: [(usize, usize); 8] = [
    (0, 0),
    (0, 1),
    (0, 2),
    (1, 0),
    (1, 1),
    (1, 2),
    (0, 3),
    (1, 3),
];

fn braille(c: char) -> Option<u8> {
    let offset = u32::from(c).checked_sub(0x2800)?;
    u8::try_from(offset).ok()
}

fn glyph(c: char) -> Option<[u8; 8]> {
    font8x8::BASIC_FONTS
        .get(c)
        .or_else(|| font8x8::BOX_FONTS.get(c))
        .or_else(|| font8x8::BLOCK_FONTS.get(c))
        .or_else(|| font8x8::LATIN_FONTS.get(c))
        .or_else(|| font8x8::GREEK_FONTS.get(c))
        .or_else(|| font8x8::MISC_FONTS.get(c))
        .or_else(|| font8x8::HIRAGANA_FONTS.get(c))
}

/// The RGB for a cell color, or None for the default background.
fn resolve(color: TermColor, colors: &Colors) -> Option<[u8; 3]> {
    let idx = match color {
        TermColor::Spec(c) => return Some([c.r, c.g, c.b]),
        TermColor::Indexed(i) => usize::from(i),
        TermColor::Named(n) => n as usize,
    };
    if let Some(c) = colors[idx] {
        return Some([c.r, c.g, c.b]);
    }
    match color {
        TermColor::Named(NamedColor::Background) => None,
        TermColor::Named(n) if (n as usize) < 16 => Some(xterm_color(n as u8)),
        TermColor::Named(n) if (NamedColor::DimBlack..=NamedColor::DimWhite).contains(&n) => {
            Some(xterm_color((n as usize - NamedColor::DimBlack as usize) as u8).map(|c| c / 2))
        }
        TermColor::Named(_) => Some(DEFAULT_FG),
        TermColor::Indexed(i) => Some(xterm_color(i)),
        TermColor::Spec(_) => unreachable!(),
    }
}

/// xterm's default 256-color palette.
fn xterm_color(i: u8) -> [u8; 3] {
    const ANSI: [[u8; 3]; 16] = [
        [0x00, 0x00, 0x00],
        [0xcd, 0x00, 0x00],
        [0x00, 0xcd, 0x00],
        [0xcd, 0xcd, 0x00],
        [0x00, 0x00, 0xee],
        [0xcd, 0x00, 0xcd],
        [0x00, 0xcd, 0xcd],
        [0xe5, 0xe5, 0xe5],
        [0x7f, 0x7f, 0x7f],
        [0xff, 0x00, 0x00],
        [0x00, 0xff, 0x00],
        [0xff, 0xff, 0x00],
        [0x5c, 0x5c, 0xff],
        [0xff, 0x00, 0xff],
        [0x00, 0xff, 0xff],
        [0xff, 0xff, 0xff],
    ];
    match i {
        0..16 => ANSI[usize::from(i)],
        16..232 => {
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            let i = i - 16;
            [level(i / 36), level(i / 6 % 6), level(i % 6)]
        }
        232.. => {
            let v = 8 + (i - 232) * 10;
            [v, v, v]
        }
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::{Config, test::TermSize};
    use alacritty_terminal::vte::ansi::Processor;

    use super::*;

    fn term_with(cols: usize, rows: usize, bytes: &[u8]) -> Term<VoidListener> {
        let mut term = Term::new(Config::default(), &TermSize::new(cols, rows), VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, bytes);
        term
    }

    fn pixel(image: &Image, x: usize, y: usize) -> [u8; 4] {
        let i = (y * image.width as usize + x) * 4;
        image.rgba[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn image_is_eight_by_sixteen_pixels_per_cell() {
        let image = rasterize(&term_with(10, 3, b""));
        assert_eq!((image.width, image.height), (80, 48));
        assert!(
            image.rgba.iter().all(|&b| b == 0),
            "blank grid is transparent"
        );
    }

    #[test]
    fn draws_glyphs_left_to_right_in_their_color() {
        // 'L' has its stem on the left and its foot along the bottom.
        let image = rasterize(&term_with(2, 1, b"\x1b[31mL"));
        let red = [0xcd, 0x00, 0x00, 0xff];
        assert_eq!(pixel(&image, 0, 0), red);
        assert_eq!(pixel(&image, 6, 12), red);
        assert_eq!(pixel(&image, 7, 0)[3], 0);
    }

    #[test]
    fn vertical_lines_are_thick_enough_to_survive_scaling() {
        let image = rasterize(&term_with(1, 1, "\u{2502}".as_bytes()));
        let row: Vec<u8> = (0..8).map(|x| pixel(&image, x, 8)[3]).collect();
        assert!(row.iter().filter(|&&a| a == 0xff).count() >= 2, "{row:?}");
    }

    #[test]
    fn backgrounds_fill_the_cell_and_default_stays_clear() {
        let image = rasterize(&term_with(2, 1, b"\x1b[48;2;1;2;3m \x1b[0m "));
        assert_eq!(pixel(&image, 3, 8), [1, 2, 3, 0xff]);
        assert_eq!(pixel(&image, 11, 8)[3], 0);
    }

    #[test]
    fn braille_dots_land_in_the_right_corners() {
        // U+2801 is dot 1 (top left); U+2880 is dot 8 (bottom right).
        let image = rasterize(&term_with(2, 1, "\u{2801}\u{2880}".as_bytes()));
        assert_eq!(pixel(&image, 1, 1)[3], 0xff);
        assert_eq!(pixel(&image, 8 + 5, 13)[3], 0xff);
        assert_eq!(pixel(&image, 8 + 1, 1)[3], 0);
    }

    #[test]
    fn xterm_palette_spot_checks() {
        assert_eq!(xterm_color(16), [0, 0, 0]);
        assert_eq!(xterm_color(196), [0xff, 0, 0]);
        assert_eq!(xterm_color(231), [0xff, 0xff, 0xff]);
        assert_eq!(xterm_color(255), [0xee, 0xee, 0xee]);
    }
}
