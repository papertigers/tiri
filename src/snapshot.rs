// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Snapshots of a pane's terminal: output that rebuilds it in a fresh
//! emulator of the same size. Not just what's on screen and in recent
//! history, but the state that decides where later output lands: the
//! cursor and its pen, the scroll region, character sets, modes, colors a
//! program set. A client starts its copy of a pane from one, then keeps it
//! in step by feeding it the same output the server's gets.
//!
//! Two pieces of that state are private to alacritty's emulator, so a
//! [`Tracker`] watches the output for them alongside it.
//!
//! Not carried: tab stops a program set, which are rare and also private;
//! links (OSC 8), which tiri doesn't show; the keyboard modes and the stack
//! of window titles, which only the server, answering the program, needs;
//! and, when the cursor is below the scroll region in origin mode, the
//! saved cursor, which becomes the cursor (see [`snapshot`]).

use std::io::Write as _;

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Cursor, Dimensions, Grid};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{
    CharsetIndex, Color, CursorShape, NamedColor, StandardCharset,
};
use alacritty_terminal::vte::{Params, Parser, Perform};

use crate::colors::ANSI_COLORS;
use crate::escape::{
    CHARSET_ASCII, CHARSET_LINE_DRAWING, CLEAR_TO_LINE_END, CSI, CURSOR_COLUMN,
    CURSOR_DOWN, CURSOR_POSITION, CURSOR_STYLE, CURSOR_UP, DELETE_CHARACTERS,
    DESIGNATE, ERASE_CHARACTERS, ESC, INSERT_CHARACTERS, KEYPAD_APPLICATION,
    KEYPAD_NUMERIC, OSC, RESET, RESET_SCROLL_REGION, RESTORE_CURSOR,
    SAVE_CURSOR, SET_SCROLL_REGION, SHIFT_IN, SHIFT_OUT, ST, ansi_mode,
    cursor_shape, decrst, decset, mode, osc_code, set_ansi_mode, sgr,
};

/// Watches a pane's output for the state alacritty keeps to itself: the
/// scroll region and which character set is active. Fed the same bytes as
/// the emulator, it agrees with it.
pub struct Tracker {
    parser: Parser,
    hidden: Hidden,
}

/// The emulator's state a snapshot can't read from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hidden {
    /// Rows the scroll region covers, from the top: the start, and one past
    /// the end. All of them unless a program set it.
    region: (usize, usize),
    /// Which of the four character sets output is mapped through.
    charset: CharsetIndex,
    /// The screen's height, which bounds the region.
    rows: usize,
}

impl Tracker {
    pub fn new(rows: usize) -> Self {
        Self { parser: Parser::new(), hidden: Hidden::new(rows) }
    }

    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.hidden, bytes);
    }

    /// The screen changed height, which resets the region, as the emulator
    /// does.
    pub fn resize(&mut self, rows: usize) {
        self.hidden.rows = rows;
        self.hidden.region = (0, rows);
    }

    pub fn hidden(&self) -> &Hidden {
        &self.hidden
    }
}

impl Hidden {
    fn new(rows: usize) -> Self {
        Self { region: (0, rows), charset: CharsetIndex::G0, rows }
    }
}

impl Perform for Hidden {
    fn execute(&mut self, byte: u8) {
        match byte {
            SHIFT_OUT => self.charset = CharsetIndex::G1,
            SHIFT_IN => self.charset = CharsetIndex::G0,
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if action != SET_SCROLL_REGION || !intermediates.is_empty() || ignore {
            return;
        }
        // As alacritty reads it: a missing or zero top is the first row, a
        // missing or zero bottom the last, and an empty region is ignored.
        let mut params = params.iter().map(|p| p.first().copied().unwrap_or(0));
        let top = params.next().filter(|&p| p != 0).unwrap_or(1) as usize;
        let bottom =
            params.next().filter(|&p| p != 0).map_or(self.rows, usize::from);
        if top >= bottom {
            return;
        }
        self.region = ((top - 1).min(self.rows), bottom.min(self.rows));
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if byte == RESET && intermediates.is_empty() {
            *self = Self::new(self.rows);
        }
    }
}

/// Output that rebuilds `term` in a fresh emulator of its size, with up to
/// `history` lines of its history. `hidden` is its [`Tracker`]'s.
///
/// A full-screen program's alternate screen hides the normal one and its
/// history, which the emulator only shows by switching to it. So this
/// switches there and back, leaving `term` as it found it.
pub fn snapshot<T: EventListener>(
    term: &mut Term<T>,
    hidden: &Hidden,
    history: usize,
) -> Vec<u8> {
    let mut out = Vec::new();
    // Whatever the copy held before.
    out.extend_from_slice(&[ESC, RESET]);

    // The saved cursors first, on a blank screen: saving one keeps its
    // place, not what's there, and the contents written after can't be
    // spoiled by setting them. Each screen has its own. The alternate one
    // is cleared each time a program switches to it, but keeps its saved
    // cursor from the time before.
    let in_alternate = term.mode().contains(TermMode::ALT_SCREEN);
    let (normal_saved, alternate_saved) = if in_alternate {
        (None, term.grid().saved_cursor.clone())
    } else {
        // Read there, putting back the normal screen's, which switching
        // overwrites.
        let saved = term.grid().saved_cursor.clone();
        term.swap_alt();
        let alternate_saved = term.grid().saved_cursor.clone();
        term.swap_alt();
        term.grid_mut().saved_cursor = saved.clone();
        (Some(saved), alternate_saved)
    };
    decset(&mut out, mode::ALT_SCREEN);
    save_cursor(&mut out, &alternate_saved, true);
    decrst(&mut out, mode::ALT_SCREEN);
    // Switching to the alternate screen saves the normal screen's cursor,
    // so in a full-screen program its saved cursor is its cursor, set
    // below.
    if let Some(saved) = &normal_saved {
        save_cursor(&mut out, saved, true);
    }
    goto(&mut out, 0, 0);
    set_pen(&mut out, &Cell::default());
    set_charsets(&mut out, &Cursor::default());

    if in_alternate {
        let alternate = term.grid().clone();
        term.swap_alt();
        write_lines(&mut out, term.grid(), history);
        // Where switching to the alternate screen saves, to come back to.
        // Wrapping is still on here, as in any fresh terminal.
        place_cursor(&mut out, &term.grid().cursor, 0, true);
        set_pen(&mut out, &term.grid().cursor.template);
        set_charsets(&mut out, &term.grid().cursor);
        term.swap_alt();
        *term.grid_mut() = alternate;

        decset(&mut out, mode::ALT_SCREEN);
        // The switch keeps the normal screen's cursor, pen and character
        // sets; its contents are written from the top, in plain ones.
        goto(&mut out, 0, 0);
        set_pen(&mut out, &Cell::default());
        set_charsets(&mut out, &Cursor::default());
        write_lines(&mut out, term.grid(), 0);
    } else {
        write_lines(&mut out, term.grid(), history);
    }

    let (top, bottom) = hidden.region;
    let origin = term.mode().contains(TermMode::ORIGIN);
    let cursor = term.grid().cursor.clone();
    let line = cursor.point.line.0 as usize;
    // In origin mode, moving the cursor keeps it inside the scroll region;
    // only restoring a saved cursor puts it below. Saved here, it's
    // restored once the mode's on, at the cost of the saved cursor, which
    // is then this one. (A program can only leave the two apart by
    // restoring a cursor outside its region, then writing along that row.)
    let below = origin && line >= bottom;
    if below {
        save_cursor(&mut out, &cursor, true);
    }

    set_modes(&mut out, *term.mode());
    set_colors(&mut out, term);
    set_cursor_style(&mut out, term);

    if (top, bottom) != (0, hidden.rows) {
        // Rows count from 1 here. A region that reaches the last row may
        // have got there by being clamped, and be as little as a row, which
        // asking for afresh would be refused: past the end gets the same.
        let first = top + 1;
        let last =
            if bottom == hidden.rows { bottom.max(first) + 1 } else { bottom };
        write!(out, "{CSI}{first};{last}{SET_SCROLL_REGION}")
            .expect("writing to memory can't fail");
    }
    if origin {
        decset(&mut out, mode::ORIGIN);
    }
    let wraps = term.mode().contains(TermMode::LINE_WRAP);
    if below {
        out.extend_from_slice(&[ESC, RESTORE_CURSOR]);
    } else if origin && line < top {
        // Above the region, which origin mode's moves can still reach:
        // moving up counts the region's top a second time, from the top.
        goto(&mut out, 0, cursor.point.column.0);
        write!(out, "{CSI}{}{CURSOR_UP}", 2 * top - line)
            .expect("writing to memory can't fail");
        restore_pending_wrap(&mut out, &cursor, wraps);
    } else {
        let top = if origin { top } else { 0 };
        place_cursor(&mut out, &cursor, top, wraps);
    }
    set_pen(&mut out, &cursor.template);
    set_charsets(&mut out, &cursor);
    out.push(match hidden.charset {
        CharsetIndex::G1 => SHIFT_OUT,
        _ => SHIFT_IN,
    });
    // Last, since they change how text is written: insert would push the
    // cell written above along.
    let modes = *term.mode();
    set_ansi_mode(
        &mut out,
        ansi_mode::INSERT,
        modes.contains(TermMode::INSERT),
    );
    set_ansi_mode(
        &mut out,
        ansi_mode::NEWLINE,
        modes.contains(TermMode::LINE_FEED_NEW_LINE),
    );
    out
}

/// A wide character written, then erased, to leave the spacer cells only
/// a wide character can: before a wide character that didn't fit at the
/// end of a line, and after one since erased.
const FILLER: char = '\u{3000}';
/// The columns [`FILLER`] takes.
const FILLER_WIDTH: usize = 2;

/// Writes `grid`'s screen, after up to `history` lines of its history,
/// from the top of a fresh screen: the earliest lines scroll off into the
/// copy's history as the later ones are written.
fn write_lines(out: &mut Vec<u8>, grid: &Grid<Cell>, history: usize) {
    let (rows, columns) = (grid.screen_lines(), grid.columns());
    let history = grid.history_size().min(history);
    let lines = -(history as i32)..rows as i32;
    let last = lines.end - 1;
    let mut out = Writer { out, pen: Cell::default(), columns };
    // The line before wrapped onto this one.
    let mut after_wrap = false;
    // A filler wrapped onto this line's start, to be erased first.
    let mut filler_here = false;
    for line in lines {
        let row = &grid[Line(line)];
        // A wrapped line is written to its end, so the next one wraps on
        // from it as it did.
        let wrapped = row[Column(columns - 1)].flags.contains(Flags::WRAPLINE);
        let end = if wrapped {
            columns
        } else {
            (0..columns)
                .rev()
                .find(|&c| !is_blank(&row[Column(c)]))
                .map_or(0, |c| c + 1)
        };
        // Cells writing can't leave where they are, put in place first, on
        // the blank line, where moving cells along disturbs nothing: a wide
        // character in the last column, with no room for its spacer; and
        // spacers whose wide characters were erased since.
        let last_column = columns - 1;
        let lone_last = columns > 1
            && row[Column(last_column)].flags.contains(Flags::WIDE_CHAR);
        let orphans: Vec<usize> =
            (0..end).filter(|&c| is_orphan_spacer(row, c)).collect();
        // On the last row, a wide character that didn't fit left its spacer
        // and wrapped without scrolling, as it does when the scroll region
        // ends above it: a filler likewise, onto the start of the same row,
        // where it's erased before the rest is written.
        let leading_last = line == last
            && rows >= MIN_ROWS_FOR_LAST_WRAP
            && row[Column(last_column)]
                .flags
                .contains(Flags::LEADING_WIDE_CHAR_SPACER);
        if filler_here {
            out.pen(&Cell::default());
            out.go_to_column(0);
            out.erase(FILLER_WIDTH);
        } else if after_wrap
            && (end == 0 || lone_last || leading_last || !orphans.is_empty())
        {
            // Writing is what wraps onto a line, and a tab, which wraps
            // without writing: onto this one, with nothing to write here or
            // something to put in place first.
            out.out.push(b'\t');
        }
        (after_wrap, filler_here) = (wrapped, false);
        if orphans.first() == Some(&0) {
            // A filler, its first column deleted: the line pulled back.
            out.pen(&row[Column(0)]);
            out.go_to_column(0);
            out.write(FILLER);
            out.go_to_column(0);
            write!(out.out, "{CSI}1{DELETE_CHARACTERS}")
                .expect("writing to memory can't fail");
        }
        if lone_last {
            // Written a column early, then pushed along by a blank inserted
            // before it, its spacer off the end.
            let cell = &row[Column(last_column)];
            out.pen(cell);
            out.go_to_column(last_column - 1);
            write_cell(out.out, cell);
            out.go_to_column(last_column - 1);
            write!(out.out, "{CSI}1{INSERT_CHARACTERS}")
                .expect("writing to memory can't fail");
        }
        // From the right, so each touches only the column before it, not
        // yet put in place.
        for &column in orphans.iter().rev().filter(|&&c| c > 0) {
            // A filler the column before, erased there.
            out.pen(&row[Column(column)]);
            out.go_to_column(column - 1);
            out.write(FILLER);
            out.go_to_column(column - 1);
            out.erase(1);
        }
        if lone_last || !orphans.is_empty() {
            out.go_to_column(0);
        }
        if leading_last {
            write!(out.out, "{CSI}1;{}{SET_SCROLL_REGION}", rows - 1)
                .expect("writing to memory can't fail");
            goto(out.out, rows - 1, last_column);
            out.pen(&row[Column(last_column)]);
            out.write(FILLER);
            out.out.extend_from_slice(RESET_SCROLL_REGION.as_bytes());
            if row[Column(last_column)].c == '\t' {
                goto(out.out, rows - 1, last_column);
                out.write('\t');
            }
            goto(out.out, rows - 1, 0);
            out.pen(&Cell::default());
            out.erase(FILLER_WIDTH);
        }

        for column in 0..end {
            let cell = &row[Column(column)];
            if lone_last && column == last_column {
                out.after(column);
                continue;
            }
            if leading_last && column == last_column {
                continue;
            }
            if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
                && column == last_column
            {
                // Left by a wide character that didn't fit here, which
                // wrapped: a filler does the same.
                out.pen(cell);
                out.write(FILLER);
                filler_here = true;
                // A tab that ran over it marked it: likewise, from the row
                // just wrapped onto.
                if cell.c == '\t' {
                    write!(out.out, "{CSI}1{CURSOR_UP}")
                        .expect("writing to memory can't fail");
                    out.go_to_column(last_column);
                    out.write('\t');
                    write!(out.out, "{CSI}1{CURSOR_DOWN}")
                        .expect("writing to memory can't fail");
                }
                continue;
            }
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                // Written by the wide character before it, or put in place
                // above. A tab that ran over it marked it, as it does blank
                // cells.
                if cell.c == '\t' {
                    out.go_to_column(column);
                    out.write('\t');
                }
                out.after(column);
                continue;
            }
            out.pen(cell);
            out.cell(cell, column);
            let spacer = (column + 1 < columns)
                .then(|| &row[Column(column + 1)])
                .is_some_and(|next| {
                    next.flags.contains(Flags::WIDE_CHAR_SPACER)
                });
            let put_already = lone_last && column + 1 == last_column;
            if cell.flags.contains(Flags::WIDE_CHAR)
                && !spacer
                && column + 1 < columns
                && !put_already
            {
                // Its spacer was erased since: erasing it here too keeps
                // the wide character, where writing over it wouldn't.
                out.go_to_column(column + 1);
                out.erase(1);
            }
        }
        // Lines scrolled in take the pen's background, as do cleared
        // cells: the rest of the line plain, as it is here.
        if end < columns {
            out.pen(&Cell::default());
            out.out.extend_from_slice(CLEAR_TO_LINE_END.as_bytes());
        }
        if line != last && !wrapped {
            out.out.extend_from_slice(b"\r\n");
        }
        // The last row wrapped, which it can when the scroll region ends
        // above it: a wrap there sets the mark but can't scroll. Likewise
        // here, from a pending wrap on its last cell, with a tab, which
        // wraps without writing.
        if line == last
            && wrapped
            && rows >= MIN_ROWS_FOR_LAST_WRAP
            && !leading_last
        {
            write!(out.out, "{CSI}1;{}{SET_SCROLL_REGION}", rows - 1)
                .expect("writing to memory can't fail");
            goto(out.out, rows - 1, columns - 1);
            wait_to_wrap(out.out, true);
            out.out.push(b'\t');
            out.out.extend_from_slice(RESET_SCROLL_REGION.as_bytes());
        }
    }
}

/// The fewest rows that leave room for a scroll region above the last.
const MIN_ROWS_FOR_LAST_WRAP: usize = 3;

/// Whether `row`'s cell at `column` is a spacer without the wide character
/// before it, erased since.
fn is_orphan_spacer(
    row: &alacritty_terminal::grid::Row<Cell>,
    column: usize,
) -> bool {
    row[Column(column)].flags.contains(Flags::WIDE_CHAR_SPACER)
        && (column == 0
            || !row[Column(column - 1)].flags.contains(Flags::WIDE_CHAR))
}

/// Output for a copy, with the pen it last set, so cells only set it when
/// theirs differs.
struct Writer<'a> {
    out: &'a mut Vec<u8>,
    pen: Cell,
    columns: usize,
}

impl Writer<'_> {
    fn pen(&mut self, cell: &Cell) {
        if !same_pen(&self.pen, cell) {
            set_pen(self.out, cell);
            self.pen = cell.clone();
        }
    }

    fn write(&mut self, c: char) {
        write!(self.out, "{c}").expect("writing to memory can't fail");
    }

    /// Writes `cell`'s text at `column`, where the cursor is, leaving it
    /// after.
    fn cell(&mut self, cell: &Cell, column: usize) {
        // A tab marks the cell it started from but keeps its colors, which
        // a space writes first. The tab moves the cursor on to the next
        // stop, so it's moved back after.
        if cell.c == '\t' {
            self.out.push(b' ');
            self.go_to_column(column);
            self.out.push(b'\t');
            self.after(column);
            return;
        }
        write_cell(self.out, cell);
    }

    /// Moves the cursor to just after `column`: the next column, or past
    /// the last, waiting to wrap.
    fn after(&mut self, column: usize) {
        if column + 1 < self.columns {
            self.go_to_column(column + 1);
        } else {
            self.go_to_column(column);
            wait_to_wrap(self.out, true);
        }
    }

    fn go_to_column(&mut self, column: usize) {
        // Columns count from 1 here.
        write!(self.out, "{CSI}{}{CURSOR_COLUMN}", column + 1)
            .expect("writing to memory can't fail");
    }

    /// Clears `count` cells from the cursor, in the pen's background,
    /// without the repairs to wide characters writing makes.
    fn erase(&mut self, count: usize) {
        write!(self.out, "{CSI}{count}{ERASE_CHARACTERS}")
            .expect("writing to memory can't fail");
    }
}

/// Writes `cell`'s text where the cursor is: not a tab, which
/// [`Writer::cell`] sees to.
fn write_cell(out: &mut Vec<u8>, cell: &Cell) {
    write!(out, "{}", cell.c).expect("writing to memory can't fail");
    for &mark in cell.zerowidth().unwrap_or_default() {
        write!(out, "{mark}").expect("writing to memory can't fail");
    }
}

/// A cell nothing has written to, or that's been cleared plain.
fn is_blank(cell: &Cell) -> bool {
    cell.c == ' '
        && cell.fg == Color::Named(NamedColor::Foreground)
        && cell.bg == Color::Named(NamedColor::Background)
        && (cell.flags - Flags::WRAPLINE).is_empty()
        && cell.extra.is_none()
}

/// The flags SGR sets, as opposed to those the grid keeps for itself.
const PEN_FLAGS: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

fn same_pen(a: &Cell, b: &Cell) -> bool {
    a.fg == b.fg
        && a.bg == b.bg
        && a.flags & PEN_FLAGS == b.flags & PEN_FLAGS
        && a.underline_color() == b.underline_color()
}

/// Sets the pen to `cell`'s colors and attributes, from scratch.
fn set_pen(out: &mut Vec<u8>, cell: &Cell) {
    let mut params = vec!["0".to_owned()];
    for (flag, code) in [
        (Flags::BOLD, sgr::BOLD),
        (Flags::DIM, sgr::DIM),
        (Flags::ITALIC, sgr::ITALIC),
        (Flags::INVERSE, sgr::INVERSE),
        (Flags::HIDDEN, sgr::HIDDEN),
        (Flags::STRIKEOUT, sgr::STRIKEOUT),
    ] {
        if cell.flags.contains(flag) {
            params.push(code.to_string());
        }
    }
    for (flag, style) in [
        (Flags::UNDERLINE, sgr::underline::SINGLE),
        (Flags::DOUBLE_UNDERLINE, sgr::underline::DOUBLE),
        (Flags::UNDERCURL, sgr::underline::CURLY),
        (Flags::DOTTED_UNDERLINE, sgr::underline::DOTTED),
        (Flags::DASHED_UNDERLINE, sgr::underline::DASHED),
    ] {
        if cell.flags.contains(flag) {
            params.push(format!("{}:{style}", sgr::UNDERLINE));
        }
    }
    params.extend(color_param(cell.fg, Layer::Foreground));
    params.extend(color_param(cell.bg, Layer::Background));
    if let Some(color) = cell.underline_color() {
        params.extend(color_param(color, Layer::Underline));
    }
    write!(out, "{CSI}{}{}", params.join(";"), sgr::FINAL)
        .expect("writing to memory can't fail");
}

/// Which color of a cell an SGR color sets.
#[derive(Clone, Copy)]
enum Layer {
    Foreground,
    Background,
    Underline,
}

/// The SGR parameters that set `layer` to `color`; none for the default,
/// which a reset sets.
fn color_param(color: Color, layer: Layer) -> Option<String> {
    let code = match layer {
        Layer::Foreground => sgr::FOREGROUND,
        Layer::Background => sgr::BACKGROUND,
        Layer::Underline => sgr::UNDERLINE_COLOR,
    };
    match color {
        Color::Spec(c) => {
            Some(format!("{code};{};{};{};{}", sgr::RGB, c.r, c.g, c.b))
        }
        Color::Indexed(i) => Some(format!("{code};{};{i}", sgr::INDEXED)),
        // The basic colors have codes of their own, which the emulator keeps
        // apart from the same colors by number. The underline has none.
        Color::Named(n) if (n as usize) < ANSI_COLORS => {
            let n = n as u8;
            let bright = n >= sgr::BASIC_COLORS;
            let offset = n % sgr::BASIC_COLORS;
            Some(match (layer, bright) {
                (Layer::Foreground, false) => {
                    (sgr::BASIC_FOREGROUND + offset).to_string()
                }
                (Layer::Foreground, true) => {
                    (sgr::BRIGHT_FOREGROUND + offset).to_string()
                }
                (Layer::Background, false) => {
                    (sgr::BASIC_BACKGROUND + offset).to_string()
                }
                (Layer::Background, true) => {
                    (sgr::BRIGHT_BACKGROUND + offset).to_string()
                }
                (Layer::Underline, _) => format!("{code};{};{n}", sgr::INDEXED),
            })
        }
        Color::Named(_) => None,
    }
}

/// Moves the cursor to where `cursor` is, counting rows from `top`, and
/// leaves it waiting to wrap if it was. `wraps` is whether
/// wrapping should be on after.
fn place_cursor(
    out: &mut Vec<u8>,
    cursor: &Cursor<Cell>,
    top: usize,
    wraps: bool,
) {
    goto(out, cursor.point.line.0 as usize - top, cursor.point.column.0);
    restore_pending_wrap(out, cursor, wraps);
}

/// Leaves the cursor, where it is, waiting to wrap if `cursor` was.
/// `wraps` is whether wrapping should be on after.
fn restore_pending_wrap(out: &mut Vec<u8>, cursor: &Cursor<Cell>, wraps: bool) {
    if cursor.input_needs_wrap {
        wait_to_wrap(out, wraps);
    }
}

/// Leaves the cursor, at the last column, waiting to wrap, without touching
/// the cell there: with wrapping off, a wide character that doesn't fit
/// does just that. `wraps` is whether wrapping should be on after.
fn wait_to_wrap(out: &mut Vec<u8>, wraps: bool) {
    decrst(out, mode::LINE_WRAP);
    write!(out, "{FILLER}").expect("writing to memory can't fail");
    if wraps {
        decset(out, mode::LINE_WRAP);
    }
}

/// Moves the cursor to `line` and `column`, from the top left.
fn goto(out: &mut Vec<u8>, line: usize, column: usize) {
    // Rows and columns count from 1 here.
    write!(out, "{CSI}{};{}{CURSOR_POSITION}", line + 1, column + 1)
        .expect("writing to memory can't fail");
}

/// Designates `cursor`'s four character sets.
fn set_charsets(out: &mut Vec<u8>, cursor: &Cursor<Cell>) {
    let sets = [
        CharsetIndex::G0,
        CharsetIndex::G1,
        CharsetIndex::G2,
        CharsetIndex::G3,
    ];
    for (index, designate) in sets.into_iter().zip(DESIGNATE) {
        let charset = match cursor.charsets[index] {
            StandardCharset::Ascii => CHARSET_ASCII,
            StandardCharset::SpecialCharacterAndLineDrawing => {
                CHARSET_LINE_DRAWING
            }
        };
        out.extend_from_slice(&[ESC, designate, charset]);
    }
}

fn save_cursor(out: &mut Vec<u8>, saved: &Cursor<Cell>, wraps: bool) {
    goto(out, saved.point.line.0 as usize, saved.point.column.0);
    restore_pending_wrap(out, saved, wraps);
    set_pen(out, &saved.template);
    set_charsets(out, saved);
    out.extend_from_slice(&[ESC, SAVE_CURSOR]);
}

/// Sets every mode a program can change, on or off as `modes` has it.
fn set_modes(out: &mut Vec<u8>, modes: TermMode) {
    for (flag, number) in [
        (TermMode::APP_CURSOR, mode::APP_CURSOR),
        (TermMode::LINE_WRAP, mode::LINE_WRAP),
        (TermMode::SHOW_CURSOR, mode::SHOW_CURSOR),
        (TermMode::FOCUS_IN_OUT, mode::FOCUS_EVENTS),
        (TermMode::ALTERNATE_SCROLL, mode::ALTERNATE_SCROLL),
        (TermMode::URGENCY_HINTS, mode::URGENCY_HINTS),
        (TermMode::BRACKETED_PASTE, mode::BRACKETED_PASTE),
    ] {
        if modes.contains(flag) {
            decset(out, number);
        } else {
            decrst(out, number);
        }
    }
    // One of these at most, and of the encodings below.
    for (flag, number) in [
        (TermMode::MOUSE_REPORT_CLICK, mode::NORMAL_MOUSE),
        (TermMode::MOUSE_DRAG, mode::BUTTON_EVENT_MOUSE),
        (TermMode::MOUSE_MOTION, mode::ANY_EVENT_MOUSE),
        (TermMode::UTF8_MOUSE, mode::UTF8_MOUSE),
        (TermMode::SGR_MOUSE, mode::SGR_MOUSE),
    ] {
        if modes.contains(flag) {
            decset(out, number);
        }
    }
    let keypad = if modes.contains(TermMode::APP_KEYPAD) {
        KEYPAD_APPLICATION
    } else {
        KEYPAD_NUMERIC
    };
    out.extend_from_slice(&[ESC, keypad]);
}

/// Sets the colors a program changed from the defaults.
fn set_colors<T>(out: &mut Vec<u8>, term: &Term<T>) {
    let colors = term.colors();
    let hex = |c: alacritty_terminal::vte::ansi::Rgb| {
        format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
    };
    let palette = osc_code::PALETTE;
    for index in 0..=u8::MAX {
        if let Some(c) = colors[usize::from(index)] {
            write!(out, "{OSC}{palette};{index};{}{ST}", hex(c))
                .expect("writing to memory can't fail");
        }
    }
    for (named, code) in [
        (NamedColor::Foreground, osc_code::FOREGROUND),
        (NamedColor::Background, osc_code::BACKGROUND),
        (NamedColor::Cursor, osc_code::CURSOR),
    ] {
        if let Some(c) = colors[named as usize] {
            write!(out, "{OSC}{code};{}{ST}", hex(c))
                .expect("writing to memory can't fail");
        }
    }
}

/// Sets the cursor's shape and blinking.
fn set_cursor_style<T>(out: &mut Vec<u8>, term: &Term<T>) {
    let style = term.cursor_style();
    let shape = match style.shape {
        CursorShape::Block | CursorShape::HollowBlock => cursor_shape::BLOCK,
        CursorShape::Underline => cursor_shape::UNDERLINE,
        CursorShape::Beam => cursor_shape::BEAM,
        CursorShape::Hidden => return,
    };
    let steady = u8::from(!style.blinking);
    write!(out, "{CSI}{}{CURSOR_STYLE}", shape + steady)
        .expect("writing to memory can't fail");
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::{Config, test::TermSize};
    use alacritty_terminal::vte::ansi::Processor;

    use super::*;

    /// History the snapshots in these tests carry.
    const HISTORY: usize = 40;
    /// Random terminals to snapshot. Tens of thousands more, over other
    /// seeds, were run while writing the serializer.
    const ROUNDS: usize = 500;

    /// An emulator with its tracker, fed together as a pane's are.
    struct Emulator {
        term: Term<VoidListener>,
        parser: Processor,
        tracker: Tracker,
    }

    impl Emulator {
        fn new(rows: usize, columns: usize) -> Self {
            Self {
                term: Term::new(
                    Config::default(),
                    &TermSize::new(columns, rows),
                    VoidListener,
                ),
                parser: Processor::new(),
                tracker: Tracker::new(rows),
            }
        }

        fn feed(&mut self, bytes: &[u8]) {
            self.tracker.advance(bytes);
            self.parser.advance(&mut self.term, bytes);
        }

        fn snapshot(&mut self) -> Vec<u8> {
            let hidden = self.tracker.hidden().clone();
            snapshot(&mut self.term, &hidden, HISTORY)
        }

        /// Whether the cursor is below the scroll region in origin mode.
        fn below_origin_region(&self) -> bool {
            self.term.mode().contains(TermMode::ORIGIN)
                && self.term.grid().cursor.point.line.0 as usize
                    >= self.tracker.hidden().region.1
        }

        /// Everything later output could depend on or show, as text, so
        /// a difference reads as one. `history` lines of history at most.
        fn state(&mut self, history: usize) -> Vec<String> {
            let mut lines = Vec::new();
            if self.term.mode().contains(TermMode::ALT_SCREEN) {
                lines.push("-- alternate screen".to_owned());
                lines.extend(describe_grid(self.term.grid(), 0));
                // The normal screen behind it, read as the snapshot does.
                let alternate = self.term.grid().clone();
                self.term.swap_alt();
                lines.push("-- normal screen".to_owned());
                lines.extend(describe_grid(self.term.grid(), history));
                lines.push(describe_cursor("cursor", &self.term.grid().cursor));
                self.term.swap_alt();
                *self.term.grid_mut() = alternate;
            } else {
                lines.extend(describe_grid(self.term.grid(), history));
            }
            let grid = self.term.grid();
            lines.push(describe_cursor("cursor", &grid.cursor));
            lines.push(describe_cursor("saved", &grid.saved_cursor));
            let modes = *self.term.mode()
                - TermMode::VI
                - TermMode::KITTY_KEYBOARD_PROTOCOL;
            lines.push(format!("modes {modes:?}"));
            let colors = self.term.colors();
            lines.push(format!(
                "colors {:?}",
                (0..=NamedColor::Cursor as usize)
                    .filter_map(|i| colors[i].map(|c| (i, c.r, c.g, c.b)))
                    .collect::<Vec<_>>()
            ));
            lines.push(format!("cursor style {:?}", self.term.cursor_style()));
            lines.push(format!("hidden {:?}", self.tracker.hidden()));
            lines
        }
    }

    fn describe_grid(grid: &Grid<Cell>, history: usize) -> Vec<String> {
        let history = grid.history_size().min(history) as i32;
        (-history..grid.screen_lines() as i32)
            .map(|line| {
                let row = &grid[Line(line)];
                let mut text = format!("{line:4} ");
                let last = grid.columns() - 1;
                for column in 0..grid.columns() {
                    let cell = &row[Column(column)];
                    // The emulator only sets these marks on the last
                    // column, the wrap and the spacer before a wide
                    // character that didn't fit, and only reads them there;
                    // inserting and deleting characters shifts them along
                    // with the rest, where they mean nothing.
                    let mut flags = cell.flags;
                    if column != last {
                        flags.remove(
                            Flags::WRAPLINE | Flags::LEADING_WIDE_CHAR_SPACER,
                        );
                    }
                    write!(
                        text,
                        "[{:?}{:?} {:?}/{:?} {:?} {:?}]",
                        cell.c,
                        cell.zerowidth().unwrap_or_default(),
                        cell.fg,
                        cell.bg,
                        flags,
                        cell.underline_color(),
                    )
                    .unwrap();
                }
                text
            })
            .collect()
    }

    fn describe_cursor(name: &str, cursor: &Cursor<Cell>) -> String {
        let pen = &cursor.template;
        format!(
            "{name} {:?} wrap {} pen {:?}/{:?} {:?} {:?} charsets {:?}",
            cursor.point,
            cursor.input_needs_wrap,
            pen.fg,
            pen.bg,
            pen.flags & PEN_FLAGS,
            pen.underline_color(),
            [
                cursor.charsets[CharsetIndex::G0],
                cursor.charsets[CharsetIndex::G1],
                cursor.charsets[CharsetIndex::G2],
                cursor.charsets[CharsetIndex::G3],
            ],
        )
    }

    /// Random output exercising everything a snapshot has to carry.
    struct Noise(u64);

    impl Noise {
        fn next(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }

        fn output(
            &mut self,
            pieces: usize,
            rows: u64,
            columns: u64,
        ) -> Vec<u8> {
            const WORDS: [&str; 8] = [
                "ls",
                "hello",
                "a",
                "tiri-is-a-multiplexer",
                "字界",
                "e\u{301}",
                "─┼─",
                "x",
            ];
            let mut out = String::new();
            for _ in 0..pieces {
                let piece = match self.next(40) {
                    0..=9 => {
                        WORDS[self.next(WORDS.len() as u64) as usize].to_owned()
                    }
                    10 => "\r\n".to_owned(),
                    11 => "\n".to_owned(),
                    12 => "\r".to_owned(),
                    13 => "\t".to_owned(),
                    14 => "\x08".to_owned(),
                    15 => {
                        const STYLES: [&str; 17] = [
                            "0", "1", "2", "3", "4", "4:3", "7", "8", "9",
                            "22", "24", "27", "31", "42", "91", "102", "39;49",
                        ];
                        format!("\x1b[{}m", STYLES[self.next(17) as usize])
                    }
                    16 => format!("\x1b[38;5;{}m", self.next(256)),
                    17 => format!(
                        "\x1b[48;2;{};{};{}m",
                        self.next(256),
                        self.next(256),
                        self.next(256)
                    ),
                    18 => format!("\x1b[58;5;{}m", self.next(256)),
                    19 => format!(
                        "\x1b[{};{}H",
                        self.next(rows + 2),
                        self.next(columns + 2)
                    ),
                    20 => format!("\x1b[{}J", self.next(3)),
                    21 => format!("\x1b[{}K", self.next(3)),
                    22 => {
                        if self.next(3) == 0 {
                            "\x1b[r".to_owned()
                        } else {
                            let top = 1 + self.next(rows);
                            format!("\x1b[{top};{}r", top + self.next(rows))
                        }
                    }
                    23 => ["\x1b[?6h", "\x1b[?6l"][self.next(2) as usize]
                        .to_owned(),
                    24 => ["\x1b[?7h", "\x1b[?7l"][self.next(2) as usize]
                        .to_owned(),
                    25 => ["\x1b[4h", "\x1b[4l", "\x1b[20h", "\x1b[20l"]
                        [self.next(4) as usize]
                        .to_owned(),
                    26 => {
                        ["\x1b(0", "\x1b(B", "\x1b)0", "\x1b)B", "\x0e", "\x0f"]
                            [self.next(6) as usize]
                            .to_owned()
                    }
                    27 => ["\x1b7", "\x1b8"][self.next(2) as usize].to_owned(),
                    28 => ["\x1b[?1049h", "\x1b[?1049l"][self.next(2) as usize]
                        .to_owned(),
                    29 => ["\x1bM", "\x1bD", "\x1bE"][self.next(3) as usize]
                        .to_owned(),
                    30 => format!(
                        "\x1b[{}{}",
                        1 + self.next(3),
                        ["S", "T", "L", "M", "@", "P", "X"]
                            [self.next(7) as usize]
                    ),
                    31 => [
                        "\x1b[?1h",
                        "\x1b[?1l",
                        "\x1b[?25l",
                        "\x1b[?25h",
                        "\x1b[?2004h",
                        "\x1b[?1000h",
                        "\x1b[?1002h",
                        "\x1b[?1003h",
                        "\x1b[?1006h",
                        "\x1b[?1005h",
                        "\x1b[?1004h",
                        "\x1b=",
                        "\x1b>",
                        "\x1b[?1007l",
                    ][self.next(14) as usize]
                        .to_owned(),
                    32 => format!(
                        "\x1b]4;{};#{:06x}\x1b\\",
                        self.next(256),
                        self.next(0x100_0000)
                    ),
                    33 => format!(
                        "\x1b]{};#{:06x}\x07",
                        [10, 11, 12][self.next(3) as usize],
                        self.next(0x100_0000)
                    ),
                    34 => format!("\x1b[{} q", self.next(7)),
                    35 if self.next(8) == 0 => "\x1bc".to_owned(),
                    _ => "word ".repeat(1 + self.next(4) as usize),
                };
                out.push_str(&piece);
            }
            out.into_bytes()
        }
    }

    /// Fails on the first line where `copy` and `server` differ, showing
    /// just the cells that differ.
    fn assert_same(copy: &[String], server: &[String], context: &str) {
        let Some(i) = (0..copy.len().max(server.len()))
            .find(|&i| copy.get(i) != server.get(i))
        else {
            return;
        };
        let (a, b) = (
            copy.get(i).map_or("", String::as_str),
            server.get(i).map_or("", String::as_str),
        );
        let cells =
            |s: &str| s.split('[').map(str::to_owned).collect::<Vec<_>>();
        let (ca, cb) = (cells(a), cells(b));
        let differ: Vec<String> = (0..ca.len().max(cb.len()))
            .filter(|&j| ca.get(j) != cb.get(j))
            .take(3)
            .map(|j| {
                format!(
                    "  cell {j}: copy [{} server [{}",
                    ca.get(j).map_or("-", |s| s),
                    cb.get(j).map_or("-", |s| s)
                )
            })
            .collect();
        panic!(
            "{context}: line {i} of {} differs ({} lines in the copy)\n  copy:   {}\n  server: {}\n{}",
            server.len(),
            copy.len(),
            &a[..a.len().min(160)],
            &b[..b.len().min(160)],
            differ.join("\n"),
        );
    }

    #[test]
    fn snapshots_rebuild_the_terminal_and_keep_in_step() {
        let mut noise = Noise(0x9e37_79b9_7f4a_7c15);
        for round in 0..ROUNDS {
            let rows = 3 + noise.next(10) as usize;
            let columns = 5 + noise.next(30) as usize;
            let mut server = Emulator::new(rows, columns);
            let pieces = 20 + noise.next(300) as usize;
            server.feed(&noise.output(pieces, rows as u64, columns as u64));
            if server.below_origin_region() {
                // The one state a snapshot can't carry whole: see
                // `snapshot`, on the cursor below the region.
                continue;
            }

            let mut copy = Emulator::new(rows, columns);
            copy.feed(&server.snapshot());
            assert_same(
                &copy.state(HISTORY),
                &server.state(HISTORY),
                &format!(
                    "round {round}, {rows}x{columns}, right after the snapshot"
                ),
            );

            // The same output from here on lands the same in both.
            let more = noise.output(100, rows as u64, columns as u64);
            server.feed(&more);
            copy.feed(&more);
            assert_same(
                &copy.state(HISTORY),
                &server.state(HISTORY),
                &format!("round {round}, {rows}x{columns}, after more output"),
            );
        }
    }

    #[test]
    fn a_snapshot_leaves_the_terminal_as_it_was() {
        let mut noise = Noise(0x2545_f491_4f6c_dd1d);
        for _ in 0..50 {
            let mut server = Emulator::new(8, 20);
            server.feed(&noise.output(200, 8, 20));
            // Into a full-screen program, whose screen hides the normal one.
            server.feed(b"\x1b[?1049hfull screen\x1b[31m");
            let before = server.state(HISTORY);
            server.snapshot();
            assert_eq!(server.state(HISTORY), before);
        }
    }
}
