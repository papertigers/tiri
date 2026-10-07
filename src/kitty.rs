// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The kitty graphics protocol, as far as the overview needs it: uploading
//! images, and showing them through Unicode placeholders so they live in
//! ordinary text cells that scroll and clip like everything else.
//!
//! Spec: <https://sw.kovidgoyal.net/kitty/graphics-protocol/>

use std::io::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::escape::{ST, apc};
use crate::render::{Color, Frame};
use crate::thumbnail::Image;

/// What starts every graphics command: its keys, then `;`, any payload, and
/// [`ST`].
const COMMAND: &str = apc!("G");
/// q=2: the terminal answers nothing, errors included. Answers would arrive
/// as input, typed into the pane.
const QUIET: u8 = 2;
/// f=: images are sent as 32-bit RGBA pixels, or 24-bit RGB.
const RGBA: u8 = 32;
const RGB: u8 = 24;
/// U=1: a virtual placement, shown through [`PLACEHOLDER`] cells.
const VIRTUAL: u8 = 1;

/// The image id of the query asking whether a terminal supports graphics.
const QUERY_ID: u32 = 31;
/// The query's image: one black pixel, in base64.
const QUERY_PIXEL: &str = "AAAA";

/// A cell showing part of an image. The image id is carried in the cell's
/// foreground color, and which part of the image in two combining marks.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// Payloads are sent in chunks of at most this many base64 bytes.
const CHUNK: usize = 4096;

/// Combining marks encoding row and column numbers in a placeholder, from
/// kitty's `gen/rowcolumn-diacritics.txt`. Their count limits the size of a
/// placement in cells.
const DIACRITICS: [char; 297] = [
    '\u{0305}',
    '\u{030D}',
    '\u{030E}',
    '\u{0310}',
    '\u{0312}',
    '\u{033D}',
    '\u{033E}',
    '\u{033F}',
    '\u{0346}',
    '\u{034A}',
    '\u{034B}',
    '\u{034C}',
    '\u{0350}',
    '\u{0351}',
    '\u{0352}',
    '\u{0357}',
    '\u{035B}',
    '\u{0363}',
    '\u{0364}',
    '\u{0365}',
    '\u{0366}',
    '\u{0367}',
    '\u{0368}',
    '\u{0369}',
    '\u{036A}',
    '\u{036B}',
    '\u{036C}',
    '\u{036D}',
    '\u{036E}',
    '\u{036F}',
    '\u{0483}',
    '\u{0484}',
    '\u{0485}',
    '\u{0486}',
    '\u{0487}',
    '\u{0592}',
    '\u{0593}',
    '\u{0594}',
    '\u{0595}',
    '\u{0597}',
    '\u{0598}',
    '\u{0599}',
    '\u{059C}',
    '\u{059D}',
    '\u{059E}',
    '\u{059F}',
    '\u{05A0}',
    '\u{05A1}',
    '\u{05A8}',
    '\u{05A9}',
    '\u{05AB}',
    '\u{05AC}',
    '\u{05AF}',
    '\u{05C4}',
    '\u{0610}',
    '\u{0611}',
    '\u{0612}',
    '\u{0613}',
    '\u{0614}',
    '\u{0615}',
    '\u{0616}',
    '\u{0617}',
    '\u{0657}',
    '\u{0658}',
    '\u{0659}',
    '\u{065A}',
    '\u{065B}',
    '\u{065D}',
    '\u{065E}',
    '\u{06D6}',
    '\u{06D7}',
    '\u{06D8}',
    '\u{06D9}',
    '\u{06DA}',
    '\u{06DB}',
    '\u{06DC}',
    '\u{06DF}',
    '\u{06E0}',
    '\u{06E1}',
    '\u{06E2}',
    '\u{06E4}',
    '\u{06E7}',
    '\u{06E8}',
    '\u{06EB}',
    '\u{06EC}',
    '\u{0730}',
    '\u{0732}',
    '\u{0733}',
    '\u{0735}',
    '\u{0736}',
    '\u{073A}',
    '\u{073D}',
    '\u{073F}',
    '\u{0740}',
    '\u{0741}',
    '\u{0743}',
    '\u{0745}',
    '\u{0747}',
    '\u{0749}',
    '\u{074A}',
    '\u{07EB}',
    '\u{07EC}',
    '\u{07ED}',
    '\u{07EE}',
    '\u{07EF}',
    '\u{07F0}',
    '\u{07F1}',
    '\u{07F3}',
    '\u{0816}',
    '\u{0817}',
    '\u{0818}',
    '\u{0819}',
    '\u{081B}',
    '\u{081C}',
    '\u{081D}',
    '\u{081E}',
    '\u{081F}',
    '\u{0820}',
    '\u{0821}',
    '\u{0822}',
    '\u{0823}',
    '\u{0825}',
    '\u{0826}',
    '\u{0827}',
    '\u{0829}',
    '\u{082A}',
    '\u{082B}',
    '\u{082C}',
    '\u{082D}',
    '\u{0951}',
    '\u{0953}',
    '\u{0954}',
    '\u{0F82}',
    '\u{0F83}',
    '\u{0F86}',
    '\u{0F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

pub const MAX_CELLS: u16 = DIACRITICS.len() as u16;

/// The text for the placeholder cell showing `(row, col)` of a placement.
pub fn placeholder(row: u16, col: u16) -> String {
    [PLACEHOLDER, DIACRITICS[usize::from(row)], DIACRITICS[usize::from(col)]]
        .into_iter()
        .collect()
}

/// The (row, col) a placeholder cell's marks name, if it is one with both.
fn placeholder_position(sym: &str) -> Option<(usize, usize)> {
    let mut chars = sym.chars();
    if chars.next() != Some(PLACEHOLDER) {
        return None;
    }
    let mut index = || {
        let mark = chars.next()?;
        DIACRITICS.iter().position(|&d| d == mark)
    };
    Some((index()?, index()?))
}

/// Leaves the marks off placeholders that carry on a run: the protocol
/// lets a placeholder without them continue the one to its left, at the
/// next column. That halves what an overview of thumbnails costs to send.
///
/// Only a cell whose left neighbor in the finished frame is the same
/// placement's previous column goes bare, since that's what the terminal
/// reads it from. One after a border, a label drawn over the image, or the
/// screen's edge keeps its marks.
pub fn compact_placeholders(frame: &mut Frame) {
    let bare = PLACEHOLDER.to_string();
    for y in 0..frame.height() {
        // The placement and position of the cell just left, if it's one.
        let mut left = None;
        for x in 0..frame.width() {
            let (sym, wide, style) = frame.content(x, y);
            let here = (!wide)
                .then(|| placeholder_position(sym))
                .flatten()
                .map(|(row, col)| (style, row, col));
            if let Some((style, row, col)) = here
                && left == Some((style, row, col.wrapping_sub(1)))
            {
                frame.put(i32::from(x), i32::from(y), &bare, style);
            }
            left = here;
        }
    }
}

/// The foreground color that makes a placeholder cell refer to image `id`.
pub fn id_color(id: u32) -> Color {
    let [_, r, g, b] = id.to_be_bytes();
    Color::Rgb(r, g, b)
}

/// zlib's compression levels run from 1, fastest, to 9, smallest. Past 6,
/// thumbnails shrink little for the extra time.
const FAST_LEVEL: u8 = 1;
const SMALL_LEVEL: u8 = 6;

/// How hard [`compress`] works at an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// For an image that's soon replaced, as in a fade: an animation
    /// can't wait for anything slower.
    Fast,
    /// For an image that stays: about a third the size [`Self::Fast`]
    /// makes, for a few milliseconds more, which a slow link repays.
    Small,
}

/// An image compressed for [`transmit_compressed`].
pub struct Compressed {
    width: usize,
    height: usize,
    zlib: Vec<u8>,
}

/// Compresses `image` for uploading. The slow part of an upload, and
/// separate from writing it out so that several can run at once.
pub fn compress(image: &Image, compression: Compression) -> Compressed {
    let level = match compression {
        Compression::Fast => FAST_LEVEL,
        Compression::Small => SMALL_LEVEL,
    };
    Compressed {
        width: image.width,
        height: image.height,
        zlib: miniz_oxide::deflate::compress_to_vec_zlib(&image.rgba, level),
    }
}

/// Uploads `image` as image `id`, replacing any earlier one.
#[cfg(test)]
pub fn transmit(
    out: &mut Vec<u8>,
    id: u32,
    image: &Image,
    compression: Compression,
) {
    transmit_compressed(out, id, &compress(image, compression));
}

/// Uploads an image [`compress`]ed already as image `id`, replacing any
/// earlier one.
pub fn transmit_compressed(out: &mut Vec<u8>, id: u32, image: &Compressed) {
    let payload = BASE64.encode(&image.zlib);
    let chunks = payload.as_bytes().chunks(CHUNK);
    let last = chunks.len().saturating_sub(1);
    for (n, chunk) in chunks.enumerate() {
        out.extend_from_slice(COMMAND.as_bytes());
        if n == 0 {
            let (width, height) = (image.width, image.height);
            write!(out, "a=t,f={RGBA},o=z,s={width},v={height},i={id},")
                .expect("writing to memory can't fail");
        }
        // Quiet on every chunk, since terminals differ on which chunk's
        // they go by.
        let more = u8::from(n < last);
        write!(out, "q={QUIET},m={more};")
            .expect("writing to memory can't fail");
        out.extend_from_slice(chunk);
        out.extend_from_slice(ST.as_bytes());
    }
}

/// Shows image `id` through placeholders `cols` x `rows` cells big, by
/// creating or resizing its virtual placement. The terminal scales the
/// image to fit.
///
/// The placement always gets the same id, so it replaces the previous one.
/// Re-uploading an image keeps its placements, and an unnumbered placement
/// is added alongside them; placeholders then use whichever the terminal
/// finds first, which after a resize is often one with the old size.
///
/// That id is the image's own. Placement ids belong to their image, but
/// iTerm2 treats them as global, so images sharing one would take each
/// other's placement there. Placeholders name it in their underline color.
pub fn place(out: &mut Vec<u8>, id: u32, cols: u16, rows: u16) {
    write!(
        out,
        "{COMMAND}a=p,U={VIRTUAL},i={id},p={id},c={cols},r={rows},q={QUIET}{ST}"
    )
    .expect("writing to memory can't fail");
}

/// Frees image `id` and its placements.
pub fn delete(out: &mut Vec<u8>, id: u32) {
    write!(out, "{COMMAND}a=d,d=I,i={id},q={QUIET}{ST}")
        .expect("writing to memory can't fail");
}

/// Asks whether the terminal supports graphics, with a one-pixel image it
/// checks but doesn't keep. One that does answers as [`supported`] spots.
pub fn query(out: &mut impl std::io::Write) -> std::io::Result<()> {
    write!(
        out,
        "{COMMAND}i={QUERY_ID},s=1,v=1,a=q,t=d,f={RGB};{QUERY_PIXEL}{ST}"
    )
}

/// Whether a terminal's `answers` say it supports graphics: an OK to the
/// [`query`].
pub fn supported(answers: &str) -> bool {
    answers.contains(&format!("{COMMAND}i={QUERY_ID};OK"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_encodes_row_then_column() {
        assert_eq!(placeholder(0, 2), "\u{10EEEE}\u{0305}\u{030E}");
        assert_eq!(placeholder(296, 0).chars().nth(1), Some('\u{1D244}'));
    }

    #[test]
    fn placeholders_after_the_first_in_a_run_go_bare() {
        use crate::render::Style;
        let image = Style { fg: id_color(7), ..Style::default() };
        let other = Style { fg: id_color(8), ..Style::default() };
        let mut frame = Frame::new(8, 2);
        // A border, then columns 0 to 4 of row 0 of image 7.
        frame.put(0, 0, "│", Style::default());
        for col in 0..5 {
            frame.put(i32::from(col) + 1, 0, &placeholder(0, col), image);
        }
        // Then a label over column 3, and image 8 right after.
        frame.put(4, 0, "x", Style::default());
        frame.put(6, 0, &placeholder(0, 1), other);
        // Row 1 starts partway into the image, as if scrolled.
        for col in 0..3 {
            frame.put(i32::from(col), 1, &placeholder(1, col + 4), image);
        }
        compact_placeholders(&mut frame);

        let bare = PLACEHOLDER.to_string();
        let row = |y: u16| -> Vec<String> {
            (0..8).map(|x| frame.content(x, y).0.to_owned()).collect()
        };
        let r0 = row(0);
        assert_eq!(r0[1], placeholder(0, 0));
        assert_eq!((&r0[2], &r0[3]), (&bare, &bare));
        assert_eq!(r0[5], placeholder(0, 4), "after the label");
        assert_eq!(r0[6], placeholder(0, 1), "another image");
        let r1 = row(1);
        assert_eq!(r1[0], placeholder(1, 4), "at the screen's edge");
        assert_eq!((&r1[1], &r1[2]), (&bare, &bare));
    }

    #[test]
    fn id_is_carried_in_the_foreground() {
        assert_eq!(id_color(0x01_02_03), Color::Rgb(1, 2, 3));
    }

    #[test]
    fn every_chunk_of_a_big_upload_is_quiet() {
        // Noise doesn't compress, so this takes several chunks.
        let mut image = Image::new(64, 64);
        let mut seed = 1u32;
        for byte in &mut image.rgba {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *byte = (seed >> 24) as u8;
        }
        let mut out = Vec::new();
        transmit(&mut out, 7, &image, Compression::Small);
        let text = String::from_utf8(out).unwrap();
        let chunks: Vec<&str> =
            text.split("\x1b\\").filter(|s| !s.is_empty()).collect();
        assert!(chunks.len() > 2);
        assert!(chunks.iter().all(|c| c.contains("q=2,m=")));
        assert!(chunks[1].starts_with("\x1b_Gq=2,m=1;"));
        assert!(chunks.last().unwrap().starts_with("\x1b_Gq=2,m=0;"));
    }

    #[test]
    fn transmit_chunks_the_payload_then_place_shows_it() {
        let image = Image::new(64, 64);
        let mut out = Vec::new();
        transmit(&mut out, 7, &image, Compression::Small);
        place(&mut out, 7, 10, 5);
        let text = String::from_utf8(out).unwrap();
        let commands: Vec<&str> =
            text.split("\x1b\\").filter(|s| !s.is_empty()).collect();
        assert!(
            commands[0].starts_with("\x1b_Ga=t,f=32,o=z,s=64,v=64,i=7,q=2,m=")
        );
        assert!(commands[commands.len() - 2].contains("m=0;"));
        // Every chunk of the upload asks for no reply.
        let upload = &commands[..commands.len() - 1];
        assert!(upload.iter().all(|c| c.contains("q=2,m=")), "{upload:?}");
        assert_eq!(
            commands[commands.len() - 1],
            "\x1b_Ga=p,U=1,i=7,p=7,c=10,r=5,q=2"
        );

        // The payload decodes back to the image.
        let payload: String = commands[..commands.len() - 1]
            .iter()
            .map(|c| &c[c.find(';').unwrap() + 1..])
            .collect();
        let zlib = BASE64.decode(payload).unwrap();
        let rgba = miniz_oxide::inflate::decompress_to_vec_zlib(&zlib).unwrap();
        assert_eq!(rgba, image.rgba);
    }

    #[test]
    fn placements_are_unique_between_images() {
        let mut out = Vec::new();
        place(&mut out, 6, 10, 5);
        place(&mut out, 7, 10, 5);

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("i=6,p=6"));
        assert!(text.contains("i=7,p=7"));
    }
}
