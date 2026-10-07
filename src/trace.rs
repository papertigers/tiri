// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A record of a client's session, for finding out what went wrong in
//! what it drew. With `$TIRI_TRACE` set to a path, the client writes there
//! everything it sends its terminal, a frame at a time, and notes on what
//! it was doing: keys, what the server sent, each pass through its loop.
//! Replaying the frames through a terminal emulator shows the screen as it
//! was at any moment.
//!
//! The file is compressed with zstd, as one stream flushed after each
//! frame, so even a client that's killed leaves one `zstd -d` can read.
//! Inside, it starts with the line `tiri-trace 1`. Then come records, each
//! a kind byte, microseconds since the start as a little-endian u64, a
//! length as a little-endian u32, and that many bytes: for `O`, output to
//! the terminal; for `N`, a note, as text.
//!
//! Frames hold whatever the panes showed, so a trace is as private as the
//! screen was. Typed text and pastes are left out of the notes.

use std::fmt::Display;
use std::fs::File;
use std::io::{self, Write};
use std::time::Instant;

/// Output to the terminal.
const OUTPUT: u8 = b'O';
/// A note on what the client was doing.
const NOTE: u8 = b'N';

/// How hard to compress: zstd's quickest, which still shrinks frames of
/// escape sequences many times over without slowing the client.
const COMPRESSION_LEVEL: i32 = 1;

pub struct Trace {
    file: zstd::stream::AutoFinishEncoder<'static, File>,
    start: Instant,
}

impl Trace {
    /// A trace at `$TIRI_TRACE`, if it's set.
    pub fn from_env() -> io::Result<Option<Self>> {
        let Some(path) = std::env::var_os("TIRI_TRACE") else {
            return Ok(None);
        };
        Self::create(File::create(path)?).map(Some)
    }

    fn create(file: File) -> io::Result<Self> {
        let mut file =
            zstd::stream::Encoder::new(file, COMPRESSION_LEVEL)?.auto_finish();
        file.write_all(b"tiri-trace 1\n")?;
        Ok(Self { file, start: Instant::now() })
    }

    fn record(&mut self, kind: u8, bytes: &[u8]) {
        let micros = self.start.elapsed().as_micros() as u64;
        let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        // A trace that can't be written isn't worth stopping for.
        let _ = (self.file.write_all(&[kind]))
            .and_then(|()| self.file.write_all(&micros.to_le_bytes()))
            .and_then(|()| self.file.write_all(&len.to_le_bytes()))
            .and_then(|()| self.file.write_all(&bytes[..len as usize]));
    }

    /// Records what went to the terminal, and makes sure it's on disk, with
    /// the notes before it: a client that's killed still leaves its trace.
    pub fn output(&mut self, bytes: &[u8]) {
        self.record(OUTPUT, bytes);
        let _ = self.file.flush();
    }

    pub fn note(&mut self, note: impl Display) {
        self.record(NOTE, note.to_string().as_bytes());
    }
}

/// A writer to the terminal that also records, each time it's flushed,
/// what was written since, if there's a trace.
pub struct Traced<'a, W: Write> {
    inner: W,
    trace: Option<&'a mut Trace>,
    pending: Vec<u8>,
}

impl<'a, W: Write> Traced<'a, W> {
    pub fn new(inner: W, trace: Option<&'a mut Trace>) -> Self {
        Self { inner, trace, pending: Vec::new() }
    }

    pub fn trace(&mut self) -> Option<&mut Trace> {
        self.trace.as_deref_mut()
    }
}

impl<W: Write> Write for Traced<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        if self.trace.is_some() {
            self.pending.extend_from_slice(&buf[..n]);
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()?;
        if let Some(trace) = self.trace.as_deref_mut()
            && !self.pending.is_empty()
        {
            trace.output(&self.pending);
            self.pending.clear();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The records in a trace: kind, time and bytes.
    fn records(bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut rest = bytes.strip_prefix(b"tiri-trace 1\n").expect("header");
        let mut out = Vec::new();
        while let [kind, more @ ..] = rest {
            let len = u32::from_le_bytes(more[8..12].try_into().unwrap());
            let end = 12 + len as usize;
            out.push((*kind, more[12..end].to_vec()));
            rest = &more[end..];
        }
        out
    }

    #[test]
    fn traces_hold_each_frame_and_the_notes_between() {
        let path = std::env::temp_dir()
            .join(format!("tiri-trace-test-{}", std::process::id()));
        let mut trace = Trace::create(File::create(&path).unwrap()).unwrap();
        trace.note("size 80x24");
        let mut screen = Vec::new();
        {
            let mut out = Traced::new(&mut screen, Some(&mut trace));
            out.write_all(b"\x1b[H").unwrap();
            out.write_all(b"hello").unwrap();
            out.flush().unwrap();
            out.trace().unwrap().note("drew");
            out.flush().unwrap(); // nothing new: no record
        }
        drop(trace);
        assert_eq!(screen, b"\x1b[Hhello");
        let file = std::fs::read(&path).unwrap();
        let records = records(&zstd::decode_all(&file[..]).unwrap());
        assert_eq!(
            records,
            [
                (NOTE, b"size 80x24".to_vec()),
                (OUTPUT, b"\x1b[Hhello".to_vec()),
                (NOTE, b"drew".to_vec()),
            ]
        );
        let _ = std::fs::remove_file(&path);
    }
}
