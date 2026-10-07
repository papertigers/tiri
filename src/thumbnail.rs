// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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

use crate::colors::{ANSI_COLORS, Palette, Rgb};

/// The built-in font's glyphs are this many pixels square.
const GLYPH_SIZE: usize = 8;
/// Bytes in an RGBA pixel, and where its alpha is among them.
const BYTES_PER_PIXEL: usize = 4;
const ALPHA: usize = 3;
const OPAQUE: u8 = u8::MAX;
/// The cell shapes a real font has: at least half as tall as wide, and at
/// most this many times taller.
const TALLEST_CELL: u16 = 4;
/// Where braille starts, its dots then given by the code's bits.
const BRAILLE_BASE: u32 = 0x2800;
/// Braille's dots form a grid this many across and down.
const BRAILLE_COLUMNS: usize = 2;
const BRAILLE_ROWS: usize = 4;
/// Box drawing and block elements, which fill their cells' full height so
/// that lines join up.
const BOX_AND_BLOCKS: std::ops::RangeInclusive<char> = '\u{2500}'..='\u{259f}';
/// Where the smudge for a character the font lacks goes: from this far down
/// the cell and this much of its height, about where lowercase letters sit.
const SMUDGE_TOP: f64 = 5.0 / 16.0;
const SMUDGE_HEIGHT: f64 = 0.5;

/// A thumbnail cell's size in pixels, (width, height).
pub type CellSize = (usize, usize);

/// Cell size for terminals that don't say how big their cells are.
pub const DEFAULT_CELL: CellSize = (8, 16);

/// The thumbnail cell size for a terminal whose cells are `cell_pixels`
/// (width, height) pixels: exactly the same shape, as small as allows at
/// least 8 pixels across for the font. The terminal fits an image into its
/// placement without stretching it, so any difference in shape leaves a gap
/// that visibly closes when the overview hands back to live text.
///
/// Shapes that only reduce to something large (say 250×509) are rounded to
/// 8 pixels across instead, and sizes no font has are ignored.
pub fn cell_size_for(cell_pixels: Option<(u16, u16)>) -> CellSize {
    let plausible = |&(w, h): &(u16, u16)| {
        (1..=MAX_CELL_PIXELS).contains(&w)
            && (1..=MAX_CELL_PIXELS).contains(&h)
            && h >= w / 2
            && h <= w * TALLEST_CELL
    };
    let Some((w, h)) = cell_pixels.filter(plausible) else {
        return DEFAULT_CELL;
    };
    let gcd = |mut a: usize, mut b: usize| {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    };
    let (w, h) = (usize::from(w), usize::from(h));
    let g = gcd(w, h);
    let (w, h) = (w / g, h / g);
    if w > MAX_EXACT_WIDTH {
        return (GLYPH_SIZE, (GLYPH_SIZE * h + w / 2) / w);
    }
    let scale = GLYPH_SIZE.div_ceil(w);
    (w * scale, h * scale)
}

/// Larger than any real font's cell.
const MAX_CELL_PIXELS: u16 = 256;
/// The widest thumbnail cell kept at exactly the terminal's shape.
const MAX_EXACT_WIDTH: usize = 32;

/// Opacity of the smudge drawn for characters the font doesn't have.
const UNKNOWN_GLYPH_ALPHA: u8 = 0x90;

#[derive(Clone)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl Image {
    pub fn new(width: usize, height: usize) -> Self {
        Self { width, height, rgba: vec![0; width * height * BYTES_PER_PIXEL] }
    }

    /// The same image at `opacity` (0 to 1) of its own opacity. Thumbnails
    /// are transparent where the pane has its default background, so this
    /// fades them over the terminal's own background.
    pub fn with_opacity(&self, opacity: f32) -> Self {
        let opacity = opacity.clamp(0.0, 1.0);
        let mut faded = self.clone();
        let alphas = faded.rgba.iter_mut().skip(ALPHA).step_by(BYTES_PER_PIXEL);
        for alpha in alphas {
            *alpha = (f32::from(*alpha) * opacity).round() as u8;
        }
        faded
    }

    fn fill(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        rgb: Rgb,
        alpha: u8,
    ) {
        for py in y..(y + h).min(self.height) {
            for px in x..(x + w).min(self.width) {
                let i = (py * self.width + px) * BYTES_PER_PIXEL;
                let [r, g, b] = rgb;
                self.rgba[i..i + BYTES_PER_PIXEL]
                    .copy_from_slice(&[r, g, b, alpha]);
            }
        }
    }
}

/// The most pixels a thumbnail may have: 32 MB of RGBA.
const MAX_PIXELS: usize = 8 << 20;

/// The cell size to draw a `cols` x `rows` screen at, given the size the
/// terminal's cell shape asks for: that if the image stays within
/// [`MAX_PIXELS`], otherwise the nearest shape 8 pixels across. None if
/// even that would be too big.
fn fit_cell(cell_size: CellSize, cols: usize, rows: usize) -> Option<CellSize> {
    let (w, h) = cell_size;
    let rounded = (GLYPH_SIZE, ((GLYPH_SIZE * h + w / 2) / w).max(1));
    [cell_size, rounded].into_iter().find(|&(w, h)| {
        let pixels = (cols.checked_mul(w))
            .and_then(|x| x.checked_mul(rows.checked_mul(h)?));
        pixels.is_some_and(|p| p <= MAX_PIXELS)
    })
}

/// Draws `term`'s screen in `palette`, the colors of the terminal the
/// thumbnail is for, with cells of `cell_size` or smaller (see
/// [`fit_cell`]). The default background is left transparent so that
/// terminal's own background shows through. None if the screen is too big
/// for a thumbnail.
pub fn rasterize<T>(
    term: &Term<T>,
    palette: &Palette,
    cell_size: CellSize,
) -> Option<Image> {
    let (rows, cols) = (term.screen_lines(), term.columns());
    let cell_size = fit_cell(cell_size, cols, rows)?;
    let (cell_width, cell_height) = cell_size;
    let mut image = Image::new(cols * cell_width, rows * cell_height);
    let colors = term.colors();
    for row in 0..rows {
        for col in 0..cols {
            let cell = &term.grid()[Point::new(Line(row as i32), Column(col))];
            draw_cell(
                &mut image,
                cell,
                colors,
                palette,
                col * cell_width,
                row * cell_height,
                cell_size,
            );
        }
    }
    Some(image)
}

fn draw_cell(
    image: &mut Image,
    cell: &Cell,
    colors: &Colors,
    palette: &Palette,
    x: usize,
    y: usize,
    (cell_width, cell_height): CellSize,
) {
    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
        return;
    }
    let width = if cell.flags.contains(Flags::WIDE_CHAR) {
        2 * cell_width
    } else {
        cell_width
    };

    let mut fg =
        resolve(cell.fg, colors, palette).unwrap_or(palette.foreground);
    let mut bg = resolve(cell.bg, colors, palette);
    if cell.flags.contains(Flags::INVERSE) {
        (fg, bg) = (bg.unwrap_or(palette.background), Some(fg));
    }
    if cell.flags.contains(Flags::DIM) {
        fg = fg.map(|c| c / 2);
    }
    if let Some(bg) = bg {
        image.fill(x, y, width, cell_height, bg, OPAQUE);
    }
    if cell.flags.contains(Flags::HIDDEN)
        || cell.c.is_whitespace()
        || cell.c.is_control()
    {
        return;
    }

    if let Some(dots) = braille(cell.c) {
        // A grid of dots; btop draws its graphs with it. Each dot is half
        // its share of the cell, a quarter share in from the share's edge.
        let (across, down) = (BRAILLE_COLUMNS, BRAILLE_ROWS);
        let (dot_w, dot_h) = (
            (cell_width / (2 * across)).max(1),
            (cell_height / (2 * down)).max(1),
        );
        for (bit, (dx, dy)) in BRAILLE_DOTS.iter().enumerate() {
            if dots & (1 << bit) != 0 {
                let dot_x =
                    x + cell_width / (4 * across) + dx * cell_width / across;
                let dot_y =
                    y + cell_height / (4 * down) + dy * cell_height / down;
                image.fill(dot_x, dot_y, dot_w, dot_h, fg, OPAQUE);
            }
        }
    } else if let Some(glyph) = glyph(cell.c) {
        // Stretch the 8x8 bitmap across the cell (bit 0 is the leftmost
        // pixel), so lines run on unbroken into the next cell whatever its
        // width. Text keeps whole-pixel rows, centred, so letters keep
        // their shape; box drawing and blocks fill the full height so
        // vertical lines join up. Strokes stay as thin as the font has
        // them: the terminal draws its own text thin and anti-aliased, and
        // anything heavier reads as a brighter color when the overview
        // hands back to live text.
        let full_height = BOX_AND_BLOCKS.contains(&cell.c);
        let (top, height) = if full_height {
            (y, cell_height)
        } else {
            let rows = GLYPH_SIZE * (cell_height / GLYPH_SIZE).max(1);
            (y + (cell_height - rows) / 2, rows)
        };
        for py in 0..height {
            let bits = glyph[py * GLYPH_SIZE / height];
            for px in 0..width {
                if bits & (1 << (px * GLYPH_SIZE / width)) != 0 {
                    image.fill(x + px, top + py, 1, 1, fg, OPAQUE);
                }
            }
        }
    } else {
        // A pixel in from each side, so neighbors don't run together.
        let fraction = |f: f64| (cell_height as f64 * f) as usize;
        image.fill(
            x + 1,
            y + fraction(SMUDGE_TOP),
            width - 2,
            fraction(SMUDGE_HEIGHT),
            fg,
            UNKNOWN_GLYPH_ALPHA,
        );
    }
}

/// Offsets (in 4px steps) of braille dots 1-8, indexed by bit.
const BRAILLE_DOTS: [(usize, usize); 8] =
    [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (0, 3), (1, 3)];

fn braille(c: char) -> Option<u8> {
    let offset = u32::from(c).checked_sub(BRAILLE_BASE)?;
    u8::try_from(offset).ok()
}

fn glyph(c: char) -> Option<[u8; GLYPH_SIZE]> {
    font8x8::BASIC_FONTS
        .get(c)
        .or_else(|| font8x8::BOX_FONTS.get(c))
        .or_else(|| font8x8::BLOCK_FONTS.get(c))
        .or_else(|| font8x8::LATIN_FONTS.get(c))
        .or_else(|| font8x8::GREEK_FONTS.get(c))
        .or_else(|| font8x8::MISC_FONTS.get(c))
        .or_else(|| font8x8::HIRAGANA_FONTS.get(c))
}

/// The RGB for a cell color, or None for the default background. Colors
/// the program set itself (OSC 4/10/11) win over the palette.
fn resolve(
    color: TermColor,
    colors: &Colors,
    palette: &Palette,
) -> Option<Rgb> {
    // Where the program's own setting for it would be, and the palette's.
    let (idx, from_palette) = match color {
        TermColor::Spec(c) => return Some([c.r, c.g, c.b]),
        TermColor::Indexed(i) => (usize::from(i), Some(palette.indexed(i))),
        TermColor::Named(n) => {
            let from_palette = match n {
                NamedColor::Background => None,
                n if (n as usize) < ANSI_COLORS => {
                    Some(palette.indexed(n as u8))
                }
                n if (NamedColor::DimBlack..=NamedColor::DimWhite)
                    .contains(&n) =>
                {
                    let base =
                        (n as usize - NamedColor::DimBlack as usize) as u8;
                    Some(palette.indexed(base).map(|c| c / 2))
                }
                _ => Some(palette.foreground),
            };
            (n as usize, from_palette)
        }
    };
    colors[idx].map(|c| [c.r, c.g, c.b]).or(from_palette)
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::{Config, test::TermSize};
    use alacritty_terminal::vte::ansi::Processor;

    use super::*;

    fn term_with(cols: usize, rows: usize, bytes: &[u8]) -> Term<VoidListener> {
        let mut term = Term::new(
            Config::default(),
            &TermSize::new(cols, rows),
            VoidListener,
        );
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, bytes);
        term
    }

    fn rasterize_default<T>(term: &Term<T>) -> Image {
        rasterize(term, &Palette::default(), DEFAULT_CELL)
            .expect("small enough")
    }

    #[test]
    fn big_screens_get_smaller_cells_or_no_thumbnail() {
        // An ordinary pane keeps the exact shape.
        assert_eq!(fit_cell((17, 41), 80, 24), Some((17, 41)));
        // A big one at a fine shape is rounded to 8 across.
        assert_eq!(fit_cell((17, 41), 250, 70), Some((8, 19)));
        assert_eq!(fit_cell((8, 17), 370, 87), Some((8, 17)));
        // The biggest screen a client may claim gets none.
        assert_eq!(fit_cell((32, 127), 1000, 500), None);
        assert_eq!(fit_cell((8, 16), 1000, 500), None);
    }

    #[test]
    fn cells_take_the_terminals_exact_shape() {
        assert_eq!(cell_size_for(None), (8, 16));
        assert_eq!(cell_size_for(Some((18, 40))), (9, 20));
        assert_eq!(cell_size_for(Some((16, 34))), (8, 17));
        assert_eq!(cell_size_for(Some((10, 10))), (8, 8));
        assert_eq!(cell_size_for(Some((17, 41))), (17, 41));
        assert_eq!(cell_size_for(Some((6, 13))), (12, 26));
        assert_eq!(cell_size_for(Some((0, 20))), (8, 16));
        // Shapes too fine to keep exactly, and sizes no font has.
        assert_eq!(cell_size_for(Some((250, 509))), (8, 16));
        assert_eq!(cell_size_for(Some((255, 1))), (8, 16));
        assert_eq!(cell_size_for(Some((1000, 2000))), (8, 16));
        let image =
            rasterize(&term_with(3, 2, b""), &Palette::default(), (9, 20))
                .unwrap();
        assert_eq!((image.width, image.height), (27, 40));
    }

    #[test]
    fn lines_run_unbroken_across_wider_cells() {
        // A horizontal line in a 9-pixel cell covers all 9 columns.
        let image = rasterize(
            &term_with(2, 1, "\u{2500}\u{2500}".as_bytes()),
            &Palette::default(),
            (9, 20),
        )
        .unwrap();
        let lit = (0..18)
            .filter(|&x| (0..20).any(|y| pixel(&image, x, y)[3] == 0xff))
            .count();
        assert_eq!(lit, 18);
    }

    #[test]
    fn draws_in_the_terminals_own_colors() {
        let reported = crate::colors::ReportedColors {
            foreground: Some([1, 2, 3]),
            ..Default::default()
        };
        let image = rasterize(
            &term_with(1, 1, b"L"),
            &Palette::from_reported(&reported),
            DEFAULT_CELL,
        )
        .unwrap();
        assert_eq!(pixel(&image, 0, 0), [1, 2, 3, 0xff]);
    }

    fn pixel(image: &Image, x: usize, y: usize) -> [u8; 4] {
        let i = (y * image.width + x) * 4;
        image.rgba[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn image_is_eight_by_sixteen_pixels_per_cell() {
        let image = rasterize_default(&term_with(10, 3, b""));
        assert_eq!((image.width, image.height), (80, 48));
        assert!(
            image.rgba.iter().all(|&b| b == 0),
            "blank grid is transparent"
        );
    }

    #[test]
    fn draws_glyphs_left_to_right_in_their_color() {
        // 'L' has its stem on the left and its foot along the bottom.
        let image = rasterize_default(&term_with(2, 1, b"\x1b[31mL"));
        let red = [0xcd, 0x00, 0x00, 0xff];
        assert_eq!(pixel(&image, 0, 0), red);
        assert_eq!(pixel(&image, 6, 12), red);
        assert_eq!(pixel(&image, 7, 0)[3], 0);
    }

    #[test]
    fn strokes_are_as_thin_as_the_font() {
        // The font's vertical line is a single pixel column.
        let image = rasterize_default(&term_with(1, 1, "\u{2502}".as_bytes()));
        let row: Vec<u8> = (0..8).map(|x| pixel(&image, x, 8)[3]).collect();
        assert_eq!(row.iter().filter(|&&a| a == 0xff).count(), 1, "{row:?}");
    }

    #[test]
    fn backgrounds_fill_the_cell_and_default_stays_clear() {
        let image =
            rasterize_default(&term_with(2, 1, b"\x1b[48;2;1;2;3m \x1b[0m "));
        assert_eq!(pixel(&image, 3, 8), [1, 2, 3, 0xff]);
        assert_eq!(pixel(&image, 11, 8)[3], 0);
    }

    #[test]
    fn braille_dots_land_in_the_right_corners() {
        // U+2801 is dot 1 (top left); U+2880 is dot 8 (bottom right).
        let image =
            rasterize_default(&term_with(2, 1, "\u{2801}\u{2880}".as_bytes()));
        assert_eq!(pixel(&image, 1, 1)[3], 0xff);
        assert_eq!(pixel(&image, 8 + 5, 13)[3], 0xff);
        assert_eq!(pixel(&image, 8 + 1, 1)[3], 0);
    }

    #[test]
    fn fading_scales_only_opacity() {
        let image =
            rasterize_default(&term_with(1, 1, b"\x1b[48;2;10;20;30m "));
        let half = image.with_opacity(0.5);
        assert_eq!(pixel(&half, 3, 8), [10, 20, 30, 128]);
        assert_eq!(pixel(&image.with_opacity(0.0), 3, 8)[3], 0);
        assert_eq!(pixel(&image.with_opacity(1.0), 3, 8), pixel(&image, 3, 8));
    }
}
