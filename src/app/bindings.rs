//! What keys do: the prefix, overview and Alt tables, and running the
//! actions they map to.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::effects::Transition;
use crate::input::encode_key;

use super::{App, Client, PREFIX};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    NewColumn,
    FocusLeft,
    FocusRight,
    FocusFirst,
    FocusLast,
    MoveLeft,
    MoveRight,
    FocusUp,
    FocusDown,
    MoveUp,
    MoveDown,
    ConsumeOrExpelLeft,
    ConsumeOrExpelRight,
    ConsumeIntoColumn,
    ExpelFromColumn,
    CycleWidth,
    ToggleMaximized,
    ToggleFullscreen,
    Center,
    Close,
    FocusWorkspaceDown,
    FocusWorkspaceUp,
    MoveColumnToWorkspaceDown,
    MoveColumnToWorkspaceUp,
    ToggleOverview,
    ExitOverview,
    ToggleThumbnails,
    Detach,
    Quit,
}

impl App {
    pub fn key(&mut self, client: &mut Client, key: KeyEvent) -> Result<()> {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        self.lay_out_for(client);
        let is_prefix = key.code == KeyCode::Char(PREFIX) && key.modifiers == KeyModifiers::CONTROL;

        if std::mem::take(&mut client.prefix_pending) {
            if is_prefix {
                // Prefix twice sends it through to the pane.
                if let Some(pane) = self.focused_pane_mut(client) {
                    pane.write(&[PREFIX as u8 - b'a' + 1]);
                }
            } else if let Some(action) = prefix_binding(key) {
                self.run(client, action)?;
            }
            return Ok(());
        }
        if is_prefix {
            client.prefix_pending = true;
            return Ok(());
        }
        if self.workspaces.in_overview(client.id) {
            // The overview takes the keyboard; nothing reaches the panes.
            if let Some(action) = overview_binding(key).or_else(|| alt_binding(key)) {
                self.run(client, action)?;
            }
            return Ok(());
        }
        if let Some(action) = alt_binding(key) {
            return self.run(client, action);
        }
        client.selection = None;
        if let Some(id) = self.workspaces.focused(client.id) {
            // Typing returns to the live screen, as in any terminal.
            client.scrollback.remove(&id);
        }
        if let Some(pane) = self.focused_pane_mut(client) {
            let bytes = encode_key(key, pane.application_cursor());
            pane.write(&bytes);
        }
        Ok(())
    }

    fn run(&mut self, client: &mut Client, action: Action) -> Result<()> {
        let id = client.id;
        match action {
            Action::NewColumn => self.open_column(client)?,
            Action::FocusLeft => self.workspaces.active_mut(id).focus_left(),
            Action::FocusRight => self.workspaces.active_mut(id).focus_right(),
            Action::FocusFirst => self.workspaces.active_mut(id).focus_first(),
            Action::FocusLast => self.workspaces.active_mut(id).focus_last(),
            Action::MoveLeft => self.workspaces.active_mut(id).move_left(),
            Action::MoveRight => self.workspaces.active_mut(id).move_right(),
            Action::FocusUp => self.workspaces.active_mut(id).focus_up(),
            Action::FocusDown => self.workspaces.active_mut(id).focus_down(),
            Action::MoveUp => self.workspaces.active_mut(id).move_up(),
            Action::MoveDown => self.workspaces.active_mut(id).move_down(),
            Action::ConsumeOrExpelLeft => self.workspaces.active_mut(id).consume_or_expel_left(),
            Action::ConsumeOrExpelRight => self.workspaces.active_mut(id).consume_or_expel_right(),
            Action::ConsumeIntoColumn => self.workspaces.active_mut(id).consume_into_column(),
            Action::ExpelFromColumn => self.workspaces.active_mut(id).expel_from_column(),
            Action::CycleWidth => self.workspaces.active_mut(id).cycle_width(),
            Action::ToggleMaximized => self.workspaces.active_mut(id).toggle_maximized(),
            Action::ToggleFullscreen => self.workspaces.active_mut(id).toggle_fullscreen(),
            Action::Center => self.workspaces.active_mut(id).center_focused(),
            Action::Close => {
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
            Action::ExitOverview => self.set_overview(client, false),
            Action::ToggleThumbnails => client.kitty_overview = !client.kitty_overview,
            Action::Detach => client.detach_requested = true,
            Action::Quit => self.quit = true,
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

/// Keys that mean the same after the prefix, in the overview and with Alt.
fn common_binding(code: KeyCode) -> Option<Action> {
    let action = match code {
        KeyCode::Char('h') | KeyCode::Left => Action::FocusLeft,
        KeyCode::Char('l') | KeyCode::Right => Action::FocusRight,
        KeyCode::Char('H') => Action::MoveLeft,
        KeyCode::Char('L') => Action::MoveRight,
        KeyCode::Char('j') | KeyCode::Down => Action::FocusDown,
        KeyCode::Char('k') | KeyCode::Up => Action::FocusUp,
        KeyCode::Char('J') => Action::MoveDown,
        KeyCode::Char('K') => Action::MoveUp,
        KeyCode::Char('u') | KeyCode::PageDown => Action::FocusWorkspaceDown,
        KeyCode::Char('i') | KeyCode::PageUp => Action::FocusWorkspaceUp,
        KeyCode::Char('U') => Action::MoveColumnToWorkspaceDown,
        KeyCode::Char('I') => Action::MoveColumnToWorkspaceUp,
        KeyCode::Char(',') => Action::ConsumeIntoColumn,
        KeyCode::Char('.') => Action::ExpelFromColumn,
        KeyCode::Char('r') => Action::CycleWidth,
        KeyCode::Char('f') => Action::ToggleMaximized,
        KeyCode::Char('F') => Action::ToggleFullscreen,
        _ => return None,
    };
    Some(action)
}

fn prefix_binding(key: KeyEvent) -> Option<Action> {
    let action = match key.code {
        KeyCode::Char('n') | KeyCode::Enter => Action::NewColumn,
        KeyCode::Char('0') | KeyCode::Home => Action::FocusFirst,
        KeyCode::Char('$') | KeyCode::End => Action::FocusLast,
        KeyCode::Char('[') => Action::ConsumeOrExpelLeft,
        KeyCode::Char(']') => Action::ConsumeOrExpelRight,
        KeyCode::Char('c') => Action::Center,
        KeyCode::Char('o') => Action::ToggleOverview,
        KeyCode::Char('x') => Action::Close,
        KeyCode::Char('d') => Action::Detach,
        KeyCode::Char('q') => Action::Quit,
        code => return common_binding(code),
    };
    Some(action)
}

/// Plain keys while the overview is open.
fn overview_binding(key: KeyEvent) -> Option<Action> {
    if key
        .modifiers
        .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
    {
        return None;
    }
    let action = match key.code {
        KeyCode::Char('n') => Action::NewColumn,
        KeyCode::Char('0') | KeyCode::Home => Action::FocusFirst,
        KeyCode::Char('$') | KeyCode::End => Action::FocusLast,
        KeyCode::Char('[') => Action::ConsumeOrExpelLeft,
        KeyCode::Char(']') => Action::ConsumeOrExpelRight,
        KeyCode::Char('x') => Action::Close,
        KeyCode::Char('t') => Action::ToggleThumbnails,
        KeyCode::Char('o') | KeyCode::Enter | KeyCode::Esc => Action::ExitOverview,
        code => return common_binding(code),
    };
    Some(action)
}

/// Direct niri-like bindings on Alt. On macOS these need the terminal's
/// "Option as Meta" setting.
fn alt_binding(key: KeyEvent) -> Option<Action> {
    if !key.modifiers.contains(KeyModifiers::ALT) || key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    let action = match key.code {
        KeyCode::Enter => Action::NewColumn,
        // Alt-[ would be read as the start of an escape sequence, so
        // consume-or-expel is on Alt-{ and Alt-} instead.
        KeyCode::Char('{') => Action::ConsumeOrExpelLeft,
        KeyCode::Char('}') => Action::ConsumeOrExpelRight,
        KeyCode::Char('c') => Action::Center,
        KeyCode::Char('o') => Action::ToggleOverview,
        code => return common_binding(code),
    };
    Some(action)
}
