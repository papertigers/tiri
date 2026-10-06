// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Composing a client's frame: every visible pane, the workspace labels,
//! and the overview's indicators.

use std::time::Instant;

use crate::effects::Transition;
use crate::kitty;
use crate::render::{Frame, Style, fit_width, text_width};

use super::geometry::PaneBox;
use super::screen::{content_top, draw_screen};
use super::thumbnails::image_id;
use super::{App, Client};

impl App {
    /// Composes `client`'s view of the workspaces plus its status bar.
    /// Returns the frame and where the cursor should be shown, if anywhere.
    pub fn draw(&self, client: &mut Client) -> (Frame, Option<(u16, u16)>) {
        client.follow_selection(&self.panes);
        self.report_motion_if_wanted(client);
        if (client.notice.as_ref())
            .is_some_and(|notice| notice.until <= Instant::now())
        {
            client.notice = None;
        }
        let visible = self.visible_panes(client);

        let thumbnails = self.showing_thumbnails(client);
        // Thumbnails are fully opaque unless the overview is fading.
        let opacity =
            (client.transition.as_ref()).map_or(1.0, Transition::image_opacity);
        if thumbnails {
            let now = Instant::now();
            let uploads = (visible.iter())
                .filter_map(|b| {
                    let size = Client::thumbnail_size(b.w, b.h);
                    client.refresh_thumbnail(
                        &self.panes,
                        b.id,
                        size,
                        opacity,
                        now,
                    )
                })
                .collect();
            // Panes scrolled out of view would otherwise keep asking for
            // redraws they never get.
            client.retain_thumbnails(|id| visible.iter().any(|b| b.id == *id));
            client.upload(uploads);
        } else if client.transition.is_some() {
            // The overview fading out: its thumbnails fade with it.
            client.retain_thumbnails(|id| self.panes.contains_key(id));
            let uploads = client.fade_thumbnails(opacity);
            client.upload(uploads);
        } else {
            client.park_thumbnails(&self.panes);
        }

        // Effects stay off while thumbnails show: they'd change the colors
        // that tell the terminal which image a cell shows.
        if thumbnails {
            client.effects.clear();
        }

        let (mut frame, cursor) = self.compose(client, &visible, thumbnails);
        if thumbnails {
            kitty::compact_placeholders(&mut frame);
        }
        let areas: Vec<_> = (client.effects.panes().into_iter())
            .map(|id| (id, self.pane_area(client, id)))
            .collect();
        let palette = client.palette;
        let now = Instant::now();
        client.effects.apply(&mut frame, &palette, now, &areas);
        if let Some(transition) = client.transition.as_mut() {
            if transition.apply(&mut frame, &palette, now) {
                return (frame, None);
            }
            client.transition = None;
            // Thumbnails kept for the overview going out are parked now; no
            // more frames may come to do it later.
            if !thumbnails {
                client.park_thumbnails(&self.panes);
            }
        }
        (frame, cursor)
    }

    /// Composes the frame itself: every visible pane, labels and status bar.
    fn compose(
        &self,
        client: &Client,
        visible: &[PaneBox],
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
        for &PaneBox { id, ws, column: idx, x, y, w, h } in visible {
            let strip = self.workspaces.list()[ws].strip();
            let column = &strip.columns()[idx];
            let stacked = column.panes().len() > 1;
            let Some(pane) = self.panes.get(&id) else {
                continue;
            };
            // Its place in the stack, even when it's alone on screen
            // because it's fullscreen.
            let row = column.panes().iter().position(|&p| p == id).unwrap_or(0);
            let focused = ws == active_ws
                && idx == strip.focus_index()
                && row == column.focus_index();
            let border = if focused {
                Style {
                    bold: true,
                    ..Style::fg(self.config.theme.focused_border)
                }
            } else {
                Style::fg(self.config.theme.unfocused_border)
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
            let room = usize::try_from(w - 4).unwrap_or(0);
            frame.put_str(x + 2, y, fit_width(&title, room), border);

            let top = content_top(pane, h - 2, scrolled);
            match client.thumbnails.get(&id) {
                Some(thumb) if thumbnails => {
                    let id = kitty::id_color(image_id(id));
                    let style = Style {
                        fg: id,
                        underline_color: id,
                        ..Style::default()
                    };
                    let (cols, rows) = thumb.size;
                    for r in 0..rows {
                        for c in 0..cols {
                            let cell = kitty::placeholder(r, c);
                            frame.put(
                                x + 1 + i32::from(c),
                                y + 1 + i32::from(r),
                                &cell,
                                style,
                            );
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
                    self.config.theme.selection_bg,
                ),
            }

            if focused && show_cursor && scrolled == 0 && pane.cursor_visible()
            {
                let (r, c) = pane.cursor();
                let (cx, cy) =
                    (x + 1 + i32::from(c), y + 1 + i32::from(r) - top);
                if (0..i32::from(client.width)).contains(&cx)
                    && (0..client.area_height()).contains(&cy)
                    && cy < y + h - 1
                {
                    cursor = Some((cx as u16, cy as u16));
                }
            }
        }

        self.draw_status(client, &mut frame);
        (frame, cursor)
    }

    /// Has `client`'s terminal report every mouse movement only while the
    /// focused pane's program asks for it, since that's the only place
    /// they go. Otherwise each move would cross the network for nothing.
    fn report_motion_if_wanted(&self, client: &mut Client) {
        let wanted = !self.workspaces.in_overview(client.id)
            && (self.workspaces.focused(client.id))
                .and_then(|id| self.panes.get(&id))
                .is_some_and(|pane| pane.mouse_modes().motion);
        if wanted != client.all_motion {
            client.all_motion = wanted;
            let mode: &[u8] =
                if wanted { b"\x1b[?1003h" } else { b"\x1b[?1003l" };
            client.escapes.extend_from_slice(mode);
        }
    }

    /// Whether `ws` is the empty workspace that's always kept at the bottom.
    pub(super) fn is_new_workspace(&self, ws: usize) -> bool {
        let list = self.workspaces.list();
        ws + 1 == list.len() && list[ws].is_empty() && list[ws].name().is_none()
    }

    /// A workspace's name: its own, its position if it has none, or "+" for
    /// the empty one at the bottom.
    pub(super) fn workspace_label(&self, ws: usize) -> String {
        match self.workspaces.list()[ws].name() {
            Some(name) => name.to_owned(),
            None if self.is_new_workspace(ws) => "+".to_owned(),
            None => format!("{}", ws + 1),
        }
    }

    /// Zoomed out, labels each workspace row on the line above it. An empty
    /// workspace gets a hint, or in the overview a placeholder box, so
    /// there's something to see and select.
    fn draw_workspace_label(
        &self,
        client: &Client,
        frame: &mut Frame,
        ws: usize,
    ) {
        let top = self.row_top(client, ws);
        let row_height = self.row_height(client);
        let overview = self.workspaces.in_overview(client.id);
        let active = ws == self.workspaces.active_index(client.id);
        let style = if active {
            Style { bold: true, ..Style::fg(self.config.theme.focused_border) }
        } else {
            Style::fg(self.config.theme.dim)
        };
        if overview {
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
        let hint_width = text_width(hint) as i32;
        let middle = top + row_height / 2;
        if overview {
            // A box the size of a default column, where one would open.
            let zoom = self.workspaces.zoom(client.id);
            let w = ((f64::from(client.width) * 0.5 * zoom).round() as i32)
                .max(hint_width + 4);
            let x = (i32::from(client.width) - w) / 2;
            draw_box(frame, x, top, w, row_height, style);
            frame.put_str(x + (w - hint_width) / 2, middle, hint, style);
        } else {
            let x = (i32::from(client.width) - hint_width) / 2;
            frame.put_str(x, middle, hint, Style::fg(self.config.theme.dim));
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
        let below: Vec<usize> =
            (0..count).filter(|&ws| self.row_top(client, ws) >= area).collect();
        let style = Style {
            bg: self.config.theme.status_bg,
            ..Style::fg(self.config.theme.status_fg)
        };
        let mut note = |y: i32, text: String| {
            let x = i32::from(client.width) - text_width(&text) as i32 - 1;
            frame.put_str(x, y, &text, style);
        };
        if above > 0 {
            note(0, format!(" ▲ {above} above "));
        }
        if let Some(&last) = below.last() {
            let extra =
                if self.is_new_workspace(last) { " (incl. new)" } else { "" };
            note(area - 1, format!(" ▼ {} below{extra} ", below.len()));
        }
    }
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
