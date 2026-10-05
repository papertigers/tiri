//! Workspaces stacked vertically, niri-style, each with its own strip of
//! columns. They're tiri's sessions: named workspaces stay until closed even
//! when empty, unnamed ones disappear once nobody is on them and they're
//! empty, and there's always one empty workspace at the bottom to start
//! something new in.
//!
//! The workspaces and their columns are shared. Each attached client has its
//! own [`View`] of them: which workspace it's on, whether its overview is
//! open, and its own animations.

use std::collections::HashMap;
use std::time::Duration;

use crate::layout::{OVERVIEW_MAX_ZOOM, PaneId, Strip, StripView};

/// Time constant for the vertical slide between workspaces; matches the
/// strip's horizontal scrolling.
const SLIDE_TAU: f64 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(pub u32);

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

/// What one client sees of the workspaces.
#[derive(Debug)]
struct View {
    /// Index of the workspace the client is on.
    active: usize,
    /// The vertical position, in workspaces from the top; eases toward
    /// `active` to slide between them.
    y: f64,
    overview: bool,
    /// The client's view of each workspace's strip, by workspace index.
    strips: Vec<StripView>,
}

#[derive(Debug)]
pub struct Workspaces {
    list: Vec<Workspace>,
    view_width: u16,
    max_stack: usize,
    views: HashMap<ClientId, View>,
}

impl Workspaces {
    /// Starts with a named workspace for each of `names`, plus the empty one
    /// at the bottom, and no clients.
    pub fn new(view_width: u16, names: &[String]) -> Self {
        let mut workspaces = Self {
            list: Vec::new(),
            view_width,
            max_stack: usize::MAX,
            views: HashMap::new(),
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
        Workspace { name, strip }
    }

    /// Starts tracking a client's view, on the first workspace.
    pub fn add_client(&mut self, client: ClientId) {
        let strips = self
            .list
            .iter()
            .map(|w| StripView::settled(&w.strip, None))
            .collect();
        let view = View {
            active: 0,
            y: 0.0,
            overview: false,
            strips,
        };
        self.views.insert(client, view);
    }

    pub fn remove_client(&mut self, client: ClientId) {
        self.views.remove(&client);
        self.normalize();
    }

    fn view(&self, client: ClientId) -> &View {
        &self.views[&client]
    }

    fn view_mut(&mut self, client: ClientId) -> &mut View {
        self.views.get_mut(&client).expect("client is attached")
    }

    pub fn list(&self) -> &[Workspace] {
        &self.list
    }

    pub fn active_index(&self, client: ClientId) -> usize {
        self.view(client).active
    }

    pub fn active(&self, client: ClientId) -> &Strip {
        &self.list[self.active_index(client)].strip
    }

    /// The strip of the workspace `client` is on, for changes within it.
    /// Adding or removing panes goes through [`Self::insert`] and
    /// [`Self::remove`] instead, since that can empty or fill a workspace.
    pub fn active_mut(&mut self, client: ClientId) -> &mut Strip {
        let active = self.active_index(client);
        &mut self.list[active].strip
    }

    pub fn focused(&self, client: ClientId) -> Option<PaneId> {
        self.active(client).focused()
    }

    /// Opens `pane` in a new column on `client`'s workspace.
    pub fn insert(&mut self, client: ClientId, pane: PaneId) {
        self.active_mut(client).insert(pane);
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

    pub fn focus_down(&mut self, client: ClientId) {
        let len = self.list.len();
        let view = self.view_mut(client);
        if view.active + 1 < len {
            view.active += 1;
            self.normalize();
        }
    }

    pub fn focus_up(&mut self, client: ClientId) {
        let view = self.view_mut(client);
        if view.active > 0 {
            view.active -= 1;
            self.normalize();
        }
    }

    /// Moves the focused column to the workspace below, and follows it.
    pub fn move_column_down(&mut self, client: ClientId) {
        let active = self.active_index(client);
        if active + 1 < self.list.len() {
            self.move_column_to(client, active + 1);
        }
    }

    /// Moves the focused column to the workspace above, and follows it.
    pub fn move_column_up(&mut self, client: ClientId) {
        let active = self.active_index(client);
        if active > 0 {
            self.move_column_to(client, active - 1);
        }
    }

    fn move_column_to(&mut self, client: ClientId, target: usize) {
        let Some(column) = self.active_mut(client).take_focused_column() else {
            return;
        };
        self.list[target].strip.insert_column(column);
        self.view_mut(client).active = target;
        self.normalize();
    }

    /// Restores the invariants after any change: empty unnamed workspaces
    /// vanish once no client is on them (unless they're last), and the last
    /// workspace is always an empty unnamed one.
    fn normalize(&mut self) {
        let mut idx = 0;
        while idx < self.list.len() {
            let workspace = &self.list[idx];
            let disposable = workspace.is_empty() && workspace.name.is_none();
            let in_use = self.views.values().any(|v| v.active == idx);
            if disposable && !in_use && idx + 1 != self.list.len() {
                self.list.remove(idx);
                for view in self.views.values_mut() {
                    view.strips.remove(idx);
                    if idx < view.active {
                        view.active -= 1;
                        // Keep the slide in step so nothing visibly jumps.
                        view.y -= 1.0;
                    }
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
            let zoom = self.overview_zoom();
            for view in self.views.values_mut() {
                let overview = view.overview.then_some(zoom);
                view.strips
                    .push(StripView::settled(&workspace.strip, overview));
            }
            self.list.push(workspace);
        }
    }

    pub fn set_view_width(&mut self, width: u16) {
        self.view_width = width;
        for workspace in &mut self.list {
            workspace.strip.set_view_width(width);
        }
        // Jump straight to the new layout rather than animating to it.
        let zoom = self.overview_zoom();
        for view in self.views.values_mut() {
            let overview = view.overview.then_some(zoom);
            for (strip_view, workspace) in view.strips.iter_mut().zip(&self.list) {
                *strip_view = StripView::settled(&workspace.strip, overview);
            }
        }
    }

    pub fn set_max_stack(&mut self, max: usize) {
        self.max_stack = max;
        for workspace in &mut self.list {
            workspace.strip.set_max_stack(max);
        }
    }

    pub fn in_overview(&self, client: ClientId) -> bool {
        self.view(client).overview
    }

    pub fn set_overview(&mut self, client: ClientId, on: bool) {
        self.view_mut(client).overview = on;
    }

    /// In the overview every workspace shrinks by the same amount: enough to
    /// fit the widest one.
    fn overview_zoom(&self) -> f64 {
        (self.list.iter())
            .filter(|w| !w.is_empty())
            .map(|w| w.strip.fit_zoom())
            .fold(OVERVIEW_MAX_ZOOM, f64::min)
    }

    /// The zoom `client`'s strips are heading for.
    fn zoom_target(&self, view: &View) -> Option<f64> {
        view.overview.then(|| self.overview_zoom())
    }

    /// `client`'s current zoom, 1.0 outside the overview.
    pub fn zoom(&self, client: ClientId) -> f64 {
        let view = self.view(client);
        view.strips[view.active].zoom()
    }

    /// `client`'s vertical position, in workspaces from the top.
    pub fn y(&self, client: ClientId) -> f64 {
        self.view(client).y
    }

    /// Column `idx` of workspace `ws`'s left edge and width on `client`'s
    /// screen.
    pub fn column_span(&self, client: ClientId, ws: usize, idx: usize) -> (i32, i32) {
        self.view(client).strips[ws].column_span(&self.list[ws].strip, idx)
    }

    pub fn is_animating(&self, client: ClientId) -> bool {
        let view = self.view(client);
        let zoom = self.zoom_target(view);
        view.y != view.active as f64
            || (view.strips.iter())
                .zip(&self.list)
                .any(|(s, w)| s.is_animating(&w.strip, zoom))
    }

    /// Advances every client's animations. Returns true while anything
    /// still moves.
    pub fn tick(&mut self, dt: Duration) -> bool {
        let overview_zoom = self.overview_zoom();
        let mut moving = false;
        for view in self.views.values_mut() {
            let zoom = view.overview.then_some(overview_zoom);
            for (strip_view, workspace) in view.strips.iter_mut().zip(&self.list) {
                moving |= strip_view.tick(&workspace.strip, zoom, dt);
            }
            let target = view.active as f64;
            if (target - view.y).abs() < 0.01 {
                view.y = target;
            } else {
                view.y += (target - view.y) * (1.0 - (-dt.as_secs_f64() / SLIDE_TAU).exp());
                moving = true;
            }
        }
        moving
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: ClientId = ClientId(0);
    const B: ClientId = ClientId(1);

    fn with_client(names: &[&str]) -> Workspaces {
        let names: Vec<String> = names.iter().map(|&n| n.to_owned()).collect();
        let mut ws = Workspaces::new(100, &names);
        ws.add_client(A);
        ws
    }

    /// Each workspace as (name, panes in column order).
    fn shape(ws: &Workspaces) -> Vec<(Option<&str>, Vec<u32>)> {
        ws.list()
            .iter()
            .map(|w| {
                let panes = (w.strip().columns().iter())
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
        let ws = with_client(&["work", "play"]);
        assert_eq!(
            shape(&ws),
            [
                (Some("work"), vec![]),
                (Some("play"), vec![]),
                (None, vec![])
            ]
        );
        assert_eq!(ws.active_index(A), 0);
    }

    #[test]
    fn filling_the_bottom_workspace_adds_another() {
        let mut ws = with_client(&[]);
        assert_eq!(shape(&ws), [(None, vec![])]);
        ws.insert(A, PaneId(0));
        assert_eq!(shape(&ws), [(None, vec![0]), (None, vec![])]);
        ws.focus_down(A);
        ws.insert(A, PaneId(1));
        assert_eq!(
            shape(&ws),
            [(None, vec![0]), (None, vec![1]), (None, vec![])]
        );
    }

    #[test]
    fn empty_unnamed_workspaces_vanish_once_left() {
        let mut ws = with_client(&[]);
        ws.insert(A, PaneId(0));
        ws.focus_down(A);
        ws.insert(A, PaneId(1));
        ws.remove(PaneId(1));
        // Still here while focused, even though it's empty.
        assert_eq!(
            shape(&ws),
            [(None, vec![0]), (None, vec![]), (None, vec![])]
        );
        ws.focus_up(A);
        assert_eq!(shape(&ws), [(None, vec![0]), (None, vec![])]);
    }

    #[test]
    fn named_workspaces_stay_when_empty() {
        let mut ws = with_client(&["work"]);
        ws.insert(A, PaneId(0));
        ws.remove(PaneId(0));
        ws.focus_down(A);
        assert_eq!(shape(&ws), [(Some("work"), vec![]), (None, vec![])]);
        assert_eq!(ws.active_index(A), 1);
    }

    #[test]
    fn removing_a_workspace_above_keeps_the_view_steady() {
        let mut ws = with_client(&[]);
        ws.insert(A, PaneId(0));
        ws.focus_down(A);
        ws.insert(A, PaneId(1));
        settle(&mut ws);
        assert_eq!(ws.y(A), 1.0);
        // Pane 0's workspace empties while we're looking at pane 1's.
        ws.remove(PaneId(0));
        assert_eq!(shape(&ws), [(None, vec![1]), (None, vec![])]);
        assert_eq!((ws.active_index(A), ws.y(A)), (0, 0.0));
        assert_eq!(ws.focused(A), Some(PaneId(1)));
    }

    #[test]
    fn moving_a_column_down_follows_it() {
        let mut ws = with_client(&[]);
        ws.insert(A, PaneId(0));
        ws.insert(A, PaneId(1));
        ws.move_column_down(A);
        assert_eq!(
            shape(&ws),
            [(None, vec![0]), (None, vec![1]), (None, vec![])]
        );
        assert_eq!((ws.active_index(A), ws.focused(A)), (1, Some(PaneId(1))));

        ws.move_column_up(A);
        assert_eq!(shape(&ws), [(None, vec![0, 1]), (None, vec![])]);
        assert_eq!((ws.active_index(A), ws.focused(A)), (0, Some(PaneId(1))));
    }

    #[test]
    fn moving_the_last_column_away_drops_the_workspace() {
        let mut ws = with_client(&[]);
        ws.insert(A, PaneId(0));
        ws.focus_down(A);
        ws.insert(A, PaneId(1));
        ws.move_column_up(A);
        assert_eq!(shape(&ws), [(None, vec![0, 1]), (None, vec![])]);
    }

    #[test]
    fn focus_stops_at_the_ends() {
        let mut ws = with_client(&["a"]);
        ws.focus_up(A);
        assert_eq!(ws.active_index(A), 0);
        ws.focus_down(A);
        ws.focus_down(A);
        assert_eq!(ws.active_index(A), 1);
        ws.move_column_up(A); // nothing to move
        assert_eq!(ws.active_index(A), 1);
    }

    #[test]
    fn overview_shrinks_every_workspace_alike() {
        let mut ws = with_client(&[]);
        ws.insert(A, PaneId(0));
        ws.focus_down(A);
        for i in 1..7 {
            ws.insert(A, PaneId(i));
        }
        ws.set_overview(A, true);
        settle(&mut ws);
        // Six half-width columns need a third to fit; that applies to both.
        let view = ws.view(A);
        let zooms: Vec<f64> = view.strips.iter().map(StripView::zoom).collect();
        assert!(
            zooms.iter().all(|&z| (z - 1.0 / 3.0).abs() < 1e-9),
            "{zooms:?}"
        );
    }

    #[test]
    fn slides_vertically_to_the_active_workspace() {
        let mut ws = with_client(&["a", "b"]);
        ws.focus_down(A);
        assert!(ws.is_animating(A));
        assert!(ws.tick(Duration::from_millis(16)));
        assert!(ws.y(A) > 0.0 && ws.y(A) < 1.0);
        settle(&mut ws);
        assert_eq!(ws.y(A), 1.0);
    }

    #[test]
    fn clients_move_between_workspaces_independently() {
        let mut ws = with_client(&["work", "play"]);
        ws.add_client(B);
        ws.focus_down(B);
        assert_eq!((ws.active_index(A), ws.active_index(B)), (0, 1));
        ws.insert(A, PaneId(0));
        ws.insert(B, PaneId(1));
        assert_eq!(
            (ws.focused(A), ws.focused(B)),
            (Some(PaneId(0)), Some(PaneId(1)))
        );
    }

    #[test]
    fn clients_on_one_workspace_share_its_focus() {
        let mut ws = with_client(&[]);
        ws.add_client(B);
        ws.insert(A, PaneId(0));
        ws.insert(A, PaneId(1));
        assert_eq!(ws.focused(B), Some(PaneId(1)));
        ws.active_mut(B).focus_left();
        assert_eq!(ws.focused(A), Some(PaneId(0)));
    }

    #[test]
    fn an_empty_workspace_stays_while_any_client_is_on_it() {
        let mut ws = with_client(&[]);
        ws.add_client(B);
        ws.insert(A, PaneId(0));
        ws.focus_down(A);
        ws.focus_down(B);
        ws.insert(A, PaneId(1));
        ws.remove(PaneId(1));
        ws.focus_up(A);
        // B is still on the emptied workspace, so it stays.
        assert_eq!(
            shape(&ws),
            [(None, vec![0]), (None, vec![]), (None, vec![])]
        );
        ws.remove_client(B);
        assert_eq!(shape(&ws), [(None, vec![0]), (None, vec![])]);
    }

    #[test]
    fn removing_a_workspace_shifts_every_client_below_it() {
        let mut ws = with_client(&[]);
        ws.add_client(B);
        ws.insert(A, PaneId(0));
        ws.focus_down(A);
        ws.focus_down(B);
        ws.insert(A, PaneId(1));
        settle(&mut ws);
        // Both on workspace 1; workspace 0 empties and goes.
        ws.remove(PaneId(0));
        assert_eq!((ws.active_index(A), ws.active_index(B)), (0, 0));
        assert_eq!((ws.y(A), ws.y(B)), (0.0, 0.0));
    }

    #[test]
    fn overview_is_per_client() {
        let mut ws = with_client(&[]);
        ws.add_client(B);
        ws.insert(A, PaneId(0));
        ws.set_overview(A, true);
        settle(&mut ws);
        assert!(ws.in_overview(A) && !ws.in_overview(B));
        assert_eq!((ws.zoom(A), ws.zoom(B)), (0.5, 1.0));
    }
}
