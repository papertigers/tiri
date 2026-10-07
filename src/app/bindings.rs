// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What keys do: looking them up in the configured bindings, and running
//! the actions they map to.

use anyhow::Result;
use crossterm::event::{KeyEvent, KeyEventKind};

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

        let after_prefix = std::mem::take(&mut client.prefix_pending);
        let overview = self.workspaces.in_overview(client.id);
        let actions = match (after_prefix, is_prefix) {
            // Prefix twice types it, like any key not bound; see below.
            (true, true) => None,
            // Holding Ctrl through, as in C-a C-n, works too.
            (true, false) => {
                let actions = (bindings.prefix_binds.get(key))
                    .or_else(|| bindings.prefix_binds.get(key.without_ctrl()?));
                let actions =
                    actions.map(<[Action]>::to_vec).unwrap_or_default();
                return self.run_all(client, &actions);
            }
            (false, true) => {
                client.prefix_pending = true;
                return Ok(());
            }
            (false, false) if overview => (bindings.overview_binds.get(key))
                .or_else(|| bindings.binds.get(key)),
            (false, false) => bindings.binds.get(key),
        };
        if let Some(actions) = actions {
            let actions = actions.to_vec();
            return self.run_all(client, &actions);
        }
        if overview {
            // The overview takes the keyboard; nothing reaches the panes.
            return Ok(());
        }
        client.selection = None;
        if let Some(id) = self.workspaces.focused(client.id) {
            // Typing returns to the live screen, as in any terminal.
            client.scrollback.remove(&id);
        }
        if let Some(pane) = self.focused_pane_mut(client) {
            let bytes = encode_key(event, pane.emulator().application_cursor());
            pane.write(&bytes);
        }
        Ok(())
    }

    /// Runs a binding's actions in order, each seeing what the one before
    /// did. A failure stops the rest.
    fn run_all(
        &mut self,
        client: &mut Client,
        actions: &[Action],
    ) -> Result<()> {
        actions.iter().try_for_each(|&action| self.run(client, action))
    }

    fn run(&mut self, client: &mut Client, action: Action) -> Result<()> {
        let id = client.id;
        match action {
            Action::NewColumn => self.open_column(client)?,
            Action::FocusColumnLeft => {
                self.workspaces.active_mut(id).focus_left()
            }
            Action::FocusColumnRight => {
                self.workspaces.active_mut(id).focus_right()
            }
            Action::FocusColumnFirst => {
                self.workspaces.active_mut(id).focus_first()
            }
            Action::FocusColumnLast => {
                self.workspaces.active_mut(id).focus_last()
            }
            Action::MoveColumnLeft => {
                self.workspaces.active_mut(id).move_left()
            }
            Action::MoveColumnRight => {
                self.workspaces.active_mut(id).move_right()
            }
            Action::FocusPaneUp => self.workspaces.active_mut(id).focus_up(),
            Action::FocusPaneDown => {
                self.workspaces.active_mut(id).focus_down()
            }
            Action::MovePaneUp => self.workspaces.active_mut(id).move_up(),
            Action::MovePaneDown => self.workspaces.active_mut(id).move_down(),
            Action::ConsumeOrExpelPaneLeft => {
                self.workspaces.active_mut(id).consume_or_expel_left();
            }
            Action::ConsumeOrExpelPaneRight => {
                self.workspaces.active_mut(id).consume_or_expel_right();
            }
            Action::ConsumePaneIntoColumn => {
                self.workspaces.active_mut(id).consume_into_column()
            }
            Action::ExpelPaneFromColumn => {
                self.workspaces.active_mut(id).expel_from_column()
            }
            Action::SwitchPresetPaneHeight => {
                self.workspaces.active_mut(id).switch_preset_height();
            }
            Action::ResetPaneHeight => {
                self.workspaces.active_mut(id).reset_pane_height();
            }
            Action::SwitchPresetColumnWidth => {
                self.workspaces.active_mut(id).cycle_width()
            }
            Action::MaximizeColumn => {
                self.workspaces.active_mut(id).toggle_maximized()
            }
            Action::FullscreenPane => {
                self.workspaces.active_mut(id).toggle_fullscreen()
            }
            Action::CenterColumn => {
                self.workspaces.active_mut(id).center_focused()
            }
            Action::ClosePane => {
                if let Some(pane_id) = self.workspaces.focused(id) {
                    self.close_pane(pane_id);
                }
            }
            Action::FocusWorkspaceDown => self.workspaces.focus_down(id),
            Action::FocusWorkspaceUp => self.workspaces.focus_up(id),
            Action::MoveColumnToWorkspaceDown => {
                self.workspaces.move_column_down(id)
            }
            Action::MoveColumnToWorkspaceUp => {
                self.workspaces.move_column_up(id)
            }
            Action::ToggleOverview => {
                let on = !self.workspaces.in_overview(id);
                self.set_overview(client, on);
            }
            Action::CloseOverview => self.set_overview(client, false),
            Action::ToggleThumbnails => {
                client.kitty_overview = !client.kitty_overview
            }
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
        if !self.config.animations {
            return;
        }
        if let Some(from) = client.renderer.last_frame().cloned() {
            client.transition =
                Some(Transition::new(from, on, &client.palette));
        }
    }
}
