//! The status bar: workspaces and columns on the left, key hints on the
//! right.

use crate::layout::Visibility;
use crate::render::{Frame, Style};

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
        let theme = &client.theme;
        let base = Style {
            bg: theme.status_bg,
            ..Style::fg(theme.status_fg)
        };
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
                Style {
                    fg: theme.dim,
                    ..base
                }
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
                Style {
                    fg: theme.focused_border,
                    bold: true,
                    ..base
                }
            } else {
                match strip.visibility(idx) {
                    Visibility::Full => Style { bold: true, ..base },
                    Visibility::Partial => base,
                    Visibility::Hidden => Style {
                        fg: theme.dim,
                        ..base
                    },
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
        let theme = &client.theme;
        let base = Style {
            bg: theme.status_bg,
            ..Style::fg(theme.status_fg)
        };
        frame.put_str(0, y, &" ".repeat(usize::from(client.width)), base);
        let mut x = 0;
        for segment in self.status_segments(client) {
            frame.put_str(x, y, &segment.text, segment.style);
            x += segment.text.chars().count() as i32;
        }

        let overview = self.workspaces.in_overview(client.id);
        let hints: &[&str] = if client.prefix_pending {
            &[
                "C-a:",
                "n new",
                "hjkl focus",
                "HJKL move",
                "u/i workspace",
                "o overview",
                "x close",
                "d detach",
                "r width",
                "f max",
                "F full",
                "c center",
                "U/I to workspace",
                "[/] consume/expel",
                ",/. into/out of column",
                "0/$ first/last",
                "q kill server",
            ]
        } else if overview {
            &[
                "OVERVIEW",
                "⏎/o/Esc open",
                "hjkl select",
                "u/i workspace",
                "HJKL/U/I move",
                "x close",
                if client.kitty_overview {
                    "t text"
                } else {
                    "t thumbnails"
                },
            ]
        } else {
            &[
                "C-a n or Alt-⏎ new",
                "Alt-h/l focus",
                "Alt-o overview",
                "C-a d detach",
                "C-a for more",
            ]
        };
        let room = (i32::from(client.width) - x - 2).max(0) as usize;
        let hint = fit_hints(hints, room);
        let hint_x = i32::from(client.width) - hint.chars().count() as i32;
        if !hint.is_empty() {
            frame.put_str(hint_x, y, &hint, base);
        }
    }
}

/// As many of `hints` as fit in `room` columns, from the first, two spaces
/// apart and with one after. Empty if not even the first fits.
fn fit_hints(hints: &[&str], room: usize) -> String {
    let mut out = String::new();
    for hint in hints {
        let sep = if out.is_empty() { "" } else { "  " };
        if out.chars().count() + sep.len() + hint.chars().count() + 1 > room {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_drop_from_the_end_to_fit() {
        let hints = ["C-a:", "n new", "x close"];
        assert_eq!(fit_hints(&hints, 100), "C-a:  n new  x close ");
        assert_eq!(fit_hints(&hints, 13), "C-a:  n new ");
        assert_eq!(fit_hints(&hints, 4), "");
    }
}
