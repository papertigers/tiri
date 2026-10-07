// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Asking a client's terminal, as it attaches, whether it supports kitty
//! graphics and which colors it uses.
//!
//! The queries are sent together, followed by a request for the device's
//! primary attributes, which every terminal answers. Answers come back in
//! order, so once that one arrives, anything unanswered isn't supported;
//! there's no need to wait out a timeout except on terminals that answer
//! nothing at all.

use std::io::{self, Write};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use crate::colors::{ANSI_COLORS, HEX_RADIX, ReportedColors, Rgb};
use crate::escape::{self, BEL, CSI, ESC, OSC, osc_code, report};
use crate::kitty;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use rustix::termios::{QueueSelector, tcflush};

/// Gives up on a terminal that doesn't answer even the attributes request.
/// Generous, since answering terminals end the wait as soon as they have:
/// over a slow ssh link the answers can take well over a second, and
/// any that came after giving up would be typed into the first pane.
const TIMEOUT: Duration = Duration::from_secs(5);

/// How much of the answers is read at a time.
const READ_CHUNK: usize = 4096;

/// What a client's terminal said about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalInfo {
    pub kitty_graphics: bool,
    pub colors: ReportedColors,
    /// A cell's (width, height) in pixels.
    pub cell_pixels: Option<(u16, u16)>,
    /// The whole text area's (width, height) in pixels, for working out the
    /// cell size when the terminal only reports that.
    pub area_pixels: Option<(u16, u16)>,
}

/// Queries the terminal on stdin/stdout, which must be in raw mode so the
/// answers aren't echoed or held back for a newline.
pub fn probe() -> io::Result<TerminalInfo> {
    let mut out = io::stdout().lock();
    kitty::query(&mut out)?;
    escape::query_color(&mut out, osc_code::FOREGROUND)?;
    escape::query_color(&mut out, osc_code::BACKGROUND)?;
    for i in 0..ANSI_COLORS {
        escape::query_palette(&mut out, i)?;
    }
    // Pixel sizes: the size of the terminal itself often comes without.
    out.write_all(escape::QUERY_CELL_SIZE.as_bytes())?;
    out.write_all(escape::QUERY_TEXT_AREA_SIZE.as_bytes())?;
    out.write_all(escape::QUERY_DEVICE_ATTRIBUTES.as_bytes())?;
    out.flush()?;

    let stdin = io::stdin();
    let deadline = Instant::now() + TIMEOUT;
    let mut answers = Vec::new();
    let mut buf = [0u8; READ_CHUNK];
    while !has_device_attributes(&answers) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        // Under the few-second timeout, so it always fits.
        let timeout = Timespec::try_from(left).expect("a short timeout");
        let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => break,
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
        let n = rustix::io::read(stdin.as_fd(), &mut buf)?;
        if n == 0 {
            break;
        }
        answers.extend_from_slice(&buf[..n]);
    }
    if !has_device_attributes(&answers) {
        // It gave up waiting. Answers that came in late would be read as
        // typing, so drop what has arrived by now.
        let _ = tcflush(&stdin, QueueSelector::IFlush);
    }
    Ok(parse(&answers))
}

/// Whether `answers` include the reply to the primary device attributes
/// request: `CSI ? …numbers… c`.
fn has_device_attributes(answers: &[u8]) -> bool {
    let start = report::DEVICE_ATTRIBUTES.as_bytes();
    answers.windows(start.len()).enumerate().any(|(i, w)| {
        w == start
            && answers[i + start.len()..]
                .iter()
                .find(|b| !(b.is_ascii_digit() || **b == b';'))
                == Some(&report::DEVICE_ATTRIBUTES_FINAL)
    })
}

/// Reads the terminal's answers.
pub fn parse(answers: &[u8]) -> TerminalInfo {
    let text = String::from_utf8_lossy(answers);
    let mut info = TerminalInfo {
        kitty_graphics: kitty::supported(&text),
        ..TerminalInfo::default()
    };
    // Window reports: CSI kind ; height ; width t.
    for report in text.split(CSI).skip(1) {
        let Some(end) =
            report.find(|c: char| !(c.is_ascii_digit() || c == ';'))
        else {
            continue;
        };
        if !report[end..].starts_with(report::WINDOW_FINAL) {
            continue;
        }
        let numbers: Vec<u16> =
            report[..end].split(';').filter_map(|n| n.parse().ok()).collect();
        let [kind, h, w] = numbers[..] else {
            continue;
        };
        if h == 0 || w == 0 {
            continue;
        }
        match kind {
            report::CELL_SIZE => info.cell_pixels = Some((w, h)),
            report::TEXT_AREA_SIZE => info.area_pixels = Some((w, h)),
            _ => {}
        }
    }
    // Color answers: OSC code ; rgb:… and OSC 4 ; index ; rgb:…, each ended
    // by BEL or ST.
    for osc in text.split(OSC).skip(1) {
        let end = osc.find([BEL, char::from(ESC)]).unwrap_or(osc.len());
        let parts: Vec<&str> = osc[..end].split(';').collect();
        let Some(code) = parts.first().and_then(|c| c.parse::<u16>().ok())
        else {
            continue;
        };
        match (code, &parts[1..]) {
            (osc_code::FOREGROUND, [color]) => {
                info.colors.foreground = parse_rgb(color);
            }
            (osc_code::BACKGROUND, [color]) => {
                info.colors.background = parse_rgb(color);
            }
            (osc_code::PALETTE, [index, color]) => {
                if let Ok(i) = index.parse::<usize>()
                    && i < ANSI_COLORS
                {
                    info.colors.ansi[i] = parse_rgb(color);
                }
            }
            _ => {}
        }
    }
    info
}

/// The most hex digits a color report gives a channel.
const MAX_CHANNEL_DIGITS: usize = 4;
const BITS_PER_HEX_DIGIT: u32 = 4;

/// Parses `rgb:RRRR/GGGG/BBBB`, where each part has one to four hex digits.
fn parse_rgb(spec: &str) -> Option<Rgb> {
    let mut channels = spec.strip_prefix(report::RGB_PREFIX)?.split('/');
    let mut rgb = Rgb::default();
    for channel in &mut rgb {
        let hex = channels.next()?;
        if hex.is_empty() || hex.len() > MAX_CHANNEL_DIGITS {
            return None;
        }
        let value = u32::from_str_radix(hex, HEX_RADIX).ok()?;
        let digits = u32::try_from(hex.len()).ok()?;
        let max = (1u32 << (BITS_PER_HEX_DIGIT * digits)) - 1;
        *channel = ((value * u32::from(u8::MAX) + max / 2) / max) as u8;
    }
    channels.next().is_none().then_some(rgb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_full_set_of_answers() {
        let answers = b"\x1b_Gi=31;OK\x1b\\\
            \x1b]10;rgb:d8d8/d8d8/d8d8\x1b\\\
            \x1b]11;rgb:0d0d/0f0f/1414\x07\
            \x1b]4;1;rgb:ff/55/55\x1b\\\
            \x1b[?62;22;52c";
        let info = parse(answers);
        assert!(info.kitty_graphics);
        assert_eq!(info.colors.foreground, Some([0xd8, 0xd8, 0xd8]));
        assert_eq!(info.colors.background, Some([0x0d, 0x0f, 0x14]));
        assert_eq!(info.colors.ansi[1], Some([0xff, 0x55, 0x55]));
        assert_eq!(info.colors.ansi[2], None);
        assert!(has_device_attributes(answers));
    }

    #[test]
    fn a_terminal_that_only_knows_attributes() {
        let answers = b"\x1b[?1;2c";
        assert_eq!(parse(answers), TerminalInfo::default());
        assert!(has_device_attributes(answers));
        assert!(!has_device_attributes(b"\x1b[?1;2"));
    }

    #[test]
    fn kitty_errors_mean_no_graphics() {
        let info = parse(b"\x1b_Gi=31;ENOTSUPPORTED:no\x1b\\\x1b[?62c");
        assert!(!info.kitty_graphics);
    }

    #[test]
    fn color_specs_scale_to_eight_bits() {
        assert_eq!(parse_rgb("rgb:f/8/0"), Some([0xff, 0x88, 0x00]));
        assert_eq!(parse_rgb("rgb:ffff/0000/8080"), Some([0xff, 0x00, 0x80]));
        assert_eq!(parse_rgb("rgb:12/34"), None);
        assert_eq!(parse_rgb("#123456"), None);
    }

    #[test]
    fn reads_size_reports() {
        let info = parse(b"\x1b[6;34;16t\x1b[4;3026;5952t\x1b[?62c");
        assert_eq!(info.cell_pixels, Some((16, 34)));
        assert_eq!(info.area_pixels, Some((5952, 3026)));
        // Not confused by the attributes reply or color answers.
        let info = parse(b"\x1b]11;rgb:00/00/00\x07\x1b[?62;22c");
        assert_eq!((info.cell_pixels, info.area_pixels), (None, None));
    }
}
