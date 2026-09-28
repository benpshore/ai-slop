//! A synthetic "scanned" PDF: text drawn with a built-in 5x7 block-letter
//! bitmap font into an 8-bit grayscale raster, embedded as the only content
//! of a US Letter page. The page has no text layer at all, so every glyph a
//! backend reports must come from OCR.
//!
//! The raster is placed at exactly 300 dpi (25 pixels = 6 points), so at
//! scale 7 a capital letter is 49 pixels tall, like a 16 pt heading in a
//! 300 dpi scan. The image stream is stored raw (no `/Filter`): `lopdf`
//! only compresses streams when `Document::compress` is called, which this
//! module never does.

use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream, dictionary};

/// Text the default scanned fixture prints.
const EXPECTED_TEXT: &str = "HELLO WORLD 2026";

/// Font scale (image pixels per font dot) that the tests use.
pub const SCAN_SCALE: u32 = 7;

/// Glyph width in font dots.
const GLYPH_WIDTH: u32 = 5;
/// Glyph height in font dots.
const GLYPH_HEIGHT: u32 = 7;
/// Horizontal advance per character in font dots (glyph plus one blank column).
const ADVANCE: u32 = GLYPH_WIDTH + 1;
/// Baseline-to-baseline distance in font dots.
const LINE_PITCH: u32 = GLYPH_HEIGHT + 4;

/// Raster width: 8.5 inches at 300 dpi, i.e. the full page width.
pub const IMAGE_WIDTH_PX: u32 = 2550;
/// White margin left of the text and above the first line (1 inch).
const MARGIN_PX: u32 = 300;
/// White space below the last line before the raster ends.
const BOTTOM_PAD_PX: u32 = 150;

/// Page size in points (US Letter).
const PAGE_WIDTH_PT: u32 = 612;
const PAGE_HEIGHT_PT: u32 = 792;

/// Black ink and white paper in `DeviceGray`.
const INK: u8 = 0;
const PAPER: u8 = 255;

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
}

/// The 5x7 bitmap of `c`: seven rows, top first, bit 4 is the leftmost dot.
///
/// Covers `A`-`Z` (lower case is folded to upper case), `0`-`9` and space.
///
/// # Panics
///
/// On any other character, so a fixture never silently drops text.
fn glyph(c: char) -> [u8; 7] {
    match c.to_ascii_uppercase() {
        ' ' => [0, 0, 0, 0, 0, 0, 0],
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        other => panic!("raster font has no glyph for {other:?}"),
    }
}

/// Round `value` up to a multiple of 25 so the 300 dpi placement
/// (25 pixels = 6 points) lands on whole points.
fn round_up_to_25(value: u32) -> u32 {
    value.div_ceil(25) * 25
}

/// Render `text_lines` in black on white, each font dot a `scale` x `scale`
/// block of pixels, starting one inch from the top-left corner.
///
/// The raster is always [`IMAGE_WIDTH_PX`] wide (the page width at 300 dpi)
/// and just tall enough for the lines plus margins, rounded up to a multiple
/// of 25 pixels.
///
/// # Panics
///
/// When `scale` is 0, a line does not fit the page width, the lines do not
/// fit the page height, or a character has no glyph.
#[must_use]
pub fn render_lines(text_lines: &[&str], scale: u32) -> GrayImage {
    assert!(scale > 0, "scale must be positive");
    let line_count = u32::try_from(text_lines.len()).unwrap();
    let text_height = if line_count == 0 {
        0
    } else {
        ((line_count - 1) * LINE_PITCH + GLYPH_HEIGHT) * scale
    };
    let height = round_up_to_25(MARGIN_PX + text_height + BOTTOM_PAD_PX);
    assert!(
        height * 6 / 25 <= PAGE_HEIGHT_PT,
        "{line_count} lines at scale {scale} do not fit the page"
    );
    let mut image = GrayImage::blank(IMAGE_WIDTH_PX, height);

    let mut top = MARGIN_PX;
    for line in text_lines {
        let chars = u32::try_from(line.chars().count()).unwrap();
        let line_width = chars * ADVANCE * scale;
        assert!(
            MARGIN_PX + line_width <= IMAGE_WIDTH_PX,
            "line {line:?} at scale {scale} does not fit the page width"
        );
        let mut left = MARGIN_PX;
        for c in line.chars() {
            let rows = glyph(c);
            for (dot_row, bits) in (0_u32..).zip(rows) {
                for dot_col in 0..GLYPH_WIDTH {
                    if bits & (0x10 >> dot_col) != 0 {
                        let x = left + dot_col * scale;
                        let y = top + dot_row * scale;
                        image.fill_block(x, y, scale);
                    }
                }
            }
            left += ADVANCE * scale;
        }
        top += LINE_PITCH * scale;
    }
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
