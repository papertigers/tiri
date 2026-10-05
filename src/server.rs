//! The tiri server: owns every pane and workspace, and serves the clients
//! attached over its Unix socket. Everything runs on one `polling` loop:
//! pane PTYs, the listening socket and client connections are all
//! non-blocking, so a busy pane or a slow client never holds up the rest.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::Event;
use polling::{Event as PollEvent, Events, Poller};

use crate::app::{App, Client};
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
/// A server started for a client that never attaches gives up after this.
const STARTUP_GRACE: Duration = Duration::from_secs(10);

/// One connection to the socket: an attached client, or a one-off request
/// such as `tiri ls`.
struct Connection {
    stream: UnixStream,
    decoder: Decoder,
    /// Encoded messages the socket hasn't accepted yet.
    outgoing: Vec<u8>,
    /// Set once the connection's hello has been accepted.
    client: Option<Client>,
    /// Close once `outgoing` has been sent.
    closing: bool,
    /// The other end hung up, or the connection failed.
    dead: bool,
}

impl Connection {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            decoder: Decoder::default(),
            outgoing: Vec::new(),
            client: None,
            closing: false,
            dead: false,
        }
    }

    fn send(&mut self, msg: &ServerMsg) {
        self.outgoing.extend(encode(msg));
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
                Err(_) => {
                    self.dead = true;
                    return;
                }
            }
        }
    }

    /// Takes in whatever the client has sent.
    fn read(&mut self) {
        let mut buf = [0u8; 64 * 1024];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    self.dead = true;
                    return;
                }
                Ok(n) => self.decoder.push(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.dead = true;
                    return;
                }
            }
        }
    }

    fn finished(&self) -> bool {
        self.dead || (self.closing && self.outgoing.is_empty())
    }
}

pub fn run(socket: &Path) -> Result<()> {
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("couldn't listen on {}", socket.display()))?;
    listener.set_nonblocking(true)?;
    eprintln!(
        "tiri server {} listening on {}",
        std::process::id(),
        socket.display()
    );
    let result = serve(&listener);
    let _ = std::fs::remove_file(socket);
    result
}

fn serve(listener: &UnixListener) -> Result<()> {
    let poller = Arc::new(Poller::new()?);
    // SAFETY: deleted from the poller before `serve` returns.
    unsafe { poller.add(listener, PollEvent::readable(LISTENER_KEY))? };
    let result = serve_with(listener, &poller);
    let _ = poller.delete(listener);
    result
}

fn serve_with(listener: &UnixListener, poller: &Arc<Poller>) -> Result<()> {
    let mut app = App::new(&[], Arc::clone(poller));
    let mut connections: HashMap<usize, Connection> = HashMap::new();
    let mut next_connection = CONNECTION_KEY_BASE;
    let mut events = Events::new();
    let started = Instant::now();
    let mut had_panes = false;
    let mut kill = false;
    let mut animating = false;
    let mut last_tick = Instant::now();

    loop {
        // Wake for the next animation frame, a pane or thumbnail deadline,
        // or giving up on a first client that never came; otherwise only
        // for I/O.
        let client_deadlines = (connections.values())
            .filter_map(|c| c.client.as_ref())
            .filter_map(|client| app.next_deadline(client));
        let deadline = [
            animating.then(|| Instant::now() + FRAME),
            (!had_panes).then_some(started + STARTUP_GRACE),
        ]
        .into_iter()
        .flatten()
        .chain(client_deadlines)
        .min();

        // `polling` reports each source once per arming, so re-arm them all.
        app.arm_panes();
        poller.modify(listener, PollEvent::readable(LISTENER_KEY))?;
        for (&key, connection) in &connections {
            let interest = PollEvent::new(key, true, !connection.outgoing.is_empty());
            let _ = poller.modify(&connection.stream, interest);
        }
        events.clear();
        match poller.wait(
            &mut events,
            deadline.map(|d| d.saturating_duration_since(Instant::now())),
        ) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }

        for event in events.iter() {
            match event.key {
                LISTENER_KEY => accept(listener, poller, &mut connections, &mut next_connection),
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
            loop {
                match connection.decoder.next::<ClientMsg>() {
                    Ok(Some(msg)) => handle(&mut app, connection, msg, &mut kill),
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("tiri server: dropping a client: {e:#}");
                        connection.dead = true;
                        break;
                    }
                }
            }
            if connection
                .client
                .as_ref()
                .is_some_and(Client::detach_requested)
            {
                let mut client = connection.client.take().expect("checked above");
                app.detach(&mut client);
                // Thumbnail cleanup, before the client leaves the screen.
                connection.send(&ServerMsg::Output(client.take_escapes()));
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

        had_panes |= !app.is_empty();
        let abandoned = !had_panes && connections.is_empty() && started.elapsed() > STARTUP_GRACE;
        if kill || app.quit || (had_panes && app.is_empty()) || abandoned {
            shut_down(&mut app, poller, connections);
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

        for connection in connections.values_mut() {
            if connection.closing || connection.outgoing.len() > BACKLOG_LIMIT {
                continue;
            }
            let Some(client) = connection.client.as_mut() else {
                continue;
            };
            let (frame, cursor) = app.draw(client);
            let mut bytes = Vec::new();
            client.render(&mut bytes, frame, cursor)?;
            connection.send(&ServerMsg::Output(bytes));
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

/// Accepts every pending connection.
fn accept(
    listener: &UnixListener,
    poller: &Poller,
    connections: &mut HashMap<usize, Connection>,
    next_connection: &mut usize,
) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(e) = stream.set_nonblocking(true) {
                    eprintln!("tiri server: couldn't accept a client: {e}");
                    continue;
                }
                let key = *next_connection;
                *next_connection += 1;
                // SAFETY: deleted from the poller when the connection is
                // dropped from `connections`, or at shutdown.
                if let Err(e) = unsafe { poller.add(&stream, PollEvent::readable(key)) } {
                    eprintln!("tiri server: couldn't watch a client: {e}");
                    continue;
                }
                connections.insert(key, Connection::new(stream));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                eprintln!("tiri server: accept failed: {e}");
                return;
            }
        }
    }
}

fn handle(app: &mut App, connection: &mut Connection, msg: ClientMsg, kill: &mut bool) {
    match msg {
        ClientMsg::Hello(hello) if connection.client.is_none() => {
            eprintln!(
                "tiri server: client attaching: {}x{} cells, cell pixels {:?}, \
                 kitty overview {}, foreground {:?}, background {:?}",
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
                eprintln!("tiri server: {e:#}");
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
            connection.send(&ServerMsg::Output(client.take_escapes()));
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
