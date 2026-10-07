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
use crate::keys::Action;
use crate::layout::PaneId;
use crate::workspace::Workspace;

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
/// The lines of history a pane's snapshot carries unless more are asked
/// for.
pub const SNAPSHOT_HISTORY: usize = 1000;

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
    /// Where panes this client opens should start; for a client on another
    /// machine, None, and they start in the home directory.
    pub cwd: Option<PathBuf>,
    /// The colors its terminal reported, for answering programs that ask.
    pub colors: ReportedColors,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMsg {
    /// The first message from an attaching client.
    Hello(Hello),
    /// Bytes for pane `pane`'s program: typed, pasted, or a mouse report.
    /// None is whichever pane the client has focused when the server gets
    /// it, so keys typed just after a focus change follow it. Pastes come
    /// in parts of at most [`PASTE_CHUNK`] bytes.
    Input {
        pane: Option<PaneId>,
        bytes: Vec<u8>,
    },
    /// A change to the layout.
    Command(Command),
    /// The client's terminal changed size.
    Resize {
        width: u16,
        height: u16,
    },
    /// The client has taken in this many bytes of messages from the
    /// server, length prefixes included, since it connected. The server
    /// holds back panes' output from a client too far behind.
    Ack(u64),
    /// Asks for pane `pane` again with up to `lines` lines of history, for
    /// scrolling back past what the client's copy has.
    History {
        pane: PaneId,
        lines: u32,
    },
    /// The client is leaving; its panes keep running.
    Detach,
    /// Asks for the workspace list instead of attaching.
    List,
    KillServer,
}

/// A change to the layout a client asks the server for. The server makes
/// it, if it still can, and sends everyone the new [`Layout`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Command {
    /// One of the configured actions that change the layout.
    Action(Action),
    /// Focuses this pane, wherever it is, and its workspace.
    FocusPane(PaneId),
    /// Goes to workspace `ws`.
    FocusWorkspace(usize),
    /// Focuses column `column` of the client's workspace.
    FocusColumn(usize),
    /// Makes column `column` of workspace `ws` `cells` wide, as dragging
    /// its edge does.
    ResizeColumn { ws: usize, column: usize, cells: i32 },
    /// Makes pane `row` of column `column` of workspace `ws` `rows` tall.
    ResizePane { ws: usize, column: usize, row: usize, rows: i32 },
    /// Scrolls workspace `ws` to its focused column, after a drag.
    ShowFocus { ws: usize },
}

/// The layout as a client is to draw it: every workspace, and which one
/// the client is on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layout {
    pub workspaces: Vec<Workspace>,
    /// Index of the workspace this client is on.
    pub active: usize,
    /// Each pane's title until its program sets one: its shell's name.
    pub titles: Vec<(PaneId, String)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMsg {
    /// The hello was accepted; the layout and the panes follow.
    Attached,
    /// The layout, whole, whenever it changes.
    Layout(Layout),
    /// Pane `pane`'s terminal, to start a copy from: output that rebuilds
    /// it in a fresh emulator `rows` by `cols`. Its output follows. Also
    /// sent in place of output a client fell too far behind to be sent,
    /// and in answer to [`ClientMsg::History`].
    PaneSnapshot {
        pane: PaneId,
        rows: u16,
        cols: u16,
        bytes: Vec<u8>,
        /// Whether it has all the pane's history, or older lines are left
        /// for [`ClientMsg::History`] to ask for.
        complete: bool,
        /// Whether it answers [`ClientMsg::History`]: the same terminal
        /// as the client's copy, further back, so the client keeps its
        /// place in it.
        requested: bool,
    },
    /// What pane `pane`'s program wrote, in order.
    PaneOutput {
        pane: PaneId,
        bytes: Vec<u8>,
    },
    /// Pane `pane` was resized here, between the output before and after.
    PaneResize {
        pane: PaneId,
        rows: u16,
        cols: u16,
    },
    /// Something to tell the user.
    Notice(String),
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
    /// Bytes of whole messages decoded so far, length prefixes included.
    taken: u64,
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
        Self { buf: Vec::new(), start: 0, limit, taken: 0 }
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
        self.taken += (LEN_PREFIX + len) as u64;
        Ok(Some(msg))
    }

    /// Bytes of whole messages decoded so far, as [`ClientMsg::Ack`]
    /// counts them.
    pub fn taken(&self) -> u64 {
        self.taken
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
    use super::*;

    #[test]
    fn messages_survive_arriving_a_byte_at_a_time() {
        let input = ClientMsg::Input {
            pane: Some(PaneId(3)),
            bytes: b"\x1bx".to_vec(),
        };
        let mut bytes = encode(&input);
        bytes.extend(encode(&ClientMsg::KillServer));

        let mut decoder = Decoder::from_server();
        let mut got = Vec::new();
        for byte in bytes {
            decoder.push(&[byte]);
            while let Some(msg) = decoder.next::<ClientMsg>().unwrap() {
                got.push(msg);
            }
        }
        assert!(matches!(
            &got[..],
            [ClientMsg::Input { pane: Some(PaneId(3)), bytes }, ClientMsg::KillServer]
                if bytes == b"\x1bx"
        ));
    }

    #[test]
    fn decoders_count_the_bytes_of_whole_messages() {
        let ack = encode(&ClientMsg::Ack(7));
        let mut decoder = Decoder::from_client();
        decoder.push(&ack);
        decoder.push(&ack[..2]);
        assert!(decoder.next::<ClientMsg>().unwrap().is_some());
        assert!(decoder.next::<ClientMsg>().unwrap().is_none());
        assert_eq!(decoder.taken(), ack.len() as u64);
    }

    #[test]
    fn clients_may_only_send_small_messages() {
        let input =
            |len| ClientMsg::Input { pane: None, bytes: vec![b'x'; len] };
        let mut decoder = Decoder::from_client();
        decoder.push(&encode(&input(MAX_CLIENT_MESSAGE)));
        assert!(decoder.next::<ClientMsg>().is_err());

        let mut decoder = Decoder::from_client();
        decoder.push(&encode(&input(PASTE_CHUNK)));
        assert!(matches!(
            decoder.next::<ClientMsg>(),
            Ok(Some(ClientMsg::Input { .. }))
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
        let output =
            ServerMsg::PaneOutput { pane: PaneId(1), bytes: b"hello".to_vec() };
        send(&mut wire, &output).unwrap();
        send(&mut wire, &ServerMsg::Exit(ExitReason::Detached)).unwrap();
        let mut reader = &wire[..];
        let mut decoder = Decoder::from_server();
        let first: ServerMsg =
            recv(&mut reader, &mut decoder).unwrap().unwrap();
        assert!(
            matches!(first, ServerMsg::PaneOutput { bytes, .. } if bytes == b"hello")
        );
        let second: ServerMsg =
            recv(&mut reader, &mut decoder).unwrap().unwrap();
        assert!(matches!(second, ServerMsg::Exit(ExitReason::Detached)));
        assert!(
            recv::<ServerMsg>(&mut reader, &mut decoder).unwrap().is_none()
        );
    }

    #[test]
    fn the_layout_goes_over_whole() {
        use crate::workspace::Workspaces;
        let mut workspaces = Workspaces::new(80, &["notes".to_owned()]);
        workspaces.add_client(crate::workspace::ClientId(0));
        workspaces.insert(crate::workspace::ClientId(0), PaneId(7));
        let layout = Layout {
            workspaces: workspaces.list().to_vec(),
            active: 0,
            titles: vec![(PaneId(7), "zsh".to_owned())],
        };
        let mut decoder = Decoder::from_server();
        decoder.push(&encode(&ServerMsg::Layout(layout)));
        let Some(ServerMsg::Layout(got)) = decoder.next().unwrap() else {
            panic!("not a layout");
        };
        assert_eq!(got.workspaces.len(), workspaces.list().len());
        assert_eq!(got.workspaces[0].name(), Some("notes"));
        assert_eq!(got.workspaces[0].strip().columns()[0].panes(), [PaneId(7)]);
        assert_eq!(got.titles, [(PaneId(7), "zsh".to_owned())]);
    }
}
