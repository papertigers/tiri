// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The tiri client: connects to the server (starting one if needed), puts
//! the terminal in raw mode, and draws. The server sends the layout and
//! each pane's output; the client keeps copies of the panes and draws and
//! animates them itself, sending the server what's typed and what's asked
//! of the layout. Also the bridge that relays to a server from another
//! machine.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use crossterm::{ExecutableCommand, cursor, event, terminal};
use rustix::fs::{FlockOperation, flock};

use crate::app::{App, Client};
use crate::colors::Palette;
use crate::config::{self, Config};
use crate::escape;
use crate::link::{Link, LinkReader, LinkWriter, Server};
use crate::probe::{self, TerminalInfo};
use crate::protocol::{
    ClientMsg, Decoder, ExitReason, Hello, ServerMsg, Target, recv, send,
};
use crate::socket;

/// How long to wait for a freshly started server to start listening.
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(3);
/// How often to look for a starting server's socket.
const SERVER_START_POLL: Duration = Duration::from_millis(20);
/// The shortest time between frames.
const FRAME: Duration = Duration::from_millis(16);

/// Attaches this terminal to `target`, starting a server if none is running.
pub fn attach(server: &Server, target: Target) -> Result<()> {
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
    // A config with mistakes gives way to the default one, with a word in
    // the status bar: refusing would shut you out of your own panes.
    // The whole of what's wrong goes to stderr once the terminal's back.
    let (config, config_error) = match config::default_path() {
        Some(path) => match Config::load(&path) {
            Ok(config) => (config, None),
            Err(e) => (Config::default(), Some(e)),
        },
        None => (Config::default(), None),
    };
    let talking =
        || format!("couldn't talk to the tiri server at {}", server.describe());
    let mut link = match server {
        Server::Local(socket) => Link::socket(connect_or_start(socket)?)?,
        Server::Remote { host, socket } => {
            Link::ssh(host, socket.as_deref(), config.remote(host))?
        }
    };
    // The client's directory means nothing on another machine.
    let cwd = match server {
        Server::Local(_) => Some(
            std::env::current_dir()
                .context("couldn't read the current directory")?,
        ),
        Server::Remote { .. } => None,
    };
    let cell_pixels = probed_cell_pixels(&terminal_info, width, height)
        .or_else(size_cell_pixels);
    send(
        &mut link,
        &ClientMsg::Hello(Hello {
            width,
            height,
            target,
            cwd,
            colors: terminal_info.colors.clone(),
            agent: match server {
                Server::Local(_) => ssh_agent(),
                Server::Remote { .. } => None,
            },
        }),
    )
    .with_context(talking)?;
    let mut decoder = Decoder::from_server();
    match recv(&mut link, &mut decoder).with_context(talking)? {
        Some(ServerMsg::Attached) => {}
        Some(ServerMsg::Error(e)) => bail!(e),
        Some(other) => bail!("unexpected reply from the server: {other:?}"),
        None => bail!(
            "the server closed the connection; its log may say why: {}",
            server.log_hint()
        ),
    }

    let mut app = App::new(config);
    let mut client = Client::new(
        width,
        height,
        Palette::from_reported(&terminal_info.colors),
        cell_pixels,
        kitty_overview,
    );
    if let Some(error) = &config_error {
        client.notify(format!("{}; using the default config", error.summary));
    }

    let reason = {
        let _guard = TerminalGuard::enter()?;
        let (reader, mut writer) = link.split();
        let (events, incoming) = mpsc::channel();
        spawn_input(events.clone());
        spawn_reader(reader, decoder, events);
        let result = run(&mut app, &mut client, &incoming, &mut writer, server);
        // Done with the server, so the reading thread stops, and ssh with
        // it if that's the way there.
        writer.hang_up();
        // Thumbnails out of the terminal, before leaving its screen.
        app.detach(&mut client);
        let mut out = io::stdout();
        let _ = out.write_all(&client.take_escapes());
        let _ = out.flush();
        result
    };
    if let Some(error) = config_error {
        eprintln!("tiri used the default config, since:\n{error}");
    }
    match reason? {
        ExitReason::Detached => println!("[detached]"),
        ExitReason::ServerExited => println!("[tiri server exited]"),
    }
    Ok(())
}

/// What wakes the client: the terminal or the server.
enum Incoming {
    Input(event::Event),
    InputFailed(io::Error),
    /// A message from the server, and how many bytes of messages have
    /// come, it included, for acknowledging.
    Server(ServerMsg, u64),
    /// The server hung up, or sent something that makes no sense: None
    /// for the first.
    Lost(Option<anyhow::Error>),
}

/// Reads the terminal's input on a thread of its own.
fn spawn_input(events: mpsc::Sender<Incoming>) {
    thread::spawn(move || {
        loop {
            let incoming = match event::read() {
                Ok(event) => Incoming::Input(event),
                Err(e) => {
                    let _ = events.send(Incoming::InputFailed(e));
                    return;
                }
            };
            if events.send(incoming).is_err() {
                return;
            }
        }
    });
}

/// Reads what the server sends on a thread of its own.
fn spawn_reader(
    mut reader: LinkReader,
    mut decoder: Decoder,
    events: mpsc::Sender<Incoming>,
) {
    thread::spawn(move || {
        loop {
            let incoming = match recv(&mut reader, &mut decoder) {
                Ok(Some(msg)) => Incoming::Server(msg, decoder.taken()),
                Ok(None) => Incoming::Lost(None),
                Err(e) => Incoming::Lost(Some(e)),
            };
            let lost = matches!(incoming, Incoming::Lost(_));
            if events.send(incoming).is_err() || lost {
                return;
            }
        }
    });
}

/// Draws and animates, takes input, and keeps the copies of the panes up
/// to date, until the server says goodbye.
fn run(
    app: &mut App,
    client: &mut Client,
    incoming: &mpsc::Receiver<Incoming>,
    writer: &mut LinkWriter,
    server: &Server,
) -> Result<ExitReason> {
    let mut stdout = io::stdout().lock();
    let mut animating = false;
    let mut dirty = true;
    let mut last_tick = Instant::now();
    let mut last_draw = last_tick - FRAME;
    // What the server's been told this client has taken in, and what it
    // has.
    let (mut acked, mut taken) = (0, 0);
    loop {
        // Wake for the next frame, if one's owed, or anything else due.
        let next_frame = (dirty || animating).then(|| last_draw + FRAME);
        let deadline =
            [next_frame, app.next_deadline(client)].into_iter().flatten().min();
        let first = match deadline {
            Some(deadline) => {
                let wait = deadline.saturating_duration_since(Instant::now());
                match incoming.recv_timeout(wait) {
                    Ok(incoming) => Some(incoming),
                    // Whatever was due changes the screen: an animation's
                    // next step (its last included, which ends it), a
                    // pane's update showing, or a notice going. So a frame
                    // is owed, even if it's too soon to draw one now.
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        dirty = true;
                        None
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        bail!("lost the terminal and the server")
                    }
                }
            }
            None => Some(incoming.recv()?),
        };
        // Everything waiting goes in before the next frame.
        for incoming in first.into_iter().chain(incoming.try_iter()) {
            dirty = true;
            match incoming {
                Incoming::Input(event) => input(app, client, event),
                Incoming::InputFailed(e) => {
                    return Err(e)
                        .context("couldn't read input from the terminal");
                }
                Incoming::Server(ServerMsg::Exit(reason), _) => {
                    return Ok(reason);
                }
                Incoming::Server(ServerMsg::Error(e), _) => bail!(e),
                Incoming::Server(msg, total) => {
                    app.apply(client, msg);
                    taken = total;
                }
                // A message that doesn't decode says what's wrong itself.
                Incoming::Lost(Some(e)) if !e.is::<io::Error>() => {
                    return Err(e);
                }
                Incoming::Lost(e) => {
                    let lost = format!(
                        "lost connection to the tiri server; its log may say \
                         why: {}",
                        server.log_hint()
                    );
                    return Err(match e {
                        Some(e) => e.context(lost),
                        None => anyhow!(lost),
                    });
                }
            }
        }
        // Taken in, so the server can send more.
        let ack = (taken > acked).then_some(ClientMsg::Ack(taken));
        acked = taken;
        for msg in app.take_outbox(client).into_iter().chain(ack) {
            // A server that's gone says so on the reading side.
            if send(writer, &msg).is_err() {
                break;
            }
        }

        let now = Instant::now();
        app.expire_syncs(now);
        // Don't let a long idle wait turn into one giant animation step.
        let dt = if animating { now - last_tick } else { Duration::ZERO };
        last_tick = now;
        // Effects and fades run on frames as animations do.
        animating = app.tick(dt.max(Duration::from_millis(1)))
            || client.effects_running();
        if !(dirty || animating) || now < last_draw + FRAME {
            // Nothing to draw, or too soon after the last frame: the next
            // comes when it's due.
            continue;
        }
        let (frame, cursor) = app.draw(client);
        client.render(&mut stdout, frame, cursor)?;
        stdout.flush()?;
        last_draw = now;
        dirty = false;
    }
}

/// Acts on input from the terminal.
fn input(app: &mut App, client: &mut Client, event: event::Event) {
    match event {
        event::Event::Key(key) => app.key(client, key),
        event::Event::Paste(text) => app.paste(client, &text),
        event::Event::Mouse(mouse) => app.mouse(client, mouse),
        event::Event::Resize(width, height) => {
            app.resize(client, width, height);
            // A resize may come with new cell proportions, say from a font
            // size change. Only when the terminal says: many, Ghostty among
            // them, leave pixel sizes out, and the size probed at attach
            // still stands.
            if let Some(pixels) = size_cell_pixels() {
                client.set_cell_pixels(Some(pixels));
            }
        }
        _ => {}
    }
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

/// Prints the server's workspaces.
pub fn list(socket: &Path) -> Result<()> {
    let Some(mut stream) = connect(socket)? else {
        println!("{NO_SERVER}");
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
        println!("{NO_SERVER}");
        return Ok(());
    };
    send(&mut stream, &ClientMsg::KillServer)?;
    // Wait for the server to hang up, so it's gone when we return.
    let _ = recv::<ServerMsg>(&mut stream, &mut Decoder::from_server());
    Ok(())
}

/// What `ls` and `kill-server` say when there's no server.
const NO_SERVER: &str = "no tiri server running";

/// Relays between this process's standard input and output and the server
/// at `socket`, for a client on another machine connected through ssh,
/// starting the server if none is running.
pub fn bridge(socket: &Path) -> Result<()> {
    let mut stream = connect_or_start(socket)?;
    // Before the client's hello, so it's known when the client attaches.
    send(&mut stream, &ClientMsg::Agent(ssh_agent()))?;
    let mut to_server = stream.try_clone()?;
    thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut to_server);
        // The client is gone: the server will hang up in turn.
        let _ = to_server.shutdown(Shutdown::Write);
    });
    // Compressed, and flushed as it comes: messages are often small, and
    // the client is waiting on them.
    let mut from_server = stream;
    let mut out = crate::link::compressing(io::stdout().lock())?;
    let mut buf = vec![0u8; crate::protocol::READ_CHUNK];
    loop {
        match from_server.read(&mut buf) {
            Ok(0) => {
                // The end of the stream, so the client reads it as one.
                out.finish()?.flush()?;
                return Ok(());
            }
            Ok(n) => {
                out.write_all(&buf[..n])?;
                out.flush()?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// This process's ssh agent, as `$SSH_AUTH_SOCK` says: for a bridge, the
/// one ssh forwarded, if it was asked to.
fn ssh_agent() -> Option<PathBuf> {
    std::env::var_os("SSH_AUTH_SOCK")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
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
        thread::sleep(SERVER_START_POLL);
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
        // The server switches it from here, as programs want more or less.
        let mut mouse = Vec::new();
        escape::MouseReporting::default().enable(&mut mouse);
        out.write_all(&mouse)?;
        out.flush()?;
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
