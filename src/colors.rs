// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The colors of a client's terminal: what it reported when it attached,
//! filled in with xterm's defaults wherever it didn't say.

use serde::{Deserialize, Serialize};

pub type Rgb = [u8; 3];

/// What a terminal said about its colors; None where it didn't answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedColors {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    /// The 16 ANSI colors.
    pub ansi: [Option<Rgb>; 16],
}

/// A complete set of colors to draw or answer with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub foreground: Rgb,
    pub background: Rgb,
    ansi: [Rgb; 16],
}

impl Default for Palette {
    fn default() -> Self {
        Self::from_reported(&ReportedColors::default())
    }
}

/// xterm's 16 ANSI colors.
const XTERM_ANSI: [Rgb; 16] = [
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
        match i {
            0..16 => self.ansi[usize::from(i)],
            16..232 => {
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                let i = i - 16;
                [level(i / 36), level(i / 6 % 6), level(i % 6)]
            }
            232.. => {
                let v = 8 + (i - 232) * 10;
                [v, v, v]
            }
        }
    }

    /// The color for one of alacritty's color indexes, as in its color
    /// queries: 0-255 the palette, 256 the foreground, 257 the background.
    pub fn by_index(&self, idx: usize) -> Rgb {
        if let Ok(i) = u8::try_from(idx) {
            self.indexed(i)
        } else if idx == 257 {
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
