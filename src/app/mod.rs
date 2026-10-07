// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a client shows, split in two: [`App`] holds what the server shares
//! (copies of the panes, fed the same output as the real ones, and the
//! layout they're in), and [`Client`] holds the terminal's own state (its
//! size, its drawing, scrollback, selections and effects). The client draws
//! and animates from these itself. Typing, and changes to the layout, go
//! to the server as messages, collected in an outbox.

mod bindings;
mod client_state;
mod draw;
mod geometry;
mod mouse;
mod screen;
mod status;
mod thumbnails;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::emulator::Emulator;
use crate::escape;
use crate::layout::PaneId;
use crate::protocol::{ClientMsg, Command, Layout, PASTE_CHUNK, ServerMsg};
use crate::workspace::{ClientId, Workspaces};

pub use client_state::Client;
use client_state::Drag;
use geometry::Seam;
use thumbnails::THUMBNAIL_INTERVAL;

/// The one client a client's copy of the workspaces has: itself.
const LOCAL: ClientId = ClientId(0);

/// A copy of a pane: its terminal, fed the pane's output as the server
/// passes it on.
pub(super) struct PaneCopy {
    emulator: Emulator,
    /// The title until the program sets one: its shell's name.
    fallback_title: String,
}

impl PaneCopy {
    pub(super) fn emulator(&self) -> &Emulator {
        &self.emulator
    }

    pub(super) fn title(&self) -> &str {
        self.emulator.title().unwrap_or(&self.fallback_title)
    }
}

/// What the server shares: the panes, the workspaces and their columns.
pub struct App {
    workspaces: Workspaces,
    panes: HashMap<PaneId, PaneCopy>,
    /// The config: the theme and key bindings.
    config: Config,
    /// Each pane's title until its program sets one, from the layout.
    titles: HashMap<PaneId, String>,
    /// What to tell the server, in order.
    outbox: Vec<ClientMsg>,
    /// Whether a layout has come yet: panes in later ones are new.
    laid_out: bool,
}

impl App {
    pub fn new(config: Config) -> Self {
        let mut workspaces =
            Workspaces::new(crate::layout::DEFAULT_VIEW.0, &[]);
        workspaces.add_client(LOCAL);
        Self {
            workspaces,
            panes: HashMap::new(),
            titles: HashMap::new(),
            config,
            outbox: Vec::new(),
            laid_out: false,
        }
    }

    /// Takes in what the server sent about the panes and the layout.
    pub fn apply(&mut self, client: &mut Client, msg: ServerMsg) {
        match msg {
            ServerMsg::Layout(layout) => self.lay_out(client, layout),
            ServerMsg::PaneSnapshot { pane, rows, cols, bytes } => {
                let mut emulator = Emulator::new(rows, cols);
                emulator.feed(&bytes);
                // The server answers the program; the copy only listens.
                drop(emulator.take_questions());
                drop(emulator.take_copied());
                let fallback_title =
                    self.titles.get(&pane).cloned().unwrap_or_default();
                self.panes.insert(pane, PaneCopy { emulator, fallback_title });
            }
            ServerMsg::PaneOutput { pane, bytes } => {
                let Some(copy) = self.panes.get_mut(&pane) else {
                    return;
                };
                copy.emulator.feed(&bytes);
                drop(copy.emulator.take_questions());
                for text in copy.emulator.take_copied() {
                    client.copy(&text);
                }
            }
            ServerMsg::PaneResize { pane, rows, cols } => {
                if let Some(copy) = self.panes.get_mut(&pane) {
                    copy.emulator.resize(rows, cols);
                }
            }
            ServerMsg::Notice(text) => client.notify(text),
            // The client's loop handles the rest.
            _ => {}
        }
    }

    /// Takes the layout as the server has it now.
    fn lay_out(&mut self, client: &mut Client, layout: Layout) {
        let Layout { workspaces, active, titles } = layout;
        self.workspaces.set_shared(LOCAL, workspaces, active);
        // A column being dragged stays under the mouse, rather than going
        // back to wherever the server had it a round trip ago.
        if let Drag::Resizing { seam, to: Some(to), .. } = client.drag {
            self.resize_seam(seam, to);
        }
        let titles: HashMap<PaneId, String> = titles.into_iter().collect();
        // Panes the layout no longer has have closed. Those it has are sent
        // before it, except to a client just attached.
        self.panes.retain(|id, _| titles.contains_key(id));
        for (id, title) in &titles {
            let new = !self.titles.contains_key(id);
            if new && self.laid_out && self.config.animations {
                client.effects.pane_opened(*id, &client.palette);
            }
            if let Some(copy) = self.panes.get_mut(id) {
                copy.fallback_title.clone_from(title);
            }
        }
        self.titles = titles;
        self.laid_out = true;
    }

    /// Resizes what `seam` divides, as dragging it to `to` does: for a
    /// column, its width; for a pane, its height.
    fn resize_seam(&mut self, seam: Seam, to: i32) {
        match seam {
            Seam::Column { ws, column } => {
                if ws < self.workspaces.list().len() {
                    self.workspaces.strip_mut(ws).resize_column(column, to);
                }
            }
            Seam::Pane { ws, column, row } => {
                if ws < self.workspaces.list().len() {
                    self.workspaces.strip_mut(ws).resize_pane(column, row, to);
                }
            }
        }
    }

    /// What to tell the server, in order, since the last call.
    pub fn take_outbox(&mut self) -> Vec<ClientMsg> {
        std::mem::take(&mut self.outbox)
    }

    fn command(&mut self, command: Command) {
        self.outbox.push(ClientMsg::Command(command));
    }

    /// Clears what the client put on its terminal, before it goes.
    pub fn detach(&mut self, client: &mut Client) {
        client.clear_thumbnails();
    }

    /// Advances the client's scroll and slide animations. Returns true
    /// while anything still moves. With animations off, everything goes
    /// straight where it's heading.
    pub fn tick(&mut self, dt: Duration) -> bool {
        if !self.config.animations {
            self.workspaces.snap_all();
            return false;
        }
        self.workspaces.tick(dt)
    }

    /// The client's terminal changed size.
    pub fn resize(&mut self, client: &mut Client, width: u16, height: u16) {
        (client.width, client.height) = (width, height);
        client.renderer.invalidate();
        self.outbox.push(ClientMsg::Resize { width, height });
    }

    /// The next time something needs doing for `client` without any input:
    /// a pane's synchronized update timing out, a thumbnail due for a
    /// redraw, or a notice due to come down.
    pub fn next_deadline(&self, client: &Client) -> Option<Instant> {
        // Parked thumbnails wait for the overview to open again.
        let showing = self.showing_thumbnails(client);
        let stale_thumbnails = (client.thumbnails.iter())
            .filter(|_| showing)
            .filter_map(|(id, thumb)| {
                let pane = self.panes.get(id)?;
                (pane.emulator().generation() != thumb.generation)
                    .then(|| thumb.uploaded + THUMBNAIL_INTERVAL)
            });
        (self.panes.values())
            .filter_map(|pane| pane.emulator().sync_deadline())
            .chain(stale_thumbnails)
            .chain(client.notice.as_ref().map(|notice| notice.until))
            .min()
    }

    pub fn expire_syncs(&mut self, now: Instant) {
        for pane in self.panes.values_mut() {
            pane.emulator.expire_sync(now);
        }
    }

    /// Pastes `text` into the focused pane: bracketed, if its program asked
    /// for that.
    pub fn paste(&mut self, client: &mut Client, text: &str) {
        client.selection = None;
        let bracketed = (self.focused_pane(client))
            .is_some_and(|pane| pane.emulator().bracketed_paste());
        for bytes in paste_parts(text, bracketed) {
            self.outbox.push(ClientMsg::Input { pane: None, bytes });
        }
    }

    fn focused_pane(&self, client: &Client) -> Option<&PaneCopy> {
        let id = self.workspaces.focused(client.id)?;
        self.panes.get(&id)
    }
}

/// A paste as the bytes to send the program, in parts of at most
/// [`PASTE_CHUNK`] bytes.
///
/// The end-of-paste marker is taken out first, as xterm does: in a paste it
/// would end the program's bracketed paste early, and what followed would
/// arrive as if typed.
fn paste_parts(text: &str, bracketed: bool) -> Vec<Vec<u8>> {
    let text = text.replace(escape::PASTE_END, "");
    let mut bytes = Vec::with_capacity(text.len());
    if bracketed {
        bytes.extend(escape::PASTE_START.as_bytes());
    }
    bytes.extend(text.as_bytes());
    if bracketed {
        bytes.extend(escape::PASTE_END.as_bytes());
    }
    bytes.chunks(PASTE_CHUNK).map(<[u8]>::to_vec).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pastes_go_in_chunks() {
        let text = "é".repeat(PASTE_CHUNK); // two bytes each
        let parts = paste_parts(&text, false);
        assert_eq!(parts.len(), 2);
        assert!(parts.iter().all(|part| part.len() <= PASTE_CHUNK));
        assert_eq!(parts.concat(), text.as_bytes());
    }

    #[test]
    fn bracketed_pastes_are_bracketed_as_a_whole() {
        let text = "x".repeat(PASTE_CHUNK);
        let parts = paste_parts(&text, true);
        assert_eq!(parts.len(), 2);
        let whole = parts.concat();
        assert!(whole.starts_with(escape::PASTE_START.as_bytes()));
        assert!(whole.ends_with(escape::PASTE_END.as_bytes()));
    }

    #[test]
    fn empty_pastes_send_nothing_unless_bracketed() {
        assert!(paste_parts("", false).is_empty());
        assert_eq!(paste_parts("", true).len(), 1);
    }

    #[test]
    fn pastes_cant_end_a_bracketed_paste_early() {
        assert_eq!(
            paste_parts("a\x1b[201~rm -rf ~\r", false),
            [b"arm -rf ~\r".to_vec()]
        );
    }
}
