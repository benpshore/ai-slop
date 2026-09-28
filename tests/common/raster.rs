//! A synthetic "scanned" PDF: text drawn with a built-in 13x19 dot-matrix
//! font into an 8-bit grayscale raster, embedded as the only content of a
//! US Letter page. The page has no text layer at all, so every glyph a
//! backend reports must come from OCR.
//!
//! The glyphs have rounded bowls, diagonal strokes three dots thick and
//! consistent cap height, so a real OCR model reads them like a heavy sans
//! serif rather than like square blocks. Each font dot becomes a
//! `scale` x `scale` block of dark ink, and the whole raster then gets a
//! 3x3 box blur so stroke edges are soft grey, as in a scan.
//!
//! The raster is placed at exactly 300 dpi (25 pixels = 6 points), so at
//! [`SCAN_SCALE`] 3 a capital letter is 57 pixels tall, like a 19 pt
//! heading in a 300 dpi scan. The image stream is stored raw (no
//! `/Filter`): `lopdf` only compresses streams when `Document::compress` is
//! called, which this module never does.

use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream, dictionary};

/// Text the default scanned fixture prints.
const EXPECTED_TEXT: &str = "HELLO WORLD SCAN 2026";

/// Font scale (image pixels per font dot) that the tests use.
pub const SCAN_SCALE: u32 = 3;

/// Glyph width in font dots.
pub const GLYPH_WIDTH: u32 = 13;
/// Glyph height in font dots.
pub const GLYPH_HEIGHT: u32 = 19;
/// Glyph height in font dots, as an array length (equal to [`GLYPH_HEIGHT`]).
pub const GLYPH_ROWS: usize = 19;
/// Empty font-dot columns between neighbouring glyphs.
pub const LETTER_GAP: u32 = 3;
/// Horizontal advance per character in font dots.
pub const ADVANCE: u32 = GLYPH_WIDTH + LETTER_GAP;
/// Empty font-dot rows between neighbouring lines.
pub const LINE_GAP: u32 = 7;
/// Baseline-to-baseline distance in font dots.
pub const LINE_PITCH: u32 = GLYPH_HEIGHT + LINE_GAP;

/// Raster width: 8.5 inches at 300 dpi, i.e. the full page width.
pub const IMAGE_WIDTH_PX: u32 = 2550;
/// Minimum white space above the first line and below the last one; the
/// text block is centred vertically in the raster.
pub const PAD_PX: u32 = 150;

/// Page size in points (US Letter).
const PAGE_WIDTH_PT: u32 = 612;
const PAGE_HEIGHT_PT: u32 = 792;

/// Dark ink in `DeviceGray` (the value of a fully inked pixel).
pub const INK: u8 = 24;
/// White paper in `DeviceGray`.
pub const PAPER: u8 = 255;

/// An 8-bit grayscale raster, row-major, top row first, one byte per pixel.
pub struct GrayImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl GrayImage {
    /// A white image of the given size.
    fn blank(width: u32, height: u32) -> Self {
        let len = usize::try_from(u64::from(width) * u64::from(height)).unwrap();
        Self {
            width,
            height,
            pixels: vec![PAPER; len],
        }
    }

    /// Paint the `size` x `size` block whose top-left pixel is (`x`, `y`).
    fn fill_block(&mut self, x: u32, y: u32, size: u32) {
        let width = usize::try_from(self.width).unwrap();
        for row in y..(y + size).min(self.height) {
            let start = usize::try_from(row).unwrap() * width;
            let first = start + usize::try_from(x.min(self.width)).unwrap();
            let last = start + usize::try_from((x + size).min(self.width)).unwrap();
            self.pixels[first..last].fill(INK);
        }
    }

    /// Replace every interior pixel by the rounded mean of its 3x3
    /// neighbourhood, softening stroke edges to grey. The outermost rows and
    /// columns are left as they are (they are always paper).
    fn box_blur(&mut self) {
        let width = usize::try_from(self.width).unwrap();
        let height = usize::try_from(self.height).unwrap();
        let source = self.pixels.clone();
        for row in 1..height.saturating_sub(1) {
            for col in 1..width.saturating_sub(1) {
                let mut sum: u32 = 0;
                for near_row in (row - 1)..=(row + 1) {
                    let start = near_row * width;
                    for &value in &source[(start + col - 1)..=(start + col + 1)] {
                        sum += u32::from(value);
                    }
                }
                self.pixels[row * width + col] = u8::try_from((sum + 4) / 9).unwrap();
            }
        }
    }
}

/// The font: every character it covers with its 13x19 bitmap, top row
/// first, `#` for an inked dot and `.` for paper.
pub static GLYPHS: [(char, [&str; GLYPH_ROWS]); 37] = [
    (
        ' ',
        [
            ".............",
            ".............",
            ".............",
            ".............",
            ".............",
            ".............",
            ".............",
            ".............",
            ".............",
            ".............",
            ".............",
            ".............",
            ".............",
            ".............",
            ".............",
            ".............",
            ".............",
            ".............",
            ".............",
        ],
    ),
    (
        'A',
        [
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            "....#####....",
            "....#####....",
            "....##.##....",
            "...###.###...",
            "...###.###...",
            "...##...##...",
            "..###...###..",
            "..#########..",
            "..#########..",
            ".###.....###.",
            ".###.....###.",
            ".##.......##.",
            "###.......###",
            "###.......###",
            "###.......###",
        ],
    ),
    (
        'B',
        [
            "########.....",
            "###########..",
            "###########..",
            "###......###.",
            "###......###.",
            "###.......##.",
            "###......###.",
            "###......###.",
            "###########..",
            "###########..",
            "############.",
            "###......###.",
            "###.......###",
            "###.......###",
            "###.......###",
            "###......###.",
            "############.",
            "###########..",
            "########.....",
        ],
    ),
    (
        'C',
        [
            ".............",
            "...#######...",
            "..#########..",
            "..###...###..",
            ".###.....##..",
            ".###.........",
            ".##..........",
            ".##..........",
            "###..........",
            "###..........",
            "###..........",
            ".##..........",
            ".##..........",
            ".###.........",
            ".###.....##..",
            "..###...###..",
            "..#########..",
            "...#######...",
            ".............",
        ],
    ),
    (
        'D',
        [
            "######.......",
            "##########...",
            "###########..",
            "###.....###..",
            "###......###.",
            "###......###.",
            "###.......##.",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......##.",
            "###......###.",
            "###......###.",
            "###.....###..",
            "###########..",
            "##########...",
            "######.......",
        ],
    ),
    (
        'E',
        [
            "#############",
            "#############",
            "#############",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "##########...",
            "##########...",
            "##########...",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "#############",
            "#############",
            "#############",
        ],
    ),
    (
        'F',
        [
            "#############",
            "#############",
            "#############",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "##########...",
            "##########...",
            "##########...",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
        ],
    ),
    (
        'G',
        [
            ".............",
            "...#######...",
            "..#########..",
            "..###...###..",
            ".###.....##..",
            ".###.........",
            ".##..........",
            ".##..........",
            "###...#######",
            "###...#######",
            "###...#######",
            ".##.......###",
            ".##.......###",
            ".###.....####",
            ".###.....####",
            "..###...#####",
            "..###########",
            "...#######...",
            ".............",
        ],
    ),
    (
        'H',
        [
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "#############",
            "#############",
            "#############",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
        ],
    ),
    (
        'I',
        [
            "..#########..",
            "..#########..",
            "..#########..",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            "..#########..",
            "..#########..",
            "..#########..",
        ],
    ),
    (
        'J',
        [
            "....#########",
            "....#########",
            "....#########",
            ".........##..",
            ".........##..",
            ".........##..",
            ".........##..",
            ".........##..",
            ".........##..",
            ".........##..",
            ".........##..",
            ".........##..",
            "..##.....##..",
            "..##.....##..",
            "..##.....##..",
            "..###...###..",
            "..#########..",
            "...#######...",
            ".....###.....",
        ],
    ),
    (
        'K',
        [
            "###.......###",
            "###.......###",
            "###......####",
            "###.....####.",
            "###....####..",
            "###...####...",
            "###..####....",
            "###.####.....",
            "#######......",
            "########.....",
            "########.....",
            "####..###....",
            "###...####...",
            "###....###...",
            "###.....###..",
            "###......###.",
            "###......####",
            "###.......###",
            "###.......###",
        ],
    ),
    (
        'L',
        [
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "#############",
            "#############",
            "#############",
        ],
    ),
    (
        'M',
        [
            "###.......###",
            "###.......###",
            "####.....####",
            "####.....####",
            "#####...#####",
            "#####...#####",
            "######.######",
            "######.######",
            "###.#####.###",
            "###.#####.###",
            "###..###..###",
            "###..###..###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
        ],
    ),
    (
        'N',
        [
            "###.......###",
            "###.......###",
            "####......###",
            "####......###",
            "#####.....###",
            "######....###",
            "######....###",
            "###.###...###",
            "###.####..###",
            "###..###..###",
            "###..####.###",
            "###...###.###",
            "###....######",
            "###....######",
            "###.....#####",
            "###......####",
            "###......####",
            "###.......###",
            "###.......###",
        ],
    ),
    (
        'O',
        [
            ".....###.....",
            "...#######...",
            "..#########..",
            "..###...###..",
            ".###.....###.",
            ".###.....###.",
            ".##.......##.",
            ".##.......##.",
            "###.......###",
            "###.......###",
            "###.......###",
            ".##.......##.",
            ".##.......##.",
            ".###.....###.",
            ".###.....###.",
            "..###...###..",
            "..#########..",
            "...#######...",
            ".....###.....",
        ],
    ),
    (
        'P',
        [
            "########.....",
            "###########..",
            "############.",
            "###......###.",
            "###.......##.",
            "###.......###",
            "###.......###",
            "###.......##.",
            "###......###.",
            "###.....####.",
            "###########..",
            "##########...",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
            "###..........",
        ],
    ),
    (
        'Q',
        [
            ".....###.....",
            "...#######...",
            "..#########..",
            "..###...###..",
            ".###.....###.",
            ".###.....###.",
            ".##.......##.",
            ".##.......##.",
            "###.......###",
            "###.......###",
            "###.......###",
            ".##.......##.",
            ".##....##.##.",
            ".###...#####.",
            ".###...#####.",
            "..###...####.",
            "..##########.",
            "...##########",
            ".....###...##",
        ],
    ),
    (
        'R',
        [
            "########.....",
            "###########..",
            "############.",
            "###......###.",
            "###.......##.",
            "###.......###",
            "###.......###",
            "###.......##.",
            "###......###.",
            "###.....####.",
            "###########..",
            "##########...",
            "###...###....",
            "###....###...",
            "###.....###..",
            "###.....####.",
            "###......####",
            "###.......###",
            "###.......###",
        ],
    ),
    (
        'S',
        [
            ".............",
            "..#########..",
            ".###########.",
            ".###.....###.",
            ".##.......##.",
            ".##..........",
            ".##..........",
            ".###.........",
            ".#######.....",
            "..#########..",
            ".....#######.",
            ".........###.",
            "..........##.",
            "..........##.",
            ".##.......##.",
            ".###.....###.",
            ".###########.",
            "..#########..",
            ".............",
        ],
    ),
    (
        'T',
        [
            "#############",
            "#############",
            "#############",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
        ],
    ),
    (
        'U',
        [
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            "###.......###",
            ".##.......##.",
            ".###.....###.",
            ".####...####.",
            "..#########..",
            "...#######...",
            ".....###.....",
        ],
    ),
    (
        'V',
        [
            "###.......###",
            "###.......###",
            "###.......###",
            ".##.......##.",
            ".###.....###.",
            ".###.....###.",
            "..##.....##..",
            "..###...###..",
            "..###...###..",
            "...##...##...",
            "...###.###...",
            "...###.###...",
            "....##.##....",
            "....#####....",
            "....#####....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
        ],
    ),
    (
        'W',
        [
            "##.........##",
            "##.........##",
            "##.........##",
            "##.........##",
            "###.......###",
            "###.......###",
            "###..###..###",
            ".##..###..##.",
            ".##..###..##.",
            ".##.#####.##.",
            ".##.#####.##.",
            ".#####.#####.",
            ".#####.#####.",
            "..####.####..",
            "..####.####..",
            "..###...###..",
            "..###...###..",
            "..###...###..",
            "..##.....##..",
        ],
    ),
    (
        'X',
        [
            "###.......###",
            "###.......###",
            "####.....####",
            ".###.....###.",
            "..###...###..",
            "..####.####..",
            "...###.###...",
            "....#####....",
            "....#####....",
            ".....###.....",
            "....#####....",
            "....#####....",
            "...###.###...",
            "..####.####..",
            "..###...###..",
            ".###.....###.",
            "####.....####",
            "###.......###",
            "###.......###",
        ],
    ),
    (
        'Y',
        [
            "###.......###",
            "###.......###",
            "####.....####",
            ".###.....###.",
            "..###...###..",
            "..####.####..",
            "...###.###...",
            "....#####....",
            "....#####....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
        ],
    ),
    (
        'Z',
        [
            "#############",
            "#############",
            "#############",
            ".........###.",
            "........###..",
            ".......####..",
            ".......###...",
            "......###....",
            ".....####....",
            ".....###.....",
            "....####.....",
            "....###......",
            "...###.......",
            "..####.......",
            "..###........",
            ".###.........",
            "#############",
            "#############",
            "#############",
        ],
    ),
    (
        '0',
        [
            ".....###.....",
            "....#####....",
            "...#######...",
            "...###.###...",
            "..###...###..",
            "..###...###..",
            "..##.....##..",
            "..##.....##..",
            "..##.....##..",
            "..##.....##..",
            "..##.....##..",
            "..##.....##..",
            "..##.....##..",
            "..###...###..",
            "..###...###..",
            "...###.###...",
            "...#######...",
            "....#####....",
            ".....###.....",
        ],
    ),
    (
        '1',
        [
            ".....###.....",
            ".....###.....",
            "....####.....",
            "...#####.....",
            "..######.....",
            "..######.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            ".....###.....",
            "..#########..",
            "..#########..",
            "..#########..",
        ],
    ),
    (
        '2',
        [
            ".............",
            "..#########..",
            ".###########.",
            ".###.....###.",
            ".##.......##.",
            "###.......##.",
            "###.......##.",
            "..........##.",
            ".........###.",
            "........####.",
            ".......####..",
            "......####...",
            ".....####....",
            "....####.....",
            "...####......",
            "..####.......",
            "#############",
            "#############",
            "#############",
        ],
    ),
    (
        '3',
        [
            ".............",
            "..#########..",
            ".###########.",
            ".###.....###.",
            ".##.......##.",
            "..........##.",
            "..........##.",
            ".........###.",
            ".....#######.",
            ".....######..",
            ".....#######.",
            ".........###.",
            "..........##.",
            "..........##.",
            ".##.......##.",
            ".###.....###.",
            ".###########.",
            "..#########..",
            ".............",
        ],
    ),
    (
        '4',
        [
            "........##...",
            ".......###...",
            ".......###...",
            "......####...",
            ".....#####...",
            ".....#####...",
            "....###.##...",
            "...####.##...",
            "...###..##...",
            "..###...##...",
            ".####...##...",
            ".###....##...",
            "#############",
            "#############",
            "........##...",
            "........##...",
            "........##...",
            "........##...",
            "........##...",
        ],
    ),
    (
        '5',
        [
            "..##########.",
            "..##########.",
            "..##########.",
            ".###.........",
            ".###.........",
            ".##..........",
            ".##.#####....",
            ".##########..",
            ".#####.#####.",
            ".##......###.",
            "..........##.",
            "..........##.",
            "..........##.",
            ".##.......##.",
            ".###.....###.",
            ".####...####.",
            "..#########..",
            "...#######...",
            ".............",
        ],
    ),
    (
        '6',
        [
            ".............",
            "...######....",
            "..#######....",
            "..###........",
            ".###.........",
            ".###.........",
            ".##..........",
            ".##.#####....",
            "###########..",
            "######.#####.",
            "####.....###.",
            "###.......##.",
            "###.......###",
            "###.......###",
            ".##.......##.",
            ".###.....###.",
            ".###########.",
            "..#########..",
            ".....###.....",
        ],
    ),
    (
        '7',
        [
            "#############",
            "#############",
            "#############",
            ".........###.",
            ".........###.",
            ".........##..",
            "........###..",
            "........###..",
            ".......###...",
            ".......###...",
            ".......##....",
            "......###....",
            "......###....",
            ".....###.....",
            ".....###.....",
            ".....##......",
            "....###......",
            "....###......",
            "....##.......",
        ],
    ),
    (
        '8',
        [
            ".....###.....",
            "...#######...",
            "..#########..",
            ".####...####.",
            ".###.....###.",
            ".##.......##.",
            ".###.....###.",
            ".####...####.",
            "..#########..",
            "..#########..",
            ".###########.",
            ".###.....###.",
            "###.......###",
            "###.......###",
            "###.......###",
            ".###.....###.",
            ".###########.",
            "..#########..",
            ".....###.....",
        ],
    ),
    (
        '9',
        [
            ".....###.....",
            "..#########..",
            ".###########.",
            ".###.....###.",
            ".##.......##.",
            "###.......###",
            "###.......###",
            ".##.......###",
            ".###.....####",
            ".#####.######",
            "..###########",
            "....#####.##.",
            "..........##.",
            ".........###.",
            ".........###.",
            "........###..",
            "....#######..",
            "....######...",
            ".............",
        ],
    ),
];

/// The bitmap of `c` from [`GLYPHS`].
///
/// Covers `A`-`Z` (lower case is folded to upper case), `0`-`9` and space.
///
/// # Panics
///
/// On any other character, so a fixture never silently drops text.
#[must_use]
pub fn glyph(c: char) -> &'static [&'static str; GLYPH_ROWS] {
    let upper = c.to_ascii_uppercase();
    match GLYPHS.iter().find(|(key, _)| *key == upper) {
        Some((_, rows)) => rows,
        None => panic!("raster font has no glyph for {c:?}"),
    }
}

/// Round `value` up to a multiple of 25 so the 300 dpi placement
/// (25 pixels = 6 points) lands on whole points.
#[must_use]
pub fn round_up_to_25(value: u32) -> u32 {
    value.div_ceil(25) * 25
}

/// Width in pixels of `line` at `scale`, from the left edge of its first
/// glyph cell to the right edge of its last one (no trailing gap).
#[must_use]
pub fn line_width_px(line: &str, scale: u32) -> u32 {
    let chars = u32::try_from(line.chars().count()).unwrap();
    if chars == 0 {
        0
    } else {
        (chars * ADVANCE - LETTER_GAP) * scale
    }
}

/// Height in pixels of `line_count` lines at `scale`.
#[must_use]
pub fn text_height_px(line_count: u32, scale: u32) -> u32 {
    if line_count == 0 {
        0
    } else {
        ((line_count - 1) * LINE_PITCH + GLYPH_HEIGHT) * scale
    }
}

/// Render `text_lines` in dark ink on white, each font dot a `scale` x
/// `scale` block of pixels, then soften the edges with a 3x3 box blur.
///
/// The raster is always [`IMAGE_WIDTH_PX`] wide (the page width at 300 dpi)
/// and [`round_up_to_25`] of the text height plus twice [`PAD_PX`] tall.
/// Each line is centred horizontally and the block of lines vertically.
///
/// # Panics
///
/// When `scale` is 0, a line does not fit the page width, the lines do not
/// fit the page height, or a character has no glyph.
#[must_use]
pub fn render_lines(text_lines: &[&str], scale: u32) -> GrayImage {
    assert!(scale > 0, "scale must be positive");
    assert_eq!(usize::try_from(GLYPH_HEIGHT).unwrap(), GLYPH_ROWS);
    let line_count = u32::try_from(text_lines.len()).unwrap();
    let text_height = text_height_px(line_count, scale);
    let height = round_up_to_25(2 * PAD_PX + text_height);
    assert!(
        height * 6 / 25 <= PAGE_HEIGHT_PT,
        "{line_count} lines at scale {scale} do not fit the page"
    );
    let mut image = GrayImage::blank(IMAGE_WIDTH_PX, height);

    let mut top = (height - text_height) / 2;
    for line in text_lines {
        let line_width = line_width_px(line, scale);
        assert!(
            line_width + 2 * PAD_PX <= IMAGE_WIDTH_PX,
            "line {line:?} at scale {scale} does not fit the page width"
        );
        let mut left = (IMAGE_WIDTH_PX - line_width) / 2;
        for c in line.chars() {
            for (dot_row, row) in (0_u32..).zip(glyph(c)) {
                for (dot_col, dot) in (0_u32..).zip(row.bytes()) {
                    if dot == b'#' {
                        image.fill_block(left + dot_col * scale, top + dot_row * scale, scale);
                    }
                }
            }
            left += ADVANCE * scale;
        }
        top += LINE_PITCH * scale;
    }
    image.box_blur();
    image
}

/// A one-page, image-only PDF (612 x 792 pt) whose only content is the
/// raster of [`render_lines`], placed at the top of the page at 300 dpi.
///
/// The image `XObject` is `/DeviceGray`, 8 bits per component, stored raw
/// without a `/Filter`. The page has no fonts and no text operators.
///
/// # Panics
///
/// As [`render_lines`], or if `lopdf` fails to serialise the document.
#[must_use]
pub fn scanned_page_pdf(text_lines: &[&str], scale: u32) -> Vec<u8> {
    let image = render_lines(text_lines, scale);
    let width_pt = image.width * 6 / 25;
    let height_pt = image.height * 6 / 25;

    let mut doc = Document::with_version("1.5");
    let pages_root_id = doc.new_object_id();
    let image_dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Image",
        "Width" => i64::from(image.width),
        "Height" => i64::from(image.height),
        "ColorSpace" => "DeviceGray",
        "BitsPerComponent" => 8_i64,
    };
    let image_id = doc.add_object(Stream::new(image_dict, image.pixels));

    // `q W 0 0 H x y cm /Im0 Do Q`: the unit square scaled to the image's
    // size in points, its top edge on the top edge of the page.
    let matrix = vec![
        Object::Integer(i64::from(width_pt)),
        Object::Integer(0),
        Object::Integer(0),
        Object::Integer(i64::from(height_pt)),
        Object::Integer(0),
        Object::Integer(i64::from(PAGE_HEIGHT_PT - height_pt)),
    ];
    let operations = vec![
        Operation::new("q", vec![]),
        Operation::new("cm", matrix),
        Operation::new("Do", vec![Object::Name(b"Im0".to_vec())]),
        Operation::new("Q", vec![]),
    ];
    let content = Content { operations }
        .encode()
        .expect("content stream encodes");
    let content_id = doc.add_object(Stream::new(dictionary! {}, content));

    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_root_id,
        "MediaBox" => vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Integer(i64::from(PAGE_WIDTH_PT)),
            Object::Integer(i64::from(PAGE_HEIGHT_PT)),
        ],
        "Resources" => dictionary! {
            "XObject" => dictionary! { "Im0" => image_id },
        },
        "Contents" => content_id,
    });
    doc.objects.insert(
        pages_root_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1_i64,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_root_id,
    });
    doc.trailer.set("Root", catalog_id);

    let mut bytes: Vec<u8> = Vec::new();
    doc.save_to(&mut bytes).expect("document serialises");
    bytes
}

/// The text printed by the scanned fixture, as one line.
#[must_use]
pub fn expected_text() -> &'static str {
    EXPECTED_TEXT
}

/// The scanned fixture: [`expected_text`] on one line at [`SCAN_SCALE`].
#[must_use]
pub fn scanned_fixture() -> Vec<u8> {
    scanned_page_pdf(&[EXPECTED_TEXT], SCAN_SCALE)
}
