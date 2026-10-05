//! The niri-style scrollable strip: an unbounded row of columns with a
//! viewport sliding over it. Opening a column never resizes the others; the
//! viewport scrolls instead. Each column is a stack of one or more panes
//! sharing its height.

use std::time::Duration;

/// Column widths as fractions of the viewport, cycled with "cycle width".
pub const WIDTH_PRESETS: [f64; 4] = [1.0 / 3.0, 0.5, 2.0 / 3.0, 1.0];
const DEFAULT_PRESET: usize = 1;
const MIN_COLUMN_WIDTH: u16 = 8;

/// Time constant for the viewport easing; ~95% of the way there after 3x this.
const SCROLL_TAU: f64 = 0.05;

/// The overview zooms out to fit the whole strip, within these bounds.
const OVERVIEW_MAX_ZOOM: f64 = 0.5;
const OVERVIEW_MIN_ZOOM: f64 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// Top to bottom; never empty.
    panes: Vec<PaneId>,
    /// Index of the focused pane in `panes`.
    focus: usize,
    preset: usize,
}

impl Column {
    fn new(pane: PaneId) -> Self {
        Self {
            panes: vec![pane],
            focus: 0,
            preset: DEFAULT_PRESET,
        }
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

    /// Takes out the pane at `idx`, keeping focus on the same pane if it
    /// stays, or on its neighbor if it was the one removed.
    fn take(&mut self, idx: usize) -> PaneId {
        let pane = self.panes.remove(idx);
        if idx < self.focus || self.focus >= self.panes.len() {
            self.focus = self.focus.saturating_sub(1);
        }
        pane
    }
}

/// Splits `total` rows among `n` stacked panes as evenly as possible, giving
/// any leftover rows to the topmost panes.
pub fn split_heights(total: i32, n: usize) -> Vec<i32> {
    let n = n.max(1) as i32;
    (0..n)
        .map(|i| total / n + i32::from(i < total % n))
        .collect()
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

#[derive(Debug)]
pub struct Strip {
    columns: Vec<Column>,
    focus: usize,
    view_width: u16,
    /// Where the viewport is heading, in cells from the strip's left edge.
    target_offset: i32,
    /// Where the viewport is drawn right now; eases toward `view_target`.
    offset: f64,
    overview: bool,
    /// Current scale, 1.0 outside the overview; eases like `offset`.
    zoom: f64,
    /// The most panes a column may hold before consuming is refused.
    max_stack: usize,
    /// The zoom to use in the overview, when set by whoever owns this strip
    /// (so stacked workspaces all shrink alike). Otherwise it fits itself.
    overview_zoom: Option<f64>,
}

impl Strip {
    pub fn new(view_width: u16) -> Self {
        Self {
            columns: Vec::new(),
            focus: 0,
            view_width,
            target_offset: 0,
            offset: 0.0,
            overview: false,
            zoom: 1.0,
            max_stack: usize::MAX,
            overview_zoom: None,
        }
    }

    pub fn set_overview_zoom(&mut self, zoom: Option<f64>) {
        self.overview_zoom = zoom;
    }

    /// The overview zoom that would fit this whole strip on screen.
    pub fn fit_zoom(&self) -> f64 {
        let total = f64::from(self.total_width());
        (f64::from(self.view_width) / total.max(1.0)).clamp(OVERVIEW_MIN_ZOOM, OVERVIEW_MAX_ZOOM)
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
        let idx = if self.columns.is_empty() {
            0
        } else {
            self.focus + 1
        };
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
        let fraction = WIDTH_PRESETS[self.columns[idx].preset];
        ((f64::from(self.view_width) * fraction).floor() as u16).max(MIN_COLUMN_WIDTH)
    }

    /// Left edge of column `idx` in strip coordinates.
    pub fn column_x(&self, idx: usize) -> i32 {
        (0..idx).map(|i| i32::from(self.column_width(i))).sum()
    }

    fn total_width(&self) -> i32 {
        self.column_x(self.columns.len())
    }

    pub fn zoom(&self) -> f64 {
        self.zoom
    }

    pub fn set_overview(&mut self, on: bool) {
        self.overview = on;
    }

    /// Column `idx`'s left edge and width on screen, at the current scroll
    /// position and zoom.
    pub fn column_span(&self, idx: usize) -> (i32, i32) {
        let to_screen = |x: i32| ((f64::from(x) - self.offset) * self.zoom).round() as i32;
        let x = self.column_x(idx);
        let left = to_screen(x);
        (
            left,
            to_screen(x + i32::from(self.column_width(idx))) - left,
        )
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
        let idx = if self.columns.is_empty() {
            0
        } else {
            self.focus + 1
        };
        self.columns.insert(idx, Column::new(pane));
        self.focus = idx;
        self.scroll_to_focus();
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
        let max_offset = (self.total_width() - i32::from(self.view_width)).max(0);
        self.target_offset = self.target_offset.min(max_offset);
        self.scroll_to_focus();
        column
    }

    pub fn focus_up(&mut self) {
        if let Some(col) = self.columns.get_mut(self.focus) {
            col.focus = col.focus.saturating_sub(1);
        }
    }

    pub fn focus_down(&mut self) {
        if let Some(col) = self.columns.get_mut(self.focus) {
            col.focus = (col.focus + 1).min(col.panes.len() - 1);
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
            Side::Right => Some(self.focus + 1).filter(|&i| i < self.columns.len()),
        };
        let Some(neighbor) = neighbor.filter(|&i| self.has_room(i)) else {
            return;
        };
        let pane = self.columns[self.focus].focused();
        let target = &mut self.columns[neighbor];
        target.panes.push(pane);
        target.focus = target.panes.len() - 1;
        // Removing our column shifts the neighbor left if it was to the right.
        let neighbor = if neighbor > self.focus {
            neighbor - 1
        } else {
            neighbor
        };
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
            self.columns.remove(right);
        }
        self.columns[self.focus].panes.push(pane);
        self.scroll_to_focus();
    }

    /// niri's `expel-window-from-column`: the focused pane leaves its column
    /// for a new column on the right, if it shares its column.
    pub fn expel_from_column(&mut self) {
        if self
            .columns
            .get(self.focus)
            .is_some_and(|c| c.panes.len() > 1)
        {
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
        self.columns.insert(idx, Column::new(pane));
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

    pub fn cycle_width(&mut self) {
        if let Some(col) = self.columns.get_mut(self.focus) {
            col.preset = (col.preset + 1) % WIDTH_PRESETS.len();
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
        (self.offset, self.zoom) = self.view_target();
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

    /// Where the viewport is heading: its left edge in strip coordinates,
    /// and its zoom.
    fn view_target(&self) -> (f64, f64) {
        if !self.overview {
            return (f64::from(self.target_offset), 1.0);
        }
        let total = f64::from(self.total_width());
        let view = f64::from(self.view_width);
        let zoom = self.overview_zoom.unwrap_or_else(|| self.fit_zoom());
        // How much of the strip fits on screen at this zoom.
        let visible = view / zoom;
        let offset = if total <= visible || self.columns.is_empty() {
            (total - visible) / 2.0
        } else {
            let x = f64::from(self.column_x(self.focus));
            let center = x + f64::from(self.column_width(self.focus)) / 2.0;
            (center - visible / 2.0).clamp(0.0, total - visible)
        };
        (offset, zoom)
    }

    /// Whether the viewport is still scrolling or zooming toward its target.
    pub fn is_animating(&self) -> bool {
        let (offset, zoom) = self.view_target();
        offset != self.offset || zoom != self.zoom
    }

    /// Advances the scroll and zoom animations. Returns true while still moving.
    pub fn tick(&mut self, dt: Duration) -> bool {
        let (offset, zoom) = self.view_target();
        if (offset - self.offset).abs() < 0.5 && (zoom - self.zoom).abs() < 0.005 {
            (self.offset, self.zoom) = (offset, zoom);
            return false;
        }
        let k = 1.0 - (-dt.as_secs_f64() / SCROLL_TAU).exp();
        self.offset += (offset - self.offset) * k;
        self.zoom += (zoom - self.zoom) * k;
        true
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
        let order: Vec<_> = strip.columns().iter().map(|c| c.focused().0).collect();
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

    #[test]
    fn tick_converges_on_target() {
        let mut strip = strip_with(3, 100);
        assert!(strip.tick(Duration::from_millis(16)));
        settle(&mut strip);
        assert_eq!(strip.column_span(1), (0, 50));
    }

    fn settle(strip: &mut Strip) {
        let mut frames = 0;
        while strip.tick(Duration::from_millis(16)) {
            frames += 1;
            assert!(frames < 100, "animation never settled");
        }
    }

    #[test]
    fn overview_fits_and_centers_a_short_strip() {
        let mut strip = strip_with(3, 100);
        strip.set_overview(true);
        settle(&mut strip);
        // 150 cells of strip at half zoom is 75 wide, centered in 100.
        assert_eq!(strip.zoom(), 0.5);
        assert_eq!(strip.column_span(0), (13, 25));
        assert_eq!(strip.column_span(2), (63, 25));
    }

    #[test]
    fn overview_zooms_further_to_fit_more_columns() {
        let mut strip = strip_with(6, 100);
        strip.set_overview(true);
        settle(&mut strip);
        assert!((strip.zoom() - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(strip.column_span(0).0, 0);
        assert_eq!(strip.column_span(5), (83, 17));
    }

    #[test]
    fn overview_scrolls_to_selection_when_strip_is_too_long() {
        let mut strip = strip_with(12, 100);
        strip.set_overview(true);
        settle(&mut strip);
        assert_eq!(strip.zoom(), OVERVIEW_MIN_ZOOM);
        // Selection is the last column, so the strip's right end is on screen.
        assert_eq!(strip.column_span(11), (88, 12));
        strip.focus_first();
        settle(&mut strip);
        assert_eq!(strip.column_span(0), (0, 13));
    }

    #[test]
    fn leaving_overview_lands_on_the_selection() {
        let mut strip = strip_with(4, 100);
        strip.set_overview(true);
        strip.focus_first();
        settle(&mut strip);
        strip.set_overview(false);
        settle(&mut strip);
        assert_eq!(strip.zoom(), 1.0);
        assert_eq!(strip.column_span(0), (0, 50));
    }

    #[test]
    fn consume_or_expel_moves_a_lone_pane_into_its_neighbor() {
        let mut strip = strip_with(3, 100);
        strip.consume_or_expel_left();
        assert_eq!(layout(&strip), [vec![0], vec![1, 2]]);
        assert_eq!((strip.focus_index(), strip.focused()), (1, Some(PaneId(2))));

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
        assert_eq!((strip.focus_index(), strip.focused()), (0, Some(PaneId(1))));

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
        assert_eq!((strip.focus_index(), strip.focused()), (1, Some(PaneId(0))));
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
}
