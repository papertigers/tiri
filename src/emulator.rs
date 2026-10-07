// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A pane's terminal emulator: the screen its program draws into, apart
//! from the program and its PTY. The server keeps one for each pane, and
//! answers the program's questions from it; a client keeps a copy of each
//! it shows, fed the same output, to draw from.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::{TermMode, cell::Cell};
use alacritty_terminal::vte::ansi::{Processor, Rgb};

use crate::escape;
use crate::input::{KeyModes, MouseModes};
use crate::snapshot::{self, Tracker};

/// The most text a program may put on the clipboard with OSC 52.
const MAX_COPY: usize = 1 << 20;

pub struct Emulator {
    term: Term<Listener>,
    parser: Processor,
    events: Rc<RefCell<Vec<TermEvent>>>,
    /// The title the program set, if it has.
    title: Option<String>,
    /// Bumped whenever the screen may have changed.
    generation: u64,
    /// Text the program copied with OSC 52, for the clients' clipboards.
    copied: Vec<String>,
    /// Questions the program asked its terminal, for whoever answers them.
    questions: Vec<TermEvent>,
    /// How far the screen has scrolled, for [`Self::scroll_mark`].
    scroll: ScrollMark,
    /// The most lines of history the emulator keeps.
    history_limit: usize,
    /// What the emulator keeps to itself that a snapshot needs.
    tracker: Tracker,
}

/// A moment in a pane's scrolling. What a client has scrolled back to, or
/// selected, is so many lines above the live screen; as output arrives
/// those lines move up, and [`Emulator::scrolled_since`] says how far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScrollMark {
    /// Lines scrolled off the top of the screen into history, in total.
    lines: u64,
    /// Bumped when history is cleared, which leaves nothing to follow.
    epoch: u64,
}

impl Emulator {
    pub fn new(rows: u16, cols: u16) -> Self {
        let events = Rc::default();
        let config = snapshot::config();
        let history_limit = config.scrolling_history;
        let term = Term::new(
            config,
            &Size { rows, cols },
            Listener(Rc::clone(&events)),
        );
        Self {
            term,
            parser: Processor::new(),
            events,
            title: None,
            generation: 0,
            copied: Vec::new(),
            questions: Vec::new(),
            scroll: ScrollMark::default(),
            history_limit,
            tracker: Tracker::new(usize::from(rows)),
        }
    }

    /// Takes in the program's output.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.generation += 1;
        self.tracker.advance(bytes);
        self.track_scroll(|e| e.parser.advance(&mut e.term, bytes));
        self.handle_events();
    }

    /// The title the program set, if it has.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Rows and columns.
    pub fn size(&self) -> (u16, u16) {
        (self.term.screen_lines() as u16, self.term.columns() as u16)
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.size() == (rows, cols) {
            return;
        }
        self.generation += 1;
        self.term.resize(Size { rows, cols });
        self.tracker.resize(usize::from(rows));
    }

    /// The cell at `line`, `col`. Line 0 is the top of the live screen;
    /// negative lines reach back into the scrollback.
    pub fn cell(&self, line: i32, col: u16) -> &Cell {
        &self.term.grid()[Point::new(Line(line), Column(usize::from(col)))]
    }

    /// How many lines of scrollback there are above the screen.
    pub fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    /// The kitty keyboard protocol flags the program asked for, as the
    /// protocol numbers them.
    pub fn keyboard_flags(&self) -> u8 {
        snapshot::keyboard_flags(*self.term.mode())
    }

    /// How the program wants its keys sent.
    pub fn key_modes(&self) -> KeyModes {
        KeyModes {
            application_cursor: self.application_cursor(),
            keyboard: self.keyboard_flags(),
        }
    }

    /// What the program asked to hear about the mouse.
    pub fn mouse_modes(&self) -> MouseModes {
        let mode = self.term.mode();
        MouseModes {
            click: mode.contains(TermMode::MOUSE_REPORT_CLICK),
            drag: mode.contains(TermMode::MOUSE_DRAG),
            motion: mode.contains(TermMode::MOUSE_MOTION),
            sgr: mode.contains(TermMode::SGR_MOUSE),
            utf8: mode.contains(TermMode::UTF8_MOUSE),
        }
    }

    /// Whether the wheel should become arrow keys: a full-screen program
    /// that doesn't take the mouse itself, like `less`.
    pub fn alternate_scroll(&self) -> bool {
        let mode = self.term.mode();
        mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
            && !mode.intersects(TermMode::MOUSE_MODE)
    }

    /// The color the program has set for palette entry `idx` with OSC 4,
    /// if any.
    pub fn palette(&self, idx: usize) -> Option<Rgb> {
        self.term.colors()[idx]
    }

    pub fn cursor(&self) -> (u16, u16) {
        let point = self.term.grid().cursor.point;
        (point.line.0.max(0) as u16, point.column.0 as u16)
    }

    pub fn cursor_visible(&self) -> bool {
        self.term.mode().contains(TermMode::SHOW_CURSOR)
    }

    pub fn application_cursor(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// The emulated terminal, for drawing thumbnails from.
    pub fn term(&self) -> &Term<impl EventListener> {
        &self.term
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Text the program has copied since the last call.
    pub fn take_copied(&mut self) -> Vec<String> {
        std::mem::take(&mut self.copied)
    }

    /// Questions the program has asked since the last call: for its
    /// terminal to write something back, or what colors or size it has.
    pub fn take_questions(&mut self) -> Vec<TermEvent> {
        std::mem::take(&mut self.questions)
    }

    /// Runs `feed`, which gives the emulator output, and counts the lines
    /// that scrolls into history.
    fn track_scroll(&mut self, feed: impl FnOnce(&mut Self)) {
        let alt_screen =
            |e: &Self| e.term.mode().contains(TermMode::ALT_SCREEN);
        let (before, was_alt) = (self.history_size(), alt_screen(self));
        // History growing counts them, until it's full and stops growing.
        // The grid's display offset still can then: once it's scrolled
        // back at all, it follows the text up line for line. So scroll it
        // back one line as a probe. Nothing draws from the display offset.
        if before > 0 {
            self.term.grid_mut().scroll_display(Scroll::Delta(1));
        }
        feed(self);
        let probe = self.term.grid().display_offset().saturating_sub(1);
        self.term.grid_mut().scroll_display(Scroll::Bottom);

        let after = self.history_size();
        if was_alt || alt_screen(self) {
            // The alternate screen has no history to scroll into.
        } else if after < before {
            self.scroll.epoch += 1;
        } else if before > 0 && after >= self.history_limit {
            self.scroll.lines += (after - before).max(probe) as u64;
        } else {
            self.scroll.lines += (after - before) as u64;
        }
    }

    /// Now, for measuring scrolling from with [`Self::scrolled_since`].
    pub fn scroll_mark(&self) -> ScrollMark {
        self.scroll
    }

    /// How many lines have scrolled into history since `mark`, or None if
    /// history was cleared since, taking what was there with it.
    pub fn scrolled_since(&self, mark: ScrollMark) -> Option<usize> {
        (mark.epoch == self.scroll.epoch)
            .then(|| (self.scroll.lines - mark.lines) as usize)
    }

    /// Output that rebuilds this terminal from scratch, with up to
    /// `history` lines of history, and its title. A synchronized update in
    /// progress is applied first, as the output the tracker has seen
    /// already is.
    pub fn snapshot(&mut self, history: usize) -> Vec<u8> {
        if self.sync_deadline().is_some() {
            self.stop_sync();
        }
        let mut out =
            snapshot::snapshot(&mut self.term, self.tracker.hidden(), history);
        if let Some(title) = &self.title {
            escape::set_title(&mut out, title);
        }
        out
    }

    /// When the program is mid synchronized update, the time at which we
    /// stop waiting for it to finish.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// Applies a synchronized update that has run past its deadline.
    pub fn expire_sync(&mut self, now: Instant) {
        if self.sync_deadline().is_some_and(|deadline| deadline <= now) {
            self.stop_sync();
        }
    }

    fn stop_sync(&mut self) {
        self.generation += 1;
        self.track_scroll(|e| e.parser.stop_sync(&mut e.term));
        self.handle_events();
    }

    fn handle_events(&mut self) {
        let events = std::mem::take(&mut *self.events.borrow_mut());
        for event in events {
            match event {
                TermEvent::Title(title) => self.title = Some(title),
                TermEvent::ResetTitle => self.title = None,
                TermEvent::ClipboardStore(_, text)
                    if text.len() <= MAX_COPY =>
                {
                    self.copied.push(text);
                }
                TermEvent::ClipboardStore(_, text) => {
                    log::warn!(
                        "a pane tried to copy {} bytes; the most is {MAX_COPY}",
                        text.len()
                    );
                }
                TermEvent::PtyWrite(_)
                | TermEvent::ColorRequest(..)
                | TermEvent::TextAreaSizeRequest(_) => {
                    self.questions.push(event);
                }
                _ => {}
            }
        }
    }
}

/// Queues the emulator's events, to handle after each parse.
struct Listener(Rc<RefCell<Vec<TermEvent>>>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        self.0.borrow_mut().push(event);
    }
}

#[derive(Clone, Copy)]
struct Size {
    rows: u16,
    cols: u16,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        usize::from(self.rows)
    }

    fn columns(&self) -> usize {
        usize::from(self.cols)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_lines_scrolled_into_history_even_once_its_full() {
        let mut emulator = Emulator::new(5, 20);
        let start = emulator.scroll_mark();
        emulator.feed("line\r\n".repeat(30).as_bytes());
        assert_eq!(
            emulator.scrolled_since(start),
            Some(emulator.history_size())
        );
        assert!(emulator.history_size() > 20);

        // Fill history, in pieces as output arrives.
        for _ in 0..=(emulator.history_limit / 100) {
            emulator.feed("line\r\n".repeat(100).as_bytes());
        }
        assert_eq!(emulator.history_size(), emulator.history_limit);
        let full = emulator.scroll_mark();
        emulator.feed("line\r\n".repeat(7).as_bytes());
        assert_eq!(emulator.history_size(), emulator.history_limit);
        assert_eq!(emulator.scrolled_since(full), Some(7));
        assert_eq!(
            emulator.term.grid().display_offset(),
            0,
            "the probe is put back"
        );

        // On the alternate screen nothing scrolls into history.
        emulator.feed(b"\x1b[?1049h");
        emulator.feed("line\r\n".repeat(9).as_bytes());
        emulator.feed(b"\x1b[?1049l");
        assert_eq!(emulator.scrolled_since(full), Some(7));

        // Clearing history leaves nothing to measure from.
        emulator.feed(b"\x1b[3J");
        assert_eq!(emulator.scrolled_since(full), None);
    }

    #[test]
    fn programs_can_have_the_kitty_keyboard_protocol() {
        let mut emulator = Emulator::new(5, 20);
        emulator.feed(b"\x1b[>1u");
        assert_eq!(emulator.keyboard_flags(), 1);
        // Asked, it answers with what's in force.
        emulator.feed(b"\x1b[?u");
        let answers: Vec<String> = (emulator.take_questions().into_iter())
            .filter_map(|event| match event {
                TermEvent::PtyWrite(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(answers, ["\x1b[?1u"]);
        emulator.feed(b"\x1b[<u");
        assert_eq!(emulator.keyboard_flags(), 0);
    }

    #[test]
    fn snapshots_carry_the_title() {
        let mut emulator = Emulator::new(5, 20);
        emulator.feed(b"\x1b]2;vim notes.txt\x07hi");
        let mut copy = Emulator::new(5, 20);
        copy.feed(&emulator.snapshot(0));
        assert_eq!(copy.title(), Some("vim notes.txt"));
    }

    #[test]
    fn questions_are_left_for_whoever_answers() {
        let mut emulator = Emulator::new(5, 20);
        // Where's the cursor, and what's the background color?
        emulator.feed(b"\x1b[6n\x1b]11;?\x07");
        let questions = emulator.take_questions();
        assert!(matches!(
            questions.as_slice(),
            [TermEvent::PtyWrite(_), TermEvent::ColorRequest(..)]
        ));
        assert!(emulator.take_questions().is_empty());
    }
}
