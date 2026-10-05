//! Application state: the workspaces, their panes, keybindings, and drawing.

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color as TermColor, NamedColor};
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use polling::{Event as PollEvent, Poller};

use crate::input::encode_key;
use crate::kitty;
use crate::layout::{PaneId, Visibility, split_heights};
use crate::pane::Pane;
use crate::render::{Color, Frame, Style};
use crate::thumbnail;
use crate::workspace::Workspaces;

/// The prefix key, tmux-style: Ctrl-a, then a command key.
const PREFIX: char = 'a';
const STATUS_HEIGHT: u16 = 1;
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
    Center,
    Close,
    FocusWorkspaceDown,
    FocusWorkspaceUp,
    MoveColumnToWorkspaceDown,
    MoveColumnToWorkspaceUp,
    ToggleOverview,
    ExitOverview,
    ToggleThumbnails,
    Quit,
}

/// An overview thumbnail uploaded to the outer terminal.
struct Thumbnail {
    /// Placement size in cells.
    size: (u16, u16),
    /// The pane's generation when this was drawn.
    generation: u64,
    uploaded: Instant,
}

pub struct App {
    workspaces: Workspaces,
    panes: HashMap<PaneId, Pane>,
    next_id: u32,
    /// Watches every pane's PTY; panes are keyed by their id.
    poller: Arc<Poller>,
    width: u16,
    height: u16,
    prefix_pending: bool,
    /// Whether the overview shows kitty graphics thumbnails instead of text.
    kitty_overview: bool,
    thumbnails: HashMap<PaneId, Thumbnail>,
    /// Graphics protocol commands to send before the next frame.
    graphics: Vec<u8>,
    pub quit: bool,
}

impl App {
    /// Starts with a named workspace for each of `names` (plus the usual
    /// empty one), and a shell in the first.
    pub fn new(width: u16, height: u16, names: &[String], poller: Arc<Poller>) -> Result<Self> {
        let mut app = Self {
            workspaces: Workspaces::new(width, names),
            panes: HashMap::new(),
            next_id: 0,
            poller,
            width,
            height,
            prefix_pending: false,
            kitty_overview: std::env::var_os("TIRI_KITTY_OVERVIEW").is_some(),
            thumbnails: HashMap::new(),
            graphics: Vec::new(),
            quit: false,
        };
        app.open_column()?;
        Ok(app)
    }

    /// True once every pane in every workspace has gone.
    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    /// Advances the scroll, slide and zoom animations. Returns true while
    /// anything still moves.
    pub fn tick(&mut self, dt: Duration) -> bool {
        self.workspaces.tick(dt)
    }

    fn pane_rows(&self) -> u16 {
        self.height.saturating_sub(STATUS_HEIGHT + 2).max(1)
    }

    fn open_column(&mut self) -> Result<()> {
        let id = PaneId(self.next_id);
        self.next_id += 1;
        // Width isn't known until it's in the strip, so start narrow and fix it below.
        let pane = Pane::spawn(self.pane_rows(), 1)?;
        // SAFETY: the pane is deleted from the poller in `pane_exited` or
        // `shutdown`, before it's dropped and its PTY closed.
        unsafe {
            self.poller
                .add(pane.fd().as_raw_fd(), PollEvent::readable(id.0 as usize))
        }
        .context("failed to watch the new pane's pty")?;
        self.panes.insert(id, pane);
        self.workspaces.insert(id);
        self.resize_panes();
        Ok(())
    }

    /// Brings every pane's PTY size in line with its share of its column.
    fn resize_panes(&mut self) {
        let area = i32::from(self.height.saturating_sub(STATUS_HEIGHT));
        self.workspaces
            .set_max_stack((area / MIN_PANE_HEIGHT).max(1) as usize);
        for workspace in self.workspaces.list() {
            let strip = workspace.strip();
            for (idx, col) in strip.columns().iter().enumerate() {
                let cols = strip.column_width(idx).saturating_sub(2).max(1);
                let heights = split_heights(area, col.panes().len());
                for (id, h) in col.panes().iter().zip(heights) {
                    if let Some(pane) = self.panes.get_mut(id) {
                        pane.resize((h - 2).max(1) as u16, cols);
                    }
                }
            }
        }
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.workspaces.set_view_width(width);
        self.resize_panes();
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

    /// The next time something needs doing without any input: a pane's
    /// synchronized update timing out, or a thumbnail due for a redraw.
    pub fn next_deadline(&self) -> Option<Instant> {
        let stale_thumbnails = self.thumbnails.iter().filter_map(|(id, thumb)| {
            let pane = self.panes.get(id)?;
            (pane.generation() != thumb.generation).then(|| thumb.uploaded + THUMBNAIL_INTERVAL)
        });
        self.panes
            .values()
            .filter_map(Pane::sync_deadline)
            .chain(stale_thumbnails)
            .min()
    }

    /// Graphics commands to write before drawing the next frame.
    pub fn take_graphics(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.graphics)
    }

    pub fn expire_syncs(&mut self, now: Instant) {
        for pane in self.panes.values_mut() {
            pane.expire_sync(now);
        }
    }

    pub fn pane_exited(&mut self, id: PaneId) {
        if let Some(mut pane) = self.panes.remove(&id) {
            let _ = self.poller.delete(pane.fd());
            pane.reap();
        }
        if self.thumbnails.remove(&id).is_some() {
            kitty::delete(&mut self.graphics, THUMBNAIL_ID_BASE + id.0);
        }
        self.workspaces.remove(id);
        // Whatever shared its column grows into the space.
        self.resize_panes();
    }

    pub fn shutdown(&mut self) {
        for pane in self.panes.values_mut() {
            let _ = self.poller.delete(pane.fd());
            pane.kill();
        }
        self.clear_thumbnails();
    }

    fn clear_thumbnails(&mut self) {
        for (id, _) in self.thumbnails.drain() {
            kitty::delete(&mut self.graphics, THUMBNAIL_ID_BASE + id.0);
        }
    }

    /// Thumbnails show once the overview has settled, since a placement's
    /// size is fixed and the boxes change size while zooming.
    fn showing_thumbnails(&self) -> bool {
        self.kitty_overview && self.workspaces.in_overview() && !self.workspaces.is_animating()
    }

    /// Uploads a thumbnail of `id` sized `size` cells if there's none yet, it
    /// changed size, or the pane changed and the last upload isn't too recent.
    fn refresh_thumbnail(&mut self, id: PaneId, size: (u16, u16), now: Instant) {
        let Some(pane) = self.panes.get(&id) else {
            return;
        };
        let generation = pane.generation();
        let fresh = self.thumbnails.get(&id).is_some_and(|t| {
            t.size == size && (t.generation == generation || now < t.uploaded + THUMBNAIL_INTERVAL)
        });
        if fresh {
            return;
        }
        let image = thumbnail::rasterize(pane.term());
        kitty::upload(
            &mut self.graphics,
            THUMBNAIL_ID_BASE + id.0,
            &image,
            size.0,
            size.1,
        );
        self.thumbnails.insert(
            id,
            Thumbnail {
                size,
                generation,
                uploaded: now,
            },
        );
    }

    fn area_height(&self) -> i32 {
        i32::from(self.height.saturating_sub(STATUS_HEIGHT))
    }

    /// The height of a workspace row on screen: the whole pane area, or less
    /// as the overview zooms out.
    fn row_height(&self) -> i32 {
        let area = self.area_height();
        ((f64::from(area) * self.workspaces.zoom()).round() as i32).clamp(area.min(3), area)
    }

    /// Where workspace `ws`'s row starts on screen. The active workspace is
    /// centered; the others stack above and below it, sliding as the active
    /// one changes. Zoomed out, a line between rows holds their labels.
    fn row_top(&self, ws: usize) -> i32 {
        let (area, row) = (self.area_height(), self.row_height());
        let gap = if self.workspaces.zoom() < 1.0 { 1 } else { 0 };
        let pitch = f64::from(row + gap);
        (area - row) / 2 + ((ws as f64 - self.workspaces.y()) * pitch).round() as i32
    }

    /// The workspaces with any part of their row on screen.
    fn visible_workspaces(&self) -> Vec<usize> {
        let (area, row) = (self.area_height(), self.row_height());
        (0..self.workspaces.list().len())
            .filter(|&ws| {
                let top = self.row_top(ws);
                top + row > 0 && top < area
            })
            .collect()
    }

    /// Where column `idx` of workspace `ws` is drawn: x, y, width, height.
    fn column_box(&self, ws: usize, idx: usize) -> (i32, i32, i32, i32) {
        let (x, w) = self.workspaces.list()[ws].strip().column_span(idx);
        (x, self.row_top(ws), w, self.row_height())
    }

    /// The columns of workspace `ws` that are at least partly on screen.
    fn visible_columns(&self, ws: usize) -> Vec<usize> {
        (0..self.workspaces.list()[ws].strip().columns().len())
            .filter(|&idx| {
                let (x, _, w, _) = self.column_box(ws, idx);
                x + w > 0 && x < i32::from(self.width)
            })
            .collect()
    }

    /// Where each pane in column `idx` of workspace `ws` is drawn, top to
    /// bottom, borders included: the column's box split among its panes.
    fn pane_boxes(&self, ws: usize, idx: usize) -> Vec<(PaneId, i32, i32, i32, i32)> {
        let (x, mut y, w, h) = self.column_box(ws, idx);
        let panes = self.workspaces.list()[ws].strip().columns()[idx].panes();
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

    /// The inner size of a column's box in cells, as a thumbnail placement.
    fn thumbnail_size(w: i32, h: i32) -> (u16, u16) {
        let clamp = |n: i32| (n.max(1) as u16).min(kitty::MAX_CELLS);
        (clamp(w - 2), clamp(h - 2))
    }

    pub fn paste(&mut self, text: &str) {
        let Some(pane) = self.focused_pane_mut() else {
            return;
        };
        if pane.bracketed_paste() {
            pane.write(format!("\x1b[200~{text}\x1b[201~").as_bytes());
        } else {
            pane.write(text.as_bytes());
        }
    }

    fn focused_pane_mut(&mut self) -> Option<&mut Pane> {
        let id = self.workspaces.focused()?;
        self.panes.get_mut(&id)
    }

    pub fn key(&mut self, key: KeyEvent) -> Result<()> {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        let is_prefix = key.code == KeyCode::Char(PREFIX) && key.modifiers == KeyModifiers::CONTROL;

        if std::mem::take(&mut self.prefix_pending) {
            if is_prefix {
                // Prefix twice sends it through to the pane.
                if let Some(pane) = self.focused_pane_mut() {
                    pane.write(&[PREFIX as u8 - b'a' + 1]);
                }
            } else if let Some(action) = prefix_binding(key) {
                self.run(action)?;
            }
            return Ok(());
        }
        if is_prefix {
            self.prefix_pending = true;
            return Ok(());
        }
        if self.workspaces.in_overview() {
            // The overview takes the keyboard; nothing reaches the panes.
            if let Some(action) = overview_binding(key).or_else(|| alt_binding(key)) {
                self.run(action)?;
            }
            return Ok(());
        }
        if let Some(action) = alt_binding(key) {
            return self.run(action);
        }
        if let Some(pane) = self.focused_pane_mut() {
            let bytes = encode_key(key, pane.application_cursor());
            pane.write(&bytes);
        }
        Ok(())
    }

    fn run(&mut self, action: Action) -> Result<()> {
        match action {
            Action::NewColumn => self.open_column()?,
            Action::FocusLeft => self.workspaces.active_mut().focus_left(),
            Action::FocusRight => self.workspaces.active_mut().focus_right(),
            Action::FocusFirst => self.workspaces.active_mut().focus_first(),
            Action::FocusLast => self.workspaces.active_mut().focus_last(),
            Action::MoveLeft => self.workspaces.active_mut().move_left(),
            Action::MoveRight => self.workspaces.active_mut().move_right(),
            Action::FocusUp => self.workspaces.active_mut().focus_up(),
            Action::FocusDown => self.workspaces.active_mut().focus_down(),
            Action::MoveUp => self.workspaces.active_mut().move_up(),
            Action::MoveDown => self.workspaces.active_mut().move_down(),
            Action::ConsumeOrExpelLeft => {
                self.workspaces.active_mut().consume_or_expel_left();
                self.resize_panes();
            }
            Action::ConsumeOrExpelRight => {
                self.workspaces.active_mut().consume_or_expel_right();
                self.resize_panes();
            }
            Action::ConsumeIntoColumn => {
                self.workspaces.active_mut().consume_into_column();
                self.resize_panes();
            }
            Action::ExpelFromColumn => {
                self.workspaces.active_mut().expel_from_column();
                self.resize_panes();
            }
            Action::CycleWidth => {
                self.workspaces.active_mut().cycle_width();
                self.resize_panes();
            }
            Action::Center => self.workspaces.active_mut().center_focused(),
            Action::Close => {
                if let Some(id) = self.workspaces.focused() {
                    if let Some(pane) = self.panes.get_mut(&id) {
                        pane.kill();
                    }
                    self.pane_exited(id);
                }
            }
            Action::FocusWorkspaceDown => self.workspaces.focus_down(),
            Action::FocusWorkspaceUp => self.workspaces.focus_up(),
            Action::MoveColumnToWorkspaceDown => self.workspaces.move_column_down(),
            Action::MoveColumnToWorkspaceUp => self.workspaces.move_column_up(),
            Action::ToggleOverview => {
                let on = !self.workspaces.in_overview();
                self.workspaces.set_overview(on);
            }
            Action::ExitOverview => self.workspaces.set_overview(false),
            Action::ToggleThumbnails => self.kitty_overview = !self.kitty_overview,
            Action::Quit => self.quit = true,
        }
        Ok(())
    }

    /// Composes the visible part of the workspaces plus the status bar.
    /// Returns the frame and where the cursor should be shown, if anywhere.
    pub fn draw(&mut self) -> (Frame, Option<(u16, u16)>) {
        let visible: Vec<(usize, usize)> = (self.visible_workspaces().into_iter())
            .flat_map(|ws| {
                self.visible_columns(ws)
                    .into_iter()
                    .map(move |idx| (ws, idx))
            })
            .collect();

        let thumbnails = self.showing_thumbnails();
        if thumbnails {
            let now = Instant::now();
            for &(ws, idx) in &visible {
                for (id, _, _, w, h) in self.pane_boxes(ws, idx) {
                    self.refresh_thumbnail(id, Self::thumbnail_size(w, h), now);
                }
            }
        } else if !(self.kitty_overview && self.workspaces.in_overview())
            && !self.thumbnails.is_empty()
        {
            self.clear_thumbnails();
        }

        let mut frame = Frame::new(self.width, self.height);
        let overview = self.workspaces.in_overview();
        let show_cursor = !overview && !self.workspaces.is_animating();
        let mut cursor = None;

        for ws in self.visible_workspaces() {
            self.draw_workspace_label(&mut frame, ws);
        }
        if overview {
            self.draw_offscreen_indicators(&mut frame);
        }
        for (ws, idx) in visible {
            let strip = self.workspaces.list()[ws].strip();
            let column = &strip.columns()[idx];
            let stacked = column.panes().len() > 1;
            let active = ws == self.workspaces.active_index();
            for (row, (id, x, y, w, h)) in self.pane_boxes(ws, idx).into_iter().enumerate() {
                let Some(pane) = self.panes.get(&id) else {
                    continue;
                };
                let focused = active && idx == strip.focus_index() && row == column.focus_index();
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
                let title = format!(" {number}: {} ", pane.title());
                let title: String = title
                    .chars()
                    .take(w.saturating_sub(4).max(0) as usize)
                    .collect();
                frame.put_str(x + 2, y, &title, border);

                let first_row = match self.thumbnails.get(&id) {
                    Some(thumb) if thumbnails => {
                        let style = Style::fg(kitty::id_color(THUMBNAIL_ID_BASE + id.0));
                        let (cols, rows) = thumb.size;
                        for r in 0..rows {
                            for c in 0..cols {
                                let cell = kitty::placeholder(r, c);
                                frame.put(x + 1 + i32::from(c), y + 1 + i32::from(r), &cell, style);
                            }
                        }
                        0
                    }
                    _ => draw_screen(&mut frame, pane, x + 1, y + 1, w - 2, h - 2),
                };

                if focused && show_cursor && pane.cursor_visible() {
                    let (r, c) = pane.cursor();
                    let (cx, cy) = (x + 1 + i32::from(c), y + 1 + i32::from(r - first_row));
                    if (0..i32::from(self.width)).contains(&cx) && cy < y + h - 1 {
                        cursor = Some((cx as u16, cy as u16));
                    }
                }
            }
        }

        self.draw_status(&mut frame);
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
    fn draw_workspace_label(&self, frame: &mut Frame, ws: usize) {
        let top = self.row_top(ws);
        let active = ws == self.workspaces.active_index();
        let style = if active {
            Style {
                bold: true,
                ..Style::fg(FOCUSED_BORDER)
            }
        } else {
            Style::fg(DIM_TEXT)
        };
        if self.workspaces.zoom() < 1.0 {
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
        let middle = top + self.row_height() / 2;
        if self.workspaces.in_overview() {
            // A box the size of a default column, where one would open.
            let zoom = self.workspaces.zoom();
            let w = ((f64::from(self.width) * 0.5 * zoom).round() as i32).max(hint_width + 4);
            let x = (i32::from(self.width) - w) / 2;
            draw_box(frame, x, top, w, self.row_height(), style);
            frame.put_str(x + (w - hint_width) / 2, middle, hint, style);
        } else {
            let x = (i32::from(self.width) - hint_width) / 2;
            frame.put_str(x, middle, hint, Style::fg(DIM_TEXT));
        }
    }

    /// In the overview, notes at the top and bottom edges for workspace rows
    /// scrolled out of sight, so none of them get forgotten.
    fn draw_offscreen_indicators(&self, frame: &mut Frame) {
        let (area, row) = (self.area_height(), self.row_height());
        let count = self.workspaces.list().len();
        let above = (0..count).filter(|&ws| self.row_top(ws) + row <= 0).count();
        let below: Vec<usize> = (0..count).filter(|&ws| self.row_top(ws) >= area).collect();
        let style = Style {
            bg: STATUS_BG,
            ..Style::fg(STATUS_FG)
        };
        let mut note = |y: i32, text: String| {
            let x = i32::from(self.width) - text.chars().count() as i32 - 1;
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

    fn draw_status(&self, frame: &mut Frame) {
        let y = i32::from(self.height) - 1;
        let base = Style {
            bg: STATUS_BG,
            ..Style::fg(STATUS_FG)
        };
        frame.put_str(0, y, &" ".repeat(usize::from(self.width)), base);

        let mut x = 0;
        let mut put = |frame: &mut Frame, s: &str, style: Style| {
            frame.put_str(x, y, s, style);
            x += s.chars().count() as i32;
        };
        put(frame, " tiri ", Style { bold: true, ..base });

        // The workspaces, top to bottom, ending with "+" for the empty one.
        let active_ws = self.workspaces.active_index();
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
            put(frame, &format!(" {} ", self.workspace_label(ws)), style);
        }
        put(frame, " │ ", base);

        // A minimap of the active workspace's columns: the focused one
        // filled, the rest hollow, dimmed when scrolled out of view.
        let strip = self.workspaces.active();
        for idx in 0..strip.columns().len() {
            let label = if idx == strip.focus_index() {
                "■ "
            } else {
                "□ "
            };
            let style = if idx == strip.focus_index() {
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
            put(frame, label, style);
        }

        let hint = if self.prefix_pending {
            "C-a: n new  hjkl focus  HJKL move  u/i workspace  U/I move to ws  [/] consume/expel  ,/. in/out  r width  o overview  x close  q quit "
        } else if self.workspaces.in_overview() && self.kitty_overview {
            "OVERVIEW (kitty)  hjkl select  u/i workspace  HJKL/U/I move  x close  t text  ⏎/o/Esc open "
        } else if self.workspaces.in_overview() {
            "OVERVIEW  hjkl select  u/i workspace  HJKL/U/I move  x close  t thumbnails  ⏎/o/Esc open "
        } else {
            "C-a or Alt: n/⏎ new  h/l focus  u/i workspace  r width  o overview "
        };
        let hint_x = i32::from(self.width) - hint.chars().count() as i32;
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
        KeyCode::Char('c') => Action::Center,
        KeyCode::Char('o') => Action::ToggleOverview,
        KeyCode::Char('x') => Action::Close,
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

/// Copies a pane's screen into a `w` x `h` box at (x, y), clipping at the
/// frame's edges. A box smaller than the screen (in the overview) shows the
/// left edge of the rows around the cursor. Returns the first row shown.
fn draw_screen(frame: &mut Frame, pane: &Pane, x: i32, y: i32, w: i32, h: i32) -> u16 {
    let (rows, cols) = pane.size();
    let w = w.clamp(0, i32::from(cols)) as u16;
    let h = h.clamp(0, i32::from(rows)) as u16;
    let (cursor_row, _) = pane.cursor();
    let first_row = (cursor_row + 1).saturating_sub(h).min(rows - h);

    let mut sym = String::new();
    for row in first_row..first_row + h {
        let fy = y + i32::from(row - first_row);
        for col in 0..w {
            let fx = x + i32::from(col);
            if fx < 0 || fx >= i32::from(frame.width()) || fy >= i32::from(frame.height()) {
                continue;
            }
            let cell = pane.cell(row, col);
            let style = cell_style(pane, cell);
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
    first_row
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
