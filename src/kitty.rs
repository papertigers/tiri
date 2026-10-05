//! The kitty graphics protocol, as far as the overview needs it: uploading
//! images, and showing them through Unicode placeholders so they live in
//! ordinary text cells that scroll and clip like everything else.
//!
//! Spec: <https://sw.kovidgoyal.net/kitty/graphics-protocol/>

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::render::Color;
use crate::thumbnail::Image;

/// A cell showing part of an image. The image id is carried in the cell's
/// foreground color, and which part of the image in two combining marks.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// Each image has a single virtual placement, with this id.
const PLACEMENT_ID: u32 = 1;

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
    [
        PLACEHOLDER,
        DIACRITICS[usize::from(row)],
        DIACRITICS[usize::from(col)],
    ]
    .into_iter()
    .collect()
}

/// The foreground color that makes a placeholder cell refer to image `id`.
pub fn id_color(id: u32) -> Color {
    let [_, r, g, b] = id.to_be_bytes();
    Color::Rgb(r, g, b)
}

/// Uploads `image` as image `id`, replacing any earlier one.
pub fn transmit(out: &mut Vec<u8>, id: u32, image: &Image) {
    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&image.rgba, 1);
    let payload = BASE64.encode(compressed);
    // q=2 keeps the terminal from replying; replies would arrive as input.
    let control = format!(
        "a=t,f=32,o=z,s={},v={},i={id},q=2",
        image.width, image.height
    );
    let chunks: Vec<&[u8]> = payload.as_bytes().chunks(CHUNK).collect();
    for (n, chunk) in chunks.iter().enumerate() {
        let more = u8::from(n + 1 < chunks.len());
        out.extend_from_slice(b"\x1b_G");
        if n == 0 {
            out.extend_from_slice(control.as_bytes());
            out.push(b',');
        }
        out.extend_from_slice(format!("m={more};").as_bytes());
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
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
pub fn place(out: &mut Vec<u8>, id: u32, cols: u16, rows: u16) {
    out.extend_from_slice(
        format!("\x1b_Ga=p,U=1,i={id},p={PLACEMENT_ID},c={cols},r={rows},q=2\x1b\\").as_bytes(),
    );
}

/// Frees image `id` and its placements.
pub fn delete(out: &mut Vec<u8>, id: u32) {
    out.extend_from_slice(format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\").as_bytes());
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
    fn id_is_carried_in_the_foreground() {
        assert_eq!(id_color(0x01_02_03), Color::Rgb(1, 2, 3));
    }

    #[test]
    fn transmit_chunks_the_payload_then_place_shows_it() {
        let image = Image::new(64, 64);
        let mut out = Vec::new();
        transmit(&mut out, 7, &image);
        place(&mut out, 7, 10, 5);
        let text = String::from_utf8(out).unwrap();
        let commands: Vec<&str> = text.split("\x1b\\").filter(|s| !s.is_empty()).collect();
        assert!(commands[0].starts_with("\x1b_Ga=t,f=32,o=z,s=64,v=64,i=7,q=2,m="));
        assert!(commands[commands.len() - 2].contains("m=0;"));
        assert_eq!(
            commands[commands.len() - 1],
            "\x1b_Ga=p,U=1,i=7,p=1,c=10,r=5,q=2"
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
}
