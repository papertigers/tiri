// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The niri-style scrollable strip: an unbounded row of columns with a
//! viewport sliding over it. Opening a column never resizes the others; the
//! viewport scrolls instead. Each column is a stack of one or more panes
//! sharing its height.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const MIN_COLUMN_WIDTH: u16 = 8;
/// The shortest a stacked pane's box may get, borders included. Columns
/// refuse to consume more panes than fit at this height.
pub const MIN_PANE_HEIGHT: i32 = 5;

/// The size, in columns and rows, panes are laid out for before any
/// terminal attaches: a classic terminal's.
pub const DEFAULT_VIEW: (u16, u16) = (80, 24);
/// The rows the status bar takes, at the foot of each client's screen.
pub const STATUS_HEIGHT: u16 = 1;

/// Added before rounding a share of the view down to whole cells, so a
/// share written with a few decimals, like 0.33333, comes to a whole third.
const ROUNDING_SLACK: f64 = 1e-3;

/// How wide a column is, or how tall a pane in a stack.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PresetSize {
    /// A share of the view's width or height, from just above 0 to 1.
    Proportion(f64),
    /// So many cells across, or rows down, borders included.
    Fixed(u16),
}

/// The full view width, as maximizing makes a column.
const FULL_WIDTH: PresetSize = PresetSize::Proportion(1.0);

/// The sizes the config gives: what `switch-preset-column-width` and
/// `switch-preset-pane-height` step through, and the width new columns
/// start at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SizePresets {
    /// Never empty.
    pub column_widths: Vec<PresetSize>,
    pub default_column_width: PresetSize,
    /// Never empty.
    pub pane_heights: Vec<PresetSize>,
}

impl Default for SizePresets {
    /// As the default config has them: columns a third, a half, two thirds
    /// or the whole width, starting at a half; panes a third, a half or two
    /// thirds of the height.
    fn default() -> Self {
        use PresetSize::Proportion;
        Self {
            column_widths: vec![
                Proportion(0.33333),
                Proportion(0.5),
                Proportion(0.66667),
                Proportion(1.0),
            ],
            default_column_width: Proportion(0.5),
            pane_heights: vec![
                Proportion(0.33333),
                Proportion(0.5),
                Proportion(0.66667),
            ],
        }
    }
}

/// Time constant for the viewport easing; ~95% of the way there after 3x this.
const SCROLL_TAU: f64 = 0.05;

/// The overview zooms out to fit the whole strip, within these bounds.
pub const OVERVIEW_MAX_ZOOM: f64 = 0.5;
const OVERVIEW_MIN_ZOOM: f64 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaneId(pub u32);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Column {
    /// Top to bottom; never empty.
    panes: Vec<PaneId>,
    /// Index of the focused pane in `panes`.
    focus: usize,
    width: PresetSize,
    /// The width to go back to when un-maximizing.
    unmaximized: Option<PresetSize>,
    /// The heights of panes that have one. The rest share what's left.
    heights: HashMap<PaneId, PresetSize>,
    /// A pane shown fullscreen: the column takes the whole view width and
    /// this pane its whole height, hiding the rest of the stack.
    fullscreen: Option<PaneId>,
}

impl Column {
    fn new(pane: PaneId, width: PresetSize) -> Self {
        Self {
            panes: vec![pane],
            focus: 0,
            width,
            unmaximized: None,
            heights: HashMap::new(),
            fullscreen: None,
        }
    }

    pub fn fullscreen(&self) -> Option<PaneId> {
        self.fullscreen
    }

    pub fn panes(&self) -> &[PaneId] {
        &self.panes
    }

    pub fn focus_index(&self) -> usize {
        self.focus
    }

    pub fn focused(&self) -> PaneId {
        self.panes[self.focus]
    }

    /// Focuses the pane at `row`. Moving off a fullscreen pane would focus
    /// one it hides, so that ends fullscreen.
    fn set_focus(&mut self, row: usize) {
        if row != self.focus {
            self.fullscreen = None;
            self.focus = row;
        }
    }

    /// Takes out the pane at `idx`, keeping focus on the same pane if it
    /// stays, or on its neighbor if it was the one removed.
    fn take(&mut self, idx: usize) -> PaneId {
        // Changing the stack under a fullscreen pane ends it.
        self.fullscreen = None;
        let pane = self.panes.remove(idx);
        // Its height was for this stack: it goes into another one sharing.
        self.heights.remove(&pane);
        if idx < self.focus || self.focus >= self.panes.len() {
            self.focus = self.focus.saturating_sub(1);
        }
        pane
    }
}

/// `parts` scaled to add up to `total`, rounding where the boundaries
/// between them fall rather than each part, so nothing is lost or gained.
fn scale(parts: &[i32], total: i32) -> Vec<i32> {
    let sum: i64 = parts.iter().map(|&p| i64::from(p)).sum();
    if sum <= 0 {
        return split_heights(total, parts.len());
    }
    let (mut done, mut before) = (0i64, 0i32);
    (parts.iter())
        .map(|&part| {
            done += i64::from(part);
            let boundary = (done * i64::from(total) + sum / 2) / sum;
            let boundary = i32::try_from(boundary).unwrap_or(total);
            let height = boundary - before;
            before = boundary;
            height
        })
        .collect()
}

/// Splits `total` rows among `n` stacked panes as evenly as possible, giving
/// any leftover rows to the topmost panes.
fn split_heights(total: i32, n: usize) -> Vec<i32> {
    let n = n.max(1) as i32;
    (0..n).map(|i| total / n + i32::from(i < total % n)).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Full,
    Partial,
    Hidden,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Strip {
    columns: Vec<Column>,
    focus: usize,
    view_width: u16,
    /// Where the viewport is scrolled to, in cells from the strip's left
    /// edge. Shared by everyone viewing the strip; each viewer's
    /// [`StripView`] eases toward it.
    target_offset: i32,
    /// The most panes a column may hold before consuming is refused.
    max_stack: usize,
    /// The rows a column gets: the view's height less the status bar.
    view_height: u16,
    presets: SizePresets,
}

impl Strip {
    pub fn new(view_width: u16) -> Self {
        Self {
            columns: Vec::new(),
            focus: 0,
            view_width,
            target_offset: 0,
            max_stack: usize::MAX,
            view_height: DEFAULT_VIEW.1,
            presets: SizePresets::default(),
        }
    }

    /// Sets the widths to cycle through and to open columns at. Columns
    /// already open keep their widths.
    pub fn set_size_presets(&mut self, presets: &SizePresets) {
        self.presets.clone_from(presets);
    }

    /// The view's width, which columns' shares are of.
    pub fn view_width(&self) -> u16 {
        self.view_width
    }

    /// The view's height, which panes' shares are of.
    pub fn view_height(&self) -> u16 {
        self.view_height
    }

    pub fn set_view_height(&mut self, rows: u16) {
        self.view_height = rows;
    }

    /// The overview zoom that would fit this whole strip on screen.
    pub fn fit_zoom(&self) -> f64 {
        let total = f64::from(self.total_width());
        (f64::from(self.view_width) / total.max(1.0))
            .clamp(OVERVIEW_MIN_ZOOM, OVERVIEW_MAX_ZOOM)
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        self.locate(pane).is_some()
    }

    /// Takes the focused column out of the strip, for moving it elsewhere.
    pub fn take_focused_column(&mut self) -> Option<Column> {
        (!self.columns.is_empty()).then(|| self.remove_column(self.focus))
    }

    /// Adds a column taken from another strip to the right of the focused
    /// one, and focuses it.
    pub fn insert_column(&mut self, column: Column) {
        let idx = if self.columns.is_empty() { 0 } else { self.focus + 1 };
        self.columns.insert(idx, column);
        self.focus = idx;
        self.scroll_to_focus();
    }

    pub fn set_max_stack(&mut self, max: usize) {
        self.max_stack = max.max(1);
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    pub fn focus_index(&self) -> usize {
        self.focus
    }

    pub fn focused(&self) -> Option<PaneId> {
        self.columns.get(self.focus).map(Column::focused)
    }

    pub fn column_width(&self, idx: usize) -> u16 {
        let column = &self.columns[idx];
        if column.fullscreen.is_some() {
            self.cells(FULL_WIDTH)
        } else {
            self.cells(column.width)
        }
    }

    /// `width` in cells, in this strip's view.
    fn cells(&self, width: PresetSize) -> u16 {
        let cells = match width {
            // A share written with a few decimals, like 0.33333, should
            // still come to a whole third of 99, not just under it.
            PresetSize::Proportion(share) => {
                (f64::from(self.view_width) * share + ROUNDING_SLACK).floor()
                    as u16
            }
            PresetSize::Fixed(cells) => cells.min(self.view_width),
        };
        cells.max(MIN_COLUMN_WIDTH)
    }

    /// `height` in rows, in this strip's view.
    fn rows(&self, height: PresetSize) -> i32 {
        let view = i32::from(self.view_height);
        let rows = match height {
            PresetSize::Proportion(share) => {
                (f64::from(view) * share + ROUNDING_SLACK).floor() as i32
            }
            PresetSize::Fixed(rows) => i32::from(rows),
        };
        rows.clamp(MIN_PANE_HEIGHT.min(view), view.max(1))
    }

    /// How tall each pane in column `idx` is, top to bottom, when the
    /// column is `total` rows tall: the view's height, or less in the
    /// overview, which scales everything down alike.
    ///
    /// Panes with a set height get it, and the rest share what's left
    /// equally, none shorter than [`MIN_PANE_HEIGHT`]. Set heights give way
    /// as needed for that, and if every pane has one, they're stretched or
    /// squeezed to fill the column.
    pub fn pane_heights(&self, idx: usize, total: i32) -> Vec<i32> {
        let column = &self.columns[idx];
        let view = i32::from(self.view_height).max(1);
        let set: Vec<Option<i32>> = (column.panes.iter())
            .map(|pane| column.heights.get(pane).map(|h| self.rows(*h)))
            .collect();
        let sharing = set.iter().filter(|h| h.is_none()).count();
        let set_rows: Vec<i32> = set.iter().flatten().copied().collect();
        let wanted: i32 = set_rows.iter().sum();
        // The rows the set heights end up with between them.
        let room = view - i32::try_from(sharing).unwrap_or(0) * MIN_PANE_HEIGHT;
        let given = if sharing == 0 { view } else { wanted.min(room.max(0)) };
        let mut set_rows = scale(&set_rows, given).into_iter();
        let mut shared = split_heights(view - given, sharing).into_iter();
        let heights: Vec<i32> = (set.iter())
            .map(|h| match h {
                Some(_) => set_rows.next().unwrap_or(0),
                None => shared.next().unwrap_or(0),
            })
            .collect();
        scale(&heights, total)
    }

    /// niri's `switch-preset-window-height`: the focused pane in a stack
    /// takes the next preset height. One without a set height goes to the
    /// next preset taller than it is now.
    pub fn switch_preset_height(&mut self) {
        let Some(col) = self.columns.get(self.focus) else {
            return;
        };
        if col.panes.len() < 2 {
            // A pane alone has the column's whole height.
            return;
        }
        let pane = col.focused();
        let presets = &self.presets.pane_heights;
        let current = col.heights.get(&pane).copied();
        let position =
            current.and_then(|h| presets.iter().position(|p| *p == h));
        let next = match position {
            Some(i) => presets[(i + 1) % presets.len()],
            None => {
                let view = i32::from(self.view_height);
                let now = self.pane_heights(self.focus, view)[col.focus];
                let taller = presets.iter().find(|p| self.rows(**p) > now);
                *taller.unwrap_or(&presets[0])
            }
        };
        self.columns[self.focus].heights.insert(pane, next);
    }

    /// Makes column `idx` `cells` wide, as dragging its right edge does.
    /// The view doesn't scroll to follow, so the edge stays under the
    /// mouse; [`Self::show_focus`] catches up once the drag ends.
    pub fn resize_column(&mut self, idx: usize, cells: i32) {
        let view = i32::from(self.view_width).max(1);
        let cells = cells.clamp(i32::from(MIN_COLUMN_WIDTH).min(view), view);
        if let Some(col) = self.columns.get_mut(idx) {
            col.width =
                PresetSize::Proportion(f64::from(cells) / f64::from(view));
            col.unmaximized = None;
        }
    }

    /// Makes pane `row` of column `idx` `rows` tall, as dragging the line
    /// below it does. The panes above keep the heights they have, so the
    /// line stays under the mouse, and the pane just below gives or takes
    /// the difference.
    pub fn resize_pane(&mut self, idx: usize, row: usize, rows: i32) {
        let view = i32::from(self.view_height).max(1);
        let Some(col) = self.columns.get(idx) else {
            return;
        };
        if row + 1 >= col.panes.len() {
            return;
        }
        let now = self.pane_heights(idx, view);
        // What's left once the panes above have theirs and those below
        // their minimum: any more, and squeezing to fit would move the
        // panes above.
        let above: i32 = now[..row].iter().sum();
        let below = i32::try_from(col.panes.len() - row - 1).unwrap_or(0);
        let most = view - above - below * MIN_PANE_HEIGHT;
        let rows = rows.clamp(MIN_PANE_HEIGHT, most.max(MIN_PANE_HEIGHT));
        let share = |rows: i32| {
            PresetSize::Proportion(f64::from(rows) / f64::from(view))
        };
        let col = &mut self.columns[idx];
        for (pane, &height) in col.panes[..row].iter().zip(&now) {
            col.heights.entry(*pane).or_insert(share(height));
        }
        col.heights.insert(col.panes[row], share(rows));
        let below = col.panes[row + 1];
        col.heights.remove(&below);
    }

    /// Scrolls to bring the focused column into view, as after a resize.
    pub fn show_focus(&mut self) {
        self.scroll_to_focus();
    }

    /// niri's `reset-window-height`: the focused pane goes back to sharing
    /// its column's height equally.
    pub fn reset_pane_height(&mut self) {
        if let Some(col) = self.columns.get_mut(self.focus) {
            let pane = col.focused();
            col.heights.remove(&pane);
        }
    }

    /// Left edge of column `idx` in strip coordinates.
    pub fn column_x(&self, idx: usize) -> i32 {
        (0..idx).map(|i| i32::from(self.column_width(i))).sum()
    }

    fn total_width(&self) -> i32 {
        self.column_x(self.columns.len())
    }

    pub fn visibility(&self, idx: usize) -> Visibility {
        let left = self.column_x(idx) - self.target_offset;
        let right = left + i32::from(self.column_width(idx));
        let view = i32::from(self.view_width);
        if left >= 0 && right <= view {
            Visibility::Full
        } else if right <= 0 || left >= view {
            Visibility::Hidden
        } else {
            Visibility::Partial
        }
    }

    /// Opens a new column to the right of the focused one and focuses it.
    pub fn insert(&mut self, pane: PaneId) {
        self.insert_column(Column::new(
            pane,
            self.presets.default_column_width,
        ));
    }

    /// Removes `pane`, and its column if it was the last pane there.
    /// Returns false if it wasn't here.
    pub fn remove(&mut self, pane: PaneId) -> bool {
        let Some((col, row)) = self.locate(pane) else {
            return false;
        };
        if self.columns[col].panes.len() > 1 {
            self.columns[col].take(row);
        } else {
            self.remove_column(col);
        }
        true
    }

    fn locate(&self, pane: PaneId) -> Option<(usize, usize)> {
        self.columns.iter().enumerate().find_map(|(c, col)| {
            let row = col.panes.iter().position(|&p| p == pane)?;
            Some((c, row))
        })
    }

    fn remove_column(&mut self, idx: usize) -> Column {
        let column = self.columns.remove(idx);
        if idx < self.focus || self.focus >= self.columns.len() {
            self.focus = self.focus.saturating_sub(1);
        }
        // Don't leave empty space on the right where columns used to be.
        let max_offset =
            (self.total_width() - i32::from(self.view_width)).max(0);
        self.target_offset = self.target_offset.min(max_offset);
        self.scroll_to_focus();
        column
    }

    pub fn focus_up(&mut self) {
        if let Some(col) = self.columns.get_mut(self.focus) {
            col.set_focus(col.focus.saturating_sub(1));
        }
    }

    pub fn focus_down(&mut self) {
        if let Some(col) = self.columns.get_mut(self.focus) {
            col.set_focus((col.focus + 1).min(col.panes.len() - 1));
        }
    }

    pub fn move_up(&mut self) {
        if let Some(col) = self.columns.get_mut(self.focus)
            && col.focus > 0
        {
            col.panes.swap(col.focus, col.focus - 1);
            col.focus -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if let Some(col) = self.columns.get_mut(self.focus)
            && col.focus + 1 < col.panes.len()
        {
            col.panes.swap(col.focus, col.focus + 1);
            col.focus += 1;
        }
    }

    /// niri's `consume-or-expel-window-left`: a pane sharing its column is
    /// expelled into a new column on the left; a pane alone in its column
    /// joins the bottom of the column on its left.
    pub fn consume_or_expel_left(&mut self) {
        self.consume_or_expel(Side::Left);
    }

    /// Like [`Self::consume_or_expel_left`], to the right.
    pub fn consume_or_expel_right(&mut self) {
        self.consume_or_expel(Side::Right);
    }

    fn consume_or_expel(&mut self, side: Side) {
        let Some(col) = self.columns.get(self.focus) else {
            return;
        };
        if col.panes.len() > 1 {
            self.expel(side);
            return;
        }
        let neighbor = match side {
            Side::Left => self.focus.checked_sub(1),
            Side::Right => {
                Some(self.focus + 1).filter(|&i| i < self.columns.len())
            }
        };
        let Some(neighbor) = neighbor.filter(|&i| self.has_room(i)) else {
            return;
        };
        let pane = self.columns[self.focus].focused();
        let target = &mut self.columns[neighbor];
        target.fullscreen = None;
        target.panes.push(pane);
        target.focus = target.panes.len() - 1;
        // Removing our column shifts the neighbor left if it was to the right.
        let neighbor =
            if neighbor > self.focus { neighbor - 1 } else { neighbor };
        let current = self.focus;
        self.remove_column(current);
        self.focus = neighbor;
        self.scroll_to_focus();
    }

    /// niri's `consume-window-into-column`: the top pane of the column to the
    /// right joins the bottom of the focused column. Focus stays put.
    pub fn consume_into_column(&mut self) {
        let right = self.focus + 1;
        if right >= self.columns.len() || !self.has_room(self.focus) {
            return;
        }
        let pane = self.columns[right].panes[0];
        if self.columns[right].panes.len() > 1 {
            self.columns[right].take(0);
        } else {
            self.remove_column(right);
        }
        let column = &mut self.columns[self.focus];
        column.fullscreen = None;
        column.panes.push(pane);
        self.scroll_to_focus();
    }

    /// niri's `expel-window-from-column`: the focused pane leaves its column
    /// for a new column on the right, if it shares its column.
    pub fn expel_from_column(&mut self) {
        if self.columns.get(self.focus).is_some_and(|c| c.panes.len() > 1) {
            self.expel(Side::Right);
        }
    }

    /// Moves the focused pane out of its (shared) column into a new column on
    /// `side`, and focuses it there.
    fn expel(&mut self, side: Side) {
        let col = &mut self.columns[self.focus];
        let pane = col.take(col.focus);
        let idx = match side {
            Side::Left => self.focus,
            Side::Right => self.focus + 1,
        };
        let column = Column::new(pane, self.presets.default_column_width);
        self.columns.insert(idx, column);
        self.focus = idx;
        self.scroll_to_focus();
    }

    fn has_room(&self, idx: usize) -> bool {
        self.columns[idx].panes.len() < self.max_stack
    }

    pub fn focus_left(&mut self) {
        self.set_focus(self.focus.saturating_sub(1));
    }

    pub fn focus_right(&mut self) {
        self.set_focus(self.focus + 1);
    }

    pub fn focus_first(&mut self) {
        self.set_focus(0);
    }

    pub fn focus_last(&mut self) {
        self.set_focus(self.columns.len().saturating_sub(1));
    }

    pub fn focus_column(&mut self, idx: usize) {
        self.set_focus(idx);
    }

    /// Focuses `pane`, wherever it is in the strip. Returns false if it
    /// isn't here.
    pub fn focus_pane(&mut self, pane: PaneId) -> bool {
        let Some((col, row)) = self.locate(pane) else {
            return false;
        };
        self.columns[col].set_focus(row);
        self.set_focus(col);
        true
    }

    fn set_focus(&mut self, idx: usize) {
        if idx < self.columns.len() {
            self.focus = idx;
            self.scroll_to_focus();
        }
    }

    pub fn move_left(&mut self) {
        if self.focus > 0 {
            self.columns.swap(self.focus, self.focus - 1);
            self.focus -= 1;
            self.scroll_to_focus();
        }
    }

    pub fn move_right(&mut self) {
        if self.focus + 1 < self.columns.len() {
            self.columns.swap(self.focus, self.focus + 1);
            self.focus += 1;
            self.scroll_to_focus();
        }
    }

    /// niri's `switch-preset-column-width`: the focused column takes the
    /// next preset width. A width that isn't one of them (the presets
    /// changed since, say) goes to the next wider preset.
    pub fn cycle_width(&mut self) {
        let Some(col) = self.columns.get(self.focus) else {
            return;
        };
        let presets = &self.presets.column_widths;
        let next = match presets.iter().position(|w| *w == col.width) {
            Some(i) => presets[(i + 1) % presets.len()],
            None => {
                let now = self.cells(col.width);
                let wider = presets.iter().find(|w| self.cells(**w) > now);
                *wider.unwrap_or(&presets[0])
            }
        };
        let col = &mut self.columns[self.focus];
        col.width = next;
        col.unmaximized = None;
        self.scroll_to_focus();
    }

    /// niri's `maximize-column`: toggles the focused column between the
    /// full view width and the width it had before.
    pub fn toggle_maximized(&mut self) {
        if let Some(col) = self.columns.get_mut(self.focus) {
            match col.unmaximized.take() {
                Some(width) => col.width = width,
                None if col.width != FULL_WIDTH => {
                    col.unmaximized = Some(col.width);
                    col.width = FULL_WIDTH;
                }
                None => {}
            }
            self.scroll_to_focus();
        }
    }

    /// niri's `fullscreen-window`: toggles the focused pane filling the
    /// whole view, its column at full width and the pane alone in it.
    pub fn toggle_fullscreen(&mut self) {
        if let Some(col) = self.columns.get_mut(self.focus) {
            let pane = col.focused();
            col.fullscreen =
                if col.fullscreen == Some(pane) { None } else { Some(pane) };
            self.scroll_to_focus();
        }
    }

    pub fn center_focused(&mut self) {
        if self.columns.is_empty() {
            return;
        }
        let x = self.column_x(self.focus);
        let w = i32::from(self.column_width(self.focus));
        self.target_offset = x + w / 2 - i32::from(self.view_width) / 2;
    }

    pub fn set_view_width(&mut self, width: u16) {
        self.view_width = width;
        self.scroll_to_focus();
    }

    /// Scrolls the minimum distance needed to bring the focused column fully
    /// into view, like niri's default `center-focused-column "never"`.
    fn scroll_to_focus(&mut self) {
        if self.columns.is_empty() {
            self.target_offset = 0;
            return;
        }
        let x = self.column_x(self.focus);
        let w = i32::from(self.column_width(self.focus));
        let view = i32::from(self.view_width);
        if w >= view || x < self.target_offset {
            self.target_offset = x;
        } else if x + w > self.target_offset + view {
            self.target_offset = x + w - view;
        }
    }

    /// Where a viewer's viewport is heading: its left edge in strip
    /// coordinates. `overview_zoom` is the zoom if that viewer has the
    /// overview open.
    pub fn view_target(&self, overview_zoom: Option<f64>) -> f64 {
        let Some(zoom) = overview_zoom else {
            return f64::from(self.target_offset);
        };
        let total = f64::from(self.total_width());
        let view = f64::from(self.view_width);
        // How much of the strip fits on screen at this zoom.
        let visible = view / zoom;
        if total <= visible || self.columns.is_empty() {
            (total - visible) / 2.0
        } else {
            let x = f64::from(self.column_x(self.focus));
            let center = x + f64::from(self.column_width(self.focus)) / 2.0;
            (center - visible / 2.0).clamp(0.0, total - visible)
        }
    }
}

/// One viewer's animated view of a strip: how far it's scrolled right now,
/// easing toward [`Strip::view_target`]. Every client has its own, so one
/// scrolling doesn't scroll the others.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StripView {
    offset: f64,
}

impl StripView {
    /// A view already at its target, without animating there.
    pub fn settled(strip: &Strip, overview_zoom: Option<f64>) -> Self {
        Self { offset: strip.view_target(overview_zoom) }
    }

    pub fn is_animating(
        self,
        strip: &Strip,
        overview_zoom: Option<f64>,
    ) -> bool {
        strip.view_target(overview_zoom) != self.offset
    }

    /// Advances the scroll animation. Returns true while still moving.
    pub fn tick(
        &mut self,
        strip: &Strip,
        overview_zoom: Option<f64>,
        dt: Duration,
    ) -> bool {
        let offset = strip.view_target(overview_zoom);
        if (offset - self.offset).abs() < 0.5 {
            self.offset = offset;
            return false;
        }
        self.offset += (offset - self.offset)
            * (1.0 - (-dt.as_secs_f64() / SCROLL_TAU).exp());
        true
    }

    /// Column `idx`'s left edge and width on screen, at this view's scroll
    /// position and `zoom`.
    pub fn column_span(
        self,
        strip: &Strip,
        idx: usize,
        zoom: f64,
    ) -> (i32, i32) {
        let to_screen =
            |x: i32| ((f64::from(x) - self.offset) * zoom).round() as i32;
        let x = strip.column_x(idx);
        let left = to_screen(x);
        (left, to_screen(x + i32::from(strip.column_width(idx))) - left)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_with(n: u32, view_width: u16) -> Strip {
        let mut strip = Strip::new(view_width);
        for i in 0..n {
            strip.insert(PaneId(i));
        }
        strip
    }

    #[test]
    fn opening_columns_never_resizes_existing_ones() {
        let strip = strip_with(5, 100);
        for i in 0..5 {
            assert_eq!(strip.column_width(i), 50);
            assert_eq!(strip.column_x(i), 50 * i as i32);
        }
    }

    #[test]
    fn new_column_scrolls_into_view() {
        let strip = strip_with(3, 100);
        assert_eq!(strip.focus_index(), 2);
        assert_eq!(strip.target_offset, 50);
        assert_eq!(strip.visibility(0), Visibility::Hidden);
        assert_eq!(strip.visibility(1), Visibility::Full);
        assert_eq!(strip.visibility(2), Visibility::Full);
    }

    #[test]
    fn focus_scrolls_minimally() {
        let mut strip = strip_with(4, 100);
        assert_eq!(strip.target_offset, 100);
        // Column 2 is already visible: no scroll.
        strip.focus_left();
        assert_eq!(strip.target_offset, 100);
        // Column 1 is off the left edge: align it to the left.
        strip.focus_left();
        assert_eq!(strip.target_offset, 50);
        strip.focus_last();
        assert_eq!(strip.target_offset, 100);
    }

    /// The strip's panes, column by column, top to bottom.
    fn layout(strip: &Strip) -> Vec<Vec<u32>> {
        strip
            .columns()
            .iter()
            .map(|c| c.panes().iter().map(|p| p.0).collect())
            .collect()
    }

    #[test]
    fn insert_goes_right_of_focus() {
        let mut strip = strip_with(3, 100);
        strip.focus_first();
        strip.insert(PaneId(9));
        let order: Vec<_> =
            strip.columns().iter().map(|c| c.focused().0).collect();
        assert_eq!(order, [0, 9, 1, 2]);
        assert_eq!(strip.focused(), Some(PaneId(9)));
    }

    #[test]
    fn cycle_width_keeps_focus_visible() {
        let mut strip = strip_with(2, 100);
        strip.cycle_width(); // 1/2 -> 2/3
        assert_eq!(strip.column_width(1), 66);
        assert_eq!(strip.target_offset, 50 + 66 - 100);
        strip.cycle_width(); // -> full
        assert_eq!(strip.target_offset, 50);
    }

    #[test]
    fn center_can_overscroll_the_left_edge() {
        let mut strip = strip_with(1, 100);
        strip.cycle_width(); // 2/3 -> 66 wide
        strip.cycle_width(); // full
        strip.cycle_width(); // 1/3 -> 33 wide
        strip.center_focused();
        assert_eq!(strip.target_offset, 16 - 50);
    }

    #[test]
    fn remove_refocuses_and_reclaims_space() {
        let mut strip = strip_with(4, 100);
        assert!(strip.remove(PaneId(3)));
        assert_eq!(strip.focused(), Some(PaneId(2)));
        assert_eq!(strip.target_offset, 50);

        strip.focus_first();
        assert!(strip.remove(PaneId(0)));
        assert_eq!(strip.focused(), Some(PaneId(1)));
        assert_eq!(strip.target_offset, 0);
        assert!(!strip.remove(PaneId(0)));
    }

    /// Runs `view`'s animation to the end.
    fn settle(view: &mut StripView, strip: &Strip, overview_zoom: Option<f64>) {
        let mut frames = 0;
        while view.tick(strip, overview_zoom, Duration::from_millis(16)) {
            frames += 1;
            assert!(frames < 100, "animation never settled");
        }
    }

    /// A view that starts where an empty strip would, then catches up.
    fn view_of(strip: &Strip, overview_zoom: Option<f64>) -> StripView {
        let mut view = StripView::settled(&Strip::new(strip.view_width), None);
        settle(&mut view, strip, overview_zoom);
        view
    }

    #[test]
    fn tick_converges_on_target() {
        let strip = strip_with(3, 100);
        let mut view = StripView::settled(&Strip::new(100), None);
        assert!(view.tick(&strip, None, Duration::from_millis(16)));
        settle(&mut view, &strip, None);
        assert_eq!(view.column_span(&strip, 1, 1.0), (0, 50));
    }

    #[test]
    fn overview_fits_and_centers_a_short_strip() {
        let strip = strip_with(3, 100);
        let zoom = strip.fit_zoom();
        let view = view_of(&strip, Some(zoom));
        // 150 cells of strip at half zoom is 75 wide, centered in 100.
        assert_eq!(zoom, 0.5);
        assert_eq!(view.column_span(&strip, 0, zoom), (13, 25));
        assert_eq!(view.column_span(&strip, 2, zoom), (63, 25));
    }

    #[test]
    fn overview_zooms_further_to_fit_more_columns() {
        let strip = strip_with(6, 100);
        let zoom = strip.fit_zoom();
        let view = view_of(&strip, Some(zoom));
        assert!((zoom - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(view.column_span(&strip, 0, zoom).0, 0);
        assert_eq!(view.column_span(&strip, 5, zoom), (83, 17));
    }

    #[test]
    fn overview_scrolls_to_selection_when_strip_is_too_long() {
        let mut strip = strip_with(12, 100);
        let zoom = strip.fit_zoom();
        let mut view = view_of(&strip, Some(zoom));
        assert_eq!(zoom, OVERVIEW_MIN_ZOOM);
        // Selection is the last column, so the strip's right end is on screen.
        assert_eq!(view.column_span(&strip, 11, zoom), (88, 12));
        strip.focus_first();
        settle(&mut view, &strip, Some(zoom));
        assert_eq!(view.column_span(&strip, 0, zoom), (0, 13));
    }

    #[test]
    fn leaving_overview_lands_on_the_selection() {
        let mut strip = strip_with(4, 100);
        let zoom = strip.fit_zoom();
        let mut view = view_of(&strip, Some(zoom));
        strip.focus_first();
        settle(&mut view, &strip, Some(zoom));
        settle(&mut view, &strip, None);
        assert_eq!(view.column_span(&strip, 0, 1.0), (0, 50));
    }

    #[test]
    fn consume_or_expel_moves_a_lone_pane_into_its_neighbor() {
        let mut strip = strip_with(3, 100);
        strip.consume_or_expel_left();
        assert_eq!(layout(&strip), [vec![0], vec![1, 2]]);
        assert_eq!(
            (strip.focus_index(), strip.focused()),
            (1, Some(PaneId(2)))
        );

        strip.focus_first();
        strip.consume_or_expel_right();
        assert_eq!(layout(&strip), [vec![1, 2, 0]]);
        assert_eq!(strip.focused(), Some(PaneId(0)));
    }

    #[test]
    fn consume_or_expel_moves_a_stacked_pane_out() {
        let mut strip = strip_with(2, 100);
        strip.consume_or_expel_left();
        assert_eq!(layout(&strip), [vec![0, 1]]);

        // Pane 1 is focused and shares its column, so it's expelled.
        strip.consume_or_expel_left();
        assert_eq!(layout(&strip), [vec![1], vec![0]]);
        assert_eq!(
            (strip.focus_index(), strip.focused()),
            (0, Some(PaneId(1)))
        );

        strip.consume_or_expel_right();
        assert_eq!(layout(&strip), [vec![0, 1]]);
        strip.focus_up();
        strip.consume_or_expel_right();
        assert_eq!(layout(&strip), [vec![1], vec![0]]);
        assert_eq!(strip.focused(), Some(PaneId(0)));
    }

    #[test]
    fn consume_or_expel_at_the_edge_does_nothing() {
        let mut strip = strip_with(2, 100);
        strip.consume_or_expel_right();
        strip.focus_first();
        strip.consume_or_expel_left();
        assert_eq!(layout(&strip), [vec![0], vec![1]]);
    }

    #[test]
    fn consume_into_column_takes_the_top_pane_on_the_right() {
        let mut strip = strip_with(3, 100);
        strip.focus_last();
        strip.consume_or_expel_left(); // [0] [1 2]
        strip.focus_first();
        strip.consume_into_column();
        assert_eq!(layout(&strip), [vec![0, 1], vec![2]]);
        assert_eq!(strip.focused(), Some(PaneId(0)), "focus stays put");
        strip.consume_into_column();
        assert_eq!(layout(&strip), [vec![0, 1, 2]]);
        strip.consume_into_column();
        assert_eq!(layout(&strip), [vec![0, 1, 2]]);
    }

    #[test]
    fn expel_from_column_only_splits_shared_columns() {
        let mut strip = strip_with(2, 100);
        strip.expel_from_column();
        assert_eq!(layout(&strip), [vec![0], vec![1]]);
        strip.consume_or_expel_left();
        strip.focus_up();
        strip.expel_from_column();
        assert_eq!(layout(&strip), [vec![1], vec![0]]);
        assert_eq!(
            (strip.focus_index(), strip.focused()),
            (1, Some(PaneId(0)))
        );
    }

    #[test]
    fn consuming_respects_the_stack_limit() {
        let mut strip = strip_with(3, 100);
        strip.set_max_stack(2);
        strip.consume_or_expel_left(); // [0] [1 2]
        strip.focus_first();
        strip.consume_or_expel_right();
        assert_eq!(layout(&strip), [vec![0], vec![1, 2]], "column is full");
        strip.consume_into_column();
        assert_eq!(layout(&strip), [vec![0, 1], vec![2]]);
        strip.consume_into_column();
        assert_eq!(layout(&strip), [vec![0, 1], vec![2]], "column is full");
    }

    #[test]
    fn focus_and_move_within_a_column() {
        let mut strip = strip_with(3, 100);
        strip.focus_first();
        strip.consume_into_column();
        strip.consume_into_column(); // [0 1 2], focus on 0
        strip.focus_down();
        strip.focus_down();
        strip.focus_up();
        assert_eq!(strip.focused(), Some(PaneId(1)));
        strip.move_up();
        assert_eq!(layout(&strip), [vec![1, 0, 2]]);
        assert_eq!(strip.focused(), Some(PaneId(1)));
        strip.move_up();
        strip.focus_down();
        strip.focus_down();
        strip.focus_down();
        assert_eq!(strip.focused(), Some(PaneId(2)));
        strip.move_down();
        assert_eq!(layout(&strip), [vec![1, 0, 2]]);
    }

    #[test]
    fn removing_a_stacked_pane_keeps_its_column() {
        let mut strip = strip_with(3, 100);
        strip.consume_or_expel_left(); // [0] [1 2], focus on 2
        assert!(strip.remove(PaneId(2)));
        assert_eq!(layout(&strip), [vec![0], vec![1]]);
        assert_eq!(strip.focused(), Some(PaneId(1)));
        strip.consume_or_expel_left(); // [0 1]
        strip.focus_up();
        assert!(strip.remove(PaneId(1)));
        assert_eq!(
            strip.focused(),
            Some(PaneId(0)),
            "focus stays on the same pane"
        );
    }

    #[test]
    fn heights_split_evenly_with_extra_rows_on_top() {
        assert_eq!(split_heights(10, 3), [4, 3, 3]);
        assert_eq!(split_heights(9, 3), [3, 3, 3]);
        assert_eq!(split_heights(5, 1), [5]);
    }

    #[test]
    fn consuming_the_last_column_leaves_no_gap_on_the_right() {
        let mut strip = strip_with(3, 100);
        assert_eq!(strip.target_offset, 50);
        strip.focus_left();
        strip.consume_into_column(); // [0] [1 2], which all fits
        assert_eq!(layout(&strip), [vec![0], vec![1, 2]]);
        assert_eq!(strip.target_offset, 0);
    }

    #[test]
    fn fullscreen_survives_focus_that_cannot_move() {
        let mut strip = strip_with(1, 100);
        strip.toggle_fullscreen();
        strip.focus_down();
        strip.focus_up();
        assert_eq!(strip.columns()[0].fullscreen(), Some(PaneId(0)));

        let mut strip = strip_with(2, 100);
        strip.consume_or_expel_left(); // [0 1], on 1
        strip.toggle_fullscreen();
        strip.focus_down(); // already at the bottom
        assert_eq!(strip.columns()[0].fullscreen(), Some(PaneId(1)));
        strip.focus_up();
        assert_eq!(strip.columns()[0].fullscreen(), None);
    }

    #[test]
    fn focus_pane_finds_stacked_panes() {
        let mut strip = strip_with(3, 100);
        strip.consume_or_expel_left(); // [0] [1 2]
        assert!(strip.focus_pane(PaneId(1)));
        assert_eq!(
            (strip.focus_index(), strip.focused()),
            (1, Some(PaneId(1)))
        );
        assert!(strip.focus_pane(PaneId(0)));
        assert_eq!(strip.focus_index(), 0);
        assert!(!strip.focus_pane(PaneId(9)));
    }

    #[test]
    fn presets_come_from_the_config() {
        use PresetSize::{Fixed, Proportion};
        let mut strip = Strip::new(100);
        strip.set_size_presets(&SizePresets {
            column_widths: vec![Fixed(40), Proportion(0.75)],
            default_column_width: Fixed(30),
            ..SizePresets::default()
        });
        strip.insert(PaneId(0));
        assert_eq!(strip.column_width(0), 30, "the default width");
        strip.cycle_width(); // 30 isn't a preset: the next wider one
        assert_eq!(strip.column_width(0), 40);
        strip.cycle_width();
        assert_eq!(strip.column_width(0), 75);
        strip.cycle_width(); // round again
        assert_eq!(strip.column_width(0), 40);

        // New presets leave open columns as they are.
        strip.set_size_presets(&SizePresets::default());
        assert_eq!(strip.column_width(0), 40);
        strip.cycle_width(); // the next wider: a half
        assert_eq!(strip.column_width(0), 50);
    }

    #[test]
    fn shares_written_as_decimals_divide_exactly() {
        let mut strip = Strip::new(99);
        strip.set_size_presets(&SizePresets {
            default_column_width: PresetSize::Proportion(0.33333),
            ..SizePresets::default()
        });
        strip.insert(PaneId(0));
        assert_eq!(strip.column_width(0), 33);
        // A fixed width wider than the view is the view.
        strip.set_size_presets(&SizePresets {
            default_column_width: PresetSize::Fixed(500),
            ..SizePresets::default()
        });
        strip.insert(PaneId(1));
        assert_eq!(strip.column_width(1), 99);
    }

    /// Panes 0 and 1 stacked in one column, focus on 1, in a view 30 rows
    /// tall.
    fn stack_of_two() -> Strip {
        let mut strip = strip_with(2, 100);
        strip.set_view_height(30);
        strip.consume_or_expel_left();
        assert_eq!(layout(&strip), [vec![0, 1]]);
        strip
    }

    #[test]
    fn stacked_panes_share_until_one_gets_a_height() {
        let mut strip = stack_of_two();
        assert_eq!(strip.pane_heights(0, 30), [15, 15]);
        // From an even half, the next preset up is two thirds.
        strip.switch_preset_height();
        assert_eq!(strip.pane_heights(0, 30), [10, 20]);
        strip.switch_preset_height(); // round to a third
        assert_eq!(strip.pane_heights(0, 30), [20, 10]);
        strip.reset_pane_height();
        assert_eq!(strip.pane_heights(0, 30), [15, 15]);
    }

    #[test]
    fn set_heights_leave_others_room_and_scale_in_the_overview() {
        use PresetSize::{Fixed, Proportion};
        let mut strip = stack_of_two();
        strip.set_size_presets(&SizePresets {
            pane_heights: vec![Fixed(29), Proportion(0.5)],
            ..SizePresets::default()
        });
        strip.switch_preset_height(); // 29 rows wanted, but pane 0 needs 5
        assert_eq!(strip.pane_heights(0, 30), [MIN_PANE_HEIGHT, 25]);
        // Zoomed out to a third, everything shrinks alike.
        assert_eq!(strip.pane_heights(0, 10), [2, 8]);
        // Both set: they fill the column between them.
        strip.focus_up();
        strip.switch_preset_height(); // 29 again
        assert_eq!(strip.pane_heights(0, 30), [15, 15]);
    }

    #[test]
    fn heights_belong_to_their_stack() {
        let mut strip = stack_of_two();
        strip.switch_preset_height();
        // Expelled and consumed back, pane 1 shares again.
        strip.expel_from_column();
        strip.consume_or_expel_left();
        assert_eq!(layout(&strip), [vec![0, 1]]);
        assert_eq!(strip.pane_heights(0, 30), [15, 15]);
        // A pane alone fills its column; there's nothing to switch.
        let mut alone = strip_with(1, 100);
        alone.set_view_height(30);
        alone.switch_preset_height();
        assert_eq!(alone.pane_heights(0, 30), [30]);
    }

    #[test]
    fn dragging_an_edge_sets_the_width_exactly() {
        let mut strip = strip_with(2, 120);
        strip.resize_column(0, 80);
        assert_eq!(strip.column_width(0), 80);
        // Not past the minimum or the view.
        strip.resize_column(0, 2);
        assert_eq!(strip.column_width(0), MIN_COLUMN_WIDTH);
        strip.resize_column(0, 500);
        assert_eq!(strip.column_width(0), 120);
        // The view stays put while dragging, and catches up after.
        let offset = strip.target_offset;
        strip.resize_column(1, 100);
        assert_eq!(strip.target_offset, offset);
        strip.show_focus();
        assert_eq!(strip.target_offset, 120 + 100 - 120);
    }

    #[test]
    fn dragging_between_stacked_panes_keeps_those_above_still() {
        let mut strip = strip_with(3, 100);
        strip.set_view_height(30);
        strip.consume_or_expel_left();
        strip.focus_left();
        strip.consume_or_expel_right(); // one column of all three
        assert_eq!(strip.columns().len(), 1);
        assert_eq!(strip.columns()[0].panes().len(), 3);
        assert_eq!(strip.pane_heights(0, 30), [10, 10, 10]);
        // The line under pane 1, dragged down to make it 14 tall: pane 0
        // keeps its 10, so the line is where the mouse is.
        strip.resize_pane(0, 1, 14);
        assert_eq!(strip.pane_heights(0, 30), [10, 14, 6]);
        // Not so far that the pane below gets too short.
        strip.resize_pane(0, 1, 30);
        assert_eq!(strip.pane_heights(0, 30), [10, 15, MIN_PANE_HEIGHT]);
    }

    #[test]
    fn maximize_toggles_back_to_the_previous_width() {
        let mut strip = strip_with(2, 120);
        strip.cycle_width(); // 1/2 -> 2/3
        assert_eq!(strip.column_width(1), 80);
        strip.toggle_maximized();
        assert_eq!(strip.column_width(1), 120);
        strip.toggle_maximized();
        assert_eq!(strip.column_width(1), 80);
        // Already full: nothing to toggle back to.
        strip.cycle_width(); // -> full
        strip.toggle_maximized();
        assert_eq!(strip.column_width(1), 120);
    }

    #[test]
    fn fullscreen_widens_the_column_and_stays_with_its_pane() {
        let mut strip = strip_with(3, 100);
        strip.consume_or_expel_left(); // [0] [1 2], focus on 2
        strip.toggle_fullscreen();
        assert_eq!(strip.columns()[1].fullscreen(), Some(PaneId(2)));
        assert_eq!(strip.column_width(1), 100);
        // Focusing another column keeps it; coming back finds it still on.
        strip.focus_left();
        assert_eq!(strip.columns()[1].fullscreen(), Some(PaneId(2)));
        strip.focus_right();
        strip.toggle_fullscreen();
        assert_eq!(strip.columns()[1].fullscreen(), None);
        assert_eq!(strip.column_width(1), 50);
    }

    #[test]
    fn fullscreen_ends_when_its_stack_changes() {
        let mut strip = strip_with(3, 100);
        strip.consume_or_expel_left(); // [0] [1 2]
        strip.toggle_fullscreen();
        strip.focus_up();
        assert_eq!(
            strip.columns()[1].fullscreen(),
            None,
            "moving within the stack"
        );

        strip.toggle_fullscreen();
        strip.focus_first();
        strip.consume_or_expel_right(); // pane 0 joins [1 2]
        assert_eq!(strip.columns()[0].fullscreen(), None, "consuming into it");

        strip.toggle_fullscreen();
        strip.expel_from_column();
        assert!(
            strip.columns().iter().all(|c| c.fullscreen().is_none()),
            "expelling"
        );
    }
}
