//! Kitty's Unicode placeholders: an image as text. Each cell of a picture is
//! `U+10EEEE` followed by combining marks naming its row and column within
//! the image; the cell's foreground colour is the image id and its underline
//! colour the placement id. Every protocol's images are written into the grid
//! this way, so scrolling, reflow, clears and the alternate screen treat a
//! picture exactly as they treat text.

use std::borrow::Cow;

use alacritty_terminal::vte::ansi::{Color, Rgb};

pub const PLACEHOLDER: char = '\u{10EEEE}';

/// The combining marks that stand for 0, 1, 2…: kitty's
/// `rowcolumn-diacritics.txt`, generated from that file. In code point order,
/// so the reverse lookup is a binary search.
#[rustfmt::skip]
pub const DIACRITICS: [char; 297] = [
    '\u{0305}', '\u{030d}', '\u{030e}', '\u{0310}', '\u{0312}', '\u{033d}', '\u{033e}', '\u{033f}',
    '\u{0346}', '\u{034a}', '\u{034b}', '\u{034c}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035b}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036a}', '\u{036b}', '\u{036c}', '\u{036d}', '\u{036e}', '\u{036f}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059c}', '\u{059d}', '\u{059e}', '\u{059f}', '\u{05a0}', '\u{05a1}',
    '\u{05a8}', '\u{05a9}', '\u{05ab}', '\u{05ac}', '\u{05af}', '\u{05c4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
    '\u{0659}', '\u{065a}', '\u{065b}', '\u{065d}', '\u{065e}', '\u{06d6}', '\u{06d7}', '\u{06d8}',
    '\u{06d9}', '\u{06da}', '\u{06db}', '\u{06dc}', '\u{06df}', '\u{06e0}', '\u{06e1}', '\u{06e2}',
    '\u{06e4}', '\u{06e7}', '\u{06e8}', '\u{06eb}', '\u{06ec}', '\u{0730}', '\u{0732}', '\u{0733}',
    '\u{0735}', '\u{0736}', '\u{073a}', '\u{073d}', '\u{073f}', '\u{0740}', '\u{0741}', '\u{0743}',
    '\u{0745}', '\u{0747}', '\u{0749}', '\u{074a}', '\u{07eb}', '\u{07ec}', '\u{07ed}', '\u{07ee}',
    '\u{07ef}', '\u{07f0}', '\u{07f1}', '\u{07f3}', '\u{0816}', '\u{0817}', '\u{0818}', '\u{0819}',
    '\u{081b}', '\u{081c}', '\u{081d}', '\u{081e}', '\u{081f}', '\u{0820}', '\u{0821}', '\u{0822}',
    '\u{0823}', '\u{0825}', '\u{0826}', '\u{0827}', '\u{0829}', '\u{082a}', '\u{082b}', '\u{082c}',
    '\u{082d}', '\u{0951}', '\u{0953}', '\u{0954}', '\u{0f82}', '\u{0f83}', '\u{0f86}', '\u{0f87}',
    '\u{135d}', '\u{135e}', '\u{135f}', '\u{17dd}', '\u{193a}', '\u{1a17}', '\u{1a75}', '\u{1a76}',
    '\u{1a77}', '\u{1a78}', '\u{1a79}', '\u{1a7a}', '\u{1a7b}', '\u{1a7c}', '\u{1b6b}', '\u{1b6d}',
    '\u{1b6e}', '\u{1b6f}', '\u{1b70}', '\u{1b71}', '\u{1b72}', '\u{1b73}', '\u{1cd0}', '\u{1cd1}',
    '\u{1cd2}', '\u{1cda}', '\u{1cdb}', '\u{1ce0}', '\u{1dc0}', '\u{1dc1}', '\u{1dc3}', '\u{1dc4}',
    '\u{1dc5}', '\u{1dc6}', '\u{1dc7}', '\u{1dc8}', '\u{1dc9}', '\u{1dcb}', '\u{1dcc}', '\u{1dd1}',
    '\u{1dd2}', '\u{1dd3}', '\u{1dd4}', '\u{1dd5}', '\u{1dd6}', '\u{1dd7}', '\u{1dd8}', '\u{1dd9}',
    '\u{1dda}', '\u{1ddb}', '\u{1ddc}', '\u{1ddd}', '\u{1dde}', '\u{1ddf}', '\u{1de0}', '\u{1de1}',
    '\u{1de2}', '\u{1de3}', '\u{1de4}', '\u{1de5}', '\u{1de6}', '\u{1dfe}', '\u{20d0}', '\u{20d1}',
    '\u{20d4}', '\u{20d5}', '\u{20d6}', '\u{20d7}', '\u{20db}', '\u{20dc}', '\u{20e1}', '\u{20e7}',
    '\u{20e9}', '\u{20f0}', '\u{2cef}', '\u{2cf0}', '\u{2cf1}', '\u{2de0}', '\u{2de1}', '\u{2de2}',
    '\u{2de3}', '\u{2de4}', '\u{2de5}', '\u{2de6}', '\u{2de7}', '\u{2de8}', '\u{2de9}', '\u{2dea}',
    '\u{2deb}', '\u{2dec}', '\u{2ded}', '\u{2dee}', '\u{2def}', '\u{2df0}', '\u{2df1}', '\u{2df2}',
    '\u{2df3}', '\u{2df4}', '\u{2df5}', '\u{2df6}', '\u{2df7}', '\u{2df8}', '\u{2df9}', '\u{2dfa}',
    '\u{2dfb}', '\u{2dfc}', '\u{2dfd}', '\u{2dfe}', '\u{2dff}', '\u{a66f}', '\u{a67c}', '\u{a67d}',
    '\u{a6f0}', '\u{a6f1}', '\u{a8e0}', '\u{a8e1}', '\u{a8e2}', '\u{a8e3}', '\u{a8e4}', '\u{a8e5}',
    '\u{a8e6}', '\u{a8e7}', '\u{a8e8}', '\u{a8e9}', '\u{a8ea}', '\u{a8eb}', '\u{a8ec}', '\u{a8ed}',
    '\u{a8ee}', '\u{a8ef}', '\u{a8f0}', '\u{a8f1}', '\u{aab0}', '\u{aab2}', '\u{aab3}', '\u{aab7}',
    '\u{aab8}', '\u{aabe}', '\u{aabf}', '\u{aac1}', '\u{fe20}', '\u{fe21}', '\u{fe22}', '\u{fe23}',
    '\u{fe24}', '\u{fe25}', '\u{fe26}', '\u{10a0f}', '\u{10a38}', '\u{1d185}', '\u{1d186}', '\u{1d187}',
    '\u{1d188}', '\u{1d189}', '\u{1d1aa}', '\u{1d1ab}', '\u{1d1ac}', '\u{1d1ad}', '\u{1d242}', '\u{1d243}',
    '\u{1d244}',
];

fn diacritic_index(c: char) -> Option<u16> {
    DIACRITICS.binary_search(&c).ok().map(|i| i as u16)
}

/// The colour that carries `id`'s low 24 bits.
pub fn id_color(id: u32) -> Color {
    Color::Spec(Rgb {
        r: (id >> 16) as u8,
        g: (id >> 8) as u8,
        b: id as u8,
    })
}

fn color_id(color: Color) -> Option<u32> {
    match color {
        Color::Spec(Rgb { r, g, b }) => Some((r as u32) << 16 | (g as u32) << 8 | b as u32),
        Color::Indexed(n) => Some(n as u32),
        Color::Named(_) => None,
    }
}

/// Which slice of which picture a cell shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageCell {
    pub image: u32,
    pub placement: u32,
    pub row: u16,
    pub col: u16,
}

/// Read a grid cell as a placeholder. Whatever the cell leaves out it takes
/// from `left`, the cell before it on the same line, when that is the same
/// picture: clients rely on this to keep placeholder text short.
pub fn decode(
    c: char,
    zerowidth: Option<&[char]>,
    fg: Color,
    underline: Option<Color>,
    left: Option<ImageCell>,
) -> Option<ImageCell> {
    if c != PLACEHOLDER {
        return None;
    }
    let low = color_id(fg)?;
    let placement = underline.and_then(color_id).unwrap_or(0);
    let mut marks = zerowidth
        .unwrap_or_default()
        .iter()
        .map(|&c| diacritic_index(c));
    let (row, col, high) = (
        marks.next().flatten(),
        marks.next().flatten(),
        marks.next().flatten(),
    );
    let left = left.filter(|l| l.image & 0xFF_FFFF == low && l.placement == placement);
    let row = row.or(left.map(|l| l.row)).unwrap_or(0);
    let left = left.filter(|l| l.row == row);
    let col = col.or(left.map(|l| l.col + 1)).unwrap_or(0);
    let high = high
        .map(u32::from)
        .or(left.filter(|l| l.col + 1 == col).map(|l| l.image >> 24))
        .unwrap_or(0);
    Some(ImageCell {
        image: (high & 0xFF) << 24 | low,
        placement,
        row,
        col,
    })
}

/// Text copied off the grid, without the placeholder cells of any picture
/// it ran across.
pub fn strip_placeholders(text: &str) -> Cow<'_, str> {
    if !text.contains(PLACEHOLDER) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut in_cell = false;
    for c in text.chars() {
        if c == PLACEHOLDER {
            in_cell = true;
        } else if !(in_cell && diacritic_index(c).is_some()) {
            in_cell = false;
            out.push(c);
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthChar;

    /// Placement counts on the placeholder taking one cell and every mark
    /// taking none. A unicode-width bump that changed either would misplace
    /// every image without an error anywhere.
    #[test]
    fn widths_are_what_the_grid_assumes() {
        assert_eq!(PLACEHOLDER.width(), Some(1));
        for mark in DIACRITICS {
            assert_eq!(mark.width(), Some(0), "{mark:?}");
        }
        assert!(DIACRITICS.is_sorted());
        assert_eq!(DIACRITICS[0], '\u{305}');
    }

    fn cell(id: u32, placement: u32, marks: &[u16], left: Option<ImageCell>) -> Option<ImageCell> {
        let marks: Vec<char> = marks.iter().map(|&m| DIACRITICS[m as usize]).collect();
        decode(
            PLACEHOLDER,
            Some(&marks),
            id_color(id),
            (placement != 0).then(|| id_color(placement)),
            left,
        )
    }

    #[test]
    fn a_cell_names_its_image_placement_row_and_column() {
        let got = cell(0x12_3456, 7, &[3, 296], None).unwrap();
        assert_eq!(
            got,
            ImageCell {
                image: 0x12_3456,
                placement: 7,
                row: 3,
                col: 296
            }
        );
        // The third mark is the id's top byte.
        let got = cell(0xFFFF_FFFE, 0, &[0, 1, 0xFF], None).unwrap();
        assert_eq!(got.image, 0xFFFF_FFFE);
        // 256-colour ids, and cells that aren't images at all.
        let indexed = decode(PLACEHOLDER, None, Color::Indexed(42), None, None).unwrap();
        assert_eq!((indexed.image, indexed.row, indexed.col), (42, 0, 0));
        assert_eq!(decode('x', None, id_color(1), None, None), None);
        let named = Color::Named(alacritty_terminal::vte::ansi::NamedColor::Foreground);
        assert_eq!(decode(PLACEHOLDER, None, named, None, None), None);
    }

    #[test]
    fn missing_marks_come_from_the_left_neighbour() {
        let first = cell(0xFF00_0005, 0, &[2, 4, 0xFF], None).unwrap();
        // No marks at all: same row, next column, same top byte.
        let next = cell(0xFF00_0005, 0, &[], Some(first)).unwrap();
        assert_eq!((next.image, next.row, next.col), (0xFF00_0005, 2, 5));
        // Row only: the column still follows on.
        let next = cell(0xFF00_0005, 0, &[2], Some(first)).unwrap();
        assert_eq!((next.image, next.col), (0xFF00_0005, 5));
        // A different row starts over at column 0, with no top byte to borrow.
        let below = cell(0xFF00_0005, 0, &[3], Some(first)).unwrap();
        assert_eq!((below.image, below.row, below.col), (5, 3, 0));
        // Another image to the left is no neighbour.
        let other = cell(9, 0, &[], Some(first)).unwrap();
        assert_eq!((other.image, other.row, other.col), (9, 0, 0));
    }

    #[test]
    fn strip_removes_placeholders_and_only_their_marks() {
        let mut text = String::from("a");
        for col in &DIACRITICS[..3] {
            text.push(PLACEHOLDER);
            text.push(DIACRITICS[0]);
            text.push(*col);
        }
        text.push_str("b\u{305}c");
        // The overline after `b` is real text, not a placeholder's mark.
        assert_eq!(strip_placeholders(&text), "ab\u{305}c");
        assert!(matches!(strip_placeholders("plain"), Cow::Borrowed(_)));
    }
}
