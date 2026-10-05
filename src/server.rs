//! The tiri server: owns every pane and workspace, and serves the clients
//! attached over its Unix socket. Everything runs on one `polling` loop:
//! pane PTYs, the listening socket and client connections are all
//! non-blocking, so a busy pane or a slow client never holds up the rest.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::Event;
use polling::{Event as PollEvent, Events, Poller};

use crate::app::{App, Client};
use crate::config;
use crate::protocol::{ClientMsg, Decoder, ExitReason, ServerMsg, encode};

/// Pane PTYs are keyed by pane id, which is a u32; these sit above them.
/// (`usize::MAX` is reserved by `polling`.)
const LISTENER_KEY: usize = usize::MAX - 1;
const CONNECTION_KEY_BASE: usize = 1 << 32;

const FRAME: Duration = Duration::from_millis(16);
/// A client with more than this waiting to be sent gets no new frames until
/// it catches up. Its next frame is then diffed against the last one it was
/// sent, so it skips the states in between but never sees a broken screen.
const BACKLOG_LIMIT: usize = 1 << 20;
/// The most read from one connection per wakeup, so a client flooding the
/// socket can't keep the server from everything else.
const READ_BUDGET: usize = 256 * 1024;
/// The most terminal output in one message, far below what the client
/// refuses to take.
const MAX_OUTPUT: usize = 1 << 20;
/// A server started for a client that never attaches gives up after this.
const STARTUP_GRACE: Duration = Duration::from_secs(10);
/// How long to stop accepting after accepting fails (say, out of file
/// descriptors), rather than retrying in a tight loop.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// One connection to the socket: an attached client, or a one-off request
/// such as `tiri ls`.
struct Connection {
    /// Its key with the poller, which also names it in the log.
    key: usize,
    stream: UnixStream,
    decoder: Decoder,
    /// Encoded messages the socket hasn't accepted yet.
    outgoing: Vec<u8>,
    /// Set once the connection's hello has been accepted.
    client: Option<Client>,
    /// Close once `outgoing` has been sent.
    closing: bool,
    /// The other end has sent all it's going to. What it sent first is
    /// still handled.
    eof: bool,
    /// The connection failed, or has been dealt with after `eof`.
    dead: bool,
    /// A frame was held back while this was behind, so it's owed one.
    owed_frame: bool,
}

impl Connection {
    fn new(key: usize, stream: UnixStream) -> Self {
        Self {
            key,
            stream,
            decoder: Decoder::default(),
            outgoing: Vec::new(),
            client: None,
            closing: false,
            eof: false,
            dead: false,
            owed_frame: false,
        }
    }

    /// How log lines refer to it.
    fn name(&self) -> String {
        format!("connection {}", self.key - CONNECTION_KEY_BASE)
    }

    fn send(&mut self, msg: &ServerMsg) {
        self.outgoing.extend(encode(msg));
    }

    /// Sends bytes for the client's terminal, in messages of a size the
    /// client will take however much there is (a big clipboard copy, say).
    fn send_output(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(MAX_OUTPUT) {
            self.send(&ServerMsg::Output(chunk.to_vec()));
        }
    }

    /// Sends as much queued output as the socket will take.
    fn flush(&mut self) {
        while !self.outgoing.is_empty() {
            match self.stream.write(&self.outgoing) {
                Ok(n) => {
                    self.outgoing.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return self.fail("writing", &e),
            }
        }
    }

    /// Takes in what the client has sent, up to [`READ_BUDGET`]; the rest
    /// waits for the next wakeup.
    fn read(&mut self) {
        let mut buf = [0u8; 64 * 1024];
        let mut budget = READ_BUDGET;
        while budget > 0 {
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    self.eof = true;
                    return;
                }
                Ok(n) => {
                    self.decoder.push(&buf[..n]);
                    budget = budget.saturating_sub(n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return self.fail("reading", &e),
            }
        }
    }

    /// Gives up on the connection after an I/O error, logging it unless it's
    /// just the client going away.
    fn fail(&mut self, doing: &str, e: &io::Error) {
        if !matches!(
            e.kind(),
            io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
        ) {
            log::warn!("{}: {doing} failed: {e}", self.name());
        }
        self.dead = true;
    }

    /// Whether to wait on this client's deadlines. One that's closing or
    /// behind won't be drawn for, so they'd only wake the server for nothing.
    fn wants_frames(&self) -> bool {
        !self.closing && self.outgoing.len() <= BACKLOG_LIMIT
    }

    fn finished(&self) -> bool {
        self.dead || (self.closing && self.outgoing.is_empty())
    }
}

/// Runs the server until it has nothing left to serve. Whatever stops it
/// early is in the log by the time this returns.
pub fn run(socket: &Path) -> Result<()> {
    init_logging();
    let result = listen(socket);
    if let Err(e) = &result {
        log::error!("stopping: {e:#}");
    }
    result
}

fn listen(socket: &Path) -> Result<()> {
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("couldn't listen on {}", socket.display()))?;
    listener
        .set_nonblocking(true)
        .context("couldn't make the socket non-blocking")?;
    log::info!("listening on {}", socket.display());
    // To tell this socket from one a later server puts at the same path.
    let bound = std::fs::metadata(socket).map(|meta| (meta.dev(), meta.ino()));
    let config_path = config::default_path();
    match &config_path {
        Some(path) => log::info!("config: {}", path.display()),
        None => log::warn!("no $HOME or $XDG_CONFIG_HOME, so no config file"),
    }
    let result = serve(&listener, config_path);
    let current = std::fs::metadata(socket).map(|meta| (meta.dev(), meta.ino()));
    if let (Ok(bound), Ok(current)) = (bound, current)
        && bound == current
    {
        let _ = std::fs::remove_file(socket);
    }
    result
}

/// Logs to stderr, which is the log file beside the socket. Each line has
/// the server's pid, since the file outlives many servers. `TIRI_LOG` sets
/// the level, as `RUST_LOG` would (default `info`).
fn init_logging() {
    env_logger::Builder::from_env(env_logger::Env::new().filter_or("TIRI_LOG", "info"))
        .format(|buf, record| {
            writeln!(
                buf,
                "{} {:<5} tiri[{}] {}",
                buf.timestamp_seconds(),
                record.level(),
                std::process::id(),
                record.args()
            )
        })
        .init();
}

fn serve(listener: &UnixListener, config_path: Option<PathBuf>) -> Result<()> {
    let poller = Arc::new(Poller::new().context("couldn't create a poller")?);
    // SAFETY: deleted from the poller before `serve` returns.
    unsafe { poller.add(listener, PollEvent::readable(LISTENER_KEY)) }
        .context("couldn't watch the socket")?;
    let mut app = App::new(Arc::clone(&poller), config_path);
    let mut connections = HashMap::new();
    let result = event_loop(listener, &poller, &mut app, &mut connections);
    // However the loop ended, panes are killed and clients told.
    shut_down(&mut app, &poller, std::mem::take(&mut connections));
    let _ = poller.delete(listener);
    result
}

/// Runs until the server should stop: asked to, or out of panes.
fn event_loop(
    listener: &UnixListener,
    poller: &Poller,
    app: &mut App,
    connections: &mut HashMap<usize, Connection>,
) -> Result<()> {
    let mut next_connection = CONNECTION_KEY_BASE;
    let mut events = Events::new();
    let started = Instant::now();
    let mut had_panes = false;
    let mut kill = false;
    let mut animating = false;
    let mut last_tick = Instant::now();
    let mut last_draw = Instant::now();
    let mut accept_paused_until: Option<Instant> = None;

    loop {
        if accept_paused_until.is_some_and(|until| Instant::now() >= until) {
            accept_paused_until = None;
        }

        // Wake for the next animation frame, a pane or thumbnail deadline,
        // accepting again, reaping closed panes, or giving up on a first
        // client that never came; otherwise only for I/O.
        let client_deadlines = (connections.values())
            .filter(|c| c.wants_frames())
            .filter_map(|c| c.client.as_ref())
            .filter_map(|client| app.next_deadline(client));
        let deadline = [
            animating.then(|| last_draw + FRAME),
            (!had_panes).then_some(started + STARTUP_GRACE),
            accept_paused_until,
            app.reap_deadline(),
        ]
        .into_iter()
        .flatten()
        .chain(client_deadlines)
        .min();

        // `polling` reports each source once per arming, so re-arm them all.
        app.arm_panes();
        if accept_paused_until.is_none() {
            poller
                .modify(listener, PollEvent::readable(LISTENER_KEY))
                .context("couldn't watch the socket")?;
        }
        for (&key, connection) in connections.iter_mut() {
            // After end of input a socket stays readable, to no purpose.
            let interest = PollEvent::new(key, !connection.eof, !connection.outgoing.is_empty());
            if let Err(e) = poller.modify(&connection.stream, interest) {
                log::warn!("{}: couldn't watch it: {e}", connection.name());
                connection.dead = true;
            }
        }
        events.clear();
        match poller.wait(
            &mut events,
            deadline.map(|d| d.saturating_duration_since(Instant::now())),
        ) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e).context("couldn't wait for events"),
        }

        for event in events.iter() {
            match event.key {
                LISTENER_KEY => {
                    if let Err(e) = accept(listener, poller, connections, &mut next_connection) {
                        log::warn!("couldn't accept a connection: {e}");
                        accept_paused_until = Some(Instant::now() + ACCEPT_BACKOFF);
                    }
                }
                key if key >= CONNECTION_KEY_BASE => {
                    if let Some(connection) = connections.get_mut(&key) {
                        if event.writable {
                            connection.flush();
                        }
                        if event.readable {
                            connection.read();
                        }
                    }
                }
                _ => app.pane_ready(event),
            }
        }

        for connection in connections.values_mut() {
            // A connection that's been answered and is closing has had its
            // say; anything more is dropped with it.
            while !connection.dead && !connection.closing {
                match connection.decoder.next::<ClientMsg>() {
                    Ok(Some(msg)) => handle(app, connection, msg, &mut kill),
                    Ok(None) => break,
                    Err(e) => {
                        log::warn!("{}: dropping it: {e:#}", connection.name());
                        connection.dead = true;
                    }
                }
            }
            // Nothing more is coming. One still owed a reply gets it first.
            connection.dead |= connection.eof && !connection.closing;
            if connection
                .client
                .as_ref()
                .is_some_and(Client::detach_requested)
            {
                let mut client = connection.client.take().expect("checked above");
                app.detach(&mut client);
                // Thumbnail cleanup, before the client leaves the screen.
                connection.send_output(&client.take_escapes());
                connection.send(&ServerMsg::Exit(ExitReason::Detached));
                connection.closing = true;
            }
        }

        // Programs copying with OSC 52 reach every attached clipboard.
        for text in app.take_copied() {
            for client in connections.values_mut().filter_map(|c| c.client.as_mut()) {
                client.copy(&text);
            }
        }
        app.reap_exited();

        had_panes |= !app.is_empty();
        // Nobody attached in time. Connections that never said hello don't
        // keep the server alive; shutting down tells them it's gone.
        let abandoned = !had_panes && started.elapsed() >= STARTUP_GRACE;
        if kill || app.quit || (had_panes && app.is_empty()) || abandoned {
            return Ok(());
        }

        let now = Instant::now();
        app.expire_syncs(now);
        // Don't let a long idle wait turn into one giant animation step.
        let dt = if animating {
            now - last_tick
        } else {
            Duration::ZERO
        };
        last_tick = now;
        animating = app.tick(dt.max(Duration::from_millis(1)));

        // Draw when something may have changed: a pane or a client said
        // something, a deadline passed, an animation's next frame is due,
        // or a client that was behind has caught up. Waking only because a
        // socket can take more output isn't a reason: drawing then would
        // answer a slow client with more frames.
        let changed = events.is_empty()
            || events.iter().any(|event| match event.key {
                LISTENER_KEY => false,
                key if key >= CONNECTION_KEY_BASE => event.readable,
                _ => true,
            });
        let frame_due = animating && now.duration_since(last_draw) >= FRAME;
        let caught_up = (connections.values()).any(|c| c.owed_frame && c.wants_frames());
        let drawing = changed || frame_due || caught_up;
        if drawing {
            last_draw = now;
        }

        for connection in connections.values_mut() {
            if connection.dead || connection.client.is_none() {
                continue;
            }
            if !drawing || !connection.wants_frames() {
                connection.owed_frame |= drawing;
                // Effects still running need their next frame in time.
                animating |= connection.wants_frames()
                    && connection
                        .client
                        .as_ref()
                        .is_some_and(Client::effects_running);
                continue;
            }
            connection.owed_frame = false;
            let Some(client) = connection.client.as_mut() else {
                continue;
            };
            let (frame, cursor) = app.draw(client);
            // Running effects need further frames, as animations do.
            animating |= client.effects_running();
            let mut bytes = Vec::new();
            if let Err(e) = client.render(&mut bytes, frame, cursor) {
                log::error!("{}: couldn't render: {e}", connection.name());
                connection.dead = true;
                continue;
            }
            connection.send_output(&bytes);
            connection.flush();
        }

        connections.retain(|_, connection| {
            if !connection.finished() {
                return true;
            }
            if let Some(mut client) = connection.client.take() {
                app.detach(&mut client);
            }
            let _ = poller.delete(&connection.stream);
            false
        });
    }
}

/// Accepts every pending connection. An error means accepting is failing
/// for now, rather than for one connection.
fn accept(
    listener: &UnixListener,
    poller: &Poller,
    connections: &mut HashMap<usize, Connection>,
    next_connection: &mut usize,
) -> io::Result<()> {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let key = *next_connection;
                *next_connection += 1;
                let connection = Connection::new(key, stream);
                if let Err(e) = connection.stream.set_nonblocking(true) {
                    log::warn!("{}: couldn't make it non-blocking: {e}", connection.name());
                    continue;
                }
                // SAFETY: deleted from the poller when the connection is
                // dropped from `connections`, or at shutdown.
                if let Err(e) = unsafe { poller.add(&connection.stream, PollEvent::readable(key)) }
                {
                    log::warn!("{}: couldn't watch it: {e}", connection.name());
                    continue;
                }
                connections.insert(key, connection);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

fn handle(app: &mut App, connection: &mut Connection, msg: ClientMsg, kill: &mut bool) {
    match msg {
        ClientMsg::Hello(hello) if connection.client.is_none() => {
            log::info!(
                "{}: attaching: {}x{} cells, cell pixels {:?}, \
                 kitty overview {}, foreground {:?}, background {:?}",
                connection.name(),
                hello.width,
                hello.height,
                hello.cell_pixels,
                hello.kitty_overview,
                hello.colors.foreground,
                hello.colors.background,
            );
            match app.attach(hello) {
                Ok(client) => {
                    connection.send(&ServerMsg::Attached);
                    connection.client = Some(client);
                }
                Err(e) => {
                    log::warn!("{}: couldn't attach: {e:#}", connection.name());
                    connection.send(&ServerMsg::Error(format!("{e:#}")));
                    connection.closing = true;
                }
            }
        }
        ClientMsg::Hello(_) => {}
        ClientMsg::Event(event) => {
            let Some(client) = connection.client.as_mut() else {
                return;
            };
            let result = match event {
                Event::Key(key) => app.key(client, key),
                Event::Paste(text) => {
                    app.paste(client, &text);
                    Ok(())
                }
                Event::Resize(width, height) => {
                    app.resize(client, width, height);
                    Ok(())
                }
                Event::Mouse(mouse) => {
                    app.mouse(client, mouse);
                    Ok(())
                }
                _ => Ok(()),
            };
            if let Err(e) = result {
                // Otherwise the key just seems to do nothing.
                client.notify(format!("{e:#}"));
                log::error!("{}: {e:#}", connection.name());
            }
        }
        ClientMsg::CellPixels(cell_pixels) => {
            if let Some(client) = connection.client.as_mut() {
                client.set_cell_pixels(cell_pixels);
            }
        }
        ClientMsg::List => {
            connection.send(&ServerMsg::Workspaces(app.workspace_infos()));
            connection.closing = true;
        }
        ClientMsg::KillServer => {
            *kill = true;
            connection.closing = true;
        }
    }
}

/// Kills every pane and tells every client the server is gone, giving each
/// a moment to receive it.
fn shut_down(app: &mut App, poller: &Poller, connections: HashMap<usize, Connection>) {
    app.shutdown();
    for (_, mut connection) in connections {
        if let Some(mut client) = connection.client.take() {
            app.detach(&mut client);
            connection.send_output(&client.take_escapes());
        }
        connection.send(&ServerMsg::Exit(ExitReason::ServerExited));
        let _ = poller.delete(&connection.stream);
        let _ = connection.stream.set_nonblocking(false);
        let _ = connection
            .stream
            .set_write_timeout(Some(Duration::from_secs(1)));
        let _ = connection.stream.write_all(&connection.outgoing);
    }
}
