//! Shared helpers for the integration tests: a synthetic two-page, two-column
//! academic paper built with `lopdf`, and a temp-file writer.
//!
//! The paper uses the standard Type1 fonts Helvetica and Helvetica-Bold with
//! `WinAnsiEncoding`, one `Tj` string per printed line, and absolute `Td`
//! positioning so every line's baseline is known exactly.

#![allow(dead_code, clippy::missing_panics_doc)]

pub mod raster;

use std::path::PathBuf;

use lopdf::content::{Content, Operation};
use lopdf::{Document, Encoding, Object, ObjectId, Stream, dictionary};
use tempfile::TempDir;

/// Page width in points (US Letter).
const PAGE_WIDTH: i64 = 612;
/// Page height in points (US Letter).
const PAGE_HEIGHT: i64 = 792;

/// Left margin of the page and of the left body column.
const LEFT_X: f32 = 60.0;
/// Left edge of the right body column.
const RIGHT_X: f32 = 320.0;
/// Indent of reference continuation lines (hanging indent).
const HANGING_X: f32 = 78.0;
/// Body font size in points.
const BODY_SIZE: f32 = 10.0;
/// Baseline-to-baseline distance of body lines.
const LEADING: f32 = 12.0;

/// Regular font resource name.
const REGULAR: &str = "F1";
/// Bold font resource name.
const BOLD: &str = "F2";

/// Title printed in 18 pt bold at the top of page 1.
pub const TITLE: &str = "Faithful Extraction of Citations from Academic PDFs";
/// Authors line printed in 11 pt under the title.
pub const AUTHORS_LINE: &str = "Ada Lovelace, Charles Babbage, and Grace Hopper";
/// The three author names, in print order.
pub const AUTHORS: [&str; 3] = ["Ada Lovelace", "Charles Babbage", "Grace Hopper"];
/// DOI printed in the page-1 footer (without the `doi:` prefix).
pub const DOI: &str = "10.1000/xyz123";

const ABSTRACT_LINES: &[&str] = &[
    "We describe a pipeline that extracts positioned text from academic PDFs, orders it",
    "into columns, and resolves in-text citation markers against the reference list.",
];

/// Left body column of page 1 (below the bold `1 Introduction` heading).
const LEFT_COLUMN: &[&str] = &[
    "Academic PDFs encode text as positioned",
    "glyph runs rather than as a reading-order",
    "stream. Faithful extraction must recover",
    "the columns before any downstream parsing",
    "can succeed [1]. Prior systems relied on",
    "heuristics tuned to single-column layouts.",
    "We instead treat every span as evidence and",
    "keep the raw bytes of each reference entry.",
];

/// Right body column of page 1.
const RIGHT_COLUMN: &[&str] = &[
    "Our contribution is a pipeline that snapshots",
    "the input, extracts positioned spans with a",
    "pure Rust backend, orders them with a",
    "recursive XY-cut, and resolves numeric",
    "citation markers such as [2, 3] against the",
    "segmented reference list. Every field that we",
    "report carries provenance, and nothing is",
    "repaired or guessed by a language model.",
];

/// Single-column body of page 2 (below the bold `2 Method` heading).
const PAGE_TWO_BODY: &[&str] = &[
    "The reference list is located by its heading and segmented",
    "into entries using the printed labels and the hanging indent",
    "of each continuation line, following Turing [3]. Each entry",
    "keeps its raw text; parsed fields are best-effort and never",
    "invented or repaired.",
];

/// Reference entries as (first line, hanging-indent continuation line).
const REFERENCE_LINES: &[(&str, &str)] = &[
    (
        "[1] A. Lovelace and C. Babbage, \"Analytical engines,\" in Proceedings of the 1st",
        "Conference on Computing, 1843, pp. 1–10.",
    ),
    (
        "[2] G. Hopper, \"Compilers,\" Journal of Programming, vol. 3, no. 2, pp. 45–67, 1952,",
        "doi:10.1000/abc456.",
    ),
    (
        "[3] A. Turing, \"On computable numbers,\"",
        "arXiv:1936.00001, 1936.",
    ),
];

/// Font, size and starting baseline for a block of consecutive lines.
struct Block<'a> {
    font: &'a str,
    size: f32,
    x: f32,
    y: f32,
    leading: f32,
}

impl<'a> Block<'a> {
    fn new(font: &'a str, size: f32, x: f32, y: f32, leading: f32) -> Self {
        Self {
            font,
            size,
            x,
            y,
            leading,
        }
    }
}

/// Encode `text` for a `WinAnsiEncoding` simple font.
fn win_ansi(text: &str) -> Vec<u8> {
    Document::encode_text(&Encoding::SimpleEncoding(b"WinAnsiEncoding"), text)
}

/// Append the operators that draw `text` with its baseline starting at (`x`, `y`).
fn text_line(ops: &mut Vec<Operation>, font: &str, size: f32, x: f32, y: f32, text: &str) {
    let font_name = Object::Name(font.as_bytes().to_vec());
    ops.push(Operation::new("BT", vec![]));
    ops.push(Operation::new("Tf", vec![font_name, Object::Real(size)]));
    ops.push(Operation::new("Td", vec![Object::Real(x), Object::Real(y)]));
    ops.push(Operation::new(
        "Tj",
        vec![Object::string_literal(win_ansi(text))],
    ));
    ops.push(Operation::new("ET", vec![]));
}

/// Draw `lines` one under another, `block.leading` points apart.
fn text_block(ops: &mut Vec<Operation>, block: &Block<'_>, lines: &[&str]) {
    let mut y = block.y;
    for line in lines {
        text_line(ops, block.font, block.size, block.x, y, line);
        y -= block.leading;
    }
}

/// Content of page 1: title, authors, abstract, two body columns, DOI footer.
///
/// The right column's baselines are offset by half a line from the left
/// column's so no two columns share a baseline; the gap between the abstract
/// and the body (24 pt between line boxes) is well above one line height.
fn page_one_ops() -> Vec<Operation> {
    let mut ops: Vec<Operation> = Vec::new();
    text_line(&mut ops, BOLD, 18.0, LEFT_X, 730.0, TITLE);
    text_line(&mut ops, REGULAR, 11.0, LEFT_X, 705.0, AUTHORS_LINE);
    text_line(&mut ops, BOLD, BODY_SIZE, LEFT_X, 675.0, "Abstract");
    let abstract_block = Block::new(REGULAR, BODY_SIZE, LEFT_X, 661.0, LEADING);
    text_block(&mut ops, &abstract_block, ABSTRACT_LINES);

    text_line(&mut ops, BOLD, BODY_SIZE, LEFT_X, 615.0, "1 Introduction");
    let left = Block::new(REGULAR, BODY_SIZE, LEFT_X, 603.0, LEADING);
    text_block(&mut ops, &left, LEFT_COLUMN);
    let right = Block::new(REGULAR, BODY_SIZE, RIGHT_X, 609.0, LEADING);
    text_block(&mut ops, &right, RIGHT_COLUMN);

    text_line(&mut ops, REGULAR, 9.0, LEFT_X, 60.0, "doi:10.1000/xyz123");
    ops
}

/// Content of page 2: a short single-column body, then the reference list
/// with three `[n]` entries of two lines each, continuation lines indented.
fn page_two_ops() -> Vec<Operation> {
    let mut ops: Vec<Operation> = Vec::new();
    text_line(&mut ops, BOLD, BODY_SIZE, LEFT_X, 730.0, "2 Method");
    let body = Block::new(REGULAR, BODY_SIZE, LEFT_X, 718.0, LEADING);
    text_block(&mut ops, &body, PAGE_TWO_BODY);

    text_line(&mut ops, BOLD, BODY_SIZE, LEFT_X, 640.0, "References");
    let mut y = 622.0;
    for (first, continuation) in REFERENCE_LINES {
        text_line(&mut ops, REGULAR, BODY_SIZE, LEFT_X, y, first);
        y -= LEADING;
        text_line(&mut ops, REGULAR, BODY_SIZE, HANGING_X, y, continuation);
        y -= LEADING;
    }
    ops
}

/// A Type1 standard-14 font dictionary with `WinAnsiEncoding`.
fn standard_font(doc: &mut Document, base_font: &str) -> ObjectId {
    doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => Object::Name(base_font.as_bytes().to_vec()),
        "Encoding" => "WinAnsiEncoding",
    })
}

/// Build the synthetic two-page paper and return its serialised PDF bytes.
#[must_use]
pub fn synthetic_paper() -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_root_id = doc.new_object_id();
    let regular_id = standard_font(&mut doc, "Helvetica");
    let bold_id = standard_font(&mut doc, "Helvetica-Bold");
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! {
            "F1" => regular_id,
            "F2" => bold_id,
        },
    });
    let media_box = Object::Array(vec![
        Object::Integer(0),
        Object::Integer(0),
        Object::Integer(PAGE_WIDTH),
        Object::Integer(PAGE_HEIGHT),
    ]);

    let mut kids: Vec<Object> = Vec::new();
    for operations in [page_one_ops(), page_two_ops()] {
        let content = Content { operations };
        let encoded = content.encode().expect("content stream encodes");
        let content_id = doc.add_object(Stream::new(dictionary! {}, encoded));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_root_id,
            "MediaBox" => media_box.clone(),
            "Resources" => resources_id,
            "Contents" => content_id,
        });
        kids.push(Object::Reference(page_id));
    }

    let pages = dictionary! {
        "Type" => "Pages",
        "Kids" => Object::Array(kids),
        "Count" => Object::Integer(2),
        "Resources" => resources_id,
        "MediaBox" => media_box,
    };
    doc.objects.insert(pages_root_id, Object::Dictionary(pages));
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_root_id,
    });
    let info_id = doc.add_object(dictionary! {
        "Producer" => Object::string_literal("tpe synthetic fixture"),
    });
    doc.trailer.set("Root", catalog_id);
    doc.trailer.set("Info", info_id);

    let mut bytes: Vec<u8> = Vec::new();
    doc.save_to(&mut bytes).expect("document serialises");
    bytes
}

/// Write `bytes` to `paper.pdf` inside a fresh temporary directory.
///
/// The directory is deleted when the returned `TempDir` is dropped, so keep
/// it alive for as long as the path is used.
#[must_use]
pub fn write_temp_pdf(bytes: &[u8]) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("paper.pdf");
    std::fs::write(&path, bytes).expect("fixture written");
    (dir, path)
}
