//! A pane: a child process on a PTY plus the alacritty_terminal emulator it
//! draws into.

use std::cell::RefCell;
use std::os::fd::{BorrowedFd, RawFd};
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::{Config, TermMode, cell::Cell};
use alacritty_terminal::vte::ansi::{Processor, Rgb};
use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process};

use crate::colors::Palette;
use crate::input::MouseModes;

/// The most output to take from one pane per wakeup, so a pane streaming
/// output can't starve input or the others. The rest is read next time.
const READ_BUDGET: usize = 256 * 1024;

/// The most text a program may put on the clipboard with OSC 52.
const MAX_COPY: usize = 1 << 20;

/// Reported to apps that ask for the text area in pixels.
const CELL_WIDTH: u16 = 8;
const CELL_HEIGHT: u16 = 16;

pub struct Pane {
    term: Term<Listener>,
    parser: Processor,
    events: Rc<RefCell<Vec<TermEvent>>>,
    master: Box<dyn MasterPty + Send>,
    /// The PTY's controller side, non-blocking. Owned by `master`.
    fd: RawFd,
    /// Input for the child that the PTY hasn't accepted yet.
    outgoing: Vec<u8>,
    child: Box<dyn Child + Send + Sync>,
    title: Option<String>,
    fallback_title: String,
    /// Bumped whenever the screen may have changed.
    generation: u64,
    /// Text the program copied with OSC 52, for the clients' clipboards.
    copied: Vec<String>,
    /// The colors to answer the program's color queries with: those of
    /// the client most recently used.
    palette: Palette,
    /// How far the screen has scrolled, for [`Self::scroll_mark`].
    scroll: ScrollMark,
    /// The most lines of history the emulator keeps.
    history_limit: usize,
}

/// A moment in a pane's scrolling. What a client has scrolled back to, or
/// selected, is so many lines above the live screen; as output arrives
/// those lines move up, and [`Pane::scrolled_since`] says how far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScrollMark {
    /// Lines scrolled off the top of the screen into history, in total.
    lines: u64,
    /// Bumped when history is cleared, which leaves nothing to follow.
    epoch: u64,
}

impl Pane {
    /// Starts the user's shell in a new PTY, in `cwd`. Its output is read
    /// with [`Self::read_ready`] once [`Self::fd`] polls readable.
    pub fn spawn(rows: u16, cols: u16, cwd: &Path) -> Result<Self> {
        let pair = native_pty_system()
            .openpty(pty_size(rows, cols))
            .context("failed to open pty")?;

        let mut cmd = CommandBuilder::new_default_prog();
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TIRI", "1");
        cmd.cwd(cwd);

        // Everything that can fail comes before the shell starts, so a
        // failure never leaves one running with nobody to reap it.
        let fd = pair
            .master
            .as_raw_fd()
            .context("pty has no file descriptor")?;
        // SAFETY: `fd` belongs to `pair.master`, which is alive here.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        fcntl_getfl(borrowed)
            .and_then(|flags| fcntl_setfl(borrowed, flags | OFlags::NONBLOCK))
            .context("couldn't make the pane's pty non-blocking")?;

        // The shell portable-pty will pick: $SHELL, or the user's own.
        let shell = cmd.get_shell();
        let child = (pair.slave.spawn_command(cmd))
            .with_context(|| format!("couldn't start {shell} in {}", cwd.display()))?;
        // Drop our copy of the subsidiary side so reads see EOF when the child exits.
        drop(pair.slave);

        let fallback_title = std::env::var("SHELL")
            .ok()
            .and_then(|s| s.rsplit('/').next().map(str::to_owned))
            .unwrap_or_else(|| "sh".to_owned());

        let term_events = Rc::default();
        let config = Config::default();
        let history_limit = config.scrolling_history;
        let term = Term::new(
            config,
            &Size { rows, cols },
            Listener(Rc::clone(&term_events)),
        );

        Ok(Self {
            term,
            parser: Processor::new(),
            events: term_events,
            master: pair.master,
            fd,
            outgoing: Vec::new(),
            child,
            title: None,
            fallback_title,
            generation: 0,
            copied: Vec::new(),
            palette: Palette::default(),
            scroll: ScrollMark::default(),
            history_limit,
        })
    }

    pub fn title(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.fallback_title)
    }

    pub fn size(&self) -> (u16, u16) {
        (self.term.screen_lines() as u16, self.term.columns() as u16)
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

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    /// Text the program has copied since the last call.
    pub fn take_copied(&mut self) -> Vec<String> {
        std::mem::take(&mut self.copied)
    }

    /// The color an app has set for palette entry `idx` with OSC 4, if any.
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

    /// The PTY to poll: readable when the child has written output, writable
    /// when it can take more of [`Self::wants_write`]'s queued input.
    pub fn fd(&self) -> BorrowedFd<'_> {
        // SAFETY: `fd` belongs to `master`, which lives as long as `self`.
        unsafe { BorrowedFd::borrow_raw(self.fd) }
    }

    /// Reads and processes the child's output until the PTY runs dry or the
    /// read budget is spent. Returns false once the child has gone.
    pub fn read_ready(&mut self) -> bool {
        let mut buf = [0u8; 16 * 1024];
        let mut total = 0;
        loop {
            match rustix::io::read(self.fd(), &mut buf) {
                Ok(0) => return false,
                Ok(n) => {
                    self.process(&buf[..n]);
                    total += n;
                    if total >= READ_BUDGET {
                        return true;
                    }
                }
                Err(Errno::AGAIN) => return true,
                Err(Errno::INTR) => {}
                // Once the child and everything it started have closed the
                // PTY, reads fail with EIO rather than returning 0.
                Err(_) => return false,
            }
        }
    }

    fn process(&mut self, bytes: &[u8]) {
        self.generation += 1;
        self.track_scroll(|pane| pane.parser.advance(&mut pane.term, bytes));
        self.handle_term_events();
    }

    /// Runs `feed`, which gives the emulator output, and counts the lines
    /// that scrolls into history.
    fn track_scroll(&mut self, feed: impl FnOnce(&mut Self)) {
        let alt_screen = |pane: &Self| pane.term.mode().contains(TermMode::ALT_SCREEN);
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
        (mark.epoch == self.scroll.epoch).then(|| (self.scroll.lines - mark.lines) as usize)
    }

    /// When the child is mid synchronized update, the time at which we stop
    /// waiting for it to finish.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// Applies a synchronized update that has run past its deadline.
    pub fn expire_sync(&mut self, now: Instant) {
        if self.sync_deadline().is_some_and(|deadline| deadline <= now) {
            self.generation += 1;
            self.track_scroll(|pane| pane.parser.stop_sync(&mut pane.term));
            self.handle_term_events();
        }
    }

    fn handle_term_events(&mut self) {
        let events = std::mem::take(&mut *self.events.borrow_mut());
        for event in events {
            match event {
                TermEvent::PtyWrite(text) => self.write(text.as_bytes()),
                TermEvent::Title(title) => self.title = Some(title),
                TermEvent::ResetTitle => self.title = None,
                TermEvent::ClipboardStore(_, text) if text.len() <= MAX_COPY => {
                    self.copied.push(text);
                }
                TermEvent::ClipboardStore(_, text) => {
                    log::warn!(
                        "a pane tried to copy {} bytes; the most is {MAX_COPY}",
                        text.len()
                    );
                }
                TermEvent::ColorRequest(idx, reply) => {
                    // Colors the program set itself win; otherwise the
                    // real terminal's.
                    let rgb = self.term.colors()[idx].unwrap_or_else(|| {
                        let [r, g, b] = self.palette.by_index(idx);
                        Rgb { r, g, b }
                    });
                    self.write(reply(rgb).as_bytes());
                }
                TermEvent::TextAreaSizeRequest(reply) => {
                    let (rows, cols) = self.size();
                    let size = WindowSize {
                        num_lines: rows,
                        num_cols: cols,
                        cell_width: CELL_WIDTH,
                        cell_height: CELL_HEIGHT,
                    };
                    self.write(reply(size).as_bytes());
                }
                _ => {}
            }
        }
    }

    /// Sends input to the child. Whatever the PTY can't take right now is
    /// queued and sent by [`Self::flush`] once it polls writable, so a child
    /// that isn't reading never blocks tiri.
    pub fn write(&mut self, bytes: &[u8]) {
        self.outgoing.extend_from_slice(bytes);
        self.flush();
    }

    pub fn wants_write(&self) -> bool {
        !self.outgoing.is_empty()
    }

    /// Sends as much queued input as the PTY will take.
    pub fn flush(&mut self) {
        while !self.outgoing.is_empty() {
            match rustix::io::write(self.fd(), &self.outgoing) {
                Ok(n) => {
                    self.outgoing.drain(..n);
                }
                Err(Errno::AGAIN) => return,
                Err(Errno::INTR) => {}
                // The child is gone; reading will notice and close the pane.
                Err(_) => {
                    self.outgoing.clear();
                    return;
                }
            }
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.size() == (rows, cols) {
            return;
        }
        self.generation += 1;
        self.term.resize(Size { rows, cols });
        if let Err(e) = self.master.resize(pty_size(rows, cols)) {
            log::warn!("couldn't resize a pane to {cols}x{rows}: {e:#}");
        }
    }

    /// Hangs up on the child, as closing a terminal window would. Doesn't
    /// wait for it to go: see [`Self::into_child`].
    pub fn kill(&self) {
        let pid = self
            .child
            .process_id()
            .and_then(|pid| Pid::from_raw(i32::try_from(pid).ok()?));
        if let Some(pid) = pid {
            let _ = kill_process(pid, Signal::HUP);
        }
    }

    /// The child process, to be reaped once it has exited.
    pub fn into_child(self) -> Box<dyn Child + Send + Sync> {
        self.child
    }
}

fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Queues the emulator's events for the pane to handle after each parse.
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

    /// A pane whose shell is left alone: the tests feed its emulator directly.
    fn pane() -> Pane {
        Pane::spawn(5, 20, Path::new("/")).expect("a shell starts")
    }

    fn close(pane: Pane) {
        pane.kill();
        let _ = pane.into_child().wait();
    }

    #[test]
    fn counts_lines_scrolled_into_history_even_once_its_full() {
        let mut pane = pane();
        let start = pane.scroll_mark();
        pane.process("line\r\n".repeat(30).as_bytes());
        assert_eq!(pane.scrolled_since(start), Some(pane.history_size()));
        assert!(pane.history_size() > 20);

        // Fill history, in pieces as output arrives.
        for _ in 0..(pane.history_limit / 100 + 1) {
            pane.process("line\r\n".repeat(100).as_bytes());
        }
        assert_eq!(pane.history_size(), pane.history_limit);
        let full = pane.scroll_mark();
        pane.process("line\r\n".repeat(7).as_bytes());
        assert_eq!(pane.history_size(), pane.history_limit);
        assert_eq!(pane.scrolled_since(full), Some(7));
        assert_eq!(
            pane.term.grid().display_offset(),
            0,
            "the probe is put back"
        );

        // On the alternate screen nothing scrolls into history.
        pane.process(b"\x1b[?1049h");
        pane.process("line\r\n".repeat(9).as_bytes());
        pane.process(b"\x1b[?1049l");
        assert_eq!(pane.scrolled_since(full), Some(7));

        // Clearing history leaves nothing to measure from.
        pane.process(b"\x1b[3J");
        assert_eq!(pane.scrolled_since(full), None);
        close(pane);
    }
}
