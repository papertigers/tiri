//! Application state: the strip, its panes, keybindings, and drawing.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color as TermColor, NamedColor};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::Event;
use crate::input::encode_key;
use crate::kitty;
use crate::layout::{PaneId, Strip, Visibility, split_heights};
use crate::pane::Pane;
use crate::render::{Color, Frame, Style};
use crate::thumbnail;

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
    strip: Strip,
    panes: HashMap<PaneId, Pane>,
    next_id: u32,
    events: Sender<Event>,
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
    pub fn new(width: u16, height: u16, events: Sender<Event>) -> Result<Self> {
        let mut app = Self {
            strip: Strip::new(width),
            panes: HashMap::new(),
            next_id: 0,
            events,
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

    pub fn is_empty(&self) -> bool {
        self.strip.is_empty()
    }

    pub fn strip_mut(&mut self) -> &mut Strip {
        &mut self.strip
    }

    fn pane_rows(&self) -> u16 {
        self.height.saturating_sub(STATUS_HEIGHT + 2).max(1)
    }

    fn open_column(&mut self) -> Result<()> {
        let id = PaneId(self.next_id);
        self.next_id += 1;
        // Width isn't known until it's in the strip, so start narrow and fix it below.
        let pane = Pane::spawn(id, self.pane_rows(), 1, self.events.clone())?;
        self.panes.insert(id, pane);
        self.strip.insert(id);
        self.resize_panes();
        Ok(())
    }

    /// Brings every pane's PTY size in line with its share of its column.
    fn resize_panes(&mut self) {
        let area = i32::from(self.height.saturating_sub(STATUS_HEIGHT));
        self.strip
            .set_max_stack((area / MIN_PANE_HEIGHT).max(1) as usize);
        for (idx, col) in self.strip.columns().iter().enumerate() {
            let cols = self.strip.column_width(idx).saturating_sub(2).max(1);
            let heights = split_heights(area, col.panes().len());
            for (id, h) in col.panes().iter().zip(heights) {
                if let Some(pane) = self.panes.get_mut(id) {
                    pane.resize((h - 2).max(1) as u16, cols);
                }
            }
        }
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.strip.set_view_width(width);
        self.resize_panes();
    }

    pub fn pane_output(&mut self, id: PaneId, bytes: &[u8]) {
        if let Some(pane) = self.panes.get_mut(&id) {
            pane.process(bytes);
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
            pane.reap();
        }
        if self.thumbnails.remove(&id).is_some() {
            kitty::delete(&mut self.graphics, THUMBNAIL_ID_BASE + id.0);
        }
        self.strip.remove(id);
        // Whatever shared its column grows into the space.
        self.resize_panes();
    }

    pub fn shutdown(&mut self) {
        for pane in self.panes.values_mut() {
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
        self.kitty_overview && self.strip.in_overview() && !self.strip.is_animating()
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

    /// Where column `idx` is drawn: x, y, width, height.
    fn column_box(&self, idx: usize) -> (i32, i32, i32, i32) {
        let pane_height = i32::from(self.height.saturating_sub(STATUS_HEIGHT));
        // Columns shrink vertically with the zoom, centered in the pane area.
        let h = ((f64::from(pane_height) * self.strip.zoom()).round() as i32)
            .clamp(pane_height.min(3), pane_height);
        let (x, w) = self.strip.column_span(idx);
        (x, (pane_height - h) / 2, w, h)
    }

    /// Where each pane in column `idx` is drawn, top to bottom, borders
    /// included: the column's box split among its panes.
    fn pane_boxes(&self, idx: usize) -> Vec<(PaneId, i32, i32, i32, i32)> {
        let (x, mut y, w, h) = self.column_box(idx);
        let panes = self.strip.columns()[idx].panes();
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
        let id = self.strip.focused()?;
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
        if self.strip.in_overview() {
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
            Action::FocusLeft => self.strip.focus_left(),
            Action::FocusRight => self.strip.focus_right(),
            Action::FocusFirst => self.strip.focus_first(),
            Action::FocusLast => self.strip.focus_last(),
            Action::MoveLeft => self.strip.move_left(),
            Action::MoveRight => self.strip.move_right(),
            Action::FocusUp => self.strip.focus_up(),
            Action::FocusDown => self.strip.focus_down(),
            Action::MoveUp => self.strip.move_up(),
            Action::MoveDown => self.strip.move_down(),
            Action::ConsumeOrExpelLeft => {
                self.strip.consume_or_expel_left();
                self.resize_panes();
            }
            Action::ConsumeOrExpelRight => {
                self.strip.consume_or_expel_right();
                self.resize_panes();
            }
            Action::ConsumeIntoColumn => {
                self.strip.consume_into_column();
                self.resize_panes();
            }
            Action::ExpelFromColumn => {
                self.strip.expel_from_column();
                self.resize_panes();
            }
            Action::CycleWidth => {
                self.strip.cycle_width();
                self.resize_panes();
            }
            Action::Center => self.strip.center_focused(),
            Action::Close => {
                if let Some(id) = self.strip.focused() {
                    if let Some(pane) = self.panes.get_mut(&id) {
                        pane.kill();
                    }
                    self.pane_exited(id);
                }
            }
            Action::ToggleOverview => self.strip.set_overview(!self.strip.in_overview()),
            Action::ExitOverview => self.strip.set_overview(false),
            Action::ToggleThumbnails => self.kitty_overview = !self.kitty_overview,
            Action::Quit => self.quit = true,
        }
        Ok(())
    }

    /// Composes the visible slice of the strip plus the status bar. Returns
    /// the frame and where the cursor should be shown, if anywhere.
    pub fn draw(&mut self) -> (Frame, Option<(u16, u16)>) {
        let visible: Vec<usize> = (0..self.strip.columns().len())
            .filter(|&idx| {
                let (x, _, w, _) = self.column_box(idx);
                x + w > 0 && x < i32::from(self.width)
            })
            .collect();

        let thumbnails = self.showing_thumbnails();
        if thumbnails {
            let now = Instant::now();
            for &idx in &visible {
                for (id, _, _, w, h) in self.pane_boxes(idx) {
                    self.refresh_thumbnail(id, Self::thumbnail_size(w, h), now);
                }
            }
        } else if !(self.kitty_overview && self.strip.in_overview()) && !self.thumbnails.is_empty()
        {
            self.clear_thumbnails();
        }

        let mut frame = Frame::new(self.width, self.height);
        let show_cursor = !self.strip.in_overview() && self.strip.zoom() == 1.0;
        let mut cursor = None;

        for idx in visible {
            let column = &self.strip.columns()[idx];
            let stacked = column.panes().len() > 1;
            for (row, (id, x, y, w, h)) in self.pane_boxes(idx).into_iter().enumerate() {
                let Some(pane) = self.panes.get(&id) else {
                    continue;
                };
                let focused = idx == self.strip.focus_index() && row == column.focus_index();
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

        // A minimap of the strip: which columns are on screen right now.
        for idx in 0..self.strip.columns().len() {
            let label = format!(" {} ", idx + 1);
            let style = if idx == self.strip.focus_index() {
                Style {
                    bg: FOCUSED_BORDER,
                    fg: Color::Idx(0),
                    bold: true,
                    ..base
                }
            } else {
                match self.strip.visibility(idx) {
                    Visibility::Full => Style { bold: true, ..base },
                    Visibility::Partial => base,
                    Visibility::Hidden => Style {
                        fg: Color::Idx(242),
                        ..base
                    },
                }
            };
            put(frame, &label, style);
        }

        let hint = if self.prefix_pending {
            "C-a: n new  hjkl focus  HJKL move  [/] consume/expel  ,/. in/out  r width  o overview  x close  q quit "
        } else if self.strip.in_overview() && self.kitty_overview {
            "OVERVIEW (kitty)  h/l select  H/L move  x close  t text  ⏎/o/Esc open "
        } else if self.strip.in_overview() {
            "OVERVIEW  h/l select  H/L move  x close  t thumbnails  ⏎/o/Esc open "
        } else {
            "C-a or Alt: n/⏎ new  h/l focus  r width  c center  o overview "
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
