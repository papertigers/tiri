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
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use polling::{Event as PollEvent, Poller};
use portable_pty::Child;

use crate::colors::Palette;
use crate::effects::Effects;
use crate::layout::{PaneId, split_heights};
use crate::pane::Pane;
use crate::protocol::{Hello, Target, WorkspaceInfo};
use crate::render::Renderer;
use crate::thumbnail;
use crate::workspace::{ClientId, Workspaces};

pub use client_state::Client;
use client_state::Drag;
use thumbnails::THUMBNAIL_INTERVAL;

/// The prefix key, tmux-style: Ctrl-a, then a command key.
const PREFIX: char = 'a';
const STATUS_HEIGHT: u16 = 1;
/// The largest terminal a client may claim to have, in cells.
const MAX_WIDTH: u16 = 1000;
const MAX_HEIGHT: u16 = 500;
/// The shortest a stacked pane's box may get, borders included. Columns
/// refuse to consume more panes than fit at this height.
const MIN_PANE_HEIGHT: i32 = 5;

/// How often to check on closed panes' children until they've exited.
const REAP_INTERVAL: Duration = Duration::from_millis(100);

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
    pub quit: bool,
}

impl App {
    /// Starts with no panes; the first client to attach gets a shell.
    pub fn new(poller: Arc<Poller>) -> Self {
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
        let workspace = match &target {
            Target::Default => None,
            Target::Existing(name) => match self.workspaces.find(name) {
                Some(idx) => Some(idx),
                None => bail!("no workspace named {name:?}"),
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
            renderer: Renderer::default(),
            scrollback: HashMap::new(),
            selection: None,
            drag: Drag::None,
            last_click: None,
            effects: Effects::default(),
            transition: None,
        };
        self.lay_out_for(&client);
        if (matches!(target, Target::New(_)) || self.panes.is_empty())
            && let Err(e) = self.open_column(&mut client)
        {
            self.detach(&mut client);
            return Err(e);
        }
        Ok(client)
    }

    /// Every workspace, for `tiri ls`.
    pub fn workspace_infos(&self) -> Vec<WorkspaceInfo> {
        (0..self.workspaces.list().len())
            .map(|ws| {
                let workspace = &self.workspaces.list()[ws];
                WorkspaceInfo {
                    name: workspace.name().map(str::to_owned),
                    label: self.workspace_label(ws),
                    panes: (workspace.strip().columns().iter())
                        .map(|c| c.panes().len())
                        .sum(),
                    clients: self.workspaces.clients_on(ws),
                }
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
    /// while anything still moves.
    pub fn tick(&mut self, dt: Duration) -> bool {
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
        // SAFETY: the pane is deleted from the poller in `pane_exited` or
        // `shutdown`, before it's dropped and its PTY closed.
        unsafe {
            self.poller
                .add(pane.fd().as_raw_fd(), PollEvent::readable(id.0 as usize))
        }
        .context("failed to watch the new pane's pty")?;
        self.panes.insert(id, pane);
        self.workspaces.insert(client.id, id);
        self.resize_panes();
        client.effects.pane_opened(id, &client.palette);
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
        self.resize_panes();
    }

    /// Brings every pane's PTY size in line with its share of its column.
    fn resize_panes(&mut self) {
        let area = i32::from(self.layout_size.1.saturating_sub(STATUS_HEIGHT));
        self.workspaces
            .set_max_stack((area / MIN_PANE_HEIGHT).max(1) as usize);
        for workspace in self.workspaces.list() {
            let strip = workspace.strip();
            for (idx, col) in strip.columns().iter().enumerate() {
                let cols = strip.column_width(idx).saturating_sub(2).max(1);
                let heights = split_heights(area, col.panes().len());
                for (id, h) in col.panes().iter().zip(heights) {
                    // A fullscreen pane has its column to itself.
                    let h = if col.fullscreen() == Some(*id) {
                        area
                    } else {
                        h
                    };
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
        let id = PaneId(event.key as u32);
        let Some(pane) = self.panes.get_mut(&id) else {
            return;
        };
        if event.writable {
            pane.flush();
        }
        if event.readable && !pane.read_ready() {
            self.pane_exited(id);
        }
    }

    /// Re-arms every pane's PTY with the poller, which reports each one only
    /// once per arming. Panes with queued input also wait to be writable.
    pub fn arm_panes(&self) {
        for (id, pane) in &self.panes {
            let interest = PollEvent::new(id.0 as usize, true, pane.wants_write());
            // A failure means the PTY is gone, which reading will report.
            let _ = self.poller.modify(pane.fd(), interest);
        }
    }

    /// The next time something needs doing for `client` without any input:
    /// a pane's synchronized update timing out, or a thumbnail due for a
    /// redraw.
    pub fn next_deadline(&self, client: &Client) -> Option<Instant> {
        let stale_thumbnails = client.thumbnails.iter().filter_map(|(id, thumb)| {
            let pane = self.panes.get(id)?;
            (pane.generation() != thumb.generation).then(|| thumb.uploaded + THUMBNAIL_INTERVAL)
        });
        self.panes
            .values()
            .filter_map(Pane::sync_deadline)
            .chain(stale_thumbnails)
            .min()
    }

    pub fn expire_syncs(&mut self, now: Instant) {
        for pane in self.panes.values_mut() {
            pane.expire_sync(now);
        }
    }

    fn pane_exited(&mut self, id: PaneId) {
        if let Some(pane) = self.panes.remove(&id) {
            let _ = self.poller.delete(pane.fd());
            self.exited.push(pane.into_child());
        }
        self.workspaces.remove(id);
        // Whatever shared its column grows into the space.
        self.resize_panes();
    }

    /// Collects the exit status of closed panes' children that have gone,
    /// so they don't linger as zombies.
    pub fn reap_exited(&mut self) {
        self.exited
            .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }

    /// When to look again for closed panes' children having exited, if any
    /// haven't yet. Nothing else would wake the server when they do.
    pub fn reap_deadline(&self) -> Option<Instant> {
        (!self.exited.is_empty()).then(|| Instant::now() + REAP_INTERVAL)
    }

    pub fn shutdown(&mut self) {
        for pane in self.panes.values() {
            let _ = self.poller.delete(pane.fd());
            pane.kill();
        }
    }

    pub fn paste(&mut self, client: &Client, text: &str) {
        self.lay_out_for(client);
        let Some(pane) = self.focused_pane_mut(client) else {
            return;
        };
        if pane.bracketed_paste() {
            pane.write(format!("\x1b[200~{text}\x1b[201~").as_bytes());
        } else {
            pane.write(text.as_bytes());
        }
    }

    fn focused_pane_mut(&mut self, client: &Client) -> Option<&mut Pane> {
        let id = self.workspaces.focused(client.id)?;
        self.panes.get_mut(&id)
    }

    /// Text programs in panes have copied (OSC 52), for passing on to the
    /// clients' clipboards.
    pub fn take_copied(&mut self) -> Vec<String> {
        self.panes
            .values_mut()
            .flat_map(Pane::take_copied)
            .collect()
    }
}
