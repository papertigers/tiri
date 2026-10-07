// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What keys do: looking them up in the configured bindings, and running
//! the actions they map to, here or on the server.

use crossterm::event::{KeyEvent, KeyEventKind, KeyModifiers};

use crate::effects::Transition;
use crate::input::encode_key;
use crate::keys::{Action, Key};
use crate::protocol::{ClientMsg, Command};

use super::{App, Client};

impl App {
    pub fn key(&mut self, client: &mut Client, event: KeyEvent) {
        if event.kind != KeyEventKind::Press {
            return;
        }
        let key = Key::from_event(event);
        // Bindings have Ctrl, Alt and Shift. Keys with Super and the like,
        // which only terminals speaking the kitty keyboard protocol send,
        // are the program's, not bindings missing those.
        let bindable = !event.modifiers.intersects(
            KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META,
        );
        let bindings = &self.config.bindings;
        let is_prefix = bindable && key == bindings.prefix;

        let after_prefix = std::mem::take(&mut client.prefix_pending);
        let overview = self.workspaces.in_overview(client.id);
        let actions = match (after_prefix, is_prefix) {
            // Prefix twice types it, like any key not bound; see below.
            (true, true) => None,
            // Unbound after the prefix, as any key that isn't a binding.
            (true, false) if !bindable => return,
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
                return;
            }
            (false, false) if !bindable => None,
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
            return;
        }
        client.selection = None;
        if let Some(id) = self.workspaces.focused(client.id) {
            // Typing returns to the live screen, as in any terminal.
            client.scrollback.remove(&id);
        }
        // The server sends it to whichever pane is focused when it arrives,
        // which a focus change on its way there may have moved.
        let modes = (self.focused_pane(client))
            .map(|pane| pane.emulator().key_modes())
            .unwrap_or_default();
        let bytes = encode_key(event, modes);
        self.outbox.push(ClientMsg::Input { pane: None, bytes });
    }

    /// Runs a binding's actions in order. Those that change the layout go
    /// to the server, which runs them in the same order.
    fn run_all(&mut self, client: &mut Client, actions: &[Action]) {
        for &action in actions {
            self.run(client, action);
        }
    }

    fn run(&mut self, client: &mut Client, action: Action) {
        match action {
            Action::ToggleOverview => {
                let on = !self.workspaces.in_overview(client.id);
                self.set_overview(client, on);
            }
            Action::CloseOverview => self.set_overview(client, false),
            Action::ToggleThumbnails => {
                client.kitty_overview = !client.kitty_overview
            }
            Action::Detach => self.outbox.push(ClientMsg::Detach),
            action => self.command(Command::Action(action)),
        }
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
