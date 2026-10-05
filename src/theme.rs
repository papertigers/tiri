//! tiri's own colors: pane borders, the status bar, labels and hints, and
//! the selection. Pane contents keep their programs' and the terminal's
//! colors whatever the theme.
//!
//! The config file picks one, or defines its own on top of one.

use crate::render::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub focused_border: Color,
    pub unfocused_border: Color,
    pub status_fg: Color,
    pub status_bg: Color,
    /// The active workspace's number in the status bar.
    pub status_active_fg: Color,
    pub status_active_bg: Color,
    /// Hints, empty workspaces, and columns scrolled out of view.
    pub dim: Color,
    /// Behind selected text; None shows the selection reversed instead.
    pub selection_bg: Option<Color>,
}

impl Theme {
    /// The themes built in, by name.
    pub const ALL: [(&str, Theme); 2] = [("default", DEFAULT), ("oxide", OXIDE)];

    pub fn named(name: &str) -> Option<Theme> {
        Self::ALL.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
    }
}

impl Default for Theme {
    fn default() -> Self {
        DEFAULT
    }
}

/// From the terminal's 256-color palette, so it follows the terminal.
const DEFAULT: Theme = Theme {
    focused_border: Color::Idx(12),
    unfocused_border: Color::Idx(8),
    status_fg: Color::Idx(250),
    status_bg: Color::Idx(236),
    status_active_fg: Color::Idx(236),
    status_active_bg: Color::Idx(250),
    dim: Color::Idx(242),
    selection_bg: None,
};

/// The oxide Helix theme's interface colors.
const OXIDE: Theme = Theme {
    focused_border: rgb(0x00d992),     // oxide_green
    unfocused_border: rgb(0x363b54),   // foreground_gutter
    status_fg: rgb(0xdadada),          // foreground
    status_bg: rgb(0x16161e),          // background_menu
    status_active_fg: rgb(0x414868),   // black, as ui.statusline.normal
    status_active_bg: rgb(0x7aa2f7),   // blue
    dim: rgb(0x565f89),                // comment
    selection_bg: Some(rgb(0x30374b)), // background_highlight
};

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_themes_up_by_name() {
        assert_eq!(Theme::named("default"), Some(Theme::default()));
        assert_eq!(
            Theme::named("oxide").map(|t| t.focused_border),
            Some(Color::Rgb(0x00, 0xd9, 0x92))
        );
        assert_eq!(Theme::named("nope"), None);
    }
}
