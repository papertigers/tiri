//! What keys do: looking them up in the configured bindings, and running
//! the actions they map to.

use anyhow::Result;
use crossterm::event::{KeyEvent, KeyEventKind, KeyModifiers};

use crate::effects::Transition;
use crate::input::encode_key;
use crate::keys::{Action, Key};

use super::{App, Client};

impl App {
    pub fn key(&mut self, client: &mut Client, event: KeyEvent) -> Result<()> {
        if event.kind != KeyEventKind::Press {
            return Ok(());
        }
        self.lay_out_for(client);
        let key = Key::from_event(event);
        let bindings = &self.config.bindings;
        let is_prefix = key == bindings.prefix;

        if std::mem::take(&mut client.prefix_pending) {
            if is_prefix {
                // Prefix twice sends it through to the pane.
                if let Some(pane) = self.focused_pane_mut(client) {
                    pane.write(&encode_key(event, pane.application_cursor()));
                }
            } else {
                // Holding Ctrl through, as in C-a C-n, works too.
                let without_ctrl = Key::from_event(KeyEvent::new(
                    event.code,
                    event.modifiers - KeyModifiers::CONTROL,
                ));
                let action = (bindings.prefix_binds.get(key))
                    .or_else(|| bindings.prefix_binds.get(without_ctrl));
                if let Some(action) = action {
                    self.run(client, action)?;
                }
            }
            return Ok(());
        }
        if is_prefix {
            client.prefix_pending = true;
            return Ok(());
        }
        if self.workspaces.in_overview(client.id) {
            // The overview takes the keyboard; nothing reaches the panes.
            let action = (bindings.overview_binds.get(key)).or_else(|| bindings.binds.get(key));
            if let Some(action) = action {
                self.run(client, action)?;
            }
            return Ok(());
        }
        if let Some(action) = bindings.binds.get(key) {
            return self.run(client, action);
        }
        client.selection = None;
        if let Some(id) = self.workspaces.focused(client.id) {
            // Typing returns to the live screen, as in any terminal.
            client.scrollback.remove(&id);
        }
        if let Some(pane) = self.focused_pane_mut(client) {
            let bytes = encode_key(event, pane.application_cursor());
            pane.write(&bytes);
        }
        Ok(())
    }

    fn run(&mut self, client: &mut Client, action: Action) -> Result<()> {
        let id = client.id;
        match action {
            Action::NewColumn => self.open_column(client)?,
            Action::FocusColumnLeft => self.workspaces.active_mut(id).focus_left(),
            Action::FocusColumnRight => self.workspaces.active_mut(id).focus_right(),
            Action::FocusColumnFirst => self.workspaces.active_mut(id).focus_first(),
            Action::FocusColumnLast => self.workspaces.active_mut(id).focus_last(),
            Action::MoveColumnLeft => self.workspaces.active_mut(id).move_left(),
            Action::MoveColumnRight => self.workspaces.active_mut(id).move_right(),
            Action::FocusPaneUp => self.workspaces.active_mut(id).focus_up(),
            Action::FocusPaneDown => self.workspaces.active_mut(id).focus_down(),
            Action::MovePaneUp => self.workspaces.active_mut(id).move_up(),
            Action::MovePaneDown => self.workspaces.active_mut(id).move_down(),
            Action::ConsumeOrExpelPaneLeft => {
                self.workspaces.active_mut(id).consume_or_expel_left()
            }
            Action::ConsumeOrExpelPaneRight => {
                self.workspaces.active_mut(id).consume_or_expel_right()
            }
            Action::ConsumePaneIntoColumn => self.workspaces.active_mut(id).consume_into_column(),
            Action::ExpelPaneFromColumn => self.workspaces.active_mut(id).expel_from_column(),
            Action::SwitchPresetColumnWidth => self.workspaces.active_mut(id).cycle_width(),
            Action::MaximizeColumn => self.workspaces.active_mut(id).toggle_maximized(),
            Action::FullscreenPane => self.workspaces.active_mut(id).toggle_fullscreen(),
            Action::CenterColumn => self.workspaces.active_mut(id).center_focused(),
            Action::ClosePane => {
                if let Some(pane_id) = self.workspaces.focused(id) {
                    if let Some(pane) = self.panes.get(&pane_id) {
                        pane.kill();
                    }
                    self.pane_exited(pane_id);
                }
            }
            Action::FocusWorkspaceDown => self.workspaces.focus_down(id),
            Action::FocusWorkspaceUp => self.workspaces.focus_up(id),
            Action::MoveColumnToWorkspaceDown => self.workspaces.move_column_down(id),
            Action::MoveColumnToWorkspaceUp => self.workspaces.move_column_up(id),
            Action::ToggleOverview => {
                let on = !self.workspaces.in_overview(id);
                self.set_overview(client, on);
            }
            Action::CloseOverview => self.set_overview(client, false),
            Action::ToggleThumbnails => client.kitty_overview = !client.kitty_overview,
            Action::Detach => client.detach_requested = true,
            Action::KillServer => self.quit = true,
        }
        // Whatever changed, panes' PTYs follow their boxes' sizes.
        self.resize_panes();
        Ok(())
    }

    /// Opens or closes `client`'s overview. The layout changes at once, and
    /// a [`Transition`] fades between the two.
    pub(super) fn set_overview(&mut self, client: &mut Client, on: bool) {
        if on == self.workspaces.in_overview(client.id) {
            return;
        }
        self.workspaces.set_overview(client.id, on);
        self.workspaces.snap(client.id);
        if let Some(from) = client.renderer.last_frame().cloned() {
            client.transition = Some(Transition::new(from, on, &client.palette));
        }
    }
}
