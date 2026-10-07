// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Application state, split in two: [`App`] holds what every attached
//! client shares (the panes and the workspaces), and [`Client`] holds one
//! terminal's own state (its size, its drawing, and its view of the
//! workspaces). Keybindings and drawing act on behalf of a client.

mod bindings;
mod client_state;
mod draw;
mod geometry;
mod mouse;
mod screen;
mod status;
mod thumbnails;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use polling::{Event as PollEvent, Poller};
use portable_pty::Child;

use crate::colors::Palette;
use crate::config::Config;
use crate::effects::Effects;
use crate::escape;
use crate::layout::{MIN_PANE_HEIGHT, PaneId};
use crate::pane::Pane;
use crate::protocol::{Hello, Target, WorkspaceInfo};
use crate::render::Renderer;
use crate::thumbnail;
use crate::workspace::{ClientId, Workspaces};

pub use client_state::Client;
use client_state::{Drag, Paste};
use thumbnails::THUMBNAIL_INTERVAL;

const STATUS_HEIGHT: u16 = 1;
/// The largest terminal a client may claim to have, in cells.
const MAX_WIDTH: u16 = 1000;
const MAX_HEIGHT: u16 = 500;

/// Bounds a client's claimed terminal size, so a bogus one can't make the
/// server allocate enormous frames and terminals.
fn clamp_size(width: u16, height: u16) -> (u16, u16) {
    (width.clamp(1, MAX_WIDTH), height.clamp(1, MAX_HEIGHT))
}

/// What every client shares: the panes, the workspaces and their columns.
pub struct App {
    workspaces: Workspaces,
    panes: HashMap<PaneId, Pane>,
    next_pane: u32,
    next_client: u32,
    /// Watches every pane's PTY; panes are keyed by their id.
    poller: Arc<Poller>,
    /// The terminal size panes are laid out for: that of the client most
    /// recently used, recorded in `size_owner`.
    layout_size: (u16, u16),
    size_owner: Option<ClientId>,
    /// Children of closed panes, kept until they've exited and been reaped.
    exited: Vec<Box<dyn Child + Send + Sync>>,
    /// The config file, re-read as each client attaches; None to use only
    /// what's built in.
    config_path: Option<PathBuf>,
    /// The config: the theme and key bindings.
    config: Config,
    pub quit: bool,
}

impl App {
    /// Starts with no panes; the first client to attach gets a shell.
    /// The config is read from `config_path` as clients attach.
    pub fn new(poller: Arc<Poller>, config_path: Option<PathBuf>) -> Self {
        let layout_size = (80, 24);
        Self {
            workspaces: Workspaces::new(layout_size.0, &[]),
            panes: HashMap::new(),
            next_pane: 0,
            next_client: 0,
            poller,
            layout_size,
            size_owner: None,
            exited: Vec::new(),
            config_path,
            config: Config::default(),
            quit: false,
        }
    }

    /// Attaches a terminal of the given size to `target`. A new workspace,
    /// or a server with no panes yet, starts with a shell in `cwd`.
    pub fn attach(&mut self, hello: Hello) -> Result<Client> {
        let Hello {
            width,
            height,
            target,
            cwd,
            kitty_overview,
            colors,
            cell_pixels,
        } = hello;
        let (width, height) = clamp_size(width, height);
        // Edits to the config apply from the next attach, for everyone. A
        // config with mistakes gives way to the default one, with a word to
        // whoever attached: refusing would shut them out of their own panes,
        // and keeping whatever loaded last would depend on what came before
        // (on a first run, nothing has).
        let config_error = self.config_path.as_ref().and_then(|path| {
            let (config, error) = match Config::load(path) {
                Ok(config) => (config, None),
                Err(e) => {
                    log::warn!("using the default config: {e}");
                    (Config::default(), Some(e.summary))
                }
            };
            self.workspaces.set_size_presets(&config.size_presets);
            self.config = config;
            error
        });
        let workspace = match &target {
            Target::Default => None,
            Target::Existing(name) => match self.workspaces.find(name) {
                Some(idx) => Some(idx),
                None => {
                    bail!("no workspace named {name:?}; `tiri ls` lists them")
                }
            },
            Target::New(name) => {
                if self.workspaces.find(name).is_some() {
                    bail!("there's already a workspace named {name:?}");
                }
                Some(self.workspaces.create_named(name.clone()))
            }
        };

        let id = ClientId(self.next_client);
        self.next_client += 1;
        self.workspaces.add_client(id);
        if let Some(idx) = workspace {
            self.workspaces.set_active(id, idx);
        }
        let mut client = Client {
            id,
            width,
            height,
            cwd,
            palette: Palette::from_reported(&colors),
            thumbnail_cell: thumbnail::cell_size_for(cell_pixels),
            detach_requested: false,
            prefix_pending: false,
            kitty_overview,
            thumbnails: HashMap::new(),
            escapes: Vec::new(),
            all_motion: false,
            renderer: Renderer::default(),
            scrollback: HashMap::new(),
            selection: None,
            drag: Drag::None,
            last_click: None,
            effects: Effects::default(),
            transition: None,
            notice: None,
            paste: None,
        };
        if let Some(error) = config_error {
            client.notify(format!("{error}; using the default config"));
        }
        self.lay_out_for(&client);
        if (matches!(target, Target::New(_)) || self.panes.is_empty())
            && let Err(e) = self.open_column(&mut client)
        {
            self.detach(&mut client);
            if let Target::New(name) = &target {
                // Not left behind, empty, for the next try to trip over.
                self.workspaces.remove_named(name);
            }
            return Err(e);
        }
        Ok(client)
    }

    /// Every workspace, for `tiri ls`.
    pub fn workspace_infos(&self) -> Vec<WorkspaceInfo> {
        (self.workspaces.list().iter().enumerate())
            .map(|(ws, workspace)| WorkspaceInfo {
                name: workspace.name().map(str::to_owned),
                label: self.workspace_label(ws),
                panes: (workspace.strip().columns().iter())
                    .map(|c| c.panes().len())
                    .sum(),
                clients: self.workspaces.clients_on(ws),
            })
            .collect()
    }

    /// Detaches a client, leaving its graphics cleanup in its queue to send.
    pub fn detach(&mut self, client: &mut Client) {
        client.clear_thumbnails();
        self.workspaces.remove_client(client.id);
        if self.size_owner == Some(client.id) {
            self.size_owner = None;
        }
    }

    /// True once every pane in every workspace has gone.
    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    /// Advances every client's scroll and slide animations. Returns true
    /// while anything still moves. With animations off, everything goes
    /// straight where it's heading.
    pub fn tick(&mut self, dt: Duration) -> bool {
        if !self.config.animations {
            self.workspaces.snap_all();
            return false;
        }
        self.workspaces.tick(dt)
    }

    fn pane_rows(&self) -> u16 {
        self.layout_size.1.saturating_sub(STATUS_HEIGHT + 2).max(1)
    }

    fn open_column(&mut self, client: &mut Client) -> Result<()> {
        let id = PaneId(self.next_pane);
        self.next_pane += 1;
        // Width isn't known until it's in the strip, so start narrow and fix it below.
        let mut pane = Pane::spawn(self.pane_rows(), 1, &client.cwd)?;
        pane.set_palette(client.palette);
        // SAFETY: the pane is deleted from the poller in `close_pane` or
        // `shutdown`, before it's dropped and its PTY closed.
        let watched = unsafe {
            (self.poller).add(&pane.fd(), PollEvent::readable(id.0 as usize))
        };
        if let Err(e) = watched {
            pane.kill();
            self.exited.extend(pane.into_unreaped_child());
            return Err(e).context("couldn't watch the new pane's pty");
        }
        self.panes.insert(id, pane);
        self.workspaces.insert(client.id, id);
        self.resize_panes();
        if self.config.animations {
            client.effects.pane_opened(id, &client.palette);
        }
        Ok(())
    }

    /// Lays panes out for `client`'s terminal size, making it the client
    /// whose size counts. Called whenever a client is used, so panes follow
    /// whichever terminal you're typing in.
    fn lay_out_for(&mut self, client: &Client) {
        let size = (client.width, client.height);
        if self.size_owner == Some(client.id) && self.layout_size == size {
            return;
        }
        if self.size_owner != Some(client.id) {
            // Programs asking about colors get this terminal's now.
            for pane in self.panes.values_mut() {
                pane.set_palette(client.palette);
            }
        }
        self.size_owner = Some(client.id);
        self.layout_size = size;
        self.workspaces.set_view_width(size.0);
        self.workspaces.set_view_height(size.1.saturating_sub(STATUS_HEIGHT));
        self.resize_panes();
    }

    /// Brings every pane's PTY size in line with its share of its column.
    fn resize_panes(&mut self) {
        let area = i32::from(self.layout_size.1.saturating_sub(STATUS_HEIGHT));
        self.workspaces.set_max_stack((area / MIN_PANE_HEIGHT).max(1) as usize);
        for workspace in self.workspaces.list() {
            let strip = workspace.strip();
            for (idx, col) in strip.columns().iter().enumerate() {
                let cols = strip.column_width(idx).saturating_sub(2).max(1);
                let heights = strip.pane_heights(idx, area);
                for (id, h) in col.panes().iter().zip(heights) {
                    // A fullscreen pane has its column to itself.
                    let h =
                        if col.fullscreen() == Some(*id) { area } else { h };
                    if let Some(pane) = self.panes.get_mut(id) {
                        pane.resize((h - 2).max(1) as u16, cols);
                    }
                }
            }
        }
    }

    /// A client's terminal changed size.
    pub fn resize(&mut self, client: &mut Client, width: u16, height: u16) {
        (client.width, client.height) = clamp_size(width, height);
        client.renderer.invalidate();
        if self.size_owner == Some(client.id) || self.size_owner.is_none() {
            self.lay_out_for(client);
        }
    }

    /// Handles a pane's PTY becoming readable or writable.
    pub fn pane_ready(&mut self, event: PollEvent) {
        // Keys above a u32 are the server's own, never a pane's.
        let Ok(id) = u32::try_from(event.key).map(PaneId) else {
            return;
        };
        let Some(pane) = self.panes.get_mut(&id) else {
            return;
        };
        if event.writable {
            pane.flush();
        }
        if event.readable && !pane.read_ready() {
            log::debug!("pane {}: its terminal closed", id.0);
            self.close_pane(id);
        }
    }

    /// Re-arms every pane's PTY with the poller, which reports each one only
    /// once per arming. Panes with queued input also wait to be writable.
    pub fn arm_panes(&self) {
        for (id, pane) in &self.panes {
            let interest =
                PollEvent::new(id.0 as usize, true, pane.wants_write());
            // A failure means the PTY is gone, which reading will report.
            let _ = self.poller.modify(pane.fd(), interest);
        }
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
                (pane.generation() != thumb.generation)
                    .then(|| thumb.uploaded + THUMBNAIL_INTERVAL)
            });
        self.panes
            .values()
            .filter_map(Pane::sync_deadline)
            .chain(stale_thumbnails)
            .chain(client.notice.as_ref().map(|notice| notice.until))
            .min()
    }

    pub fn expire_syncs(&mut self, now: Instant) {
        for pane in self.panes.values_mut() {
            pane.expire_sync(now);
        }
    }

    /// Takes pane `id` away: because its shell has gone, or to make it go.
    pub(super) fn close_pane(&mut self, id: PaneId) {
        if let Some(pane) = self.panes.remove(&id) {
            let _ = self.poller.delete(pane.fd());
            // If it's still running (it may only have closed its terminal),
            // losing the terminal should end it.
            pane.kill();
            self.exited.extend(pane.into_unreaped_child());
        }
        self.workspaces.remove(id);
        // Whatever shared its column grows into the space.
        self.resize_panes();
    }

    /// Collects the exit status of closed panes' children that have gone,
    /// so they don't linger as zombies.
    fn reap_exited(&mut self) {
        self.exited.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }

    /// Some child process has exited (the server got SIGCHLD). A pane whose
    /// shell exited closes, as a terminal window would, even if something
    /// the shell left running still has the terminal open; and closed
    /// panes' children are reaped.
    pub fn children_exited(&mut self) {
        let done: Vec<PaneId> = (self.panes.iter_mut())
            .filter_map(|(&id, pane)| pane.child_exited().then_some(id))
            .collect();
        for id in done {
            log::debug!("pane {}: its shell exited", id.0);
            self.close_pane(id);
        }
        self.reap_exited();
    }

    pub fn shutdown(&mut self) {
        for pane in self.panes.values() {
            let _ = self.poller.delete(pane.fd());
            pane.kill();
        }
    }

    /// Pastes `text`, which is part of a paste that `last` ends. All its
    /// parts go to the pane focused when it began, as one paste: bracketed
    /// as a whole if the program asked for that.
    pub fn paste(&mut self, client: &mut Client, text: &str, last: bool) {
        self.lay_out_for(client);
        let paste = match client.paste {
            Some(paste) => paste,
            None => {
                let Some(id) = self.workspaces.focused(client.id) else {
                    return;
                };
                let Some(pane) = self.panes.get_mut(&id) else {
                    return;
                };
                let bracketed = pane.bracketed_paste();
                if bracketed {
                    pane.write(escape::PASTE_START.as_bytes());
                }
                Paste { pane: id, bracketed }
            }
        };
        client.paste = (!last).then_some(paste);
        // A pane that closed partway through just misses the rest.
        let Some(pane) = self.panes.get_mut(&paste.pane) else {
            return;
        };
        pane.write(text.as_bytes());
        if last && paste.bracketed {
            pane.write(escape::PASTE_END.as_bytes());
        }
    }

    fn focused_pane_mut(&mut self, client: &Client) -> Option<&mut Pane> {
        let id = self.workspaces.focused(client.id)?;
        self.panes.get_mut(&id)
    }

    /// Text programs in panes have copied (OSC 52), for passing on to the
    /// clients' clipboards.
    pub fn take_copied(&mut self) -> Vec<String> {
        self.panes.values_mut().flat_map(Pane::take_copied).collect()
    }
}
