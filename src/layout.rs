//! The niri-style scrollable strip: an unbounded row of columns with a
//! viewport sliding over it. Opening a column never resizes the others; the
//! viewport scrolls instead.

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column {
    pub pane: PaneId,
    preset: usize,
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
        }
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
        self.columns.get(self.focus).map(|c| c.pane)
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

    pub fn in_overview(&self) -> bool {
        self.overview
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
        self.columns.insert(
            idx,
            Column {
                pane,
                preset: DEFAULT_PRESET,
            },
        );
        self.focus = idx;
        self.scroll_to_focus();
    }

    /// Removes the column holding `pane`. Returns false if it wasn't here.
    pub fn remove(&mut self, pane: PaneId) -> bool {
        let Some(idx) = self.columns.iter().position(|c| c.pane == pane) else {
            return false;
        };
        self.columns.remove(idx);
        if idx < self.focus || self.focus >= self.columns.len() {
            self.focus = self.focus.saturating_sub(1);
        }
        // Don't leave empty space on the right where columns used to be.
        let max_offset = (self.total_width() - i32::from(self.view_width)).max(0);
        self.target_offset = self.target_offset.min(max_offset);
        self.scroll_to_focus();
        true
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
        let zoom = (view / total.max(1.0)).clamp(OVERVIEW_MIN_ZOOM, OVERVIEW_MAX_ZOOM);
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

    #[test]
    fn insert_goes_right_of_focus() {
        let mut strip = strip_with(3, 100);
        strip.focus_first();
        strip.insert(PaneId(9));
        let order: Vec<_> = strip.columns().iter().map(|c| c.pane.0).collect();
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
}
