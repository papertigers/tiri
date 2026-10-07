// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Text selected with the mouse, and turning it into a string to copy.

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;

use crate::emulator::ScrollMark;
use crate::layout::PaneId;

/// A cell in a pane's content. Line 0 is the top of the live screen;
/// negative lines are in its scrollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub line: i32,
    pub col: u16,
}

/// A run of text in one pane, from where the drag started to where it is
/// now, wrapping from line to line like text does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub pane: PaneId,
    pub anchor: Point,
    pub head: Point,
    /// When `anchor` and `head` were measured: output since has moved the
    /// text they're on further up.
    pub at: ScrollMark,
}

impl Selection {
    /// The selection's first and last cells, in reading order.
    pub fn bounds(&self) -> (Point, Point) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }

    pub fn contains(&self, pane: PaneId, point: Point) -> bool {
        let (start, end) = self.bounds();
        pane == self.pane && start <= point && point <= end
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// Characters that end a word for double-click selection, besides
/// whitespace. alacritty's defaults: brackets, quotes and the like split
/// words, while paths and names joined with `/`, `-`, `_` or `.` stay whole.
const WORD_SEPARATORS: &str = ",│`|:\"'()[]{}<>";

/// The run of similar characters around `point`, for double-click
/// selection: a word, a stretch of blanks, or a lone separator. Words
/// continue across rows the terminal wrapped them onto.
pub fn word_at<T>(term: &Term<T>, point: Point) -> (Point, Point) {
    let top = -(term.grid().history_size() as i32);
    let bottom = term.screen_lines() as i32 - 1;
    let last_col = term.columns().saturating_sub(1) as u16;
    let wraps = |line: i32| {
        term.grid()[Line(line)][Column(usize::from(last_col))]
            .flags
            .contains(Flags::WRAPLINE)
    };
    // Neighbouring cells in reading order, stepping across wrapped rows.
    let before = |p: Point| match p.col {
        0 if p.line > top && wraps(p.line - 1) => {
            Some(Point { line: p.line - 1, col: last_col })
        }
        0 => None,
        col => Some(Point { col: col - 1, ..p }),
    };
    let after = |p: Point| {
        if p.col < last_col {
            Some(Point { col: p.col + 1, ..p })
        } else if p.line < bottom && wraps(p.line) {
            Some(Point { line: p.line + 1, col: 0 })
        } else {
            None
        }
    };
    let class = |p: Point| {
        // The right half of a wide character belongs with its left half.
        let row = &term.grid()[Line(p.line)];
        let mut cell = &row[Column(usize::from(p.col))];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) && p.col > 0 {
            cell = &row[Column(usize::from(p.col) - 1)];
        } else if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
            && p.line < bottom
        {
            // The gap left where a wide character didn't fit and wrapped
            // belongs with that character, at the start of the next row.
            cell = &term.grid()[Line(p.line + 1)][Column(0)];
        }
        match cell.c {
            c if c.is_whitespace() => Class::Blank,
            c if WORD_SEPARATORS.contains(c) => Class::Separator,
            _ => Class::Word,
        }
    };

    let point = Point { col: point.col.min(last_col), ..point };
    let target = class(point);
    let (mut start, mut end) = (point, point);
    if target != Class::Separator {
        while let Some(p) = before(start).filter(|&p| class(p) == target) {
            start = p;
        }
        while let Some(p) = after(end).filter(|&p| class(p) == target) {
            end = p;
        }
    }
    (start, end)
}

/// The whole line around `point`, for triple-click selection, including
/// rows it wrapped onto (or from), since a long line copies as one.
pub fn line_at<T>(term: &Term<T>, point: Point) -> (Point, Point) {
    let top = -(term.grid().history_size() as i32);
    let bottom = term.screen_lines() as i32 - 1;
    let last_col = term.columns().saturating_sub(1);
    let wraps = |line: i32| {
        term.grid()[Line(line)][Column(last_col)]
            .flags
            .contains(Flags::WRAPLINE)
    };
    let mut start = point.line;
    while start > top && wraps(start - 1) {
        start -= 1;
    }
    let mut end = point.line;
    while end < bottom && wraps(end) {
        end += 1;
    }
    (Point { line: start, col: 0 }, Point { line: end, col: last_col as u16 })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Blank,
    Separator,
    Word,
}

/// The text between `start` and `end` (inclusive). Lines end in a newline
/// unless the terminal wrapped them, and trailing blanks are dropped.
pub fn text<T>(term: &Term<T>, start: Point, end: Point) -> String {
    let top = -(term.grid().history_size() as i32);
    let bottom = term.screen_lines() as i32 - 1;
    let last_col = term.columns().saturating_sub(1) as u16;
    let (first_line, last_line) = (start.line.max(top), end.line.min(bottom));

    let mut out = String::new();
    for line in first_line..=last_line {
        let row = &term.grid()[Line(line)];
        let from = if line == start.line { start.col } else { 0 };
        let to =
            if line == end.line { end.col } else { last_col }.min(last_col);
        let mut text = String::new();
        for col in from..=to {
            let cell = &row[Column(usize::from(col))];
            // Neither the right half of a wide character nor the gap one
            // left by wrapping is text.
            let spacers =
                Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER;
            if cell.flags.intersects(spacers) {
                continue;
            }
            // alacritty marks where a tab started with a literal tab.
            text.push(if cell.c == '\t' { ' ' } else { cell.c });
            text.extend(cell.zerowidth().into_iter().flatten());
        }
        let wrapped = line < last_line
            && to == last_col
            && row[Column(usize::from(last_col))]
                .flags
                .contains(Flags::WRAPLINE);
        if wrapped {
            out.push_str(&text);
        } else {
            out.push_str(text.trim_end_matches(' '));
            if line < last_line {
                out.push('\n');
            }
        }
    }
    out
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

    fn at(line: i32, col: u16) -> Point {
        Point { line, col }
    }

    #[test]
    fn copies_across_lines_without_trailing_blanks() {
        let term = term_with(10, 3, b"hello\r\nworld");
        assert_eq!(text(&term, at(0, 2), at(1, 2)), "llo\nwor");
        assert_eq!(text(&term, at(0, 0), at(0, 9)), "hello");
    }

    #[test]
    fn a_wide_character_that_wrapped_leaves_no_gap() {
        // 字 doesn't fit in the last column, so it starts the next row.
        let term = term_with(5, 3, "abcd字x".as_bytes());
        assert_eq!(text(&term, at(0, 0), at(1, 4)), "abcd字x");
        let (start, end) = word_at(&term, at(0, 1));
        assert_eq!(text(&term, start, end), "abcd字x");
    }

    #[test]
    fn wrapped_lines_join_without_a_newline() {
        let term = term_with(5, 3, b"abcdefgh");
        assert_eq!(text(&term, at(0, 0), at(1, 4)), "abcdefgh");
    }

    #[test]
    fn reaches_into_scrollback() {
        let term = term_with(10, 2, b"one\r\ntwo\r\nthree");
        assert_eq!(text(&term, at(-1, 0), at(0, 9)), "one\ntwo");
    }

    #[test]
    fn tabs_become_spaces() {
        let term = term_with(20, 1, b"a\tb");
        assert_eq!(text(&term, at(0, 0), at(0, 19)), "a       b");
    }

    #[test]
    fn selections_order_their_ends() {
        let pane = PaneId(1);
        let sel = Selection {
            pane,
            anchor: at(2, 5),
            head: at(1, 7),
            at: ScrollMark::default(),
        };
        assert_eq!(sel.bounds(), (at(1, 7), at(2, 5)));
        assert!(sel.contains(pane, at(1, 9)));
        assert!(sel.contains(pane, at(2, 0)));
        assert!(!sel.contains(pane, at(2, 6)));
        assert!(!sel.contains(PaneId(2), at(2, 0)));
    }

    /// The word `word_at` finds at column `col` of line 0, as text.
    fn word(line: &str, col: u16) -> String {
        let term = term_with(40, 1, line.as_bytes());
        let (start, end) = word_at(&term, at(0, col));
        text(&term, start, end)
    }

    #[test]
    fn double_click_selects_words() {
        assert_eq!(word("hello world", 1), "hello");
        assert_eq!(word("hello world", 8), "world");
        assert_eq!(word("ls /usr/local/bin -la", 6), "/usr/local/bin");
        assert_eq!(word("call(some_arg, x)", 7), "some_arg");
        assert_eq!(word("tiri-0.1.tar.gz done", 0), "tiri-0.1.tar.gz");
    }

    #[test]
    fn double_click_on_separators_and_wide_characters() {
        // A lone separator is selected on its own.
        assert_eq!(word("call(arg)", 4), "(");
        // Either half of a wide character finds the whole word.
        assert_eq!(word("ab字字cd ef", 3), "ab字字cd");
        assert_eq!(word("ab字字cd ef", 4), "ab字字cd");
    }

    #[test]
    fn triple_click_selects_the_whole_line() {
        let term = term_with(20, 3, b"first line\r\nsecond");
        let (start, end) = line_at(&term, at(1, 3));
        assert_eq!(text(&term, start, end), "second");
    }

    #[test]
    fn triple_click_follows_a_wrapped_line_both_ways() {
        // Twelve characters in a five-column pane wrap onto three rows.
        let term = term_with(5, 4, b"abcdefghijkl\r\nnext");
        for line in 0..3 {
            let (start, end) = line_at(&term, at(line, 1));
            assert_eq!(
                text(&term, start, end),
                "abcdefghijkl",
                "from row {line}"
            );
        }
        let (start, end) = line_at(&term, at(3, 0));
        assert_eq!(text(&term, start, end), "next");
    }

    #[test]
    fn double_click_follows_a_word_split_by_wrapping() {
        // "lambda" is split across the wrap: "l" ends row 0, "ambda" starts row 1.
        let term = term_with(5, 3, b"abc lambda");
        for p in [at(0, 4), at(1, 2)] {
            let (start, end) = word_at(&term, p);
            assert_eq!(text(&term, start, end), "lambda", "from {p:?}");
        }
    }
}
