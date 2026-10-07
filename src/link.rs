// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A client's connection to its server: the server's socket on this
//! machine, or ssh to `tiri bridge` on another, which relays to the socket
//! there. Both carry the same messages; through ssh, what the server sends
//! comes compressed with zstd, as one stream flushed as it goes.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::config::Remote;
use crate::socket;

/// The command that runs tiri on another machine, unless
/// `$TIRI_REMOTE_COMMAND` or the config's `remote` section says otherwise:
/// ssh runs it without a login shell, so it may need a full path.
const REMOTE_COMMAND: &str = "tiri";

/// How hard the bridge compresses: zstd's default, which keeps up with far
/// more output than a link that needs compressing carries.
const COMPRESSION_LEVEL: i32 = 3;

/// `out`, compressing what's written to it, for the bridge to write to ssh.
/// Each flush sends all that's been written, for the client to decompress
/// whole: the bridge flushes as the server's messages come, and the client
/// is waiting on them.
pub fn compressing<W: Write>(
    out: W,
) -> io::Result<zstd::stream::write::Encoder<'static, W>> {
    zstd::stream::write::Encoder::new(out, COMPRESSION_LEVEL)
}

/// Where a client's server is.
pub enum Server {
    /// Listening on this socket, here.
    Local(PathBuf),
    /// On `host`, reached with ssh, at `socket` there or the config's, or
    /// its default.
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
    /// server's socket there, starting the server if none is running. The
    /// socket is `socket`, or `remote`'s, or the default; the command that
    /// runs tiri is `$TIRI_REMOTE_COMMAND`, or `remote`'s, or `tiri`.
    pub fn ssh(
        host: &str,
        socket: Option<&Path>,
        remote: Option<&Remote>,
    ) -> Result<Self> {
        let command = std::env::var("TIRI_REMOTE_COMMAND")
            .ok()
            .or_else(|| remote.and_then(|remote| remote.command.clone()));
        let command = command.as_deref().unwrap_or(REMOTE_COMMAND);
        let socket = socket.or_else(|| remote?.socket.as_deref());
        let mut ssh = Command::new("ssh");
        // No terminal at the far end, so the bytes pass through untouched,
        // and no escape character, which a message could happen to contain.
        ssh.args(["-T", "-e", "none", host, "--", command]);
        if let Some(socket) = socket {
            ssh.arg("-S").arg(socket);
        }
        ssh.arg("bridge");
        // ssh's own prompts and errors go to this terminal.
        ssh.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child =
            ssh.spawn().context("couldn't run ssh to reach the server")?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let ssh = Arc::new(Mutex::new(SshChild(child)));
        Ok(Self {
            reader: LinkReader {
                inner: Box::new(zstd::stream::read::Decoder::new(stdout)?),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Decoder, ServerMsg, encode};
    use std::sync::mpsc;

    /// Reads what's been sent so far, and fails rather than waiting for
    /// more, as a pipe with nothing in it would wait.
    struct Pipe(mpsc::Receiver<Vec<u8>>);

    impl Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let bytes = self.0.try_recv().map_err(|_| {
                io::Error::new(io::ErrorKind::WouldBlock, "nothing sent yet")
            })?;
            assert!(bytes.len() <= buf.len(), "a test message fits");
            buf[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }
    }

    /// Collects the bridge's compressed output, a flush at a time.
    struct Sent(mpsc::Sender<Vec<u8>>, Vec<u8>);

    impl Write for Sent {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.1.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            let _ = self.0.send(std::mem::take(&mut self.1));
            Ok(())
        }
    }

    #[test]
    fn each_flush_arrives_whole_without_waiting_for_more() {
        let (tx, rx) = mpsc::channel();
        let mut bridge = compressing(Sent(tx, Vec::new())).unwrap();
        let mut client = zstd::stream::read::Decoder::new(Pipe(rx)).unwrap();
        let mut decoder = Decoder::from_server();
        let mut buf = [0; 4096];
        let mut decompressed = 0;
        for i in 0..50u32 {
            let text = format!("line {i}: the same old output\r\n").repeat(20);
            let msg = ServerMsg::Notice(text.clone());
            bridge.write_all(&encode(&msg)).unwrap();
            bridge.flush().unwrap();
            let msg = loop {
                if let Some(msg) = decoder.next::<ServerMsg>().unwrap() {
                    break msg;
                }
                let n = client.read(&mut buf).expect("all of it was sent");
                decompressed += n;
                decoder.push(&buf[..n]);
            };
            assert!(matches!(msg, ServerMsg::Notice(t) if t == text));
        }
        assert_eq!(decompressed as u64, decoder.taken());
    }
}
