// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the client and server say to each other over the socket: serde
//! messages encoded with postcard, each prefixed with its length so they can
//! be picked out of a byte stream that arrives in arbitrary pieces.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::colors::ReportedColors;

/// The biggest message a server may send: its output comes in pieces far
/// smaller, so this is only a backstop.
pub const MAX_SERVER_MESSAGE: usize = 64 * 1024 * 1024;
/// The biggest message a client may send. Clients send small things (keys,
/// mouse, resizes) and pastes in [`PASTE_CHUNK`]s, so the server never
/// buffers much for any one of them.
pub const MAX_CLIENT_MESSAGE: usize = 1024 * 1024;
/// Each message starts with its length, as a little-endian u32.
const LEN_PREFIX: usize = size_of::<u32>();
/// How much is read from a socket at a time.
pub const READ_CHUNK: usize = 64 * 1024;
/// How much of a paste goes in one message.
pub const PASTE_CHUNK: usize = 64 * 1024;

/// Which workspace a client wants to land on when it attaches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Target {
    /// The first workspace. A fresh server starts a shell there.
    Default,
    /// An existing named workspace.
    Existing(String),
    /// A new named workspace, started with a shell.
    New(String),
}

/// What an attaching client says about itself and where it wants to go.
#[derive(Debug, Serialize, Deserialize)]
pub struct Hello {
    pub width: u16,
    pub height: u16,
    pub target: Target,
    /// Where panes this client opens should start.
    pub cwd: PathBuf,
    /// Whether its overview should use kitty graphics thumbnails.
    pub kitty_overview: bool,
    /// The colors its terminal reported.
    pub colors: ReportedColors,
    /// Its terminal's cell size in pixels, if it says.
    pub cell_pixels: Option<(u16, u16)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMsg {
    /// The first message from an attaching client.
    Hello(Hello),
    /// Input from the client's terminal. Pastes come as [`ClientMsg::Paste`].
    Event(crossterm::event::Event),
    /// Part of a paste, in order; `last` on its last part. The parts make
    /// one paste, as far as the program receiving it can tell.
    Paste {
        text: String,
        last: bool,
    },
    /// The terminal's cell size in pixels changed, as after a font change.
    CellPixels(Option<(u16, u16)>),
    /// Asks for the workspace list instead of attaching.
    List,
    KillServer,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMsg {
    /// The hello was accepted; output follows.
    Attached,
    /// Bytes to write to the client's terminal as they are.
    Output(Vec<u8>),
    Workspaces(Vec<WorkspaceInfo>),
    /// The request failed; the server closes the connection after this.
    Error(String),
    /// The server is done with this client.
    Exit(ExitReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    Detached,
    /// The last pane closed, or the server was killed.
    ServerExited,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub name: Option<String>,
    /// How it's shown in the status bar.
    pub label: String,
    pub panes: usize,
    pub clients: usize,
}

/// Encodes `msg` with its length prefix, ready to write to a socket.
pub fn encode(msg: &impl Serialize) -> Vec<u8> {
    let body =
        postcard::to_stdvec(msg).expect("protocol messages always serialize");
    let len =
        u32::try_from(body.len()).expect("messages are far smaller than 4 GiB");
    let mut out = Vec::with_capacity(LEN_PREFIX + body.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Collects bytes from a socket and yields whole messages from them.
pub struct Decoder {
    buf: Vec<u8>,
    /// Where the next message starts in `buf`; what's before it has been
    /// decoded, and is dropped on the next push rather than per message.
    start: usize,
    /// Messages bigger than this are refused rather than buffered.
    limit: usize,
}

impl Decoder {
    /// A decoder for messages from a server.
    pub fn from_server() -> Self {
        Self::with_limit(MAX_SERVER_MESSAGE)
    }

    /// A decoder for messages from a client.
    pub fn from_client() -> Self {
        Self::with_limit(MAX_CLIENT_MESSAGE)
    }

    fn with_limit(limit: usize) -> Self {
        Self { buf: Vec::new(), start: 0, limit }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.drain(..self.start);
        self.start = 0;
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete message, if one has fully arrived.
    pub fn next<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        let buf = &self.buf[self.start..];
        let Some(header) = buf.get(..LEN_PREFIX) else {
            return Ok(None);
        };
        let len =
            u32::from_le_bytes(header.try_into().expect("four bytes")) as usize;
        if len > self.limit {
            bail!("message of {len} bytes is too big");
        }
        let Some(body) = buf.get(LEN_PREFIX..LEN_PREFIX + len) else {
            return Ok(None);
        };
        let msg = postcard::from_bytes(body).context(
            "couldn't understand a message: are the tiri client and server different \
             versions? `tiri kill-server` stops a server left from before an upgrade",
        )?;
        self.start += LEN_PREFIX + len;
        Ok(Some(msg))
    }
}

/// Writes one message to a blocking stream.
pub fn send(stream: &mut impl Write, msg: &impl Serialize) -> io::Result<()> {
    stream.write_all(&encode(msg))?;
    stream.flush()
}

/// Reads one message from a blocking stream, or None at end of stream.
pub fn recv<T: DeserializeOwned>(
    stream: &mut impl Read,
    decoder: &mut Decoder,
) -> Result<Option<T>> {
    let mut buf = [0u8; READ_CHUNK];
    loop {
        if let Some(msg) = decoder.next()? {
            return Ok(Some(msg));
        }
        match stream.read(&mut buf) {
            Ok(0) => return Ok(None),
            Ok(n) => decoder.push(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    #[test]
    fn messages_survive_arriving_a_byte_at_a_time() {
        let key =
            Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
        let mut bytes = encode(&ClientMsg::Event(key.clone()));
        bytes.extend(encode(&ClientMsg::KillServer));

        let mut decoder = Decoder::from_server();
        let mut got = Vec::new();
        for byte in bytes {
            decoder.push(&[byte]);
            while let Some(msg) = decoder.next::<ClientMsg>().unwrap() {
                got.push(msg);
            }
        }
        assert!(
            matches!(&got[..], [ClientMsg::Event(e), ClientMsg::KillServer] if *e == key)
        );
    }

    #[test]
    fn clients_may_only_send_small_messages() {
        let paste = ClientMsg::Paste {
            text: "x".repeat(MAX_CLIENT_MESSAGE),
            last: true,
        };
        let mut decoder = Decoder::from_client();
        decoder.push(&encode(&paste));
        assert!(decoder.next::<ClientMsg>().is_err());

        let paste =
            ClientMsg::Paste { text: "x".repeat(PASTE_CHUNK), last: true };
        let mut decoder = Decoder::from_client();
        decoder.push(&encode(&paste));
        assert!(matches!(
            decoder.next::<ClientMsg>(),
            Ok(Some(ClientMsg::Paste { .. }))
        ));
    }

    #[test]
    fn rejects_absurd_lengths() {
        let mut decoder = Decoder::from_server();
        decoder.push(&u32::MAX.to_le_bytes());
        assert!(decoder.next::<ServerMsg>().is_err());
    }

    #[test]
    fn blocking_send_and_recv_round_trip() {
        let mut wire = Vec::new();
        send(&mut wire, &ServerMsg::Output(b"hello".to_vec())).unwrap();
        send(&mut wire, &ServerMsg::Exit(ExitReason::Detached)).unwrap();
        let mut reader = &wire[..];
        let mut decoder = Decoder::from_server();
        let first: ServerMsg =
            recv(&mut reader, &mut decoder).unwrap().unwrap();
        assert!(matches!(first, ServerMsg::Output(b) if b == b"hello"));
        let second: ServerMsg =
            recv(&mut reader, &mut decoder).unwrap().unwrap();
        assert!(matches!(second, ServerMsg::Exit(ExitReason::Detached)));
        assert!(
            recv::<ServerMsg>(&mut reader, &mut decoder).unwrap().is_none()
        );
    }
}
