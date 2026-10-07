// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The server's side: the panes, running on their PTYs, and the layout
//! they're in, which every client shares. Clients draw them from copies;
//! the host keeps the real ones, does what clients ask, and queues what
//! they need to hear about it, in the order it happened.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use polling::{Event as PollEvent, Poller};
use portable_pty::Child;

use crate::agent::AgentProxy;
use crate::colors::Palette;
use crate::config::Config;
use crate::keys::Action;
use crate::layout::{DEFAULT_VIEW, MIN_PANE_HEIGHT, PaneId, STATUS_HEIGHT};
use crate::pane::Pane;
use crate::protocol::{
    Command, Hello, Layout, SNAPSHOT_HISTORY, ServerMsg, Target, WorkspaceInfo,
};
use crate::workspace::{ClientId, Workspaces};

/// The largest terminal a client may claim to have, in cells.
const MAX_WIDTH: u16 = 1000;
const MAX_HEIGHT: u16 = 500;
/// The most of a pane's output in one message.
const OUTPUT_CHUNK: usize = 16 * 1024;

/// Bounds a client's claimed terminal size, so a bogus one can't make the
/// server allocate enormous terminals.
fn clamp_size(width: u16, height: u16) -> (u16, u16) {
    (width.clamp(1, MAX_WIDTH), height.clamp(1, MAX_HEIGHT))
}

/// Pane `id`'s terminal as it is now, with up to `history` lines of its
/// history, for a client to start a copy from.
fn snapshot(
    id: PaneId,
    pane: &mut Pane,
    history: usize,
    requested: bool,
) -> ServerMsg {
    let emulator = pane.emulator_mut();
    let (rows, cols) = emulator.size();
    let complete = emulator.history_size() <= history;
    let bytes = emulator.snapshot(history);
    ServerMsg::PaneSnapshot { pane: id, rows, cols, bytes, complete, requested }
}

/// Where panes start for a client that hasn't said: the home directory.
fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

pub struct Host {
    workspaces: Workspaces,
    panes: HashMap<PaneId, Pane>,
    next_pane: u32,
    next_guest: u32,
    /// Watches every pane's PTY; panes are keyed by their id.
    poller: Arc<Poller>,
    /// The terminal size panes are laid out for: that of the client most
    /// recently used, recorded in `size_owner`.
    layout_size: (u16, u16),
    size_owner: Option<ClientId>,
    /// Children of closed panes, kept until they've exited and been reaped.
    exited: Vec<Box<dyn Child + Send + Sync>>,
    /// The config file, re-read as each client attaches, for its layout
    /// presets; None to use only what's built in.
    config_path: Option<PathBuf>,
    /// What every client is to hear, in order.
    outgoing: Vec<ServerMsg>,
    /// The layout has changed since clients were last sent it.
    layout_changed: bool,
    /// The ssh agent programs in panes reach, if it started.
    agent: Option<AgentProxy>,
    /// Attached clients' ssh agents, the most recently used last: the one
    /// programs in panes reach.
    agents: Vec<(ClientId, PathBuf)>,
    pub quit: bool,
}

/// An attached client, as the host knows it.
pub struct Guest {
    id: ClientId,
    width: u16,
    height: u16,
    /// Where panes this client opens start.
    cwd: PathBuf,
    /// Its terminal's colors, for answering programs that ask.
    palette: Palette,
    /// Its ssh agent's socket on this machine, if it has one.
    agent: Option<PathBuf>,
}

impl Host {
    /// Starts with no panes; the first client to attach gets a shell.
    /// Programs in panes reach the latest client's ssh agent through
    /// `agent`.
    pub fn new(
        poller: Arc<Poller>,
        config_path: Option<PathBuf>,
        agent: Option<AgentProxy>,
    ) -> Self {
        Self {
            workspaces: Workspaces::new(DEFAULT_VIEW.0, &[]),
            panes: HashMap::new(),
            next_pane: 0,
            next_guest: 0,
            poller,
            layout_size: DEFAULT_VIEW,
            size_owner: None,
            exited: Vec::new(),
            config_path,
            outgoing: Vec::new(),
            layout_changed: false,
            agent,
            agents: Vec::new(),
            quit: false,
        }
    }

    /// Attaches a terminal to `target`. A new workspace, or a server with
    /// no panes yet, starts with a shell. The caller sends the guest
    /// [`Self::welcome`] next.
    pub fn attach(&mut self, hello: Hello) -> Result<Guest> {
        let Hello { width, height, target, cwd, colors, agent } = hello;
        let (width, height) = clamp_size(width, height);
        // The layout presets come from the config. One with mistakes
        // gives way to the default, and the client, which reads it too,
        // says so.
        if let Some(path) = &self.config_path {
            let config = Config::load(path).unwrap_or_default();
            self.workspaces.set_size_presets(&config.size_presets);
        }
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

        let id = ClientId(self.next_guest);
        self.next_guest += 1;
        self.workspaces.add_client(id);
        if let Some(idx) = workspace {
            self.workspaces.set_active(id, idx);
        }
        let guest = Guest {
            id,
            width,
            height,
            // A client on another machine sends none: its directories
            // aren't this machine's.
            cwd: cwd.unwrap_or_else(home_dir),
            palette: Palette::from_reported(&colors),
            // A client inside one of these panes has the proxy as its
            // agent, which would only lead back to itself.
            agent: agent.filter(|agent| {
                self.agent.as_ref().is_none_or(|proxy| agent != proxy.path())
            }),
        };
        self.lay_out_for(&guest);
        if (matches!(target, Target::New(_)) || self.panes.is_empty())
            && let Err(e) = self.open_column(&guest)
        {
            self.detach(&guest);
            if let Target::New(name) = &target {
                // Not left behind, empty, for the next try to trip over.
                self.workspaces.remove_named(name);
            }
            return Err(e);
        }
        self.layout_changed = true;
        Ok(guest)
    }

    /// What a client hears first: the layout, and every pane to start its
    /// copies from.
    pub fn welcome(&mut self, guest: &Guest) -> Vec<ServerMsg> {
        let mut welcome = vec![ServerMsg::Layout(self.layout(guest))];
        for (&id, pane) in &mut self.panes {
            welcome.push(snapshot(id, pane, SNAPSHOT_HISTORY, false));
        }
        welcome
    }

    /// Pane `id` as it is now, for a client to start its copy again from.
    pub fn snapshot(&mut self, id: PaneId) -> Option<ServerMsg> {
        Some(snapshot(id, self.panes.get_mut(&id)?, SNAPSHOT_HISTORY, false))
    }

    /// Pane `id` as it is now with up to `lines` lines of history, for a
    /// client that asked for more than its copy has.
    pub fn history(&mut self, id: PaneId, lines: usize) -> Option<ServerMsg> {
        Some(snapshot(id, self.panes.get_mut(&id)?, lines, true))
    }

    /// The layout as `guest` is to draw it.
    pub fn layout(&self, guest: &Guest) -> Layout {
        Layout {
            workspaces: self.workspaces.list().to_vec(),
            active: self.workspaces.active_index(guest.id),
            titles: (self.panes.iter())
                .map(|(&id, pane)| (id, pane.fallback_title().to_owned()))
                .collect(),
        }
    }

    /// What every client is to hear, in order, since the last call.
    pub fn take_outgoing(&mut self) -> Vec<ServerMsg> {
        std::mem::take(&mut self.outgoing)
    }

    /// Whether the layout has changed since the last call, for clients to
    /// be sent it again.
    pub fn take_layout_changed(&mut self) -> bool {
        std::mem::take(&mut self.layout_changed)
    }

    /// Every workspace, for `tiri ls`.
    pub fn workspace_infos(&self) -> Vec<WorkspaceInfo> {
        (self.workspaces.list().iter().enumerate())
            .map(|(ws, workspace)| WorkspaceInfo {
                name: workspace.name().map(str::to_owned),
                label: self.workspaces.label(ws),
                panes: (workspace.strip().columns().iter())
                    .map(|c| c.panes().len())
                    .sum(),
                clients: self.workspaces.clients_on(ws),
            })
            .collect()
    }

    pub fn detach(&mut self, guest: &Guest) {
        self.workspaces.remove_client(guest.id);
        // The agent before it is the one programs reach now.
        self.agents.retain(|(id, _)| *id != guest.id);
        self.update_agent();
        if self.size_owner == Some(guest.id) {
            self.size_owner = None;
        }
        // Whatever shared its column grows into the space.
        self.resize_panes();
        self.layout_changed = true;
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

    /// True once every pane in every workspace has gone.
    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    /// Bytes for pane `pane`'s program, from `guest`: typed, pasted, or a
    /// mouse report. None is the pane `guest` has focused.
    pub fn input(&mut self, guest: &Guest, pane: Option<PaneId>, bytes: &[u8]) {
        self.lay_out_for(guest);
        let pane = pane.or_else(|| self.workspaces.focused(guest.id));
        if let Some(pane) = pane.and_then(|pane| self.panes.get_mut(&pane)) {
            pane.write(bytes);
        }
    }

    /// Does what `guest` asks of the layout.
    pub fn command(&mut self, guest: &Guest, command: Command) -> Result<()> {
        self.lay_out_for(guest);
        let workspaces = self.workspaces.list().len();
        match command {
            Command::Action(action) => self.run(guest, action)?,
            Command::FocusPane(pane) => {
                self.workspaces.focus_pane(guest.id, pane);
            }
            Command::FocusWorkspace(ws) if ws < workspaces => {
                self.workspaces.focus_workspace(guest.id, ws);
            }
            Command::FocusColumn(column) => {
                self.workspaces.active_mut(guest.id).focus_column(column);
            }
            Command::ResizeColumn { ws, column, cells } if ws < workspaces => {
                self.workspaces.strip_mut(ws).resize_column(column, cells);
            }
            Command::ResizePane { ws, column, row, rows }
                if ws < workspaces =>
            {
                self.workspaces.strip_mut(ws).resize_pane(column, row, rows);
            }
            Command::ShowFocus { ws } if ws < workspaces => {
                self.workspaces.strip_mut(ws).show_focus();
            }
            // About a workspace that's gone since the client asked.
            _ => {}
        }
        // Whatever changed, panes' PTYs follow their boxes' sizes.
        self.resize_panes();
        self.layout_changed = true;
        Ok(())
    }

    fn run(&mut self, guest: &Guest, action: Action) -> Result<()> {
        let id = guest.id;
        match action {
            Action::NewColumn => self.open_column(guest)?,
            Action::FocusColumnLeft => {
                self.workspaces.active_mut(id).focus_left()
            }
            Action::FocusColumnRight => {
                self.workspaces.active_mut(id).focus_right()
            }
            Action::FocusColumnFirst => {
                self.workspaces.active_mut(id).focus_first()
            }
            Action::FocusColumnLast => {
                self.workspaces.active_mut(id).focus_last()
            }
            Action::MoveColumnLeft => {
                self.workspaces.active_mut(id).move_left()
            }
            Action::MoveColumnRight => {
                self.workspaces.active_mut(id).move_right()
            }
            Action::FocusPaneUp => self.workspaces.active_mut(id).focus_up(),
            Action::FocusPaneDown => {
                self.workspaces.active_mut(id).focus_down()
            }
            Action::MovePaneUp => self.workspaces.active_mut(id).move_up(),
            Action::MovePaneDown => self.workspaces.active_mut(id).move_down(),
            Action::ConsumeOrExpelPaneLeft => {
                self.workspaces.active_mut(id).consume_or_expel_left();
            }
            Action::ConsumeOrExpelPaneRight => {
                self.workspaces.active_mut(id).consume_or_expel_right();
            }
            Action::ConsumePaneIntoColumn => {
                self.workspaces.active_mut(id).consume_into_column()
            }
            Action::ExpelPaneFromColumn => {
                self.workspaces.active_mut(id).expel_from_column()
            }
            Action::SwitchPresetPaneHeight => {
                self.workspaces.active_mut(id).switch_preset_height();
            }
            Action::ResetPaneHeight => {
                self.workspaces.active_mut(id).reset_pane_height();
            }
            Action::SwitchPresetColumnWidth => {
                self.workspaces.active_mut(id).cycle_width()
            }
            Action::MaximizeColumn => {
                self.workspaces.active_mut(id).toggle_maximized()
            }
            Action::FullscreenPane => {
                self.workspaces.active_mut(id).toggle_fullscreen()
            }
            Action::CenterColumn => {
                self.workspaces.active_mut(id).center_focused()
            }
            Action::ClosePane => {
                if let Some(pane_id) = self.workspaces.focused(id) {
                    self.close_pane(pane_id);
                }
            }
            Action::FocusWorkspaceDown => self.workspaces.focus_down(id),
            Action::FocusWorkspaceUp => self.workspaces.focus_up(id),
            Action::MoveColumnToWorkspaceDown => {
                self.workspaces.move_column_down(id)
            }
            Action::MoveColumnToWorkspaceUp => {
                self.workspaces.move_column_up(id)
            }
            Action::KillServer => self.quit = true,
            // The client's own: it doesn't ask for these.
            Action::ToggleOverview
            | Action::CloseOverview
            | Action::ToggleThumbnails
            | Action::Detach => {}
        }
        Ok(())
    }

    fn pane_rows(&self) -> u16 {
        self.layout_size.1.saturating_sub(STATUS_HEIGHT + 2).max(1)
    }

    fn open_column(&mut self, guest: &Guest) -> Result<()> {
        let id = PaneId(self.next_pane);
        self.next_pane += 1;
        // Width isn't known until it's in the strip, so start narrow and fix
        // it below.
        let agent = self.agent.as_ref().map(AgentProxy::path);
        let mut pane = Pane::spawn(self.pane_rows(), 1, &guest.cwd, agent)?;
        pane.set_palette(guest.palette);
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
        // Clients start their copies before its first resize below.
        self.outgoing.push(snapshot(id, &mut pane, SNAPSHOT_HISTORY, false));
        self.panes.insert(id, pane);
        self.workspaces.insert(guest.id, id);
        self.resize_panes();
        self.layout_changed = true;
        Ok(())
    }

    /// Lays panes out for `guest`'s terminal size, making it the client
    /// whose size counts. Called whenever a client is used, so panes follow
    /// whichever terminal you're typing in.
    fn lay_out_for(&mut self, guest: &Guest) {
        self.use_agent_of(guest);
        let size = (guest.width, guest.height);
        if self.size_owner == Some(guest.id) && self.layout_size == size {
            return;
        }
        if self.size_owner != Some(guest.id) {
            // Programs asking about colors get this terminal's now.
            for pane in self.panes.values_mut() {
                pane.set_palette(guest.palette);
            }
        }
        self.size_owner = Some(guest.id);
        self.layout_size = size;
        self.workspaces.set_view_width(size.0);
        self.workspaces.set_view_height(size.1.saturating_sub(STATUS_HEIGHT));
        self.resize_panes();
        self.layout_changed = true;
    }

    /// Makes `guest`'s ssh agent, if it has one, the one programs in panes
    /// reach.
    fn use_agent_of(&mut self, guest: &Guest) {
        let Some(agent) = &guest.agent else {
            return;
        };
        if self.agents.last().is_some_and(|(id, _)| *id == guest.id) {
            return;
        }
        self.agents.retain(|(id, _)| *id != guest.id);
        self.agents.push((guest.id, agent.clone()));
        self.update_agent();
    }

    /// Points the panes' agent at the most recently used client's.
    fn update_agent(&self) {
        if let Some(proxy) = &self.agent {
            proxy
                .set_target(self.agents.last().map(|(_, agent)| agent.clone()));
        }
    }

    /// Brings every pane's PTY size in line with its share of its column,
    /// telling clients where in its output each one changed.
    fn resize_panes(&mut self) {
        let area = i32::from(self.layout_size.1.saturating_sub(STATUS_HEIGHT));
        self.workspaces.set_max_stack((area / MIN_PANE_HEIGHT).max(1) as usize);
        for workspace in self.workspaces.list() {
            let strip = workspace.strip();
            for (idx, col) in strip.columns().iter().enumerate() {
                let cols = strip.column_width(idx).saturating_sub(2).max(1);
                let heights = strip.pane_heights(idx, area);
                for (&id, h) in col.panes().iter().zip(heights) {
                    // A fullscreen pane has its column to itself.
                    let h = if col.fullscreen() == Some(id) { area } else { h };
                    let rows = (h - 2).max(1) as u16;
                    let Some(pane) = self.panes.get_mut(&id) else {
                        continue;
                    };
                    if pane.emulator().size() != (rows, cols) {
                        pane.resize(rows, cols);
                        self.outgoing.push(ServerMsg::PaneResize {
                            pane: id,
                            rows,
                            cols,
                        });
                    }
                }
            }
        }
    }

    /// A client's terminal changed size.
    pub fn resize(&mut self, guest: &mut Guest, width: u16, height: u16) {
        (guest.width, guest.height) = clamp_size(width, height);
        if self.size_owner == Some(guest.id) || self.size_owner.is_none() {
            self.lay_out_for(guest);
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
        if !event.readable {
            return;
        }
        let open = pane.read_ready();
        // Clients' copies see what the program copies, in the same output,
        // and put it on their own clipboards.
        drop(pane.emulator_mut().take_copied());
        // In pieces, so a client falling behind can be held back partway
        // through a big read rather than after it.
        for bytes in pane.take_output().chunks(OUTPUT_CHUNK) {
            let bytes = bytes.to_vec();
            self.outgoing.push(ServerMsg::PaneOutput { pane: id, bytes });
        }
        if !open {
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

    /// The next time something needs doing without any input: a pane's
    /// synchronized update timing out.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.panes.values().filter_map(Pane::sync_deadline).min()
    }

    pub fn expire_syncs(&mut self, now: Instant) {
        for pane in self.panes.values_mut() {
            pane.expire_sync(now);
        }
    }

    /// Takes pane `id` away: because its shell has gone, or to make it go.
    fn close_pane(&mut self, id: PaneId) {
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
        self.layout_changed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::colors::ReportedColors;
    use std::path::Path;

    fn hello(agent: Option<&str>) -> Hello {
        Hello {
            width: 80,
            height: 24,
            target: Target::Default,
            cwd: Some(PathBuf::from("/")),
            colors: ReportedColors::default(),
            agent: agent.map(PathBuf::from),
        }
    }

    fn current(host: &Host) -> Option<&Path> {
        host.agents.last().map(|(_, agent)| agent.as_path())
    }

    #[test]
    fn panes_reach_the_agent_of_the_client_used_last() {
        let dir = std::env::temp_dir()
            .join(format!("tiri-host-agent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let proxy = AgentProxy::start(dir.join("default.agent")).unwrap();
        let proxy_path = proxy.path().to_owned();
        let mut host =
            Host::new(Arc::new(Poller::new().unwrap()), None, Some(proxy));

        let a = host.attach(hello(Some("/a"))).unwrap();
        assert_eq!(current(&host), Some(Path::new("/a")));
        let b = host.attach(hello(Some("/b"))).unwrap();
        assert_eq!(current(&host), Some(Path::new("/b")));
        // Using a client makes its agent the one.
        host.input(&a, None, b"");
        assert_eq!(current(&host), Some(Path::new("/a")));
        // A client with no agent, or with this server's own, leaves it be.
        let none = host.attach(hello(None)).unwrap();
        let nested = host.attach(hello(proxy_path.to_str())).unwrap();
        host.input(&none, None, b"");
        host.input(&nested, None, b"");
        assert_eq!(current(&host), Some(Path::new("/a")));
        // Once it's gone, the one used before it.
        host.detach(&a);
        assert_eq!(current(&host), Some(Path::new("/b")));
        host.detach(&b);
        assert_eq!(current(&host), None);

        host.shutdown();
        drop(host);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
