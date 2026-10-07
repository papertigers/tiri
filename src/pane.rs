// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A pane: a child process on a PTY, and the [`Emulator`] it draws into.

use std::os::fd::{BorrowedFd, RawFd};
use std::path::Path;
use std::time::Instant;

use alacritty_terminal::event::{Event as TermEvent, WindowSize};
use alacritty_terminal::vte::ansi::Rgb;
use anyhow::{Context, Result};
use portable_pty::{
    Child, CommandBuilder, MasterPty, PtySize, native_pty_system,
};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process};

use crate::colors::Palette;
use crate::emulator::Emulator;

/// The most output to take from one pane per wakeup, so a pane streaming
/// output can't starve input or the others. The rest is read next time.
const READ_BUDGET: usize = 256 * 1024;
/// How much is read from the PTY at a time, within that budget.
const READ_CHUNK: usize = 16 * 1024;

/// Reported to apps that ask for the text area in pixels.
const CELL_WIDTH: u16 = 8;
const CELL_HEIGHT: u16 = 16;

pub struct Pane {
    emulator: Emulator,
    master: Box<dyn MasterPty + Send>,
    /// The PTY's controller side, non-blocking. Owned by `master`.
    fd: RawFd,
    /// Input for the child that the PTY hasn't accepted yet.
    outgoing: Vec<u8>,
    child: Box<dyn Child + Send + Sync>,
    /// The child has exited and been reaped. Its pid may belong to another
    /// process now, so it must not be signalled.
    reaped: bool,
    /// The title until the program sets one: the shell's name.
    fallback_title: String,
    /// The colors to answer the program's color queries with: those of
    /// the client most recently used.
    palette: Palette,
    /// Output read since [`Self::take_output`], for clients' copies.
    output: Vec<u8>,
}

impl Pane {
    /// Starts the user's shell in a new PTY, in `cwd`, with `agent` as its
    /// ssh agent if given. Its output is read with [`Self::read_ready`]
    /// once [`Self::fd`] polls readable.
    pub fn spawn(
        rows: u16,
        cols: u16,
        cwd: &Path,
        agent: Option<&Path>,
    ) -> Result<Self> {
        let pair = native_pty_system()
            .openpty(pty_size(rows, cols))
            .context("failed to open pty")?;

        let mut cmd = CommandBuilder::new_default_prog();
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TIRI", "1");
        if let Some(agent) = agent {
            cmd.env("SSH_AUTH_SOCK", agent);
        }
        cmd.cwd(cwd);

        // Everything that can fail comes before the shell starts, so a
        // failure never leaves one running with nobody to reap it.
        let fd =
            pair.master.as_raw_fd().context("pty has no file descriptor")?;
        // SAFETY: `fd` belongs to `pair.master`, which is alive here.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        fcntl_getfl(borrowed)
            .and_then(|flags| fcntl_setfl(borrowed, flags | OFlags::NONBLOCK))
            .context("couldn't make the pane's pty non-blocking")?;

        // The shell portable-pty will pick: $SHELL, or the user's own.
        let shell = cmd.get_shell();
        let child = (pair.slave.spawn_command(cmd)).with_context(|| {
            format!("couldn't start {shell} in {}", cwd.display())
        })?;
        // Drop our copy of the subsidiary side so reads see EOF when the child exits.
        drop(pair.slave);

        let fallback_title = std::env::var("SHELL")
            .ok()
            .and_then(|s| s.rsplit('/').next().map(str::to_owned))
            .unwrap_or_else(|| "sh".to_owned());

        Ok(Self {
            emulator: Emulator::new(rows, cols),
            master: pair.master,
            fd,
            outgoing: Vec::new(),
            child,
            reaped: false,
            fallback_title,
            palette: Palette::default(),
            output: Vec::new(),
        })
    }

    /// The screen the program draws into.
    pub fn emulator(&self) -> &Emulator {
        &self.emulator
    }

    pub fn emulator_mut(&mut self) -> &mut Emulator {
        &mut self.emulator
    }

    /// The title until the program sets one: its shell's name.
    pub fn fallback_title(&self) -> &str {
        &self.fallback_title
    }

    /// The output read since the last call, as the program wrote it.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
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
        let mut buf = [0u8; READ_CHUNK];
        let mut total = 0;
        loop {
            match rustix::io::read(self.fd(), &mut buf) {
                Ok(0) => return false,
                Ok(n) => {
                    self.emulator.feed(&buf[..n]);
                    self.output.extend_from_slice(&buf[..n]);
                    self.answer_questions();
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

    /// When the child is mid synchronized update, the time at which we stop
    /// waiting for it to finish.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.emulator.sync_deadline()
    }

    /// Applies a synchronized update that has run past its deadline.
    pub fn expire_sync(&mut self, now: Instant) {
        self.emulator.expire_sync(now);
        self.answer_questions();
    }

    /// Answers what the program asked its terminal.
    fn answer_questions(&mut self) {
        for question in self.emulator.take_questions() {
            match question {
                TermEvent::PtyWrite(text) => self.write(text.as_bytes()),
                TermEvent::ColorRequest(idx, reply) => {
                    // Colors the program set itself win; otherwise the
                    // real terminal's.
                    let rgb = self.emulator.palette(idx).unwrap_or_else(|| {
                        let [r, g, b] = self.palette.by_index(idx);
                        Rgb { r, g, b }
                    });
                    self.write(reply(rgb).as_bytes());
                }
                TermEvent::TextAreaSizeRequest(reply) => {
                    let (rows, cols) = self.emulator.size();
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
        if self.emulator.size() == (rows, cols) {
            return;
        }
        self.emulator.resize(rows, cols);
        if let Err(e) = self.master.resize(pty_size(rows, cols)) {
            log::warn!("couldn't resize a pane to {cols}x{rows}: {e:#}");
        }
    }

    /// Hangs up on the child, as closing a terminal window would. Doesn't
    /// wait for it to go: see [`Self::into_unreaped_child`].
    pub fn kill(&self) {
        if self.reaped {
            return;
        }
        let pid = self
            .child
            .process_id()
            .and_then(|pid| Pid::from_raw(i32::try_from(pid).ok()?));
        if let Some(pid) = pid {
            let _ = kill_process(pid, Signal::HUP);
        }
    }

    /// Whether the child (the shell) has exited, collecting its exit
    /// status if so.
    pub fn child_exited(&mut self) -> bool {
        self.reaped |= matches!(self.child.try_wait(), Ok(Some(_)));
        self.reaped
    }

    /// The child process, to be reaped once it has exited, unless it has
    /// been already.
    pub fn into_unreaped_child(self) -> Option<Box<dyn Child + Send + Sync>> {
        (!self.reaped).then_some(self.child)
    }
}

fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pane whose shell is left alone: the tests feed its emulator directly.
    fn pane() -> Pane {
        Pane::spawn(5, 20, Path::new("/"), None).expect("a shell starts")
    }

    #[test]
    fn notices_the_shell_exiting_and_then_leaves_its_pid_alone() {
        let mut pane = pane();
        assert!(!pane.child_exited());
        pane.write(b"exit\r");
        pane.flush();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        // Read its output as the server would, answering any questions the
        // shell asks its terminal on the way out.
        while !pane.child_exited() {
            assert!(Instant::now() < deadline, "the shell never exited");
            pane.read_ready();
            pane.flush();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Reaped: there's nothing to signal or reap again.
        pane.kill();
        assert!(pane.into_unreaped_child().is_none());
    }
}
