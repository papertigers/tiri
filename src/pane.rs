//! A pane: a child process on a PTY plus the alacritty_terminal emulator it
//! draws into.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Instant;

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::{Config, TermMode, cell::Cell};
use alacritty_terminal::vte::ansi::{Processor, Rgb};
use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::Event;
use crate::layout::PaneId;

/// Answers for apps that ask what colors the terminal uses (OSC 10/11).
/// Assumes a dark theme until we ask the outer terminal.
const DEFAULT_FG: Rgb = Rgb {
    r: 0xd8,
    g: 0xd8,
    b: 0xd8,
};
const DEFAULT_BG: Rgb = Rgb {
    r: 0x00,
    g: 0x00,
    b: 0x00,
};
/// Reported to apps that ask for the text area in pixels.
const CELL_WIDTH: u16 = 8;
const CELL_HEIGHT: u16 = 16;

pub struct Pane {
    term: Term<Listener>,
    parser: Processor,
    events: Rc<RefCell<Vec<TermEvent>>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    title: Option<String>,
    fallback_title: String,
    /// Bumped whenever the screen may have changed.
    generation: u64,
}

impl Pane {
    /// Starts the user's shell in a new PTY. Output and exit are reported to
    /// `events` from a reader thread.
    pub fn spawn(id: PaneId, rows: u16, cols: u16, events: Sender<Event>) -> Result<Self> {
        let pair = native_pty_system()
            .openpty(pty_size(rows, cols))
            .context("failed to open pty")?;

        let mut cmd = CommandBuilder::new_default_prog();
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TIRI", "1");
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn shell")?;
        // Drop our copy of the subsidiary side so reads see EOF when the child exits.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if events.send(Event::Output(id, buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = events.send(Event::Exited(id));
        });

        let fallback_title = std::env::var("SHELL")
            .ok()
            .and_then(|s| s.rsplit('/').next().map(str::to_owned))
            .unwrap_or_else(|| "sh".to_owned());

        let term_events = Rc::default();
        let term = Term::new(
            Config::default(),
            &Size { rows, cols },
            Listener(Rc::clone(&term_events)),
        );

        Ok(Self {
            term,
            parser: Processor::new(),
            events: term_events,
            master: pair.master,
            writer,
            child,
            title: None,
            fallback_title,
            generation: 0,
        })
    }

    pub fn title(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.fallback_title)
    }

    pub fn size(&self) -> (u16, u16) {
        (self.term.screen_lines() as u16, self.term.columns() as u16)
    }

    pub fn cell(&self, row: u16, col: u16) -> &Cell {
        &self.term.grid()[Point::new(Line(i32::from(row)), Column(usize::from(col)))]
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

    /// Feeds output from the child into the emulator.
    pub fn term(&self) -> &Term<impl EventListener> {
        &self.term
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.generation += 1;
        self.parser.advance(&mut self.term, bytes);
        self.handle_term_events();
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
            self.parser.stop_sync(&mut self.term);
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
                TermEvent::ColorRequest(idx, reply) => {
                    let rgb = self.term.colors()[idx].unwrap_or(match idx {
                        256 => DEFAULT_FG,
                        _ => DEFAULT_BG,
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

    pub fn write(&mut self, bytes: &[u8]) {
        // A failed write means the child is gone; the reader thread reports that.
        let _ = self
            .writer
            .write_all(bytes)
            .and_then(|()| self.writer.flush());
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.size() == (rows, cols) {
            return;
        }
        self.generation += 1;
        self.term.resize(Size { rows, cols });
        let _ = self.master.resize(pty_size(rows, cols));
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    /// Collects the child's exit status so it doesn't linger as a zombie.
    pub fn reap(&mut self) {
        let _ = self.child.try_wait();
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
