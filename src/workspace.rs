//! Workspaces stacked vertically, niri-style, each with its own strip of
//! columns. They're tiri's sessions: named workspaces stay until closed even
//! when empty, unnamed ones disappear once left empty, and there's always one
//! empty workspace at the bottom to start something new in.

use std::time::Duration;

use crate::layout::{PaneId, Strip};

/// Time constant for the vertical slide between workspaces; matches the
/// strip's horizontal scrolling.
const SLIDE_TAU: f64 = 0.05;

#[derive(Debug)]
pub struct Workspace {
    name: Option<String>,
    strip: Strip,
}

impl Workspace {
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn strip(&self) -> &Strip {
        &self.strip
    }

    pub fn is_empty(&self) -> bool {
        self.strip.is_empty()
    }
}

#[derive(Debug)]
pub struct Workspaces {
    list: Vec<Workspace>,
    active: usize,
    view_width: u16,
    max_stack: usize,
    overview: bool,
    /// The vertical position, in workspaces from the top; eases toward
    /// `active` to slide between them.
    y: f64,
}

impl Workspaces {
    /// Starts with a named workspace for each of `names`, plus the empty one
    /// at the bottom. The first workspace is active.
    pub fn new(view_width: u16, names: &[String]) -> Self {
        let mut workspaces = Self {
            list: Vec::new(),
            active: 0,
            view_width,
            max_stack: usize::MAX,
            overview: false,
            y: 0.0,
        };
        for name in names {
            let workspace = workspaces.new_workspace(Some(name.clone()));
            workspaces.list.push(workspace);
        }
        workspaces.normalize();
        workspaces
    }

    fn new_workspace(&self, name: Option<String>) -> Workspace {
        let mut strip = Strip::new(self.view_width);
        strip.set_max_stack(self.max_stack);
        strip.set_overview(self.overview);
        Workspace { name, strip }
    }

    pub fn list(&self) -> &[Workspace] {
        &self.list
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active(&self) -> &Strip {
        &self.list[self.active].strip
    }

    /// The active workspace's strip, for changes within it. Adding or
    /// removing panes goes through [`Self::insert`] and [`Self::remove`]
    /// instead, since that can empty or fill a workspace.
    pub fn active_mut(&mut self) -> &mut Strip {
        &mut self.list[self.active].strip
    }

    pub fn focused(&self) -> Option<PaneId> {
        self.active().focused()
    }

    /// Opens `pane` in a new column on the active workspace.
    pub fn insert(&mut self, pane: PaneId) {
        self.active_mut().insert(pane);
        self.normalize();
    }

    /// Removes `pane` from whichever workspace has it.
    pub fn remove(&mut self, pane: PaneId) -> bool {
        let Some(workspace) = self.list.iter_mut().find(|w| w.strip.contains(pane)) else {
            return false;
        };
        workspace.strip.remove(pane);
        self.normalize();
        true
    }

    pub fn focus_down(&mut self) {
        if self.active + 1 < self.list.len() {
            self.active += 1;
            self.normalize();
        }
    }

    pub fn focus_up(&mut self) {
        if self.active > 0 {
            self.active -= 1;
            self.normalize();
        }
    }

    /// Moves the focused column to the workspace below, and follows it.
    pub fn move_column_down(&mut self) {
        if self.active + 1 < self.list.len() {
            self.move_column_to(self.active + 1);
        }
    }

    /// Moves the focused column to the workspace above, and follows it.
    pub fn move_column_up(&mut self) {
        if self.active > 0 {
            self.move_column_to(self.active - 1);
        }
    }

    fn move_column_to(&mut self, target: usize) {
        let Some(column) = self.active_mut().take_focused_column() else {
            return;
        };
        self.list[target].strip.insert_column(column);
        self.active = target;
        self.normalize();
    }

    /// Restores the invariants after any change: empty unnamed workspaces
    /// vanish once they're neither active nor the last, and the last
    /// workspace is always an empty unnamed one.
    fn normalize(&mut self) {
        let mut idx = 0;
        while idx < self.list.len() {
            let workspace = &self.list[idx];
            let disposable = workspace.is_empty() && workspace.name.is_none();
            if disposable && idx != self.active && idx + 1 != self.list.len() {
                self.list.remove(idx);
                if idx < self.active {
                    self.active -= 1;
                    // Keep the slide in step so nothing visibly jumps.
                    self.y -= 1.0;
                }
            } else {
                idx += 1;
            }
        }
        let needs_bottom = self
            .list
            .last()
            .is_none_or(|w| !w.is_empty() || w.name.is_some());
        if needs_bottom {
            let workspace = self.new_workspace(None);
            self.list.push(workspace);
        }
        self.sync_overview_zoom();
    }

    pub fn set_view_width(&mut self, width: u16) {
        self.view_width = width;
        for workspace in &mut self.list {
            workspace.strip.set_view_width(width);
        }
        self.sync_overview_zoom();
    }

    pub fn set_max_stack(&mut self, max: usize) {
        self.max_stack = max;
        for workspace in &mut self.list {
            workspace.strip.set_max_stack(max);
        }
    }

    pub fn in_overview(&self) -> bool {
        self.overview
    }

    pub fn set_overview(&mut self, on: bool) {
        self.overview = on;
        for workspace in &mut self.list {
            workspace.strip.set_overview(on);
        }
        self.sync_overview_zoom();
    }

    /// In the overview every workspace shrinks by the same amount: enough to
    /// fit the widest one.
    fn sync_overview_zoom(&mut self) {
        let zoom = (self.list.iter())
            .filter(|w| !w.is_empty())
            .map(|w| w.strip.fit_zoom())
            .fold(f64::INFINITY, f64::min);
        let zoom = zoom.is_finite().then_some(zoom);
        for workspace in &mut self.list {
            workspace.strip.set_overview_zoom(zoom);
        }
    }

    /// The current zoom, 1.0 outside the overview.
    pub fn zoom(&self) -> f64 {
        self.active().zoom()
    }

    /// The vertical position, in workspaces from the top.
    pub fn y(&self) -> f64 {
        self.y
    }

    pub fn is_animating(&self) -> bool {
        self.y != self.active as f64 || self.list.iter().any(|w| w.strip.is_animating())
    }

    /// Advances every animation. Returns true while anything still moves.
    pub fn tick(&mut self, dt: Duration) -> bool {
        // Column widths may have changed since the last frame.
        self.sync_overview_zoom();
        let mut moving = false;
        for workspace in &mut self.list {
            moving |= workspace.strip.tick(dt);
        }
        let target = self.active as f64;
        if (target - self.y).abs() < 0.01 {
            self.y = target;
        } else {
            self.y += (target - self.y) * (1.0 - (-dt.as_secs_f64() / SLIDE_TAU).exp());
            moving = true;
        }
        moving
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each workspace as (name, panes in column order).
    fn shape(ws: &Workspaces) -> Vec<(Option<&str>, Vec<u32>)> {
        ws.list()
            .iter()
            .map(|w| {
                let panes = w
                    .strip()
                    .columns()
                    .iter()
                    .flat_map(|c| c.panes())
                    .map(|p| p.0);
                (w.name(), panes.collect())
            })
            .collect()
    }

    fn settle(ws: &mut Workspaces) {
        let mut frames = 0;
        while ws.tick(Duration::from_millis(16)) {
            frames += 1;
            assert!(frames < 200, "animation never settled");
        }
    }

    #[test]
    fn starts_with_named_workspaces_and_an_empty_one() {
        let ws = Workspaces::new(100, &["work".into(), "play".into()]);
        assert_eq!(
            shape(&ws),
            [
                (Some("work"), vec![]),
                (Some("play"), vec![]),
                (None, vec![])
            ]
        );
        assert_eq!(ws.active_index(), 0);
    }

    #[test]
    fn filling_the_bottom_workspace_adds_another() {
        let mut ws = Workspaces::new(100, &[]);
        assert_eq!(shape(&ws), [(None, vec![])]);
        ws.insert(PaneId(0));
        assert_eq!(shape(&ws), [(None, vec![0]), (None, vec![])]);
        ws.focus_down();
        ws.insert(PaneId(1));
        assert_eq!(
            shape(&ws),
            [(None, vec![0]), (None, vec![1]), (None, vec![])]
        );
    }

    #[test]
    fn empty_unnamed_workspaces_vanish_once_left() {
        let mut ws = Workspaces::new(100, &[]);
        ws.insert(PaneId(0));
        ws.focus_down();
        ws.insert(PaneId(1));
        ws.remove(PaneId(1));
        // Still here while focused, even though it's empty.
        assert_eq!(
            shape(&ws),
            [(None, vec![0]), (None, vec![]), (None, vec![])]
        );
        ws.focus_up();
        assert_eq!(shape(&ws), [(None, vec![0]), (None, vec![])]);
    }

    #[test]
    fn named_workspaces_stay_when_empty() {
        let mut ws = Workspaces::new(100, &["work".into()]);
        ws.insert(PaneId(0));
        ws.remove(PaneId(0));
        ws.focus_down();
        assert_eq!(shape(&ws), [(Some("work"), vec![]), (None, vec![])]);
        assert_eq!(ws.active_index(), 1);
    }

    #[test]
    fn removing_a_workspace_above_keeps_the_view_steady() {
        let mut ws = Workspaces::new(100, &[]);
        ws.insert(PaneId(0));
        ws.focus_down();
        ws.insert(PaneId(1));
        settle(&mut ws);
        assert_eq!(ws.y(), 1.0);
        // Pane 0's workspace empties while we're looking at pane 1's.
        ws.remove(PaneId(0));
        assert_eq!(shape(&ws), [(None, vec![1]), (None, vec![])]);
        assert_eq!((ws.active_index(), ws.y()), (0, 0.0));
        assert_eq!(ws.focused(), Some(PaneId(1)));
    }

    #[test]
    fn moving_a_column_down_follows_it() {
        let mut ws = Workspaces::new(100, &[]);
        ws.insert(PaneId(0));
        ws.insert(PaneId(1));
        ws.move_column_down();
        assert_eq!(
            shape(&ws),
            [(None, vec![0]), (None, vec![1]), (None, vec![])]
        );
        assert_eq!((ws.active_index(), ws.focused()), (1, Some(PaneId(1))));

        ws.move_column_up();
        assert_eq!(shape(&ws), [(None, vec![0, 1]), (None, vec![])]);
        assert_eq!((ws.active_index(), ws.focused()), (0, Some(PaneId(1))));
    }

    #[test]
    fn moving_the_last_column_away_drops_the_workspace() {
        let mut ws = Workspaces::new(100, &[]);
        ws.insert(PaneId(0));
        ws.focus_down();
        ws.insert(PaneId(1));
        ws.move_column_up();
        assert_eq!(shape(&ws), [(None, vec![0, 1]), (None, vec![])]);
    }

    #[test]
    fn focus_stops_at_the_ends() {
        let mut ws = Workspaces::new(100, &["a".into()]);
        ws.focus_up();
        assert_eq!(ws.active_index(), 0);
        ws.focus_down();
        ws.focus_down();
        assert_eq!(ws.active_index(), 1);
        ws.move_column_up(); // nothing to move
        assert_eq!(ws.active_index(), 1);
    }

    #[test]
    fn overview_shrinks_every_workspace_alike() {
        let mut ws = Workspaces::new(100, &[]);
        ws.insert(PaneId(0));
        ws.focus_down();
        for i in 1..7 {
            ws.insert(PaneId(i));
        }
        ws.set_overview(true);
        settle(&mut ws);
        // Six half-width columns need a third to fit; that applies to both.
        let zooms: Vec<f64> = ws.list().iter().map(|w| w.strip().zoom()).collect();
        assert!(
            zooms.iter().all(|&z| (z - 1.0 / 3.0).abs() < 1e-9),
            "{zooms:?}"
        );
    }

    #[test]
    fn slides_vertically_to_the_active_workspace() {
        let mut ws = Workspaces::new(100, &["a".into(), "b".into()]);
        ws.focus_down();
        assert!(ws.is_animating());
        assert!(ws.tick(Duration::from_millis(16)));
        assert!(ws.y() > 0.0 && ws.y() < 1.0);
        settle(&mut ws);
        assert_eq!(ws.y(), 1.0);
    }
}
