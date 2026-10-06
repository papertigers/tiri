// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The tiri client: connects to the server (starting one if needed), puts
//! the terminal in raw mode, and relays between the two. The server does all
//! the drawing; the client just forwards input and writes out what it's
//! sent.

use std::io::{self, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::{ExecutableCommand, cursor, event, terminal};
use rustix::fs::{FlockOperation, flock};

use crate::probe::{self, TerminalInfo};
use crate::protocol::{
    ClientMsg, Decoder, ExitReason, Hello, PASTE_CHUNK, ServerMsg, Target,
    recv, send,
};
use crate::socket;

/// How long to wait for a freshly started server to start listening.
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(3);

/// Attaches this terminal to `target`, starting a server if none is running.
pub fn attach(socket: &Path, target: Target) -> Result<()> {
    let terminal_info = detect_terminal();
    // Thumbnails if the terminal can show them, unless told otherwise.
    let kitty_overview = match std::env::var("TIRI_KITTY_OVERVIEW").as_deref() {
        Ok("0") => false,
        Ok(_) => true,
        Err(_) => terminal_info.kitty_graphics,
    };
    // Before starting a server that would have nobody to serve.
    let (width, height) = terminal::size().context(
        "couldn't get the terminal's size; tiri needs to run in a terminal",
    )?;
    let talking =
        || format!("couldn't talk to the tiri server at {}", socket.display());
    let mut stream = connect_or_start(socket)?;
    let cell_pixels = probed_cell_pixels(&terminal_info, width, height)
        .or_else(size_cell_pixels);
    send(
        &mut stream,
        &ClientMsg::Hello(Hello {
            width,
            height,
            target,
            cwd: std::env::current_dir()
                .context("couldn't read the current directory")?,
            kitty_overview,
            colors: terminal_info.colors,
            cell_pixels,
        }),
    )
    .with_context(talking)?;
    let mut decoder = Decoder::from_server();
    match recv(&mut stream, &mut decoder).with_context(talking)? {
        Some(ServerMsg::Attached) => {}
        Some(ServerMsg::Error(e)) => bail!(e),
        Some(other) => bail!("unexpected reply from the server: {other:?}"),
        None => bail!(
            "the server closed the connection; its log may say why: {}",
            socket::log_path(socket).display()
        ),
    }

    let (input_failed, input_error) = mpsc::channel();
    let reason = {
        let _guard = TerminalGuard::enter()?;
        let mut writer = stream.try_clone()?;
        thread::spawn(move || {
            loop {
                let event = match event::read() {
                    Ok(event) => event,
                    Err(e) => {
                        // Hang up, so the relay below stops too and the
                        // terminal is put back before the error is shown.
                        let _ = input_failed.send(e);
                        let _ = writer.shutdown(Shutdown::Both);
                        break;
                    }
                };
                // A resize may come with new cell proportions, say from a
                // font size change. Only send them when the terminal says:
                // many, Ghostty among them, leave pixel sizes out, and the
                // size probed at attach still stands.
                let resized = matches!(event, event::Event::Resize(..));
                let pixels = if resized { size_cell_pixels() } else { None };
                let sent = match event {
                    event::Event::Paste(text) => paste_messages(&text)
                        .try_for_each(|msg| send(&mut writer, &msg)),
                    event => send(&mut writer, &ClientMsg::Event(event)),
                };
                if sent.is_err()
                    || pixels.is_some_and(|p| {
                        send(&mut writer, &ClientMsg::CellPixels(Some(p)))
                            .is_err()
                    })
                {
                    break;
                }
            }
        });
        relay(&mut stream, &mut decoder, socket)
    };
    if let Ok(e) = input_error.try_recv() {
        return Err(e).context("couldn't read input from the terminal");
    }
    match reason? {
        ExitReason::Detached => println!("[detached]"),
        ExitReason::ServerExited => println!("[tiri server exited]"),
    }
    Ok(())
}

/// A paste as messages of at most [`PASTE_CHUNK`] bytes each, split
/// between characters.
///
/// The end-of-paste marker is taken out first, as xterm does: in a paste it
/// would end the program's bracketed paste early, and what followed would
/// arrive as if typed.
fn paste_messages(text: &str) -> impl Iterator<Item = ClientMsg> {
    let mut rest = text.replace("\x1b[201~", "");
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let mut end = rest.len().min(PASTE_CHUNK);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let tail = rest.split_off(end);
        let text = std::mem::replace(&mut rest, tail);
        done = rest.is_empty();
        Some(ClientMsg::Paste { text, last: done })
    })
}

/// The cell size in pixels from the probe: the terminal's own cell size
/// report, or its text area divided by the size in cells.
fn probed_cell_pixels(
    info: &TerminalInfo,
    cols: u16,
    rows: u16,
) -> Option<(u16, u16)> {
    info.cell_pixels.or_else(|| {
        let (w, h) = info.area_pixels?;
        (cols > 0 && rows > 0).then(|| (w / cols, h / rows))
    })
}

/// The cell size in pixels from the terminal's size, if it includes pixels.
fn size_cell_pixels() -> Option<(u16, u16)> {
    let size = terminal::window_size().ok()?;
    (size.width > 0 && size.height > 0 && size.columns > 0 && size.rows > 0)
        .then(|| (size.width / size.columns, size.height / size.rows))
}

/// Asks this terminal what it supports and which colors it uses. Needs
/// raw mode for a moment, so the answers aren't echoed.
fn detect_terminal() -> TerminalInfo {
    if terminal::enable_raw_mode().is_err() {
        return TerminalInfo::default();
    }
    let info = probe::probe().unwrap_or_default();
    let _ = terminal::disable_raw_mode();
    info
}

/// Writes the server's output to the terminal until it says goodbye.
fn relay(
    stream: &mut UnixStream,
    decoder: &mut Decoder,
    socket: &Path,
) -> Result<ExitReason> {
    let mut stdout = io::stdout().lock();
    loop {
        let msg = recv(stream, decoder).map_err(|e| {
            // A message that doesn't decode says what's wrong itself.
            if !e.is::<io::Error>() {
                return e;
            }
            e.context(format!(
                "lost connection to the tiri server; its log may say why: {}",
                socket::log_path(socket).display()
            ))
        })?;
        match msg {
            Some(ServerMsg::Output(bytes)) => {
                stdout.write_all(&bytes)?;
                stdout.flush()?;
            }
            Some(ServerMsg::Exit(reason)) => return Ok(reason),
            Some(ServerMsg::Error(e)) => bail!(e),
            Some(_) => {}
            // A server that's shutting down says so first.
            None => bail!(
                "the tiri server went away unexpectedly; its log may say why: {}",
                socket::log_path(socket).display()
            ),
        }
    }
}

/// Prints the server's workspaces.
pub fn list(socket: &Path) -> Result<()> {
    let Some(mut stream) = connect(socket)? else {
        println!("no tiri server running");
        return Ok(());
    };
    send(&mut stream, &ClientMsg::List)?;
    match recv(&mut stream, &mut Decoder::from_server())? {
        Some(ServerMsg::Workspaces(workspaces)) => {
            for ws in workspaces {
                if ws.panes == 0 && ws.name.is_none() {
                    continue;
                }
                let plural = |n: usize, word: &str| {
                    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
                };
                let attached = if ws.clients > 0 {
                    format!(" ({} attached)", plural(ws.clients, "client"))
                } else {
                    String::new()
                };
                println!(
                    "{}: {}{attached}",
                    ws.label,
                    plural(ws.panes, "pane")
                );
            }
            Ok(())
        }
        Some(other) => bail!("unexpected reply from the server: {other:?}"),
        None => bail!(
            "the server closed the connection; its log may say why: {}",
            socket::log_path(socket).display()
        ),
    }
}

pub fn kill_server(socket: &Path) -> Result<()> {
    let Some(mut stream) = connect(socket)? else {
        println!("no tiri server running");
        return Ok(());
    };
    send(&mut stream, &ClientMsg::KillServer)?;
    // Wait for the server to hang up, so it's gone when we return.
    let _ = recv::<ServerMsg>(&mut stream, &mut Decoder::from_server());
    Ok(())
}

/// Connects to a running server, or returns None if there isn't one.
fn connect(socket: &Path) -> Result<Option<UnixStream>> {
    match UnixStream::connect(socket) {
        Ok(stream) => Ok(Some(stream)),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e).with_context(|| {
            format!("couldn't connect to {}", socket.display())
        }),
    }
}

/// Connects to the server, starting it first if none is running.
fn connect_or_start(socket: &Path) -> Result<UnixStream> {
    if let Some(stream) = connect(socket)? {
        return Ok(stream);
    }
    // One client at a time decides there's no server and starts one, and
    // holds the lock until its server answers. Otherwise two starting
    // together could each clear away the other's server's socket.
    let lock_path = socket::lock_path(socket);
    let lock = std::fs::File::create(&lock_path)
        .with_context(|| format!("couldn't create {}", lock_path.display()))?;
    flock(&lock, FlockOperation::LockExclusive)
        .with_context(|| format!("couldn't lock {}", lock_path.display()))?;
    // Someone else may have started it while we waited for the lock.
    if let Some(stream) = connect(socket)? {
        return Ok(stream);
    }
    // A socket nobody answers on was left by a server that died.
    if socket.exists() {
        std::fs::remove_file(socket).with_context(|| {
            format!("couldn't remove stale {}", socket.display())
        })?;
    }
    start_server(socket)?;
    let deadline = Instant::now() + SERVER_START_TIMEOUT;
    loop {
        if let Some(stream) = connect(socket)? {
            return Ok(stream);
        }
        if Instant::now() > deadline {
            bail!(
                "the tiri server didn't start; see {}",
                socket::log_path(socket).display()
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Starts `tiri server` in the background, in its own session so it
/// outlives this terminal. Its errors go to a log beside the socket.
///
/// This is the classic daemon double fork. After `setsid` the child is a
/// session leader with no controlling terminal, and on System V systems
/// like illumos such a process adopts the next terminal it sets up as its
/// own, even one opened with `O_NOCTTY`. For the server that's the first
/// pane's PTY, which then hangs it up with SIGHUP. So the session leader
/// forks again and exits, leaving the server a plain session member, which
/// can never acquire a controlling terminal.
fn start_server(socket: &Path) -> Result<()> {
    let log_path = socket::log_path(socket);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| {
            format!("couldn't open the server log {}", log_path.display())
        })?;
    let exe =
        std::env::current_exe().context("couldn't find the tiri executable")?;
    let mut command = Command::new(exe);
    command
        .arg("--socket")
        .arg(socket)
        .arg("server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        // Not wherever this client happens to be: the server would keep
        // that directory busy for as long as it runs. Panes are told where
        // to start by the client that opens them.
        .current_dir("/");
    // SAFETY: the closure runs between fork and exec, where only
    // async-signal-safe calls are allowed: another thread may have held a
    // lock (the allocator's, say) at the fork, and here it's held forever.
    // setsid, fork and _exit are such calls, and nothing in the closure
    // allocates or locks, error paths included: both make an io::Error from
    // an errno, which is a plain number.
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid()?;
            match libc::fork() {
                -1 => Err(io::Error::last_os_error()),
                // The grandchild goes on to exec the server.
                0 => Ok(()),
                // The session leader is done.
                _ => libc::_exit(0),
            }
        });
    }
    // Reap the session leader, which exits straight away. The server itself
    // is adopted by init, so nothing waits on it.
    let mut leader =
        command.spawn().context("couldn't start the tiri server")?;
    leader.wait().context("couldn't wait for the tiri server to start")?;
    Ok(())
}

/// Puts the terminal into raw mode on the alternate screen, and restores it
/// on drop or panic.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        // From here, dropping the guard puts the terminal back, whichever
        // of the steps below fails.
        let guard = Self;
        TERMINAL_TAKEN.store(true, Ordering::Relaxed);
        let mut out = io::stdout();
        out.execute(terminal::EnterAlternateScreen)?;
        out.execute(event::EnableBracketedPaste)?;
        out.execute(event::EnableMouseCapture)?;
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // So the message lands on a usable screen. Not once the guard
            // has put the terminal back already.
            if TERMINAL_TAKEN.swap(false, Ordering::Relaxed) {
                restore();
            }
            hook(info);
        }));
        Ok(guard)
    }
}

/// Whether a [`TerminalGuard`] has the terminal in raw mode.
static TERMINAL_TAKEN: AtomicBool = AtomicBool::new(false);

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if TERMINAL_TAKEN.swap(false, Ordering::Relaxed) {
            restore();
        }
    }
}

fn restore() {
    let mut out = io::stdout();
    let _ = out.execute(event::DisableMouseCapture);
    let _ = out.execute(event::DisableBracketedPaste);
    let _ = out.execute(terminal::LeaveAlternateScreen);
    let _ = out.execute(cursor::Show);
    let _ = terminal::disable_raw_mode();
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(text: &str) -> Vec<(String, bool)> {
        paste_messages(text)
            .map(|msg| match msg {
                ClientMsg::Paste { text, last } => (text, last),
                other => panic!("not a paste: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn pastes_go_in_chunks_split_between_characters() {
        let text = "é".repeat(PASTE_CHUNK); // two bytes each
        let parts = parts(&text);
        assert_eq!(parts.len(), 2);
        assert!(parts.iter().all(|(t, _)| t.len() <= PASTE_CHUNK));
        assert_eq!(
            parts.iter().map(|(t, _)| t.as_str()).collect::<String>(),
            text
        );
        assert_eq!(
            parts.iter().map(|(_, last)| *last).collect::<Vec<_>>(),
            [false, true]
        );
    }

    #[test]
    fn small_and_empty_pastes_are_one_part() {
        assert_eq!(parts("hi"), [("hi".to_owned(), true)]);
        assert_eq!(parts(""), [(String::new(), true)]);
    }

    #[test]
    fn pastes_cant_end_a_bracketed_paste_early() {
        assert_eq!(
            parts("a\x1b[201~rm -rf ~\r"),
            [("arm -rf ~\r".to_owned(), true)]
        );
    }
}
