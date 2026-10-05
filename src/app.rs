//! Application state, split in two: [`App`] holds what every attached
//! client shares (the panes and the workspaces), and [`Client`] holds one
//! terminal's own state (its size, its drawing, and its view of the
//! workspaces). Keybindings and drawing act on behalf of a client.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color as TermColor, NamedColor};
use anyhow::{Context, Result, bail};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use polling::{Event as PollEvent, Poller};
use portable_pty::Child;

use crate::colors::Palette;
use crate::effects::{Anchor, Effects, Transition};
use crate::input::{encode_key, encode_mouse};
use crate::kitty;
use crate::layout::{PaneId, Visibility, split_heights};
use crate::pane::Pane;
use crate::protocol::{Hello, Target, WorkspaceInfo};
use crate::render::{Color, Frame, Renderer, Style};
use crate::selection::{self, Point, Selection};
use crate::thumbnail;
use crate::workspace::{ClientId, Workspaces};

/// The prefix key, tmux-style: Ctrl-a, then a command key.
const PREFIX: char = 'a';
const STATUS_HEIGHT: u16 = 1;
/// The largest terminal a client may claim to have, in cells.
const MAX_WIDTH: u16 = 1000;
const MAX_HEIGHT: u16 = 500;
/// The shortest a stacked pane's box may get, borders included. Columns
/// refuse to consume more panes than fit at this height.
const MIN_PANE_HEIGHT: i32 = 5;

const FOCUSED_BORDER: Color = Color::Idx(12);
const UNFOCUSED_BORDER: Color = Color::Idx(8);
const STATUS_BG: Color = Color::Idx(236);
const STATUS_FG: Color = Color::Idx(250);
const DIM_TEXT: Color = Color::Idx(242);

/// Kitty image ids for overview thumbnails are this plus the pane id.
const THUMBNAIL_ID_BASE: u32 = 0x74_0000;
/// Thumbnails of busy panes are redrawn at most this often.
const THUMBNAIL_INTERVAL: Duration = Duration::from_millis(250);
/// How often to check on closed panes' children until they've exited.
const REAP_INTERVAL: Duration = Duration::from_millis(100);
/// Fading a thumbnail in uploads it this many times, at rising opacity.
const OPACITY_STEPS: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    NewColumn,
    FocusLeft,
    FocusRight,
    FocusFirst,
    FocusLast,
    MoveLeft,
    MoveRight,
    FocusUp,
    FocusDown,
    MoveUp,
    MoveDown,
    ConsumeOrExpelLeft,
    ConsumeOrExpelRight,
    ConsumeIntoColumn,
    ExpelFromColumn,
    CycleWidth,
    ToggleMaximized,
    ToggleFullscreen,
    Center,
    Close,
    FocusWorkspaceDown,
    FocusWorkspaceUp,
    MoveColumnToWorkspaceDown,
    MoveColumnToWorkspaceUp,
    ToggleOverview,
    ExitOverview,
    ToggleThumbnails,
    Detach,
    Quit,
}

/// A piece of the status bar.
struct Segment {
    text: String,
    style: Style,
    /// What clicking it does.
    target: Option<StatusTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusTarget {
    Workspace(usize),
    Column(usize),
}

/// What's under the mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    /// A pane, and if not on its border, the cell within its box (relative
    /// to the box's inside) and the content point there.
    Pane {
        id: PaneId,
        inner: Option<(u16, u16, Point)>,
    },
    /// An empty workspace's row.
    EmptyWorkspace(usize),
    Status(StatusTarget),
    Nothing,
}

/// Lines per wheel notch, in scrollback or as arrow keys.
const WHEEL_LINES: i32 = 3;
/// Presses on the same cell this close together make a double or
/// triple click.
const MULTI_CLICK: Duration = Duration::from_millis(400);

/// An overview thumbnail uploaded to a client's terminal.
struct Thumbnail {
    /// Placement size in cells.
    size: (u16, u16),
    /// The pane's generation when this was drawn.
    generation: u64,
    uploaded: Instant,
    /// The image as drawn, kept for fading it in.
    image: thumbnail::Image,
    /// The opacity it was last uploaded at, in steps of [`OPACITY_STEPS`].
    opacity: u8,
}

/// Uploads `thumb` as image `image_id` if it's not already there at
/// `opacity` (rounded to a few steps), and places it at `size` cells if it
/// isn't already.
fn upload(
    escapes: &mut Vec<u8>,
    image_id: u32,
    thumb: &mut Thumbnail,
    size: (u16, u16),
    opacity: f32,
) {
    let step = (opacity.clamp(0.0, 1.0) * f32::from(OPACITY_STEPS)).round() as u8;
    let uploading = thumb.opacity != step;
    if uploading {
        if step == OPACITY_STEPS {
            kitty::transmit(escapes, image_id, &thumb.image);
        } else {
            let faded = thumb
                .image
                .with_opacity(f32::from(step) / f32::from(OPACITY_STEPS));
            kitty::transmit(escapes, image_id, &faded);
        }
        thumb.opacity = step;
    }
    if uploading || thumb.size != size {
        kitty::place(escapes, image_id, size.0, size.1);
        thumb.size = size;
    }
}

/// One attached terminal: its size, its prefix-key and overview settings,
/// the thumbnails uploaded to it, and the renderer that remembers what it
/// was last sent.
pub struct Client {
    id: ClientId,
    width: u16,
    height: u16,
    /// Where panes this client opens start.
    cwd: PathBuf,
    /// Its terminal's colors, for its thumbnails and for answering
    /// programs that ask.
    palette: Palette,
    /// Its thumbnails' cell size, the same shape as its terminal's cells.
    thumbnail_cell: thumbnail::CellSize,
    /// Set when the client asks to detach; the server then lets it go.
    detach_requested: bool,
    prefix_pending: bool,
    /// Whether this client's overview shows kitty graphics thumbnails
    /// instead of text.
    kitty_overview: bool,
    thumbnails: HashMap<PaneId, Thumbnail>,
    /// Escape sequences to send before the next frame: kitty graphics
    /// commands and clipboard writes.
    escapes: Vec<u8>,
    renderer: Renderer,
    /// How far back this client has scrolled each pane it's scrolled.
    scrollback: HashMap<PaneId, Scrollback>,
    /// Text selected with the mouse, highlighted until the next click or key.
    selection: Option<Selection>,
    /// What the left button is doing while held.
    drag: Drag,
    /// When and where the last left press in a pane was, and how many
    /// presses in a row it made, to spot double and triple clicks.
    last_click: Option<(Instant, PaneId, Point, u8)>,
    /// Visual effects running on this client's screen.
    effects: Effects,
    /// The overview fading in or out, if it is.
    transition: Option<Transition>,
}

/// How far back a client has scrolled a pane, and how much history the pane
/// had then, so output arriving meanwhile doesn't drag the view along.
#[derive(Debug, Clone, Copy)]
struct Scrollback {
    lines: usize,
    history: usize,
}

/// What a held left button is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Drag {
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
        self.escapes
            .extend_from_slice(format!("\x1b]52;c;{encoded}\x07").as_bytes());
    }

    /// How many lines back this client is scrolled in `pane`. Output since
    /// it scrolled pushes the view further back, so what it was reading
    /// stays put.
    fn scrolled(&self, id: PaneId, pane: &Pane) -> usize {
        let Some(scroll) = self.scrollback.get(&id) else {
            return 0;
        };
        let history = pane.history_size();
        (scroll.lines + history.saturating_sub(scroll.history)).min(history)
    }

    /// Scrolls `pane` back by `lines` (forward if negative), returning to
    /// the live screen at the bottom.
    fn scroll(&mut self, id: PaneId, pane: &Pane, lines: i32) {
        let history = pane.history_size();
        let current = self.scrolled(id, pane) as i32;
        let target = (current + lines).clamp(0, history as i32) as usize;
        if target == 0 {
            self.scrollback.remove(&id);
        } else {
            self.scrollback.insert(
                id,
                Scrollback {
                    lines: target,
                    history,
                },
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

    fn clear_thumbnails(&mut self) {
        for (id, _) in self.thumbnails.drain() {
            kitty::delete(&mut self.escapes, THUMBNAIL_ID_BASE + id.0);
        }
    }

    /// Frees the thumbnails of panes for which `keep` is false.
    fn retain_thumbnails(&mut self, keep: impl Fn(&PaneId) -> bool) {
        let escapes = &mut self.escapes;
        self.thumbnails.retain(|id, _| {
            let kept = keep(id);
            if !kept {
                kitty::delete(escapes, THUMBNAIL_ID_BASE + id.0);
            }
            kept
        });
    }

    /// Keeps `id`'s thumbnail current: draws it if there's none yet or the
    /// pane changed (at most every [`THUMBNAIL_INTERVAL`]), uploads it at
    /// `opacity` (0 to 1, in a few steps, for fading it in), and sizes its
    /// placement to `size` cells.
    fn refresh_thumbnail(
        &mut self,
        panes: &HashMap<PaneId, Pane>,
        id: PaneId,
        size: (u16, u16),
        opacity: f32,
        now: Instant,
    ) {
        let Some(pane) = panes.get(&id) else {
            return;
        };
        let image_id = THUMBNAIL_ID_BASE + id.0;
        let generation = pane.generation();
        let stale = self
            .thumbnails
            .get(&id)
            .is_none_or(|t| t.generation != generation && now >= t.uploaded + THUMBNAIL_INTERVAL);
        if stale {
            let image = thumbnail::rasterize(pane.term(), &self.palette, self.thumbnail_cell);
            self.thumbnails.insert(
                id,
                Thumbnail {
                    size,
                    generation,
                    uploaded: now,
                    image,
                    // Not uploaded yet.
                    opacity: u8::MAX,
                },
            );
        }
        let thumb = self.thumbnails.get_mut(&id).expect("inserted if missing");
        upload(&mut self.escapes, image_id, thumb, size, opacity);
    }

    /// Re-uploads the thumbnails already shown at `opacity`, as the overview
    /// fades out.
    fn fade_thumbnails(&mut self, opacity: f32) {
        for (id, thumb) in &mut self.thumbnails {
            let size = thumb.size;
            upload(
                &mut self.escapes,
                THUMBNAIL_ID_BASE + id.0,
                thumb,
                size,
                opacity,
            );
        }
    }

    /// The inner size of a pane's box in cells, as a thumbnail placement.
    fn thumbnail_size(w: i32, h: i32) -> (u16, u16) {
        let clamp = |n: i32| (n.max(1) as u16).min(kitty::MAX_CELLS);
        (clamp(w - 2), clamp(h - 2))
    }

    fn area_height(&self) -> i32 {
        i32::from(self.height.saturating_sub(STATUS_HEIGHT))
    }
}

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
    /// Starts with a named workspace for each of `names` (plus the usual
    /// empty one). The first client to attach gets a shell.
    pub fn new(names: &[String], poller: Arc<Poller>) -> Self {
        let layout_size = (80, 24);
        Self {
            workspaces: Workspaces::new(layout_size.0, names),
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

    /// Advances every client's scroll, slide and zoom animations. Returns
    /// true while anything still moves.
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

    /// Where pane `id` is on `client`'s screen, clipped to the pane area.
    fn pane_area(&self, client: &Client, id: PaneId) -> Option<ratatui_core::layout::Rect> {
        let screen = (i32::from(client.width), client.area_height());
        for ws in self.visible_workspaces(client) {
            for idx in self.visible_columns(client, ws) {
                for (pane, x, y, w, h) in self.pane_boxes(client, ws, idx) {
                    if pane != id {
                        continue;
                    }
                    let (left, top) = (x.max(0), y.max(0));
                    let (right, bottom) = ((x + w).min(screen.0), (y + h).min(screen.1));
                    return (right > left && bottom > top).then(|| {
                        ratatui_core::layout::Rect::new(
                            left as u16,
                            top as u16,
                            (right - left) as u16,
                            (bottom - top) as u16,
                        )
                    });
                }
            }
        }
        None
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

    /// Thumbnails show whenever the view is zoomed out, which is whenever the
    /// overview is open.
    fn showing_thumbnails(&self, client: &Client) -> bool {
        client.kitty_overview && self.workspaces.zoom(client.id) < 1.0
    }

    /// The height of a workspace row on `client`'s screen: its whole pane
    /// area, or less as its overview zooms out.
    fn row_height(&self, client: &Client) -> i32 {
        let area = client.area_height();
        ((f64::from(area) * self.workspaces.zoom(client.id)).round() as i32)
            .clamp(area.min(3), area)
    }

    /// Where workspace `ws`'s row starts on `client`'s screen. Its active
    /// workspace is centered; the others stack above and below it, sliding
    /// as the active one changes. Zoomed out, a line between rows holds
    /// their labels.
    fn row_top(&self, client: &Client, ws: usize) -> i32 {
        let (area, row) = (client.area_height(), self.row_height(client));
        let gap = if self.workspaces.zoom(client.id) < 1.0 {
            1
        } else {
            0
        };
        let pitch = f64::from(row + gap);
        let from_active = ws as f64 - self.workspaces.y(client.id);
        (area - row) / 2 + (from_active * pitch).round() as i32
    }

    /// The workspaces with any part of their row on `client`'s screen.
    fn visible_workspaces(&self, client: &Client) -> Vec<usize> {
        let (area, row) = (client.area_height(), self.row_height(client));
        (0..self.workspaces.list().len())
            .filter(|&ws| {
                let top = self.row_top(client, ws);
                top + row > 0 && top < area
            })
            .collect()
    }

    /// Where column `idx` of workspace `ws` is drawn: x, y, width, height.
    fn column_box(&self, client: &Client, ws: usize, idx: usize) -> (i32, i32, i32, i32) {
        let (x, w) = self.workspaces.column_span(client.id, ws, idx);
        (x, self.row_top(client, ws), w, self.row_height(client))
    }

    /// The columns of workspace `ws` at least partly on `client`'s screen.
    fn visible_columns(&self, client: &Client, ws: usize) -> Vec<usize> {
        (0..self.workspaces.list()[ws].strip().columns().len())
            .filter(|&idx| {
                let (x, _, w, _) = self.column_box(client, ws, idx);
                x + w > 0 && x < i32::from(client.width)
            })
            .collect()
    }

    /// Where each pane in column `idx` of workspace `ws` is drawn, top to
    /// bottom, borders included: the column's box split among its panes.
    fn pane_boxes(
        &self,
        client: &Client,
        ws: usize,
        idx: usize,
    ) -> Vec<(PaneId, i32, i32, i32, i32)> {
        let (x, mut y, w, h) = self.column_box(client, ws, idx);
        let column = &self.workspaces.list()[ws].strip().columns()[idx];
        // A fullscreen pane has its column to itself; the rest are hidden.
        if let Some(id) = column.fullscreen() {
            return vec![(id, x, y, w, h)];
        }
        let panes = column.panes();
        panes
            .iter()
            .zip(split_heights(h, panes.len()))
            .map(|(&id, h)| {
                let pane_box = (id, x, y, w, h);
                y += h;
                pane_box
            })
            .collect()
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

    pub fn key(&mut self, client: &mut Client, key: KeyEvent) -> Result<()> {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        self.lay_out_for(client);
        let is_prefix = key.code == KeyCode::Char(PREFIX) && key.modifiers == KeyModifiers::CONTROL;

        if std::mem::take(&mut client.prefix_pending) {
            if is_prefix {
                // Prefix twice sends it through to the pane.
                if let Some(pane) = self.focused_pane_mut(client) {
                    pane.write(&[PREFIX as u8 - b'a' + 1]);
                }
            } else if let Some(action) = prefix_binding(key) {
                self.run(client, action)?;
            }
            return Ok(());
        }
        if is_prefix {
            client.prefix_pending = true;
            return Ok(());
        }
        if self.workspaces.in_overview(client.id) {
            // The overview takes the keyboard; nothing reaches the panes.
            if let Some(action) = overview_binding(key).or_else(|| alt_binding(key)) {
                self.run(client, action)?;
            }
            return Ok(());
        }
        if let Some(action) = alt_binding(key) {
            return self.run(client, action);
        }
        client.selection = None;
        if let Some(id) = self.workspaces.focused(client.id) {
            // Typing returns to the live screen, as in any terminal.
            client.scrollback.remove(&id);
        }
        if let Some(pane) = self.focused_pane_mut(client) {
            let bytes = encode_key(key, pane.application_cursor());
            pane.write(&bytes);
        }
        Ok(())
    }

    /// What's at (`x`, `y`) on `client`'s screen.
    fn hit(&self, client: &Client, x: i32, y: i32) -> Hit {
        if y == i32::from(client.height) - 1 {
            let mut left = 0;
            for segment in self.status_segments(client) {
                let right = left + segment.text.chars().count() as i32;
                if (left..right).contains(&x) {
                    return segment.target.map_or(Hit::Nothing, Hit::Status);
                }
                left = right;
            }
            return Hit::Nothing;
        }
        for ws in self.visible_workspaces(client) {
            for idx in self.visible_columns(client, ws) {
                for (id, bx, by, w, h) in self.pane_boxes(client, ws, idx) {
                    if !(bx..bx + w).contains(&x) || !(by..by + h).contains(&y) {
                        continue;
                    }
                    let (cx, cy) = (x - bx - 1, y - by - 1);
                    let inside = (0..w - 2).contains(&cx) && (0..h - 2).contains(&cy);
                    let inner = self.panes.get(&id).filter(|_| inside).map(|pane| {
                        let top = content_top(pane, h - 2, client.scrolled(id, pane));
                        let point = Point {
                            line: top + cy,
                            col: cx as u16,
                        };
                        (cx as u16, cy as u16, point)
                    });
                    return Hit::Pane { id, inner };
                }
            }
            let top = self.row_top(client, ws);
            let in_row = (top..top + self.row_height(client)).contains(&y);
            if in_row && self.workspaces.list()[ws].is_empty() {
                return Hit::EmptyWorkspace(ws);
            }
        }
        Hit::Nothing
    }

    /// Where (`x`, `y`) falls within pane `id`'s box on `client`'s screen,
    /// clamped to its inside, for drags that wander off the pane: the
    /// (column, row) inside the box, the content point there, and -1, 0 or
    /// 1 for whether `y` was above, within or below the box.
    fn clamped_point(
        &self,
        client: &Client,
        id: PaneId,
        x: i32,
        y: i32,
    ) -> Option<(u16, u16, Point, i32)> {
        let pane = self.panes.get(&id)?;
        for ws in self.visible_workspaces(client) {
            for idx in self.visible_columns(client, ws) {
                for (pane_id, bx, by, w, h) in self.pane_boxes(client, ws, idx) {
                    if pane_id != id || w < 3 || h < 3 {
                        continue;
                    }
                    let cx = (x - bx - 1).clamp(0, w - 3);
                    let raw_y = y - by - 1;
                    let cy = raw_y.clamp(0, h - 3);
                    let top = content_top(pane, h - 2, client.scrolled(id, pane));
                    let point = Point {
                        line: top + cy,
                        col: cx as u16,
                    };
                    return Some((cx as u16, cy as u16, point, (raw_y - cy).signum()));
                }
            }
        }
        None
    }

    /// Passes a mouse event to pane `id`'s program at (`col`, `row`) within
    /// it, if it asked for that kind of event.
    fn forward_mouse(
        &mut self,
        id: PaneId,
        kind: MouseEventKind,
        col: u16,
        row: u16,
        mods: KeyModifiers,
    ) {
        if let Some(pane) = self.panes.get_mut(&id)
            && let Some(bytes) = encode_mouse(kind, col, row, mods, pane.mouse_modes())
        {
            pane.write(&bytes);
        }
    }

    /// Handles a mouse event from `client`.
    pub fn mouse(&mut self, client: &mut Client, event: MouseEvent) {
        self.lay_out_for(client);
        let (x, y) = (i32::from(event.column), i32::from(event.row));
        let overview = self.workspaces.in_overview(client.id);
        let shift = event.modifiers.contains(KeyModifiers::SHIFT);
        match event.kind {
            // Shift+wheel steps one column per tick, which lands squarely on
            // a column. macOS sends Shift+wheel as horizontal ticks, so take
            // either axis. Horizontal ticks without Shift are ignored: they
            // leak from trackpads during ordinary scrolling and selecting,
            // and terminals don't report gestures well enough to snap a
            // free scroll the way niri does.
            MouseEventKind::ScrollUp | MouseEventKind::ScrollLeft if shift => {
                self.workspaces.active_mut(client.id).focus_left();
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollRight if shift => {
                self.workspaces.active_mut(client.id).focus_right();
            }
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {}
            MouseEventKind::ScrollUp if overview => self.workspaces.focus_up(client.id),
            MouseEventKind::ScrollDown if overview => self.workspaces.focus_down(client.id),
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.wheel(client, event, x, y);
            }
            MouseEventKind::Down(MouseButton::Left) => self.press(client, event, x, y),
            MouseEventKind::Drag(MouseButton::Left) => self.drag(client, event, x, y),
            MouseEventKind::Up(MouseButton::Left) => self.release(client, event, x, y),
            // Other buttons, and movement, only matter to programs that
            // asked for them, in the focused pane.
            kind => {
                if let Hit::Pane {
                    id,
                    inner: Some((col, row, _)),
                } = self.hit(client, x, y)
                    && Some(id) == self.workspaces.focused(client.id)
                    && !overview
                {
                    self.forward_mouse(id, kind, col, row, event.modifiers);
                }
            }
        }
    }

    /// The vertical wheel over a pane: to its program if it takes the
    /// mouse, as arrow keys in a full-screen program that doesn't, and
    /// otherwise through the client's view of its scrollback.
    fn wheel(&mut self, client: &mut Client, event: MouseEvent, x: i32, y: i32) {
        let Hit::Pane { id, inner } = self.hit(client, x, y) else {
            return;
        };
        let Some(pane) = self.panes.get_mut(&id) else {
            return;
        };
        let up = event.kind == MouseEventKind::ScrollUp;
        if pane.mouse_modes().any() {
            if let Some((col, row, _)) = inner {
                self.forward_mouse(id, event.kind, col, row, event.modifiers);
            }
        } else if pane.alternate_scroll() {
            let arrow: &[u8] = match (up, pane.application_cursor()) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            for _ in 0..WHEEL_LINES {
                pane.write(arrow);
            }
        } else {
            client.scroll(id, pane, if up { WHEEL_LINES } else { -WHEEL_LINES });
        }
    }

    fn press(&mut self, client: &mut Client, event: MouseEvent, x: i32, y: i32) {
        client.selection = None;
        client.drag = Drag::Ignored;
        let overview = self.workspaces.in_overview(client.id);
        match self.hit(client, x, y) {
            Hit::Status(StatusTarget::Workspace(ws)) => {
                self.workspaces.focus_workspace(client.id, ws);
            }
            Hit::Status(StatusTarget::Column(idx)) => {
                self.workspaces.active_mut(client.id).focus_column(idx);
            }
            Hit::EmptyWorkspace(ws) => {
                self.workspaces.focus_workspace(client.id, ws);
                self.set_overview(client, false);
            }
            Hit::Pane { id, inner } => {
                let focused = self.workspaces.focused(client.id) == Some(id);
                self.workspaces.focus_pane(client.id, id);
                if overview {
                    self.set_overview(client, false);
                    return;
                }
                // Counted before the focus check, so double-clicking a pane
                // that wasn't focused still selects a word. A fourth click
                // in a row starts over.
                let now = Instant::now();
                let clicks = inner.map_or(1, |(_, _, point)| match client.last_click {
                    Some((at, pane, last, n))
                        if pane == id && last == point && now - at < MULTI_CLICK =>
                    {
                        n % 3 + 1
                    }
                    _ => 1,
                });
                client.last_click = inner.map(|(_, _, point)| (now, id, point, clicks));

                // A click that focuses a pane isn't passed to its program.
                let Some((col, row, point)) = inner.filter(|_| focused || clicks > 1) else {
                    return;
                };
                let Some(pane) = self.panes.get(&id) else {
                    return;
                };
                if pane.mouse_modes().any() {
                    self.forward_mouse(id, event.kind, col, row, event.modifiers);
                    client.drag = Drag::Forwarded(id);
                } else {
                    let (anchor, head) = match clicks {
                        2 => selection::word_at(pane.term(), point),
                        3 => selection::line_at(pane.term(), point),
                        _ => (point, point),
                    };
                    client.drag = Drag::Selecting {
                        pane: id,
                        snapped: clicks > 1,
                    };
                    client.selection = Some(Selection {
                        pane: id,
                        anchor,
                        head,
                    });
                }
            }
            Hit::Nothing => {}
        }
    }

    fn drag(&mut self, client: &mut Client, event: MouseEvent, x: i32, y: i32) {
        match client.drag {
            Drag::Forwarded(id) => {
                if let Some((col, row, _, _)) = self.clamped_point(client, id, x, y) {
                    self.forward_mouse(id, event.kind, col, row, event.modifiers);
                }
            }
            Drag::Selecting { pane: id, .. } => {
                // Dragging above or below the pane scrolls it along.
                if let Some((_, _, _, past)) = self.clamped_point(client, id, x, y)
                    && past != 0
                    && let Some(pane) = self.panes.get(&id)
                {
                    client.scroll(id, pane, -past);
                }
                if let Some((_, _, point, _)) = self.clamped_point(client, id, x, y)
                    && let Some(selection) = client.selection.as_mut()
                {
                    selection.head = point;
                }
            }
            Drag::None | Drag::Ignored => {}
        }
    }

    fn release(&mut self, client: &mut Client, event: MouseEvent, x: i32, y: i32) {
        match std::mem::take(&mut client.drag) {
            Drag::Forwarded(id) => {
                if let Some((col, row, _, _)) = self.clamped_point(client, id, x, y) {
                    self.forward_mouse(id, event.kind, col, row, event.modifiers);
                }
            }
            Drag::Selecting { pane: id, snapped } => {
                let Some(selection) = client.selection.filter(|s| snapped || !s.is_empty()) else {
                    // A plain click selects nothing.
                    client.selection = None;
                    return;
                };
                if let Some(pane) = self.panes.get(&id) {
                    let (start, end) = selection.bounds();
                    let text = selection::text(pane.term(), start, end);
                    if !text.is_empty() {
                        client.copy(&text);
                    }
                }
            }
            Drag::None | Drag::Ignored => {}
        }
    }

    /// Text programs in panes have copied (OSC 52), for passing on to the
    /// clients' clipboards.
    pub fn take_copied(&mut self) -> Vec<String> {
        self.panes
            .values_mut()
            .flat_map(Pane::take_copied)
            .collect()
    }

    fn run(&mut self, client: &mut Client, action: Action) -> Result<()> {
        let id = client.id;
        match action {
            Action::NewColumn => self.open_column(client)?,
            Action::FocusLeft => self.workspaces.active_mut(id).focus_left(),
            Action::FocusRight => self.workspaces.active_mut(id).focus_right(),
            Action::FocusFirst => self.workspaces.active_mut(id).focus_first(),
            Action::FocusLast => self.workspaces.active_mut(id).focus_last(),
            Action::MoveLeft => self.workspaces.active_mut(id).move_left(),
            Action::MoveRight => self.workspaces.active_mut(id).move_right(),
            Action::FocusUp => self.workspaces.active_mut(id).focus_up(),
            Action::FocusDown => self.workspaces.active_mut(id).focus_down(),
            Action::MoveUp => self.workspaces.active_mut(id).move_up(),
            Action::MoveDown => self.workspaces.active_mut(id).move_down(),
            Action::ConsumeOrExpelLeft => {
                self.workspaces.active_mut(id).consume_or_expel_left();
                self.resize_panes();
            }
            Action::ConsumeOrExpelRight => {
                self.workspaces.active_mut(id).consume_or_expel_right();
                self.resize_panes();
            }
            Action::ConsumeIntoColumn => {
                self.workspaces.active_mut(id).consume_into_column();
                self.resize_panes();
            }
            Action::ExpelFromColumn => {
                self.workspaces.active_mut(id).expel_from_column();
                self.resize_panes();
            }
            Action::CycleWidth => {
                self.workspaces.active_mut(id).cycle_width();
                self.resize_panes();
            }
            Action::ToggleMaximized => {
                self.workspaces.active_mut(id).toggle_maximized();
                self.resize_panes();
            }
            Action::ToggleFullscreen => {
                self.workspaces.active_mut(id).toggle_fullscreen();
                self.resize_panes();
            }
            Action::Center => self.workspaces.active_mut(id).center_focused(),
            Action::Close => {
                if let Some(pane_id) = self.workspaces.focused(id) {
                    if let Some(pane) = self.panes.get(&pane_id) {
                        pane.kill();
                    }
                    self.pane_exited(pane_id);
                }
            }
            Action::FocusWorkspaceDown => self.workspaces.focus_down(id),
            Action::FocusWorkspaceUp => self.workspaces.focus_up(id),
            Action::MoveColumnToWorkspaceDown => self.workspaces.move_column_down(id),
            Action::MoveColumnToWorkspaceUp => self.workspaces.move_column_up(id),
            Action::ToggleOverview => {
                let on = !self.workspaces.in_overview(id);
                self.set_overview(client, on);
            }
            Action::ExitOverview => self.set_overview(client, false),
            Action::ToggleThumbnails => client.kitty_overview = !client.kitty_overview,
            Action::Detach => client.detach_requested = true,
            Action::Quit => self.quit = true,
        }
        Ok(())
    }

    /// Composes `client`'s view of the workspaces plus its status bar.
    /// Returns the frame and where the cursor should be shown, if anywhere.
    pub fn draw(&self, client: &mut Client) -> (Frame, Option<(u16, u16)>) {
        let visible: Vec<(usize, usize)> = (self.visible_workspaces(client).into_iter())
            .flat_map(|ws| {
                self.visible_columns(client, ws)
                    .into_iter()
                    .map(move |idx| (ws, idx))
            })
            .collect();

        let thumbnails = self.showing_thumbnails(client);
        if thumbnails {
            let now = Instant::now();
            // Fully opaque unless the overview is fading in.
            let opacity = (client.transition.as_ref()).map_or(1.0, Transition::image_opacity);
            let mut drawn = HashSet::new();
            for &(ws, idx) in &visible {
                for (id, _, _, w, h) in self.pane_boxes(client, ws, idx) {
                    let size = Client::thumbnail_size(w, h);
                    client.refresh_thumbnail(&self.panes, id, size, opacity, now);
                    drawn.insert(id);
                }
            }
            // Panes scrolled out of view would otherwise keep asking for
            // redraws they never get.
            client.retain_thumbnails(|id| drawn.contains(id));
        } else if let Some(transition) = &client.transition {
            // The overview fading out: its thumbnails fade with it.
            let opacity = transition.image_opacity();
            client.retain_thumbnails(|id| self.panes.contains_key(id));
            client.fade_thumbnails(opacity);
        } else {
            client.clear_thumbnails();
        }

        // Effects stay off while thumbnails show: they'd change the colors
        // that tell the terminal which image a cell shows.
        if thumbnails {
            client.effects.clear();
        }

        let (mut frame, cursor) = self.compose(client, &visible, thumbnails);
        let areas: Vec<_> = (client.effects.anchors().into_iter())
            .map(|anchor| match anchor {
                Anchor::Pane(id) => (anchor, self.pane_area(client, id)),
            })
            .collect();
        let palette = client.palette;
        let now = Instant::now();
        client.effects.apply(&mut frame, &palette, now, &areas);
        if let Some(transition) = client.transition.as_mut() {
            if transition.apply(&mut frame, &palette, now) {
                return (frame, None);
            }
            client.transition = None;
            // Thumbnails kept for the overview going out can go now; no more
            // frames may come to do it later.
            if !thumbnails {
                client.clear_thumbnails();
            }
        }
        (frame, cursor)
    }

    /// Opens or closes `client`'s overview. The layout changes at once, and
    /// a [`Transition`] fades between the two.
    fn set_overview(&mut self, client: &mut Client, on: bool) {
        if on == self.workspaces.in_overview(client.id) {
            return;
        }
        self.workspaces.set_overview(client.id, on);
        self.workspaces.snap(client.id);
        if let Some(from) = client.renderer.last_frame().cloned() {
            client.transition = Some(if on {
                Transition::opening(from, &client.palette)
            } else {
                Transition::closing(from, &client.palette)
            });
        }
    }

    /// Composes the frame itself: every visible pane, labels and status bar.
    fn compose(
        &self,
        client: &Client,
        visible: &[(usize, usize)],
        thumbnails: bool,
    ) -> (Frame, Option<(u16, u16)>) {
        let mut frame = Frame::new(client.width, client.height);
        let overview = self.workspaces.in_overview(client.id);
        let show_cursor = !overview && !self.workspaces.is_animating(client.id);
        let mut cursor = None;

        for ws in self.visible_workspaces(client) {
            self.draw_workspace_label(client, &mut frame, ws);
        }
        if overview {
            self.draw_offscreen_indicators(client, &mut frame);
        }
        let active_ws = self.workspaces.active_index(client.id);
        for &(ws, idx) in visible {
            let strip = self.workspaces.list()[ws].strip();
            let column = &strip.columns()[idx];
            let stacked = column.panes().len() > 1;
            for (id, x, y, w, h) in self.pane_boxes(client, ws, idx) {
                let Some(pane) = self.panes.get(&id) else {
                    continue;
                };
                // Its place in the stack, even when it's alone on screen
                // because it's fullscreen.
                let row = column.panes().iter().position(|&p| p == id).unwrap_or(0);
                let focused =
                    ws == active_ws && idx == strip.focus_index() && row == column.focus_index();
                let border = if focused {
                    Style {
                        bold: true,
                        ..Style::fg(FOCUSED_BORDER)
                    }
                } else {
                    Style::fg(UNFOCUSED_BORDER)
                };
                draw_box(&mut frame, x, y, w, h, border);
                let number = if stacked {
                    format!("{}.{}", idx + 1, row + 1)
                } else {
                    format!("{}", idx + 1)
                };
                let number = if column.fullscreen() == Some(id) {
                    format!("{number} [fullscreen]")
                } else {
                    number
                };
                let scrolled = client.scrolled(id, pane);
                let title = if scrolled > 0 {
                    let history = pane.history_size();
                    format!(" {number}: {} [{scrolled}/{history}] ", pane.title())
                } else {
                    format!(" {number}: {} ", pane.title())
                };
                let title: String = title
                    .chars()
                    .take(w.saturating_sub(4).max(0) as usize)
                    .collect();
                frame.put_str(x + 2, y, &title, border);

                let top = content_top(pane, h - 2, scrolled);
                match client.thumbnails.get(&id) {
                    Some(thumb) if thumbnails => {
                        let style = Style::fg(kitty::id_color(THUMBNAIL_ID_BASE + id.0));
                        let (cols, rows) = thumb.size;
                        for r in 0..rows {
                            for c in 0..cols {
                                let cell = kitty::placeholder(r, c);
                                frame.put(x + 1 + i32::from(c), y + 1 + i32::from(r), &cell, style);
                            }
                        }
                    }
                    _ => draw_screen(
                        &mut frame,
                        pane,
                        id,
                        x + 1,
                        y + 1,
                        w - 2,
                        h - 2,
                        top,
                        client.selection.as_ref(),
                    ),
                }

                if focused && show_cursor && scrolled == 0 && pane.cursor_visible() {
                    let (r, c) = pane.cursor();
                    let (cx, cy) = (x + 1 + i32::from(c), y + 1 + i32::from(r) - top);
                    if (0..i32::from(client.width)).contains(&cx)
                        && (0..client.area_height()).contains(&cy)
                        && cy < y + h - 1
                    {
                        cursor = Some((cx as u16, cy as u16));
                    }
                }
            }
        }

        self.draw_status(client, &mut frame);
        (frame, cursor)
    }

    /// Whether `ws` is the empty workspace that's always kept at the bottom.
    fn is_new_workspace(&self, ws: usize) -> bool {
        let list = self.workspaces.list();
        ws + 1 == list.len() && list[ws].is_empty() && list[ws].name().is_none()
    }

    /// A workspace's name: its own, its position if it has none, or "+" for
    /// the empty one at the bottom.
    fn workspace_label(&self, ws: usize) -> String {
        match self.workspaces.list()[ws].name() {
            Some(name) => name.to_owned(),
            None if self.is_new_workspace(ws) => "+".to_owned(),
            None => format!("{}", ws + 1),
        }
    }

    /// Zoomed out, labels each workspace row on the line above it. An empty
    /// workspace gets a hint, or in the overview a placeholder box, so
    /// there's something to see and select.
    fn draw_workspace_label(&self, client: &Client, frame: &mut Frame, ws: usize) {
        let top = self.row_top(client, ws);
        let row_height = self.row_height(client);
        let zoom = self.workspaces.zoom(client.id);
        let active = ws == self.workspaces.active_index(client.id);
        let style = if active {
            Style {
                bold: true,
                ..Style::fg(FOCUSED_BORDER)
            }
        } else {
            Style::fg(DIM_TEXT)
        };
        if zoom < 1.0 {
            frame.put_str(
                1,
                top - 1,
                &format!(" {} ", self.workspace_label(ws)),
                style,
            );
        }
        if !self.workspaces.list()[ws].is_empty() {
            return;
        }
        let hint = match (self.is_new_workspace(ws), active) {
            (true, true) => "+ new workspace: C-a n opens a column",
            (true, false) => "+ new workspace",
            (false, true) => "empty workspace: C-a n opens a column",
            (false, false) => "empty workspace",
        };
        let hint_width = hint.chars().count() as i32;
        let middle = top + row_height / 2;
        if self.workspaces.in_overview(client.id) {
            // A box the size of a default column, where one would open.
            let w = ((f64::from(client.width) * 0.5 * zoom).round() as i32).max(hint_width + 4);
            let x = (i32::from(client.width) - w) / 2;
            draw_box(frame, x, top, w, row_height, style);
            frame.put_str(x + (w - hint_width) / 2, middle, hint, style);
        } else {
            let x = (i32::from(client.width) - hint_width) / 2;
            frame.put_str(x, middle, hint, Style::fg(DIM_TEXT));
        }
    }

    /// In the overview, notes at the top and bottom edges for workspace rows
    /// scrolled out of sight, so none of them get forgotten.
    fn draw_offscreen_indicators(&self, client: &Client, frame: &mut Frame) {
        let (area, row) = (client.area_height(), self.row_height(client));
        let count = self.workspaces.list().len();
        let above = (0..count)
            .filter(|&ws| self.row_top(client, ws) + row <= 0)
            .count();
        let below: Vec<usize> = (0..count)
            .filter(|&ws| self.row_top(client, ws) >= area)
            .collect();
        let style = Style {
            bg: STATUS_BG,
            ..Style::fg(STATUS_FG)
        };
        let mut note = |y: i32, text: String| {
            let x = i32::from(client.width) - text.chars().count() as i32 - 1;
            frame.put_str(x, y, &text, style);
        };
        if above > 0 {
            note(0, format!(" ▲ {above} above "));
        }
        if let Some(&last) = below.last() {
            let extra = if self.is_new_workspace(last) {
                " (incl. new)"
            } else {
                ""
            };
            note(area - 1, format!(" ▼ {} below{extra} ", below.len()));
        }
    }

    /// The status bar's left side, piece by piece: the workspaces, then
    /// markers for the active workspace's columns. Shared by drawing and
    /// by working out what a click on the bar hit.
    fn status_segments(&self, client: &Client) -> Vec<Segment> {
        let base = Style {
            bg: STATUS_BG,
            ..Style::fg(STATUS_FG)
        };
        let mut segments = vec![Segment {
            text: " tiri ".to_owned(),
            style: Style { bold: true, ..base },
            target: None,
        }];

        // The workspaces, top to bottom, ending with "+" for the empty one.
        let active_ws = self.workspaces.active_index(client.id);
        for ws in 0..self.workspaces.list().len() {
            let style = if ws == active_ws {
                Style {
                    bg: STATUS_FG,
                    fg: STATUS_BG,
                    bold: true,
                    ..base
                }
            } else if self.is_new_workspace(ws) {
                Style {
                    fg: DIM_TEXT,
                    ..base
                }
            } else {
                base
            };
            segments.push(Segment {
                text: format!(" {} ", self.workspace_label(ws)),
                style,
                target: Some(StatusTarget::Workspace(ws)),
            });
        }
        segments.push(Segment {
            text: " │ ".to_owned(),
            style: base,
            target: None,
        });

        // A minimap of the active workspace's columns: the focused one
        // filled, the rest hollow, dimmed when scrolled out of view.
        let strip = self.workspaces.active(client.id);
        for idx in 0..strip.columns().len() {
            let focused = idx == strip.focus_index();
            let style = if focused {
                Style {
                    fg: FOCUSED_BORDER,
                    bold: true,
                    ..base
                }
            } else {
                match strip.visibility(idx) {
                    Visibility::Full => Style { bold: true, ..base },
                    Visibility::Partial => base,
                    Visibility::Hidden => Style {
                        fg: DIM_TEXT,
                        ..base
                    },
                }
            };
            segments.push(Segment {
                text: if focused { "■ " } else { "□ " }.to_owned(),
                style,
                target: Some(StatusTarget::Column(idx)),
            });
        }
        segments
    }

    fn draw_status(&self, client: &Client, frame: &mut Frame) {
        let y = i32::from(client.height) - 1;
        let base = Style {
            bg: STATUS_BG,
            ..Style::fg(STATUS_FG)
        };
        frame.put_str(0, y, &" ".repeat(usize::from(client.width)), base);
        let mut x = 0;
        for segment in self.status_segments(client) {
            frame.put_str(x, y, &segment.text, segment.style);
            x += segment.text.chars().count() as i32;
        }

        let overview = self.workspaces.in_overview(client.id);
        let hint = if client.prefix_pending {
            "C-a: n new  hjkl focus  HJKL move  u/i workspace  U/I move to ws  [/] consume/expel  ,/. in/out  r width  f max  F full  o overview  x close  d detach  q kill server "
        } else if overview && client.kitty_overview {
            "OVERVIEW (kitty)  hjkl select  u/i workspace  HJKL/U/I move  x close  t text  ⏎/o/Esc open "
        } else if overview {
            "OVERVIEW  hjkl select  u/i workspace  HJKL/U/I move  x close  t thumbnails  ⏎/o/Esc open "
        } else {
            "C-a or Alt: n/⏎ new  h/l focus  u/i workspace  r width  o overview "
        };
        let hint_x = i32::from(client.width) - hint.chars().count() as i32;
        if hint_x > x + 1 {
            frame.put_str(hint_x, y, hint, base);
        }
    }
}

fn prefix_binding(key: KeyEvent) -> Option<Action> {
    let action = match key.code {
        KeyCode::Char('n') | KeyCode::Enter => Action::NewColumn,
        KeyCode::Char('h') | KeyCode::Left => Action::FocusLeft,
        KeyCode::Char('l') | KeyCode::Right => Action::FocusRight,
        KeyCode::Char('0') | KeyCode::Home => Action::FocusFirst,
        KeyCode::Char('$') | KeyCode::End => Action::FocusLast,
        KeyCode::Char('H') => Action::MoveLeft,
        KeyCode::Char('L') => Action::MoveRight,
        KeyCode::Char('j') | KeyCode::Down => Action::FocusDown,
        KeyCode::Char('k') | KeyCode::Up => Action::FocusUp,
        KeyCode::Char('J') => Action::MoveDown,
        KeyCode::Char('K') => Action::MoveUp,
        KeyCode::Char('u') | KeyCode::PageDown => Action::FocusWorkspaceDown,
        KeyCode::Char('i') | KeyCode::PageUp => Action::FocusWorkspaceUp,
        KeyCode::Char('U') => Action::MoveColumnToWorkspaceDown,
        KeyCode::Char('I') => Action::MoveColumnToWorkspaceUp,
        KeyCode::Char('[') => Action::ConsumeOrExpelLeft,
        KeyCode::Char(']') => Action::ConsumeOrExpelRight,
        KeyCode::Char(',') => Action::ConsumeIntoColumn,
        KeyCode::Char('.') => Action::ExpelFromColumn,
        KeyCode::Char('r') => Action::CycleWidth,
        KeyCode::Char('f') => Action::ToggleMaximized,
        KeyCode::Char('F') => Action::ToggleFullscreen,
        KeyCode::Char('c') => Action::Center,
        KeyCode::Char('o') => Action::ToggleOverview,
        KeyCode::Char('x') => Action::Close,
        KeyCode::Char('d') => Action::Detach,
        KeyCode::Char('q') => Action::Quit,
        _ => return None,
    };
    Some(action)
}

/// Plain keys while the overview is open.
fn overview_binding(key: KeyEvent) -> Option<Action> {
    if key
        .modifiers
        .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
    {
        return None;
    }
    let action = match key.code {
        KeyCode::Char('h') | KeyCode::Left => Action::FocusLeft,
        KeyCode::Char('l') | KeyCode::Right => Action::FocusRight,
        KeyCode::Char('0') | KeyCode::Home => Action::FocusFirst,
        KeyCode::Char('$') | KeyCode::End => Action::FocusLast,
        KeyCode::Char('H') => Action::MoveLeft,
        KeyCode::Char('L') => Action::MoveRight,
        KeyCode::Char('j') | KeyCode::Down => Action::FocusDown,
        KeyCode::Char('k') | KeyCode::Up => Action::FocusUp,
        KeyCode::Char('J') => Action::MoveDown,
        KeyCode::Char('K') => Action::MoveUp,
        KeyCode::Char('u') | KeyCode::PageDown => Action::FocusWorkspaceDown,
        KeyCode::Char('i') | KeyCode::PageUp => Action::FocusWorkspaceUp,
        KeyCode::Char('U') => Action::MoveColumnToWorkspaceDown,
        KeyCode::Char('I') => Action::MoveColumnToWorkspaceUp,
        KeyCode::Char('[') => Action::ConsumeOrExpelLeft,
        KeyCode::Char(']') => Action::ConsumeOrExpelRight,
        KeyCode::Char(',') => Action::ConsumeIntoColumn,
        KeyCode::Char('.') => Action::ExpelFromColumn,
        KeyCode::Char('n') => Action::NewColumn,
        KeyCode::Char('r') => Action::CycleWidth,
        KeyCode::Char('f') => Action::ToggleMaximized,
        KeyCode::Char('F') => Action::ToggleFullscreen,
        KeyCode::Char('x') => Action::Close,
        KeyCode::Char('t') => Action::ToggleThumbnails,
        KeyCode::Char('o') | KeyCode::Enter | KeyCode::Esc => Action::ExitOverview,
        _ => return None,
    };
    Some(action)
}

/// Direct niri-like bindings on Alt. On macOS these need the terminal's
/// "Option as Meta" setting.
fn alt_binding(key: KeyEvent) -> Option<Action> {
    if !key.modifiers.contains(KeyModifiers::ALT) || key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    let action = match key.code {
        KeyCode::Enter => Action::NewColumn,
        KeyCode::Char('h') | KeyCode::Left => Action::FocusLeft,
        KeyCode::Char('l') | KeyCode::Right => Action::FocusRight,
        KeyCode::Char('H') => Action::MoveLeft,
        KeyCode::Char('L') => Action::MoveRight,
        KeyCode::Char('j') | KeyCode::Down => Action::FocusDown,
        KeyCode::Char('k') | KeyCode::Up => Action::FocusUp,
        KeyCode::Char('J') => Action::MoveDown,
        KeyCode::Char('K') => Action::MoveUp,
        // Alt-[ would be read as the start of an escape sequence, so
        // consume-or-expel is on Alt-{ and Alt-} instead.
        KeyCode::Char('{') => Action::ConsumeOrExpelLeft,
        KeyCode::Char('}') => Action::ConsumeOrExpelRight,
        KeyCode::Char('u') | KeyCode::PageDown => Action::FocusWorkspaceDown,
        KeyCode::Char('i') | KeyCode::PageUp => Action::FocusWorkspaceUp,
        KeyCode::Char('U') => Action::MoveColumnToWorkspaceDown,
        KeyCode::Char('I') => Action::MoveColumnToWorkspaceUp,
        KeyCode::Char(',') => Action::ConsumeIntoColumn,
        KeyCode::Char('.') => Action::ExpelFromColumn,
        KeyCode::Char('r') => Action::CycleWidth,
        KeyCode::Char('f') => Action::ToggleMaximized,
        KeyCode::Char('F') => Action::ToggleFullscreen,
        KeyCode::Char('c') => Action::Center,
        KeyCode::Char('o') => Action::ToggleOverview,
        _ => return None,
    };
    Some(action)
}

fn draw_box(frame: &mut Frame, x: i32, y: i32, w: i32, h: i32, style: Style) {
    if w < 2 || h < 2 {
        return;
    }
    let (right, bottom) = (x + w - 1, y + h - 1);
    for cx in x + 1..right {
        frame.put(cx, y, "─", style);
        frame.put(cx, bottom, "─", style);
    }
    for cy in y + 1..bottom {
        frame.put(x, cy, "│", style);
        frame.put(right, cy, "│", style);
    }
    frame.put(x, y, "┌", style);
    frame.put(right, y, "┐", style);
    frame.put(x, bottom, "└", style);
    frame.put(right, bottom, "┘", style);
}

/// The first line of `pane` to show in a box `h` rows high: the screen's
/// top, or the rows around the cursor if the box is shorter (as in the
/// overview), moved up by however far the client has scrolled back.
fn content_top(pane: &Pane, h: i32, scrolled: usize) -> i32 {
    let (rows, _) = pane.size();
    let h = h.clamp(0, i32::from(rows)) as u16;
    let (cursor_row, _) = pane.cursor();
    let crop = (cursor_row + 1).saturating_sub(h).min(rows - h);
    i32::from(crop) - scrolled as i32
}

/// Copies part of a pane into a `w` x `h` box at (x, y), starting from
/// content line `top`, clipping at the frame's edges. Selected text is
/// drawn inverted.
#[allow(clippy::too_many_arguments)]
fn draw_screen(
    frame: &mut Frame,
    pane: &Pane,
    id: PaneId,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    top: i32,
    selection: Option<&Selection>,
) {
    let (rows, cols) = pane.size();
    let w = w.clamp(0, i32::from(cols)) as u16;
    let h = h.clamp(0, i32::from(rows));

    let mut sym = String::new();
    for row in 0..h {
        let line = top + row;
        let fy = y + row;
        for col in 0..w {
            let fx = x + i32::from(col);
            if fx < 0 || fx >= i32::from(frame.width()) || fy < 0 || fy >= i32::from(frame.height())
            {
                continue;
            }
            let cell = pane.cell(line, col);
            let mut style = cell_style(pane, cell);
            if selection.is_some_and(|s| s.contains(id, Point { line, col })) {
                style.inverse = !style.inverse;
            }
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                // Its first half is drawn by the cell to the left, unless that
                // half was scrolled off the left edge.
                if fx == 0 {
                    frame.put(fx, fy, " ", style);
                }
                continue;
            }

            sym.clear();
            if cell
                .flags
                .intersects(Flags::HIDDEN | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                sym.push(' ');
            } else {
                sym.push(cell.c);
                sym.extend(cell.zerowidth().into_iter().flatten());
            }
            if !cell.flags.contains(Flags::WIDE_CHAR) {
                frame.put(fx, fy, &sym, style);
            } else if col + 1 < w {
                frame.put_wide(fx, fy, &sym, style);
            } else {
                frame.put(fx, fy, " ", style);
            }
        }
    }
}

fn cell_style(pane: &Pane, cell: &alacritty_terminal::term::cell::Cell) -> Style {
    let flags = cell.flags;
    Style {
        fg: term_color(pane, cell.fg),
        bg: term_color(pane, cell.bg),
        bold: flags.contains(Flags::BOLD),
        dim: flags.contains(Flags::DIM) || is_dim_named(cell.fg),
        italic: flags.contains(Flags::ITALIC),
        underline: flags.intersects(Flags::ALL_UNDERLINES),
        inverse: flags.contains(Flags::INVERSE),
        strikeout: flags.contains(Flags::STRIKEOUT),
    }
}

fn is_dim_named(color: TermColor) -> bool {
    matches!(color, TermColor::Named(n) if (NamedColor::DimBlack..=NamedColor::DimWhite).contains(&n))
}

/// Maps an emulator color to one the outer terminal understands. Colors an
/// app redefined (OSC 4/10/11) are sent as RGB; the rest are left to the
/// outer terminal's palette so tiri matches its theme.
fn term_color(pane: &Pane, color: TermColor) -> Color {
    let rgb = |c: alacritty_terminal::vte::ansi::Rgb| Color::Rgb(c.r, c.g, c.b);
    match color {
        TermColor::Spec(c) => rgb(c),
        TermColor::Indexed(i) => pane.palette(usize::from(i)).map_or(Color::Idx(i), rgb),
        TermColor::Named(n) => {
            if let Some(c) = pane.palette(n as usize) {
                return rgb(c);
            }
            let idx = n as usize;
            if idx < 16 {
                Color::Idx(idx as u8)
            } else if is_dim_named(color) {
                Color::Idx((idx - NamedColor::DimBlack as usize) as u8)
            } else {
                Color::Default
            }
        }
    }
}
