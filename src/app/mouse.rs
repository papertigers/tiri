// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Mouse input: focusing and selecting by clicking, dragging out
//! selections, the wheel, and passing events on to programs that want them.

use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::input::{encode_key, encode_mouse};
use crate::keys::Action;
use crate::layout::PaneId;
use crate::protocol::{ClientMsg, Command};
use crate::render::text_width;
use crate::selection::{self, Point, Selection};

use super::client_state::{Click, Drag};
use super::geometry::{MIN_BOX, Seam};
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
/// Clicks in a row select a word, then a line; a fourth starts over.
const DOUBLE_CLICK: u8 = 2;
const TRIPLE_CLICK: u8 = 3;

impl App {
    /// What's at (`x`, `y`) on `client`'s screen.
    fn hit(&self, client: &Client, x: i32, y: i32) -> Hit {
        if y == i32::from(client.height) - 1 {
            let mut left = 0;
            for segment in self.status_segments(client) {
                let right = left + text_width(&segment.text) as i32;
                if (left..right).contains(&x) {
                    return segment.target.map_or(Hit::Nothing, Hit::Status);
                }
                left = right;
            }
            return Hit::Nothing;
        }
        if let Some(b) =
            (self.visible_panes(client).into_iter()).find(|b| b.contains(x, y))
        {
            let (id, (cx, cy)) = (b.id, (x - b.x - 1, y - b.y - 1));
            let inside =
                (0..b.w - 2).contains(&cx) && (0..b.h - 2).contains(&cy);
            let inner = self.panes.get(&id).filter(|_| inside).map(|pane| {
                let top = content_top(pane, b.h - 2, client.scrolled(id, pane));
                let point = Point { line: top + cy, col: cx as u16 };
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
            .find(|b| b.id == id && b.w >= MIN_BOX && b.h >= MIN_BOX)?;
        // Inside the border, to its last cell.
        let (inner_w, inner_h) = (b.w - 2, b.h - 2);
        let cx = (x - b.x - 1).clamp(0, inner_w - 1);
        let raw_y = y - b.y - 1;
        let cy = raw_y.clamp(0, inner_h - 1);
        let top = content_top(pane, b.h - 2, client.scrolled(id, pane));
        let point = Point { line: top + cy, col: cx as u16 };
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
        if let Some(pane) = self.panes.get(&id)
            && let Some(bytes) = encode_mouse(
                kind,
                col,
                row,
                mods,
                pane.emulator().mouse_modes(),
            )
        {
            self.outbox.push(ClientMsg::Input { pane: Some(id), bytes });
        }
    }

    /// Handles a mouse event from `client`.
    pub fn mouse(&mut self, client: &mut Client, event: MouseEvent) {
        client.follow_selection(&self.panes);
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
                self.command(Command::Action(Action::FocusColumnLeft));
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollRight
                if shift =>
            {
                self.command(Command::Action(Action::FocusColumnRight));
            }
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {}
            MouseEventKind::ScrollUp if overview => {
                self.command(Command::Action(Action::FocusWorkspaceUp));
            }
            MouseEventKind::ScrollDown if overview => {
                self.command(Command::Action(Action::FocusWorkspaceDown));
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.wheel(client, event, x, y);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.press(client, event, x, y)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.drag(client, event, x, y)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.release(client, event, x, y)
            }
            // Other buttons, and movement, only matter to programs that
            // asked for them, in the focused pane.
            kind => {
                if let Hit::Pane { id, inner: Some((col, row, _)) } =
                    self.hit(client, x, y)
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
    fn wheel(
        &mut self,
        client: &mut Client,
        event: MouseEvent,
        x: i32,
        y: i32,
    ) {
        let Hit::Pane { id, inner } = self.hit(client, x, y) else {
            return;
        };
        let Some(pane) = self.panes.get(&id) else {
            return;
        };
        let up = event.kind == MouseEventKind::ScrollUp;
        if pane.emulator().mouse_modes().any() {
            if let Some((col, row, _)) = inner {
                self.forward_mouse(id, event.kind, col, row, event.modifiers);
            }
        } else if pane.emulator().alternate_scroll() {
            let arrow = KeyEvent::new(
                if up { KeyCode::Up } else { KeyCode::Down },
                KeyModifiers::NONE,
            );
            let arrow = encode_key(arrow, pane.emulator().key_modes());
            let bytes = arrow.repeat(WHEEL_LINES as usize);
            self.outbox.push(ClientMsg::Input { pane: Some(id), bytes });
        } else {
            client.scroll(
                id,
                pane,
                if up { WHEEL_LINES } else { -WHEEL_LINES },
            );
        }
    }

    fn press(
        &mut self,
        client: &mut Client,
        event: MouseEvent,
        x: i32,
        y: i32,
    ) {
        client.selection = None;
        client.drag = Drag::Ignored;
        if let Some(seam) = self.seam_at(client, x, y)
            && let Some(anchor) = self.seam_anchor(client, seam)
        {
            client.drag = Drag::Resizing { seam, anchor, to: None };
            return;
        }
        let overview = self.workspaces.in_overview(client.id);
        match self.hit(client, x, y) {
            Hit::Status(StatusTarget::Workspace(ws)) => {
                self.command(Command::FocusWorkspace(ws));
            }
            Hit::Status(StatusTarget::Column(idx)) => {
                self.command(Command::FocusColumn(idx));
            }
            Hit::EmptyWorkspace(ws) => {
                self.command(Command::FocusWorkspace(ws));
                self.set_overview(client, false);
            }
            Hit::Pane { id, inner } => {
                let focused = self.workspaces.focused(client.id) == Some(id);
                if !focused || overview {
                    self.command(Command::FocusPane(id));
                }
                if overview {
                    self.set_overview(client, false);
                    return;
                }
                // Counted before the focus check, so double-clicking a pane
                // that wasn't focused still selects a word. A fourth click
                // in a row starts over.
                let now = Instant::now();
                let clicks =
                    inner.map_or(1, |(_, _, point)| match client.last_click {
                        Some(last)
                            if last.pane == id
                                && last.point == point
                                && now - last.at < MULTI_CLICK =>
                        {
                            last.count % TRIPLE_CLICK + 1
                        }
                        _ => 1,
                    });
                client.last_click = inner.map(|(_, _, point)| Click {
                    at: now,
                    pane: id,
                    point,
                    count: clicks,
                });

                // A click that focuses a pane isn't passed to its program.
                let Some((col, row, point)) =
                    inner.filter(|_| focused || clicks > 1)
                else {
                    return;
                };
                let Some(pane) = self.panes.get(&id) else {
                    return;
                };
                if pane.emulator().mouse_modes().any() {
                    self.forward_mouse(
                        id,
                        event.kind,
                        col,
                        row,
                        event.modifiers,
                    );
                    client.drag = Drag::Forwarded(id);
                } else {
                    let (anchor, head) = match clicks {
                        DOUBLE_CLICK => {
                            selection::word_at(pane.emulator().term(), point)
                        }
                        TRIPLE_CLICK => {
                            selection::line_at(pane.emulator().term(), point)
                        }
                        _ => (point, point),
                    };
                    client.drag =
                        Drag::Selecting { pane: id, snapped: clicks > 1 };
                    client.selection = Some(Selection {
                        pane: id,
                        anchor,
                        head,
                        at: pane.emulator().scroll_mark(),
                    });
                }
            }
            Hit::Nothing => {}
        }
    }

    fn drag(&mut self, client: &mut Client, event: MouseEvent, x: i32, y: i32) {
        match client.drag {
            Drag::Forwarded(id) => {
                if let Some((col, row, _, _)) =
                    self.clamped_point(client, id, x, y)
                {
                    self.forward_mouse(
                        id,
                        event.kind,
                        col,
                        row,
                        event.modifiers,
                    );
                }
            }
            Drag::Selecting { pane: id, .. } => {
                let Some((_, _, mut point, past)) =
                    self.clamped_point(client, id, x, y)
                else {
                    return;
                };
                // Dragging above or below the pane scrolls it along, which
                // puts other text under the mouse.
                if past != 0
                    && let Some(pane) = self.panes.get(&id)
                {
                    client.scroll(id, pane, -past);
                    if let Some((_, _, scrolled_to, _)) =
                        self.clamped_point(client, id, x, y)
                    {
                        point = scrolled_to;
                    }
                }
                if let Some(selection) = client.selection.as_mut() {
                    selection.head = point;
                }
            }
            Drag::Resizing { seam, anchor, .. } => {
                // The mouse is on the box's border: the box ends there.
                let to = match seam {
                    Seam::Column { .. } => x,
                    Seam::Pane { .. } => y,
                } - anchor
                    + 1;
                client.drag = Drag::Resizing { seam, anchor, to: Some(to) };
                // Here at once, so the edge keeps up with the mouse, and on
                // the server, which tells everyone.
                self.resize_seam(seam, to);
                self.command(match seam {
                    Seam::Column { ws, column } => {
                        Command::ResizeColumn { ws, column, cells: to }
                    }
                    Seam::Pane { ws, column, row } => {
                        Command::ResizePane { ws, column, row, rows: to }
                    }
                });
            }
            Drag::None | Drag::Ignored => {}
        }
    }

    fn release(
        &mut self,
        client: &mut Client,
        event: MouseEvent,
        x: i32,
        y: i32,
    ) {
        match std::mem::take(&mut client.drag) {
            Drag::Forwarded(id) => {
                if let Some((col, row, _, _)) =
                    self.clamped_point(client, id, x, y)
                {
                    self.forward_mouse(
                        id,
                        event.kind,
                        col,
                        row,
                        event.modifiers,
                    );
                }
            }
            Drag::Selecting { pane: id, snapped } => {
                let Some(selection) =
                    client.selection.filter(|s| snapped || !s.is_empty())
                else {
                    // A plain click selects nothing.
                    client.selection = None;
                    return;
                };
                if let Some(pane) = self.panes.get(&id) {
                    let (start, end) = selection.bounds();
                    let text =
                        selection::text(pane.emulator().term(), start, end);
                    if !text.is_empty() {
                        client.copy(&text);
                    }
                }
            }
            // The view held still while the edge moved; now it can scroll
            // to keep the focused column in sight.
            Drag::Resizing {
                seam: Seam::Column { ws, .. } | Seam::Pane { ws, .. },
                ..
            } => self.command(Command::ShowFocus { ws }),
            Drag::None | Drag::Ignored => {}
        }
    }
}
