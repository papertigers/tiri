// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Drawing a pane's screen (or scrollback) into a frame.

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color as TermColor, NamedColor};

use crate::layout::PaneId;
use crate::pane::Pane;
use crate::render::{Color, Frame, Style};
use crate::selection::{Point, Selection};

/// The first line of `pane` to show in a box `h` rows high: the screen's
/// top, or the rows around the cursor if the box is shorter (as in the
/// overview), moved up by however far the client has scrolled back.
pub(super) fn content_top(pane: &Pane, h: i32, scrolled: usize) -> i32 {
    let (rows, _) = pane.size();
    let h = h.clamp(0, i32::from(rows)) as u16;
    let (cursor_row, _) = pane.cursor();
    let crop = (cursor_row + 1).saturating_sub(h).min(rows - h);
    i32::from(crop) - scrolled as i32
}

/// Copies part of a pane into a `w` x `h` box at (x, y), starting from
/// content line `top`, clipping at the frame's edges. Selected text is
/// drawn on `selection_bg`, or inverted if there's none.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_screen(
    frame: &mut Frame,
    pane: &Pane,
    id: PaneId,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    top: i32,
    selection: Option<&Selection>,
    selection_bg: Option<Color>,
) {
    let (rows, cols) = pane.size();
    let w = w.clamp(0, i32::from(cols)) as u16;
    let h = h.clamp(0, i32::from(rows));

    let mut sym = String::new();
    for row in 0..h {
        let line = top + row;
        let fy = y + row;
        for col in 0..w {
            let fx = x + i32::from(col);
            if fx < 0 || fx >= i32::from(frame.width()) || fy < 0 || fy >= i32::from(frame.height())
            {
                continue;
            }
            let cell = pane.cell(line, col);
            let mut style = cell_style(pane, cell);
            if selection.is_some_and(|s| s.contains(id, Point { line, col })) {
                match selection_bg {
                    Some(bg) => style.bg = bg,
                    None => style.inverse = !style.inverse,
                }
            }
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                // Its first half is drawn by the cell to the left, unless that
                // half was scrolled off the left edge.
                if fx == 0 {
                    frame.put(fx, fy, " ", style);
                }
                continue;
            }

            sym.clear();
            if cell
                .flags
                .intersects(Flags::HIDDEN | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                sym.push(' ');
            } else {
                sym.push(cell.c);
                sym.extend(cell.zerowidth().into_iter().flatten());
            }
            if !cell.flags.contains(Flags::WIDE_CHAR) {
                frame.put(fx, fy, &sym, style);
            } else if col + 1 < w {
                frame.put_wide(fx, fy, &sym, style);
            } else {
                frame.put(fx, fy, " ", style);
            }
        }
    }
}

fn cell_style(pane: &Pane, cell: &alacritty_terminal::term::cell::Cell) -> Style {
    let flags = cell.flags;
    Style {
        fg: term_color(pane, cell.fg),
        bg: term_color(pane, cell.bg),
        bold: flags.contains(Flags::BOLD),
        dim: flags.contains(Flags::DIM) || is_dim_named(cell.fg),
        italic: flags.contains(Flags::ITALIC),
        underline: flags.intersects(Flags::ALL_UNDERLINES),
        inverse: flags.contains(Flags::INVERSE),
        strikeout: flags.contains(Flags::STRIKEOUT),
    }
}

fn is_dim_named(color: TermColor) -> bool {
    matches!(color, TermColor::Named(n) if (NamedColor::DimBlack..=NamedColor::DimWhite).contains(&n))
}

/// Maps an emulator color to one the outer terminal understands. Colors an
/// app redefined (OSC 4/10/11) are sent as RGB; the rest are left to the
/// outer terminal's palette so tiri matches its theme.
fn term_color(pane: &Pane, color: TermColor) -> Color {
    let rgb = |c: alacritty_terminal::vte::ansi::Rgb| Color::Rgb(c.r, c.g, c.b);
    match color {
        TermColor::Spec(c) => rgb(c),
        TermColor::Indexed(i) => pane.palette(usize::from(i)).map_or(Color::Idx(i), rgb),
        TermColor::Named(n) => {
            if let Some(c) = pane.palette(n as usize) {
                return rgb(c);
            }
            let idx = n as usize;
            if idx < 16 {
                Color::Idx(idx as u8)
            } else if is_dim_named(color) {
                Color::Idx((idx - NamedColor::DimBlack as usize) as u8)
            } else {
                Color::Default
            }
        }
    }
}
