// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One attached terminal's own state: its size, scrollback positions,
//! selection and drag, and what it was last sent.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::colors::Palette;
use crate::effects::{Effects, Transition};
use crate::layout::PaneId;
use crate::pane::{Pane, ScrollMark};
use crate::render::{Frame, Renderer};
use crate::selection::{Point, Selection};
use crate::thumbnail;
use crate::workspace::ClientId;

use super::STATUS_HEIGHT;
use super::thumbnails::Thumbnail;

/// One attached terminal: its size, its prefix-key and overview settings,
/// the thumbnails uploaded to it, and the renderer that remembers what it
/// was last sent.
pub struct Client {
    pub(super) id: ClientId,
    pub(super) width: u16,
    pub(super) height: u16,
    /// Where panes this client opens start.
    pub(super) cwd: PathBuf,
    /// Its terminal's colors, for its thumbnails and for answering
    /// programs that ask.
    pub(super) palette: Palette,
    /// Its thumbnails' cell size, the same shape as its terminal's cells.
    pub(super) thumbnail_cell: thumbnail::CellSize,
    /// Set when the client asks to detach; the server then lets it go.
    pub(super) detach_requested: bool,
    pub(super) prefix_pending: bool,
    /// Whether this client's overview shows kitty graphics thumbnails
    /// instead of text.
    pub(super) kitty_overview: bool,
    pub(super) thumbnails: HashMap<PaneId, Thumbnail>,
    /// Escape sequences to send before the next frame: kitty graphics
    /// commands and clipboard writes.
    pub(super) escapes: Vec<u8>,
    pub(super) renderer: Renderer,
    /// How far back this client has scrolled each pane it's scrolled.
    pub(super) scrollback: HashMap<PaneId, Scrollback>,
    /// Text selected with the mouse, highlighted until the next click or key.
    pub(super) selection: Option<Selection>,
    /// What the left button is doing while held.
    pub(super) drag: Drag,
    /// The last left press in a pane, to spot double and triple clicks.
    pub(super) last_click: Option<Click>,
    /// Visual effects running on this client's screen.
    pub(super) effects: Effects,
    /// The overview fading in or out, if it is.
    pub(super) transition: Option<Transition>,
    /// Something to tell the user, shown in the status bar for a while.
    pub(super) notice: Option<Notice>,
    /// A paste partway through arriving.
    pub(super) paste: Option<Paste>,
}

/// Where a paste is going, while it arrives in parts.
#[derive(Debug, Clone, Copy)]
pub(super) struct Paste {
    pub(super) pane: PaneId,
    /// Whether it began with the start-of-paste marker, so needs the end.
    pub(super) bracketed: bool,
}

/// A left press in a pane.
#[derive(Debug, Clone, Copy)]
pub(super) struct Click {
    pub(super) at: Instant,
    pub(super) pane: PaneId,
    pub(super) point: Point,
    /// How many presses in a row this made, at the same place.
    pub(super) count: u8,
}

/// A message for the status bar, in place of the key hints.
#[derive(Debug, Clone)]
pub(super) struct Notice {
    pub(super) text: String,
    pub(super) until: Instant,
}

/// How long a [`Notice`] stays up.
const NOTICE_TIME: Duration = Duration::from_secs(8);

/// How far back a client has scrolled a pane, and when it was that far, so
/// output arriving meanwhile doesn't drag the view along.
#[derive(Debug, Clone, Copy)]
pub(super) struct Scrollback {
    lines: usize,
    at: ScrollMark,
}

/// What a held left button is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Drag {
    #[default]
    None,
    /// The press only focused a pane; ignore the rest of it.
    Ignored,
    /// Passing the drag to the program in this pane.
    Forwarded(PaneId),
    /// Selecting text in this pane; `snapped` if a double or triple click
    /// picked a word or line, which copies even a single character.
    Selecting { pane: PaneId, snapped: bool },
}

impl Client {
    pub fn detach_requested(&self) -> bool {
        self.detach_requested
    }

    /// Whether effects are running, so frames must keep coming.
    /// Tells the user something, say that what they asked for failed.
    pub fn notify(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice {
            text: text.into(),
            until: Instant::now() + NOTICE_TIME,
        });
    }

    pub fn effects_running(&self) -> bool {
        self.effects.is_active() || self.transition.is_some()
    }

    /// Its terminal's cells changed shape: redraw its thumbnails to match.
    pub fn set_cell_pixels(&mut self, cell_pixels: Option<(u16, u16)>) {
        let cell = thumbnail::cell_size_for(cell_pixels);
        if cell != self.thumbnail_cell {
            self.thumbnail_cell = cell;
            self.clear_thumbnails();
        }
    }

    /// Escape sequences to write before drawing the next frame.
    pub fn take_escapes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.escapes)
    }

    /// Puts `text` on this client's terminal's clipboard, with OSC 52.
    pub fn copy(&mut self, text: &str) {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        write!(self.escapes, "\x1b]52;c;{encoded}\x07")
            .expect("writing to memory can't fail");
    }

    /// How many lines back this client is scrolled in `pane`. Output since
    /// it scrolled pushes the view further back, so what it was reading
    /// stays put.
    pub(super) fn scrolled(&self, id: PaneId, pane: &Pane) -> usize {
        let Some(scroll) = self.scrollback.get(&id) else {
            return 0;
        };
        // History being cleared returns to the live screen.
        let since = pane.scrolled_since(scroll.at);
        since.map_or(0, |since| (scroll.lines + since).min(pane.history_size()))
    }

    /// Keeps the selection on the text it was made on as output moves that
    /// up, and drops it once the text or its pane has gone.
    pub(super) fn follow_selection(&mut self, panes: &HashMap<PaneId, Pane>) {
        let Some(selection) = &mut self.selection else {
            return;
        };
        let moved = panes.get(&selection.pane).and_then(|pane| {
            let scrolled = pane.scrolled_since(selection.at)? as i64;
            let top = -(pane.history_size() as i64);
            let shift = |p: Point| {
                let line = i64::from(p.line) - scrolled;
                (line >= top).then_some(Point { line: line as i32, ..p })
            };
            Some((shift(selection.anchor)?, shift(selection.head)?))
        });
        match (moved, panes.get(&selection.pane)) {
            (Some((anchor, head)), Some(pane)) => {
                *selection = Selection {
                    anchor,
                    head,
                    at: pane.scroll_mark(),
                    ..*selection
                };
            }
            _ => self.selection = None,
        }
    }

    /// Scrolls `pane` back by `lines` (forward if negative), returning to
    /// the live screen at the bottom.
    pub(super) fn scroll(&mut self, id: PaneId, pane: &Pane, lines: i32) {
        let history = pane.history_size();
        let current = self.scrolled(id, pane) as i32;
        let target = (current + lines).clamp(0, history as i32) as usize;
        if target == 0 {
            self.scrollback.remove(&id);
        } else {
            self.scrollback.insert(
                id,
                Scrollback { lines: target, at: pane.scroll_mark() },
            );
        }
    }

    /// Sends `frame` to this client's terminal, as a diff against the last,
    /// along with any pending escape sequences.
    pub fn render(
        &mut self,
        out: &mut impl Write,
        frame: Frame,
        cursor: Option<(u16, u16)>,
    ) -> io::Result<()> {
        let escapes = std::mem::take(&mut self.escapes);
        self.renderer.draw(out, &escapes, frame, cursor)
    }

    pub(super) fn area_height(&self) -> i32 {
        i32::from(self.height.saturating_sub(STATUS_HEIGHT))
    }
}
