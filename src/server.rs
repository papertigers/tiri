// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The tiri server: owns every pane and workspace, and serves the clients
//! attached over its Unix socket. It draws nothing: it passes each pane's
//! output on to every client, which keeps a copy of the pane and draws from
//! it, and does what clients ask of the layout. Everything runs on one
//! `polling` loop: pane PTYs, the listening socket and client connections
//! are all non-blocking, so a busy pane or a slow client never holds up the
//! rest.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use polling::{Event as PollEvent, Events, Poller};
use signal_hook::consts::SIGCHLD;

use crate::agent::{self, AgentProxy};
use crate::config;
use crate::host::{Guest, Host};
use crate::layout::PaneId;
use crate::protocol::{
    ClientMsg, Decoder, ExitReason, Greeting, Hello, Intent, PROTOCOL,
    READ_CHUNK, ServerMsg, encode,
};

/// Pane PTYs are keyed by pane id, which is a u32; these sit above them.
/// (`usize::MAX` is reserved by `polling`.)
const LISTENER_KEY: usize = usize::MAX - 1;
/// The pipe the SIGCHLD handler writes to.
const SIGCHLD_KEY: usize = usize::MAX - 2;
const CONNECTION_KEY_BASE: usize = 1 << 32;

/// The most read from one connection per wakeup, so a client flooding the
/// socket can't keep the server from everything else.
const READ_BUDGET: usize = 256 * 1024;
/// The most a client may fall behind by, in bytes not yet taken by its
/// socket, before it's dropped rather than queued for without end. Clients
/// that acknowledge what they take never come near it.
const MAX_BACKLOG: usize = 64 * 1024 * 1024;
/// The most a client may have been sent and not yet acknowledged before
/// panes' output stops going to it. Over a slow link, output would
/// otherwise pile up in the link's buffers faster than it drains, and
/// everything after it, the echo of what's typed included, would wait.
/// A pane whose output was held back is sent whole again instead, as a
/// snapshot, once the client has caught up to half this.
const WINDOW: u64 = 32 * 1024;
/// A server started for a client that never attaches gives up after this.
const STARTUP_GRACE: Duration = Duration::from_secs(10);
/// How long shutting down waits, in all, for clients to take their goodbyes.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
/// However late in that grace, each client gets at least this to take its
/// last messages.
const MIN_GOODBYE_WAIT: Duration = Duration::from_millis(10);
/// How much of the signal pipe is emptied at a time.
const SIGNAL_DRAIN: usize = 64;
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
    client: Option<Guest>,
    /// Close once `outgoing` has been sent.
    closing: bool,
    /// The other end has sent all it's going to. What it sent first is
    /// still handled.
    eof: bool,
    /// The connection failed, or has been dealt with after `eof`.
    dead: bool,
    /// Bytes of messages queued for it, and that it says it has taken.
    sent: u64,
    acked: u64,
    /// Panes whose output was held back while it was behind, to send it
    /// snapshots of once it catches up.
    behind: HashSet<PaneId>,
    /// Panes it asked for more history of, and how many lines, to answer
    /// once the output read before the asking has gone out.
    history: HashMap<PaneId, usize>,
    /// The ssh agent its bridge said ssh forwarded, for its hello.
    agent: Option<PathBuf>,
    /// It's greeted the server, with the same protocol: messages follow.
    greeted: bool,
}

impl Connection {
    fn new(key: usize, stream: UnixStream) -> Self {
        Self {
            key,
            stream,
            decoder: Decoder::from_client(),
            outgoing: Vec::new(),
            client: None,
            closing: false,
            eof: false,
            dead: false,
            sent: 0,
            acked: 0,
            behind: HashSet::new(),
            history: HashMap::new(),
            agent: None,
            greeted: false,
        }
    }

    /// How log lines refer to it.
    fn name(&self) -> String {
        format!("connection {}", self.key - CONNECTION_KEY_BASE)
    }

    fn send(&mut self, msg: &ServerMsg) {
        self.send_encoded(&encode(msg));
    }

    /// Queues an encoded message, unless the client is too far behind to
    /// catch up, in which case it's dropped.
    fn send_encoded(&mut self, bytes: &[u8]) {
        if self.dead {
            return;
        }
        if self.outgoing.len() + bytes.len() > MAX_BACKLOG {
            log::warn!(
                "{}: dropping it: over {MAX_BACKLOG} bytes behind",
                self.name()
            );
            self.dead = true;
            return;
        }
        self.outgoing.extend(bytes);
        self.sent += bytes.len() as u64;
    }

    /// Bytes sent that it hasn't acknowledged yet.
    fn unacked(&self) -> u64 {
        self.sent - self.acked
    }

    /// Queues pane `pane`'s output, or a change to its size, unless the
    /// client is too far behind, in which case the pane is noted for a
    /// snapshot later. Once one of its messages is held back, the rest
    /// are, until then, since they make no sense without it.
    fn send_pane(&mut self, pane: PaneId, bytes: &[u8]) {
        if self.behind.contains(&pane) || self.unacked() > WINDOW {
            self.behind.insert(pane);
        } else {
            self.send_encoded(bytes);
        }
    }

    /// Whether it's attached and still taking messages.
    fn attached(&self) -> bool {
        self.client.is_some() && !self.closing && !self.dead
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
        let mut buf = [0u8; READ_CHUNK];
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
    // The ssh agent for programs in panes. Without it they keep whatever
    // agent the server started with.
    let agent = match AgentProxy::start(agent::path(socket)) {
        Ok(agent) => Some(agent),
        Err(e) => {
            log::warn!("no ssh agent for panes: {e}");
            None
        }
    };
    let result = serve(&listener, config_path, agent);
    let current =
        std::fs::metadata(socket).map(|meta| (meta.dev(), meta.ino()));
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
    env_logger::Builder::from_env(
        env_logger::Env::new().filter_or("TIRI_LOG", "info"),
    )
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

fn serve(
    listener: &UnixListener,
    config_path: Option<PathBuf>,
    agent: Option<AgentProxy>,
) -> Result<()> {
    let poller = Arc::new(Poller::new().context("couldn't create a poller")?);
    // SAFETY: deleted from the poller before `serve` returns.
    unsafe { poller.add(listener, PollEvent::readable(LISTENER_KEY)) }
        .context("couldn't watch the socket")?;

    // A pane's shell exiting is a SIGCHLD; the handler writes to this pipe,
    // which the poller watches along with everything else.
    let (sigchld, sigchld_writer) =
        UnixStream::pair().context("couldn't make a signal pipe")?;
    sigchld
        .set_nonblocking(true)
        .context("couldn't make the signal pipe non-blocking")?;
    let handler =
        signal_hook::low_level::pipe::register(SIGCHLD, sigchld_writer)
            .context("couldn't watch for exiting shells")?;
    // SAFETY: deleted from the poller before `serve` returns.
    unsafe { poller.add(&sigchld, PollEvent::readable(SIGCHLD_KEY)) }
        .context("couldn't watch the signal pipe")?;

    let mut host = Host::new(Arc::clone(&poller), config_path, agent);
    let mut connections = HashMap::new();
    let result =
        event_loop(listener, &sigchld, &poller, &mut host, &mut connections);
    // However the loop ended, panes are killed and clients told.
    shut_down(&mut host, &poller, std::mem::take(&mut connections));
    let _ = poller.delete(&sigchld);
    signal_hook::low_level::unregister(handler);
    let _ = poller.delete(listener);
    result
}

/// Runs until the server should stop: asked to, or out of panes.
fn event_loop(
    listener: &UnixListener,
    sigchld: &UnixStream,
    poller: &Poller,
    host: &mut Host,
    connections: &mut HashMap<usize, Connection>,
) -> Result<()> {
    let mut server = Server {
        listener,
        sigchld,
        poller,
        host,
        connections,
        events: Events::new(),
        next_connection: CONNECTION_KEY_BASE,
        started: Instant::now(),
        had_panes: false,
        kill: false,
        accept_paused_until: None,
    };
    loop {
        server.wait()?;
        server.take_events();
        server.handle_messages();
        server.host.expire_syncs(Instant::now());
        server.deliver();
        if server.should_stop() {
            return Ok(());
        }
        server.flush();
        server.drop_finished();
    }
}

/// The event loop's state between turns.
struct Server<'a> {
    listener: &'a UnixListener,
    /// Readable when a child process has exited.
    sigchld: &'a UnixStream,
    poller: &'a Poller,
    host: &'a mut Host,
    connections: &'a mut HashMap<usize, Connection>,
    /// What woke the loop this turn.
    events: Events,
    next_connection: usize,
    started: Instant,
    /// Whether any pane has opened yet: until one has, an empty server is
    /// waiting for its first client rather than finished.
    had_panes: bool,
    /// A client asked the server to stop.
    kill: bool,
    /// When to accept connections again, after accepting failed.
    accept_paused_until: Option<Instant>,
}

impl Server<'_> {
    /// Waits for I/O, or until the next thing that's due without any.
    fn wait(&mut self) -> Result<()> {
        if (self.accept_paused_until)
            .is_some_and(|until| Instant::now() >= until)
        {
            self.accept_paused_until = None;
        }

        // Wake for a pane's synchronized update timing out, accepting again,
        // or giving up on a first client that never came; otherwise only
        // for I/O and exiting children.
        let deadline = [
            self.host.next_deadline(),
            (!self.had_panes).then_some(self.started + STARTUP_GRACE),
            self.accept_paused_until,
        ]
        .into_iter()
        .flatten()
        .min();

        // `polling` reports each source once per arming, so re-arm them all.
        self.host.arm_panes();
        if self.accept_paused_until.is_none() {
            (self.poller)
                .modify(self.listener, PollEvent::readable(LISTENER_KEY))
                .context("couldn't watch the socket")?;
        }
        (self.poller)
            .modify(self.sigchld, PollEvent::readable(SIGCHLD_KEY))
            .context("couldn't watch the signal pipe")?;
        for (&key, connection) in self.connections.iter_mut() {
            // After end of input a socket stays readable, to no purpose.
            let interest = PollEvent::new(
                key,
                !connection.eof,
                !connection.outgoing.is_empty(),
            );
            if let Err(e) = self.poller.modify(&connection.stream, interest) {
                log::warn!("{}: couldn't watch it: {e}", connection.name());
                connection.dead = true;
            }
        }
        self.events.clear();
        let timeout =
            deadline.map(|d| d.saturating_duration_since(Instant::now()));
        match self.poller.wait(&mut self.events, timeout) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(()),
            Err(e) => Err(e).context("couldn't wait for events"),
        }
    }

    /// Accepts connections, and reads and writes what's ready to be.
    fn take_events(&mut self) {
        for event in self.events.iter() {
            match event.key {
                LISTENER_KEY => {
                    let accepted = accept(
                        self.listener,
                        self.poller,
                        self.connections,
                        &mut self.next_connection,
                    );
                    if let Err(e) = accepted {
                        log::warn!("couldn't accept a connection: {e}");
                        self.accept_paused_until =
                            Some(Instant::now() + ACCEPT_BACKOFF);
                    }
                }
                SIGCHLD_KEY => {
                    // Several signals may be pending as one wakeup; one look
                    // at every child covers them all.
                    let mut buf = [0u8; SIGNAL_DRAIN];
                    while matches!((&*self.sigchld).read(&mut buf), Ok(n) if n > 0)
                    {
                    }
                    self.host.children_exited();
                }
                key if key >= CONNECTION_KEY_BASE => {
                    if let Some(connection) = self.connections.get_mut(&key) {
                        if event.writable {
                            connection.flush();
                        }
                        if event.readable {
                            connection.read();
                        }
                    }
                }
                _ => self.host.pane_ready(event),
            }
        }
    }

    /// Acts on what clients have sent.
    fn handle_messages(&mut self) {
        let keys: Vec<usize> = self.connections.keys().copied().collect();
        for key in keys {
            while let Some(connection) = self.connections.get_mut(&key)
                // A connection that's been answered and is closing has had
                // its say; anything more is dropped with it.
                && !connection.dead
                && !connection.closing
            {
                if !connection.greeted {
                    match connection.decoder.next_greeting() {
                        Ok(Some(greeting)) => {
                            self.greet(key, greeting);
                            continue;
                        }
                        Ok(None) => {
                            connection.dead |= connection.eof;
                            break;
                        }
                        Err(e) => {
                            log::warn!(
                                "{}: dropping it: {e:#}",
                                connection.name()
                            );
                            connection.dead = true;
                            break;
                        }
                    }
                }
                match connection.decoder.next::<ClientMsg>() {
                    Ok(Some(msg)) => self.handle(key, msg),
                    Ok(None) => {
                        // Nothing more is coming. One still owed a reply
                        // gets it first.
                        connection.dead |= connection.eof;
                        break;
                    }
                    Err(e) => {
                        log::warn!("{}: dropping it: {e:#}", connection.name());
                        connection.dead = true;
                    }
                }
            }
        }
    }

    /// Greets a client back, so it knows what it's talking to. One of
    /// another protocol hears nothing more; one that's come to stop the
    /// server is obeyed whatever its protocol.
    fn greet(&mut self, key: usize, greeting: Greeting) {
        let Some(connection) = self.connections.get_mut(&key) else {
            return;
        };
        connection.send_encoded(&Greeting::ours(Intent::Talk).encode());
        let theirs = format!(
            "tiri {} (protocol {})",
            greeting.version, greeting.protocol
        );
        match greeting.intent {
            Intent::Kill => {
                log::info!("{}: {theirs} says to stop", connection.name());
                self.kill = true;
                connection.closing = true;
            }
            Intent::Talk if greeting.protocol != PROTOCOL => {
                log::warn!(
                    "{}: turning away {theirs}: this server speaks protocol \
                     {PROTOCOL}",
                    connection.name()
                );
                connection.closing = true;
            }
            Intent::Talk => connection.greeted = true,
        }
    }

    fn handle(&mut self, key: usize, msg: ClientMsg) {
        let Some(connection) = self.connections.get_mut(&key) else {
            return;
        };
        match msg {
            ClientMsg::Agent(agent) if connection.client.is_none() => {
                connection.agent = agent;
            }
            ClientMsg::Agent(_) => {}
            ClientMsg::Hello(mut hello) if connection.client.is_none() => {
                // Through a bridge, the agent's the one ssh forwarded there.
                if let Some(agent) = connection.agent.take() {
                    hello.agent = Some(agent);
                }
                log::info!(
                    "{}: attaching: {}x{} cells, foreground {:?}, \
                     background {:?}",
                    connection.name(),
                    hello.width,
                    hello.height,
                    hello.colors.foreground,
                    hello.colors.background,
                );
                self.attach(key, hello);
            }
            ClientMsg::Hello(_) => {}
            ClientMsg::Input { pane, bytes } => {
                if let Some(guest) = &connection.client {
                    self.host.input(guest, pane, &bytes);
                }
            }
            ClientMsg::Command(command) => {
                let Some(guest) = &connection.client else {
                    return;
                };
                if let Err(e) = self.host.command(guest, command) {
                    // Otherwise the key just seems to do nothing.
                    log::error!("{}: {e:#}", connection.name());
                    connection.send(&ServerMsg::Notice(format!("{e:#}")));
                }
            }
            ClientMsg::Resize { width, height } => {
                if let Some(guest) = connection.client.as_mut() {
                    self.host.resize(guest, width, height);
                }
            }
            ClientMsg::Ack(taken) => {
                // No more than it was sent, whatever it says.
                connection.acked =
                    taken.clamp(connection.acked, connection.sent);
            }
            ClientMsg::History { pane, lines } => {
                let wanted = connection.history.entry(pane).or_default();
                *wanted = (*wanted).max(lines as usize);
            }
            ClientMsg::Detach => {
                if let Some(guest) = connection.client.take() {
                    self.host.detach(&guest);
                    connection.send(&ServerMsg::Exit(ExitReason::Detached));
                    connection.closing = true;
                }
            }
            ClientMsg::List => {
                connection
                    .send(&ServerMsg::Workspaces(self.host.workspace_infos()));
                connection.closing = true;
            }
            ClientMsg::KillServer => {
                self.kill = true;
                connection.closing = true;
            }
        }
    }

    /// Attaches the client on connection `key`, and starts its copies of
    /// the panes.
    fn attach(&mut self, key: usize, hello: Hello) {
        // What the panes did before this client's snapshots goes only to
        // those already attached; it's in the snapshots for this one.
        self.deliver_outgoing();
        let attached = self.host.attach(hello);
        self.deliver_outgoing();
        let Some(connection) = self.connections.get_mut(&key) else {
            return;
        };
        match attached {
            Ok(guest) => {
                connection.send(&ServerMsg::Attached);
                for msg in self.host.welcome(&guest) {
                    connection.send(&msg);
                }
                connection.client = Some(guest);
            }
            Err(e) => {
                log::warn!("{}: couldn't attach: {e:#}", connection.name());
                connection.send(&ServerMsg::Error(format!("{e:#}")));
                connection.closing = true;
            }
        }
    }

    /// Passes on what the host has for clients: pane output and the like,
    /// then the layout, if it's changed.
    fn deliver(&mut self) {
        self.deliver_outgoing();
        self.catch_up();
        self.answer_history();
        if self.host.take_layout_changed() {
            for connection in self.connections.values_mut() {
                if let (true, Some(guest)) =
                    (connection.attached(), &connection.client)
                {
                    let layout = self.host.layout(guest);
                    connection.send(&ServerMsg::Layout(layout));
                }
            }
        }
    }

    fn deliver_outgoing(&mut self) {
        for msg in self.host.take_outgoing() {
            let pane = match &msg {
                ServerMsg::PaneOutput { pane, .. }
                | ServerMsg::PaneResize { pane, .. }
                | ServerMsg::PaneSnapshot { pane, .. } => Some(*pane),
                _ => None,
            };
            // Encoded once, however many clients it goes to.
            let bytes = encode(&msg);
            for connection in self.connections.values_mut() {
                match pane {
                    _ if !connection.attached() => {}
                    Some(pane) => connection.send_pane(pane, &bytes),
                    None => connection.send_encoded(&bytes),
                }
            }
        }
    }

    /// Sends clients that have caught up snapshots of the panes whose
    /// output they missed. Everything those panes wrote has been passed on
    /// already, to those not behind, so the snapshots pick up where the
    /// output that follows does.
    fn catch_up(&mut self) {
        for connection in self.connections.values_mut() {
            if connection.behind.is_empty()
                || !connection.attached()
                || connection.unacked() > WINDOW / 2
            {
                continue;
            }
            for pane in std::mem::take(&mut connection.behind) {
                // A pane that's closed since has nothing to catch up on.
                if let Some(snapshot) = self.host.snapshot(pane) {
                    connection.send(&snapshot);
                }
            }
        }
    }

    /// Sends the panes clients asked for more history of. A pane they're
    /// behind on gets a snapshot when they catch up anyway.
    fn answer_history(&mut self) {
        for connection in self.connections.values_mut() {
            for (pane, lines) in std::mem::take(&mut connection.history) {
                if !connection.attached() || connection.behind.contains(&pane) {
                    continue;
                }
                if let Some(snapshot) = self.host.history(pane, lines) {
                    connection.send_pane(pane, &encode(&snapshot));
                }
            }
        }
    }

    /// Sends what each connection's socket will take now.
    fn flush(&mut self) {
        for connection in self.connections.values_mut() {
            if !connection.dead {
                connection.flush();
            }
        }
    }

    /// Whether the server is done: asked to stop, out of panes, or never
    /// attached to in time.
    fn should_stop(&mut self) -> bool {
        self.had_panes |= !self.host.is_empty();
        // Connections that never said hello don't keep the server alive;
        // shutting down tells them it's gone.
        let abandoned =
            !self.had_panes && self.started.elapsed() >= STARTUP_GRACE;
        self.kill
            || self.host.quit
            || (self.had_panes && self.host.is_empty())
            || abandoned
    }

    /// Lets go of connections that are done with.
    fn drop_finished(&mut self) {
        self.connections.retain(|_, connection| {
            if !connection.finished() {
                return true;
            }
            if let Some(guest) = connection.client.take() {
                self.host.detach(&guest);
            }
            let _ = self.poller.delete(&connection.stream);
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
                    log::warn!(
                        "{}: couldn't make it non-blocking: {e}",
                        connection.name()
                    );
                    continue;
                }
                // SAFETY: deleted from the poller when the connection is
                // dropped from `connections`, or at shutdown.
                if let Err(e) = unsafe {
                    poller.add(&connection.stream, PollEvent::readable(key))
                } {
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

/// Kills every pane and tells every client the server is gone, giving them
/// a moment, between them, to receive it.
fn shut_down(
    host: &mut Host,
    poller: &Poller,
    connections: HashMap<usize, Connection>,
) {
    host.shutdown();
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    for (_, mut connection) in connections {
        if let Some(guest) = connection.client.take() {
            host.detach(&guest);
        }
        connection.send(&ServerMsg::Exit(ExitReason::ServerExited));
        let _ = poller.delete(&connection.stream);
        let _ = connection.stream.set_nonblocking(false);
        // A client that isn't reading doesn't hold up the rest for long.
        let left = deadline.saturating_duration_since(Instant::now());
        let _ = (connection.stream)
            .set_write_timeout(Some(left.max(MIN_GOODBYE_WAIT)));
        let _ = connection.stream.write_all(&connection.outgoing);
    }
}
