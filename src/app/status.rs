// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The status bar: workspaces and columns on the left, key hints on the
//! right.

use crate::layout::Visibility;
use crate::render::{Frame, Style, fit_width, text_width};

use crate::keys::Action::{self, *};
use crate::keys::{Bindings, Key, Table, hint_keys};

use super::{App, Client};

/// A piece of the status bar.
pub(super) struct Segment {
    pub(super) text: String,
    style: Style,
    /// What clicking it does.
    pub(super) target: Option<StatusTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StatusTarget {
    Workspace(usize),
    Column(usize),
}

impl App {
    /// The status bar's left side, piece by piece: the workspaces, then
    /// markers for the active workspace's columns. Shared by drawing and
    /// by working out what a click on the bar hit.
    pub(super) fn status_segments(&self, client: &Client) -> Vec<Segment> {
        let theme = &self.config.theme;
        let base = Style { bg: theme.status_bg, ..Style::fg(theme.status_fg) };
        let mut segments = vec![Segment {
            text: " tiri ".to_owned(),
            style: Style { bold: true, ..base },
            target: None,
        }];

        // The workspaces, top to bottom, ending with "+" for the empty one.
        let active_ws = self.workspaces.active_index(client.id);
        for ws in 0..self.workspaces.list().len() {
            let style = if ws == active_ws {
                Style {
                    bg: theme.status_active_bg,
                    fg: theme.status_active_fg,
                    bold: true,
                    ..base
                }
            } else if self.is_new_workspace(ws) {
                Style { fg: theme.dim, ..base }
            } else {
                base
            };
            segments.push(Segment {
                text: format!(" {} ", self.workspace_label(ws)),
                style,
                target: Some(StatusTarget::Workspace(ws)),
            });
        }
        segments.push(Segment {
            text: " │ ".to_owned(),
            style: base,
            target: None,
        });

        // A minimap of the active workspace's columns: the focused one
        // filled, the rest hollow, dimmed when scrolled out of view.
        let strip = self.workspaces.active(client.id);
        for idx in 0..strip.columns().len() {
            let focused = idx == strip.focus_index();
            let style = if focused {
                Style { fg: theme.focused_border, bold: true, ..base }
            } else {
                match strip.visibility(idx) {
                    Visibility::Full => Style { bold: true, ..base },
                    Visibility::Partial => base,
                    Visibility::Hidden => Style { fg: theme.dim, ..base },
                }
            };
            segments.push(Segment {
                text: if focused { "■ " } else { "□ " }.to_owned(),
                style,
                target: Some(StatusTarget::Column(idx)),
            });
        }
        segments
    }

    pub(super) fn draw_status(&self, client: &Client, frame: &mut Frame) {
        let y = i32::from(client.height) - 1;
        let theme = &self.config.theme;
        let base = Style { bg: theme.status_bg, ..Style::fg(theme.status_fg) };
        frame.put_str(0, y, &" ".repeat(usize::from(client.width)), base);
        let mut x = 0;
        for segment in self.status_segments(client) {
            frame.put_str(x, y, &segment.text, segment.style);
            x += text_width(&segment.text) as i32;
        }

        let room = (i32::from(client.width) - x - 2).max(0) as usize;
        if let Some(notice) = &client.notice {
            // In the colors of the active workspace's number, to stand out.
            let style = Style {
                fg: theme.status_active_fg,
                bg: theme.status_active_bg,
                bold: true,
                ..base
            };
            let text = format!(
                " {} ",
                fit_width(&notice.text, room.saturating_sub(2))
            );
            let notice_x = i32::from(client.width) - text_width(&text) as i32;
            frame.put_str(notice_x, y, &text, style);
            return;
        }

        let overview = self.workspaces.in_overview(client.id);
        let hints = if client.prefix_pending {
            prefix_hints(&self.config.bindings)
        } else if overview {
            overview_hints(&self.config.bindings, client.kitty_overview)
        } else {
            normal_hints(&self.config.bindings)
        };
        let hint = fit_hints(&hints, room);
        let hint_x = i32::from(client.width) - text_width(&hint) as i32;
        if !hint.is_empty() {
            frame.put_str(hint_x, y, &hint, base);
        }
    }
}

/// As many of `hints` as fit in `room` columns, from the first, two spaces
/// apart and with one after. Empty if not even the first fits.
fn fit_hints(hints: &[String], room: usize) -> String {
    let mut out = String::new();
    for hint in hints {
        let sep = if out.is_empty() { "" } else { "  " };
        if text_width(&out) + sep.len() + text_width(hint) + 1 > room {
            break;
        }
        out.push_str(sep);
        out.push_str(hint);
    }
    if !out.is_empty() {
        out.push(' ');
    }
    out
}

/// Hints after the prefix, most useful first, from what's bound.
fn prefix_hints(bindings: &Bindings) -> Vec<String> {
    let table = &bindings.prefix_binds;
    let mut hints = vec![format!("{}:", bindings.prefix.short())];
    hints.extend(labelled(
        table,
        &[
            ("new", &[NewColumn]),
            ("focus", FOCUS),
            ("move", MOVE),
            ("workspace", &[FocusWorkspaceDown, FocusWorkspaceUp]),
            ("overview", &[ToggleOverview]),
            ("close", &[ClosePane]),
            ("detach", &[Detach]),
            ("width", &[SwitchPresetColumnWidth]),
            ("height", &[SwitchPresetPaneHeight]),
            ("max", &[MaximizeColumn]),
            ("full", &[FullscreenPane]),
            ("center", &[CenterColumn]),
            ("to workspace", MOVE_TO_WORKSPACE),
            (
                "consume/expel",
                &[ConsumeOrExpelPaneLeft, ConsumeOrExpelPaneRight],
            ),
            (
                "into/out of column",
                &[ConsumePaneIntoColumn, ExpelPaneFromColumn],
            ),
            ("first/last", &[FocusColumnFirst, FocusColumnLast]),
            ("kill server", &[KillServer]),
        ],
    ));
    hints
}

/// Hints in the overview.
fn overview_hints(bindings: &Bindings, kitty_overview: bool) -> Vec<String> {
    let table = &bindings.overview_binds;
    let mut hints = vec!["OVERVIEW".to_owned()];
    let open: Vec<Key> = table.keys_for(CloseOverview).collect();
    if !open.is_empty() {
        hints.push(format!("{} open", hint_keys(&open)));
    }
    hints.extend(labelled(
        table,
        &[
            ("select", FOCUS),
            ("workspace", &[FocusWorkspaceDown, FocusWorkspaceUp]),
        ],
    ));
    // Moving within the strip and between workspaces, as one hint.
    let moves: Vec<String> = [MOVE, MOVE_TO_WORKSPACE]
        .iter()
        .filter_map(|actions| table.hint_keys(actions))
        .collect();
    if !moves.is_empty() {
        hints.push(format!("{} move", moves.join("/")));
    }
    let thumbnails = if kitty_overview { "text" } else { "thumbnails" };
    hints.extend(labelled(
        table,
        &[("close", &[ClosePane]), (thumbnails, &[ToggleThumbnails])],
    ));
    hints
}

/// Hints with neither the prefix pressed nor the overview open: keys that
/// act straight away where there are any, the prefix's otherwise.
fn normal_hints(bindings: &Bindings) -> Vec<String> {
    let prefixed = |actions: &[Action]| {
        (bindings.prefix_binds.hint_keys(actions))
            .map(|keys| format!("{} {keys}", bindings.prefix.short()))
    };
    let direct = |actions: &[Action]| bindings.binds.hint_keys(actions);
    let mut hints = Vec::new();
    // Both ways to open a column, since it's the first thing to do.
    let new: Vec<String> = [prefixed(&[NewColumn]), direct(&[NewColumn])]
        .into_iter()
        .flatten()
        .collect();
    if !new.is_empty() {
        hints.push(format!("{} new", new.join(" or ")));
    }
    for (label, actions) in [
        ("focus", &[FocusColumnLeft, FocusColumnRight][..]),
        ("overview", &[ToggleOverview]),
        ("detach", &[Detach]),
    ] {
        if let Some(keys) = direct(actions).or_else(|| prefixed(actions)) {
            hints.push(format!("{keys} {label}"));
        }
    }
    hints.push(format!("{} for more", bindings.prefix.short()));
    hints
}

/// "keys label" for each of `items` whose actions all have keys in `table`.
fn labelled(table: &Table, items: &[(&str, &[Action])]) -> Vec<String> {
    (items.iter())
        .filter_map(|(label, actions)| {
            Some(format!("{} {label}", table.hint_keys(actions)?))
        })
        .collect()
}

/// Left, down, up, right: hjkl.
const FOCUS: &[Action] =
    &[FocusColumnLeft, FocusPaneDown, FocusPaneUp, FocusColumnRight];
const MOVE: &[Action] =
    &[MoveColumnLeft, MovePaneDown, MovePaneUp, MoveColumnRight];
const MOVE_TO_WORKSPACE: &[Action] =
    &[MoveColumnToWorkspaceDown, MoveColumnToWorkspaceUp];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn default_hints() {
        let bindings = Config::default().bindings;
        assert_eq!(
            normal_hints(&bindings).join("  "),
            "C-a n or Alt-⏎ new  Alt-h/l focus  Alt-o overview  C-a d detach  C-a for more"
        );
        assert_eq!(
            prefix_hints(&bindings).join("  "),
            "C-a:  n new  hjkl focus  HJKL move  u/i workspace  o overview  x close  \
             d detach  r width  R height  f max  F full  c center  U/I to workspace  \
             [/] consume/expel  ,/. into/out of column  0/$ first/last  q kill server"
        );
        assert_eq!(
            overview_hints(&bindings, false).join("  "),
            "OVERVIEW  ⏎/o/Esc open  hjkl select  u/i workspace  HJKL/U/I move  \
             x close  t thumbnails"
        );
    }

    #[test]
    fn hints_follow_the_bindings() {
        let bindings = Config::parse(
            "config.kdl",
            r#"
            prefix "Ctrl+b"
            prefix-binds { d { unbind; }; Shift+d { detach; }; }
            binds { Alt+o { unbind; }; }
            "#,
        )
        .unwrap()
        .bindings;
        let hints = normal_hints(&bindings).join("  ");
        assert!(hints.contains("C-b D detach"), "{hints}");
        // No Alt key for the overview any more, so the prefix's.
        assert!(hints.contains("C-b o overview"), "{hints}");
        assert!(hints.ends_with("C-b for more"), "{hints}");
    }

    #[test]
    fn hints_drop_from_the_end_to_fit() {
        let hints = ["C-a:", "n new", "x close"].map(String::from);
        assert_eq!(fit_hints(&hints, 100), "C-a:  n new  x close ");
        assert_eq!(fit_hints(&hints, 13), "C-a:  n new ");
        assert_eq!(fit_hints(&hints, 4), "");
    }
}
