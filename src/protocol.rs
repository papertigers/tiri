//! What the client and server say to each other over the socket: serde
//! messages encoded with postcard, each prefixed with its length so they can
//! be picked out of a byte stream that arrives in arbitrary pieces.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::colors::ReportedColors;

/// Refuse messages bigger than this, rather than trying to buffer them.
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

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
    /// The built-in theme it asked for, if any.
    pub theme: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMsg {
    /// The first message from an attaching client.
    Hello(Hello),
    /// Input from the client's terminal.
    Event(crossterm::event::Event),
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
    let body = postcard::to_stdvec(msg).expect("protocol messages always serialize");
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Collects bytes from a socket and yields whole messages from them.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// Where the next message starts in `buf`; what's before it has been
    /// decoded, and is dropped on the next push rather than per message.
    start: usize,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.drain(..self.start);
        self.start = 0;
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete message, if one has fully arrived.
    pub fn next<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        let buf = &self.buf[self.start..];
        let Some(header) = buf.get(..4) else {
            return Ok(None);
        };
        let len = u32::from_le_bytes(header.try_into().expect("four bytes")) as usize;
        if len > MAX_MESSAGE {
            bail!("message of {len} bytes is too big");
        }
        let Some(body) = buf.get(4..4 + len) else {
            return Ok(None);
        };
        let msg = postcard::from_bytes(body).context("malformed message")?;
        self.start += 4 + len;
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
    let mut buf = [0u8; 64 * 1024];
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
        let key = Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
        let mut bytes = encode(&ClientMsg::Event(key.clone()));
        bytes.extend(encode(&ClientMsg::KillServer));

        let mut decoder = Decoder::default();
        let mut got = Vec::new();
        for byte in bytes {
            decoder.push(&[byte]);
            while let Some(msg) = decoder.next::<ClientMsg>().unwrap() {
                got.push(msg);
            }
        }
        assert!(matches!(&got[..], [ClientMsg::Event(e), ClientMsg::KillServer] if *e == key));
    }

    #[test]
    fn rejects_absurd_lengths() {
        let mut decoder = Decoder::default();
        decoder.push(&u32::MAX.to_le_bytes());
        assert!(decoder.next::<ServerMsg>().is_err());
    }

    #[test]
    fn blocking_send_and_recv_round_trip() {
        let mut wire = Vec::new();
        send(&mut wire, &ServerMsg::Output(b"hello".to_vec())).unwrap();
        send(&mut wire, &ServerMsg::Exit(ExitReason::Detached)).unwrap();
        let mut reader = &wire[..];
        let mut decoder = Decoder::default();
        let first: ServerMsg = recv(&mut reader, &mut decoder).unwrap().unwrap();
        assert!(matches!(first, ServerMsg::Output(b) if b == b"hello"));
        let second: ServerMsg = recv(&mut reader, &mut decoder).unwrap().unwrap();
        assert!(matches!(second, ServerMsg::Exit(ExitReason::Detached)));
        assert!(
            recv::<ServerMsg>(&mut reader, &mut decoder)
                .unwrap()
                .is_none()
        );
    }
}
