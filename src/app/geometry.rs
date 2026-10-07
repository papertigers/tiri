// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Where things are on a client's screen: workspace rows, columns, and
//! each pane's box.

use crate::layout::PaneId;

use super::{App, Client};

/// The smallest box with room inside: a border, a cell, then a border.
pub(super) const MIN_BOX: i32 = 3;

/// Where a pane is drawn on a client's screen, borders included.
#[derive(Debug, Clone, Copy)]
pub(super) struct PaneBox {
    pub(super) id: PaneId,
    /// The workspace and column it's in.
    pub(super) ws: usize,
    pub(super) column: usize,
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) w: i32,
    pub(super) h: i32,
}

/// A line between boxes that can be dragged to resize them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Seam {
    /// The right edge of column `column` of workspace `ws`: dragging it
    /// sizes that column.
    Column { ws: usize, column: usize },
    /// The line between pane `row` of a column and the pane below it:
    /// dragging it sizes pane `row`.
    Pane { ws: usize, column: usize, row: usize },
}

impl PaneBox {
    pub(super) fn contains(&self, x: i32, y: i32) -> bool {
        (self.x..self.x + self.w).contains(&x)
            && (self.y..self.y + self.h).contains(&y)
    }
}

impl App {
    /// Where pane `id` is on `client`'s screen, clipped to the pane area.
    pub(super) fn pane_area(
        &self,
        client: &Client,
        id: PaneId,
    ) -> Option<ratatui_core::layout::Rect> {
        let screen = (i32::from(client.width), client.area_height());
        let PaneBox { x, y, w, h, .. } =
            (self.visible_panes(client).into_iter()).find(|b| b.id == id)?;
        let (left, top) = (x.max(0), y.max(0));
        let (right, bottom) = ((x + w).min(screen.0), (y + h).min(screen.1));
        (right > left && bottom > top).then(|| {
            ratatui_core::layout::Rect::new(
                left as u16,
                top as u16,
                (right - left) as u16,
                (bottom - top) as u16,
            )
        })
    }

    /// The seam at (`x`, `y`) on `client`'s screen, if there is one: the
    /// borders on either side of where two columns, or two stacked panes,
    /// meet. None in the overview, which shrinks everything too much to drag.
    pub(super) fn seam_at(
        &self,
        client: &Client,
        x: i32,
        y: i32,
    ) -> Option<Seam> {
        if self.workspaces.in_overview(client.id) {
            return None;
        }
        for ws in self.visible_workspaces(client) {
            let strip = self.workspaces.list()[ws].strip();
            for column in self.visible_columns(client, ws) {
                let (cx, cy, cw, ch) = self.column_box(client, ws, column);
                if !(cy..cy + ch).contains(&y) {
                    continue;
                }
                if x == cx + cw - 1 {
                    return Some(Seam::Column { ws, column });
                }
                if x == cx && column > 0 {
                    return Some(Seam::Column { ws, column: column - 1 });
                }
                let col = &strip.columns()[column];
                if !(cx..cx + cw).contains(&x) || col.fullscreen().is_some() {
                    continue;
                }
                let boxes = self.pane_boxes(client, ws, column);
                for (row, pair) in boxes.windows(2).enumerate() {
                    let (above, below) = (pair[0], pair[1]);
                    if y == above.y + above.h - 1 || y == below.y {
                        return Some(Seam::Pane { ws, column, row });
                    }
                }
            }
        }
        None
    }

    pub(super) fn seam_anchor(
        &self,
        client: &Client,
        seam: Seam,
    ) -> Option<i32> {
        match seam {
            Seam::Column { ws, column } => {
                Some(self.column_box(client, ws, column).0)
            }
            Seam::Pane { ws, column, row } => {
                let col = &self.workspaces.list()[ws].strip().columns()[column];
                let pane = *col.panes().get(row)?;
                let boxes = self.pane_boxes(client, ws, column);
                boxes.iter().find(|b| b.id == pane).map(|b| b.y)
            }
        }
    }

    /// The height of a workspace row on `client`'s screen: its whole pane
    /// area, or less in the overview.
    pub(super) fn row_height(&self, client: &Client) -> i32 {
        let area = client.area_height();
        ((f64::from(area) * self.workspaces.zoom(client.id)).round() as i32)
            .clamp(area.min(MIN_BOX), area)
    }

    /// Where workspace `ws`'s row starts on `client`'s screen. Its active
    /// workspace is centered; the others stack above and below it, sliding
    /// as the active one changes. In the overview, a line between rows
    /// holds their labels.
    pub(super) fn row_top(&self, client: &Client, ws: usize) -> i32 {
        let (area, row) = (client.area_height(), self.row_height(client));
        let gap = i32::from(self.workspaces.in_overview(client.id));
        let pitch = f64::from(row + gap);
        let from_active = ws as f64 - self.workspaces.y(client.id);
        (area - row) / 2 + (from_active * pitch).round() as i32
    }

    /// The workspaces with any part of their row on `client`'s screen.
    pub(super) fn visible_workspaces(&self, client: &Client) -> Vec<usize> {
        let (area, row) = (client.area_height(), self.row_height(client));
        (0..self.workspaces.list().len())
            .filter(|&ws| {
                let top = self.row_top(client, ws);
                top + row > 0 && top < area
            })
            .collect()
    }

    /// Where column `idx` of workspace `ws` is drawn: x, y, width, height.
    pub(super) fn column_box(
        &self,
        client: &Client,
        ws: usize,
        idx: usize,
    ) -> (i32, i32, i32, i32) {
        let (x, w) = self.workspaces.column_span(client.id, ws, idx);
        (x, self.row_top(client, ws), w, self.row_height(client))
    }

    /// The columns of workspace `ws` at least partly on `client`'s screen.
    pub(super) fn visible_columns(
        &self,
        client: &Client,
        ws: usize,
    ) -> Vec<usize> {
        (0..self.workspaces.list()[ws].strip().columns().len())
            .filter(|&idx| {
                let (x, _, w, _) = self.column_box(client, ws, idx);
                x + w > 0 && x < i32::from(client.width)
            })
            .collect()
    }

    /// Where each pane in column `idx` of workspace `ws` is drawn, top to
    /// bottom: the column's box split among its panes.
    pub(super) fn pane_boxes(
        &self,
        client: &Client,
        ws: usize,
        idx: usize,
    ) -> Vec<PaneBox> {
        let (x, mut y, w, h) = self.column_box(client, ws, idx);
        let column = &self.workspaces.list()[ws].strip().columns()[idx];
        let pane_box = |id, y, h| PaneBox { id, ws, column: idx, x, y, w, h };
        // A fullscreen pane has its column to itself; the rest are hidden.
        if let Some(id) = column.fullscreen() {
            return vec![pane_box(id, y, h)];
        }
        let strip = self.workspaces.list()[ws].strip();
        (column.panes().iter())
            .zip(strip.pane_heights(idx, h))
            .map(|(&id, h)| {
                let b = pane_box(id, y, h);
                y += h;
                b
            })
            .collect()
    }

    /// Every pane at least partly on `client`'s screen, by workspace and
    /// column.
    pub(super) fn visible_panes(&self, client: &Client) -> Vec<PaneBox> {
        (self.visible_workspaces(client).into_iter())
            .flat_map(|ws| {
                (self.visible_columns(client, ws).into_iter())
                    .flat_map(move |idx| self.pane_boxes(client, ws, idx))
            })
            .collect()
    }
}
