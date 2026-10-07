// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The colors of a client's terminal: what it reported when it attached,
//! filled in with xterm's defaults wherever it didn't say.

use alacritty_terminal::vte::ansi::NamedColor;
use serde::{Deserialize, Serialize};

pub type Rgb = [u8; 3];

/// The basic colors at the start of the palette, which terminals report.
pub const ANSI_COLORS: usize = 16;

/// Hex digits, as colors are written.
pub const HEX_RADIX: u32 = 16;

// The rest of the 256-color palette: a 6x6x6 color cube, then a grey ramp.
const CUBE_START: u8 = ANSI_COLORS as u8;
const CUBE_SIDE: u8 = 6;
const GREY_START: u8 = CUBE_START + CUBE_SIDE * CUBE_SIDE * CUBE_SIDE;
/// Each cube channel's levels, as xterm has them.
const CUBE_LEVELS: [u8; CUBE_SIDE as usize] = [0, 95, 135, 175, 215, 255];
const GREY_FIRST: u8 = 8;
const GREY_STEP: u8 = 10;

/// What a terminal said about its colors; None where it didn't answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedColors {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    /// The basic colors.
    pub ansi: [Option<Rgb>; ANSI_COLORS],
}

/// A complete set of colors to draw or answer with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub foreground: Rgb,
    pub background: Rgb,
    ansi: [Rgb; ANSI_COLORS],
}

impl Default for Palette {
    fn default() -> Self {
        Self::from_reported(&ReportedColors::default())
    }
}

/// xterm's 16 ANSI colors.
const XTERM_ANSI: [Rgb; ANSI_COLORS] = [
    [0x00, 0x00, 0x00],
    [0xcd, 0x00, 0x00],
    [0x00, 0xcd, 0x00],
    [0xcd, 0xcd, 0x00],
    [0x00, 0x00, 0xee],
    [0xcd, 0x00, 0xcd],
    [0x00, 0xcd, 0xcd],
    [0xe5, 0xe5, 0xe5],
    [0x7f, 0x7f, 0x7f],
    [0xff, 0x00, 0x00],
    [0x00, 0xff, 0x00],
    [0xff, 0xff, 0x00],
    [0x5c, 0x5c, 0xff],
    [0xff, 0x00, 0xff],
    [0x00, 0xff, 0xff],
    [0xff, 0xff, 0xff],
];

impl Palette {
    /// Fills in what a terminal didn't report: xterm's ANSI colors, and
    /// light text on black.
    pub fn from_reported(reported: &ReportedColors) -> Self {
        Self {
            foreground: reported.foreground.unwrap_or([0xd8, 0xd8, 0xd8]),
            background: reported.background.unwrap_or([0x00, 0x00, 0x00]),
            ansi: std::array::from_fn(|i| {
                reported.ansi[i].unwrap_or(XTERM_ANSI[i])
            }),
        }
    }

    /// The color for 256-color index `i`: the ANSI colors, then xterm's
    /// 6x6x6 cube and grey ramp.
    pub fn indexed(&self, i: u8) -> Rgb {
        if i < CUBE_START {
            self.ansi[usize::from(i)]
        } else if i < GREY_START {
            let i = i - CUBE_START;
            let level = |v: u8| CUBE_LEVELS[usize::from(v)];
            [
                level(i / (CUBE_SIDE * CUBE_SIDE)),
                level(i / CUBE_SIDE % CUBE_SIDE),
                level(i % CUBE_SIDE),
            ]
        } else {
            let v = GREY_FIRST + (i - GREY_START) * GREY_STEP;
            [v, v, v]
        }
    }

    /// The color for one of alacritty's color indexes, as in its color
    /// queries: the palette, then its named colors.
    pub fn by_index(&self, idx: usize) -> Rgb {
        if let Ok(i) = u8::try_from(idx) {
            self.indexed(i)
        } else if idx == NamedColor::Background as usize {
            self.background
        } else {
            self.foreground
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reported_colors_win_over_defaults() {
        let mut reported = ReportedColors {
            background: Some([1, 2, 3]),
            ..ReportedColors::default()
        };
        reported.ansi[1] = Some([9, 9, 9]);
        let palette = Palette::from_reported(&reported);
        assert_eq!(palette.background, [1, 2, 3]);
        assert_eq!(palette.foreground, [0xd8, 0xd8, 0xd8]);
        assert_eq!(palette.indexed(1), [9, 9, 9]);
        assert_eq!(palette.indexed(2), XTERM_ANSI[2]);
    }

    #[test]
    fn xterm_palette_spot_checks() {
        let palette = Palette::default();
        assert_eq!(palette.indexed(16), [0, 0, 0]);
        assert_eq!(palette.indexed(196), [0xff, 0, 0]);
        assert_eq!(palette.indexed(231), [0xff, 0xff, 0xff]);
        assert_eq!(palette.indexed(255), [0xee, 0xee, 0xee]);
    }

    #[test]
    fn alacritty_indexes_cover_foreground_and_background() {
        let palette = Palette::from_reported(&ReportedColors {
            foreground: Some([7, 7, 7]),
            background: Some([1, 1, 1]),
            ..ReportedColors::default()
        });
        assert_eq!(palette.by_index(256), [7, 7, 7]);
        assert_eq!(palette.by_index(257), [1, 1, 1]);
        assert_eq!(palette.by_index(196), [0xff, 0, 0]);
    }
}
