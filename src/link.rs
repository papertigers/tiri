// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A client's connection to its server: the server's socket on this
//! machine, or ssh to `tiri bridge` on another, which relays to the socket
//! there. Both carry the same messages.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::socket;

/// The command that runs tiri on another machine, unless
/// `$TIRI_REMOTE_COMMAND` says otherwise: ssh runs it without a login
/// shell, so it may need a full path.
const REMOTE_COMMAND: &str = "tiri";

/// Where a client's server is.
pub enum Server {
    /// Listening on this socket, here.
    Local(PathBuf),
    /// On `host`, reached with ssh, at `socket` there or its default.
    Remote { host: String, socket: Option<PathBuf> },
}

impl Server {
    /// Where to look for the server's log, for error messages.
    pub fn log_hint(&self) -> String {
        match self {
            Self::Local(socket) => {
                socket::log_path(socket).display().to_string()
            }
            Self::Remote { host, .. } => {
                format!("on {host}, beside the server's socket")
            }
        }
    }

    /// How error messages name the server.
    pub fn describe(&self) -> String {
        match self {
            Self::Local(socket) => socket.display().to_string(),
            Self::Remote { host, socket: None } => host.clone(),
            Self::Remote { host, socket: Some(socket) } => {
                format!("{host}:{}", socket.display())
            }
        }
    }
}

/// A connection to the server, for reading and writing messages.
pub struct Link {
    reader: LinkReader,
    writer: LinkWriter,
}

impl Link {
    /// Over the server's socket, on this machine.
    pub fn socket(stream: UnixStream) -> io::Result<Self> {
        let writer = stream.try_clone()?;
        let hang_up = stream.try_clone()?;
        Ok(Self {
            reader: LinkReader { inner: Box::new(stream), _ssh: None },
            writer: LinkWriter {
                inner: Box::new(writer),
                hang_up: HangUp::Socket(hang_up),
            },
        })
    }

    /// Over ssh to `host`, through `tiri bridge` there, which relays to the
    /// server's socket: `socket` there, or its default. With `start`, the
    /// bridge starts a server if none is running; without, it answers
    /// [`crate::protocol::ServerMsg::NoServer`].
    pub fn ssh(host: &str, socket: Option<&Path>, start: bool) -> Result<Self> {
        let remote = std::env::var("TIRI_REMOTE_COMMAND")
            .unwrap_or_else(|_| REMOTE_COMMAND.to_owned());
        let mut command = Command::new("ssh");
        // No terminal at the far end, so the bytes pass through untouched,
        // and no escape character, which a message could happen to contain.
        command.args(["-T", "-e", "none", host, "--", &remote]);
        if let Some(socket) = socket {
            command.arg("-S").arg(socket);
        }
        command.arg("bridge");
        if !start {
            command.arg("--no-start");
        }
        // ssh's own prompts and errors go to this terminal.
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child =
            command.spawn().context("couldn't run ssh to reach the server")?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let ssh = Arc::new(Mutex::new(SshChild(child)));
        Ok(Self {
            reader: LinkReader {
                inner: Box::new(stdout),
                _ssh: Some(ssh.clone()),
            },
            writer: LinkWriter {
                inner: Box::new(stdin),
                hang_up: HangUp::Ssh(ssh),
            },
        })
    }

    /// The reading half, and the writing half, which can also hang up.
    pub fn split(self) -> (LinkReader, LinkWriter) {
        (self.reader, self.writer)
    }
}

impl Read for Link {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

impl Write for Link {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// The reading half of a [`Link`].
pub struct LinkReader {
    inner: Box<dyn Read + Send>,
    /// Kept so the ssh process lasts as long as either half.
    _ssh: Option<Arc<Mutex<SshChild>>>,
}

impl Read for LinkReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

/// The writing half of a [`Link`].
pub struct LinkWriter {
    inner: Box<dyn Write + Send>,
    hang_up: HangUp,
}

impl LinkWriter {
    /// Ends the connection both ways, so the reading half stops too.
    pub fn hang_up(&mut self) {
        match &self.hang_up {
            HangUp::Socket(stream) => {
                let _ = stream.shutdown(Shutdown::Both);
            }
            HangUp::Ssh(ssh) => {
                if let Ok(mut ssh) = ssh.lock() {
                    let _ = ssh.0.kill();
                }
            }
        }
    }
}

impl Write for LinkWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// How to end a connection from the writing side.
enum HangUp {
    Socket(UnixStream),
    Ssh(Arc<Mutex<SshChild>>),
}

/// The ssh process, ended and reaped once both halves are done with it.
struct SshChild(Child);

impl Drop for SshChild {
    fn drop(&mut self) {
        // It's normally gone already, the server having hung up.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
