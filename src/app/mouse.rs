//! Mouse input: focusing and selecting by clicking, dragging out
//! selections, the wheel, and passing events on to programs that want them.

use std::time::{Duration, Instant};

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::input::encode_mouse;
use crate::layout::PaneId;
use crate::selection::{self, Point, Selection};

use super::client_state::Drag;
use super::screen::content_top;
use super::status::StatusTarget;
use super::{App, Client};

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

impl App {
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
        if let Some(b) = (self.visible_panes(client).into_iter()).find(|b| b.contains(x, y)) {
            let (id, (cx, cy)) = (b.id, (x - b.x - 1, y - b.y - 1));
            let inside = (0..b.w - 2).contains(&cx) && (0..b.h - 2).contains(&cy);
            let inner = self.panes.get(&id).filter(|_| inside).map(|pane| {
                let top = content_top(pane, b.h - 2, client.scrolled(id, pane));
                let point = Point {
                    line: top + cy,
                    col: cx as u16,
                };
                (cx as u16, cy as u16, point)
            });
            return Hit::Pane { id, inner };
        }
        for ws in self.visible_workspaces(client) {
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
        let b = (self.visible_panes(client).into_iter())
            .find(|b| b.id == id && b.w >= 3 && b.h >= 3)?;
        let cx = (x - b.x - 1).clamp(0, b.w - 3);
        let raw_y = y - b.y - 1;
        let cy = raw_y.clamp(0, b.h - 3);
        let top = content_top(pane, b.h - 2, client.scrolled(id, pane));
        let point = Point {
            line: top + cy,
            col: cx as u16,
        };
        Some((cx as u16, cy as u16, point, (raw_y - cy).signum()))
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
}
