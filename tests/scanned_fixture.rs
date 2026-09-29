//! A synthetic scanned page (text only as pixels, no text layer) through the
//! backends: `lopdf` and `pdfium` must report no text, `pdfium` must report
//! the raster as one figure, and docling's OCR must read the text back.

mod common;

use std::path::Path;

use tpe::pipeline::run_job;
use tpe::schema::Job;

use common::raster::{
    ADVANCE, GLYPH_HEIGHT, GLYPH_ROWS, GLYPH_WIDTH, GLYPHS, IMAGE_WIDTH_PX, INK, LETTER_GAP,
    LINE_GAP, PAD_PX, PAPER, SCAN_SCALE, expected_text, glyph, line_width_px, render_lines,
    round_up_to_25, scanned_fixture,
};
use common::write_temp_pdf;

// Letters at least two empty dot columns apart, lines at least six empty
// dot rows apart, and ink dark enough to read as a scan.
const _: () = assert!(LETTER_GAP >= 2 && LINE_GAP >= 6 && INK <= 40);

/// A job for `backend` over every page of `path`.
fn job_for(path: &Path, backend: &str) -> Job {
    Job {
        path: path.to_string_lossy().into_owned(),
        backend: backend.to_string(),
        pages: None,
        password: None,
        max_bytes: None,
        max_pages: None,
        figures_dir: None,
    }
}

/// Upper-case `text` and keep only ASCII letters and digits.
fn normalise(text: &str) -> Vec<char> {
    text.chars()
        .flat_map(char::to_uppercase)
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

/// Length of the longest common subsequence of `a` and `b`.
fn lcs_len(a: &[char], b: &[char]) -> usize {
    let mut previous: Vec<usize> = vec![0; b.len() + 1];
    let mut current: Vec<usize> = vec![0; b.len() + 1];
    for &left in a {
        for (j, &right) in b.iter().enumerate() {
            current[j + 1] = if left == right {
                previous[j] + 1
            } else {
                current[j].max(previous[j + 1])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

/// The words of `expected` (split on whitespace) that do not occur exactly,
/// after upper-casing and dropping punctuation, among the whitespace-separated
/// words of `recovered`.
fn missing_words<'a>(expected: &'a str, recovered: &str) -> Vec<&'a str> {
    let found: Vec<String> = recovered
        .split_whitespace()
        .map(|word| normalise(word).into_iter().collect::<String>())
        .filter(|word| !word.is_empty())
        .collect();
    expected
        .split_whitespace()
        .filter(|word| {
            let wanted: String = normalise(word).into_iter().collect();
            !found.contains(&wanted)
        })
        .collect()
}

/// Minimum number of the fixture's words docling's OCR must read exactly.
#[cfg(feature = "docling")]
const MIN_EXACT_WORDS: usize = 3;

/// True when `needle` occurs anywhere in `haystack`.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn lcs_counts_characters_in_order() {
    let expected = normalise(expected_text());
    assert_eq!(expected.iter().collect::<String>(), "HELLOWORLDSCAN2026");
    assert_eq!(lcs_len(&expected, &expected), 18);
    assert_eq!(lcs_len(&expected, &normalise("hello, w0rld 2O26!")), 12);
    assert_eq!(
        lcs_len(&expected, &normalise("hello, w0rld scan 2O26!")),
        16
    );
    assert_eq!(lcs_len(&expected, &[]), 0);
}

#[test]
fn missing_words_compares_whole_words() {
    let expected = expected_text();
    assert_eq!(expected.split_whitespace().count(), 4);
    assert!(missing_words(expected, "hello, world\nScan 2026.").is_empty());
    assert_eq!(
        missing_words(expected, "HELLD HDRL0 SCAN 2026"),
        vec!["HELLO", "WORLD"]
    );
    assert_eq!(missing_words(expected, "HELLOWORLD SCAN2026").len(), 4);
}

#[test]
fn font_covers_letters_digits_and_space_with_distinct_glyphs() {
    assert_eq!(usize::try_from(GLYPH_HEIGHT).unwrap(), GLYPH_ROWS);
    let width = usize::try_from(GLYPH_WIDTH).unwrap();
    for (c, rows) in &GLYPHS {
        for row in rows {
            assert_eq!(row.len(), width, "glyph {c:?} row {row:?}");
            assert!(row.bytes().all(|dot| dot == b'#' || dot == b'.'), "{c:?}");
        }
        let inked = rows.iter().any(|row| row.contains('#'));
        assert_eq!(inked, *c != ' ', "glyph {c:?}");
    }
    let wanted: Vec<char> = std::iter::once(' ')
        .chain('A'..='Z')
        .chain('0'..='9')
        .collect();
    for c in &wanted {
        assert_eq!(
            GLYPHS.iter().filter(|(key, _)| key == c).count(),
            1,
            "glyph {c:?} must occur once"
        );
    }
    assert_eq!(GLYPHS.len(), wanted.len());
    for (index, (first, first_rows)) in GLYPHS.iter().enumerate() {
        for (second, second_rows) in &GLYPHS[index + 1..] {
            assert_ne!(
                first_rows, second_rows,
                "{first:?} and {second:?} look the same"
            );
        }
    }
    assert_eq!(glyph('o'), glyph('O'));
    // Rounded, not square: the corner dots of O and 0 are paper.
    for c in ['O', '0'] {
        let rows = glyph(c);
        for row in [rows[0], rows[GLYPH_ROWS - 1]] {
            assert!(row.starts_with('.') && row.ends_with('.'), "{c:?}: {row:?}");
        }
    }
}

#[test]
fn raster_draws_one_centred_line_of_large_soft_glyphs() {
    let text = expected_text();
    let image = render_lines(&[text], SCAN_SCALE);
    let glyph_px = GLYPH_HEIGHT * SCAN_SCALE;
    // A capital letter 40 to 60 pixels tall, like a heading in a 300 dpi scan.
    assert!(
        (40..=60).contains(&glyph_px),
        "glyphs are {glyph_px} px tall"
    );
    assert_eq!(image.width, IMAGE_WIDTH_PX);
    assert_eq!(image.height, round_up_to_25(2 * PAD_PX + glyph_px));
    assert_eq!(image.height % 25, 0);
    let width = usize::try_from(image.width).unwrap();
    let height = usize::try_from(image.height).unwrap();
    assert_eq!(image.pixels.len(), width * height);

    // Soft edges: solid ink inside the strokes, paper outside, grey between.
    assert_eq!(image.pixels.iter().min(), Some(&INK));
    assert_eq!(image.pixels.iter().max(), Some(&PAPER));
    assert!(
        image
            .pixels
            .iter()
            .any(|&value| value > INK && value < PAPER)
    );

    // The blur spreads ink one pixel beyond each glyph cell's inked dots.
    // `H` inks its top, bottom and left dot rows and columns.
    let top = usize::try_from((image.height - glyph_px) / 2).unwrap();
    let glyph_rows_px = usize::try_from(glyph_px).unwrap();
    let inked_rows: Vec<usize> = image
        .pixels
        .chunks(width)
        .enumerate()
        .filter(|(_, row)| row.iter().any(|&value| value < PAPER))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(inked_rows.first(), Some(&(top - 1)));
    assert_eq!(inked_rows.last(), Some(&(top + glyph_rows_px)));
    assert_eq!(inked_rows.len(), glyph_rows_px + 2);

    let scale = usize::try_from(SCAN_SCALE).unwrap();
    let left = usize::try_from((IMAGE_WIDTH_PX - line_width_px(text, SCAN_SCALE)) / 2).unwrap();
    let chars = text.chars().count();
    let last_char = text.chars().next_back().unwrap();
    let last_dot = glyph(last_char)
        .iter()
        .filter_map(|row| row.rfind('#'))
        .max()
        .unwrap();
    let advance = usize::try_from(ADVANCE).unwrap();
    let inked_columns: Vec<usize> = (0..width)
        .filter(|&column| image.pixels.chunks(width).any(|row| row[column] < PAPER))
        .collect();
    assert_eq!(inked_columns.first(), Some(&(left - 1)));
    assert_eq!(
        inked_columns.last(),
        Some(&(left + (chars - 1) * advance * scale + (last_dot + 1) * scale))
    );
}

#[test]
fn scanned_pdf_is_image_only_and_unfiltered() {
    let bytes = scanned_fixture();
    assert!(bytes.starts_with(b"%PDF-1.5"), "fixture must be a PDF");
    assert!(contains_bytes(&bytes, b"/Im0 Do"));
    assert!(contains_bytes(&bytes, b"/DeviceGray"));
    assert!(!contains_bytes(&bytes, b"/Filter"), "streams must be raw");
    assert!(!contains_bytes(&bytes, b"/Font"), "no fonts on a scan");
    assert!(!contains_bytes(&bytes, b"BT"), "no text objects on a scan");
}

/// The `lopdf` backend reads text operators and records image `XObject`s as
/// `raster` figures without decoding them: no text comes back, one figure does.
#[test]
fn lopdf_backend_sees_no_text_and_one_figure() {
    let (_dir, path) = write_temp_pdf(&scanned_fixture());
    let result = run_job(&job_for(&path, "lopdf")).expect("lopdf reads the scanned page");
    assert_eq!(result.document.pages, 1);
    assert_eq!(result.pages.len(), 1);
    let page = &result.pages[0];
    let non_blank = page
        .spans
        .iter()
        .filter(|span| !span.text.trim().is_empty())
        .count();
    assert_eq!(non_blank, 0, "spans: {:?}", page.spans);
    assert!(page.text.trim().is_empty(), "text: {:?}", page.text);
    let rasters: Vec<_> = page
        .figures
        .iter()
        .filter(|figure| figure.kind == "raster")
        .collect();
    assert_eq!(rasters.len(), 1, "figures: {:?}", page.figures);
    assert!(rasters[0].bbox.is_some(), "raster figure carries a box");
}

#[cfg(feature = "pdfium")]
#[test]
fn pdfium_backend_reports_the_raster_figure() {
    use tpe::backend::BackendError;

    let bytes = scanned_fixture();
    let image = render_lines(&[expected_text()], SCAN_SCALE);
    let backend = tpe::backend::by_name("pdfium").expect("pdfium backend is registered");
    let page = {
        let mut session = match backend.open(&bytes, None) {
            Ok(session) => session,
            Err(BackendError::Unsupported(message)) => {
                eprintln!("skipped: {message}");
                return;
            }
            Err(other) => panic!("pdfium could not open the scanned fixture: {other}"),
        };
        assert_eq!(session.page_count(), 1);
        session.page_text(1).expect("pdfium reads the scanned page")
    };

    let non_blank = page
        .spans
        .iter()
        .filter(|span| !span.text.trim().is_empty())
        .count();
    assert_eq!(non_blank, 0, "spans: {:?}", page.spans);
    assert_eq!(page.figures.len(), 1, "figures: {:?}", page.figures);
    let figure = &page.figures[0];
    assert_eq!(figure.kind, "raster");
    assert_eq!(figure.width_px, Some(image.width));
    assert_eq!(figure.height_px, Some(image.height));

    // Placed at 300 dpi across the full width, top edge on the page top.
    let height_pt = f32::from(u16::try_from(image.height * 6 / 25).unwrap());
    let bbox = figure.bbox.expect("figure has a box");
    let close = |actual: f32, expected: f32| (actual - expected).abs() <= 1.0;
    assert!(close(bbox.x0, 0.0), "{bbox:?}");
    assert!(close(bbox.x1, 612.0), "{bbox:?}");
    assert!(close(bbox.y1, 792.0), "{bbox:?}");
    assert!(close(bbox.y0, 792.0 - height_pt), "{bbox:?}");
}

/// The docling model files the OCR path needs, as named in `docs/NATIVE.md`.
#[cfg(feature = "docling")]
const DOCLING_MODELS: [&str; 4] = [
    "layout_heron_int8.onnx",
    "ocr_det.onnx",
    "ocr_rec_en.onnx",
    "en_dict.txt",
];

/// Why the docling OCR test cannot run here, or `None` when it can: every
/// model must be under `.models/` or `$DOCLING_RS_MODELS_DIR`, and a
/// `pdfium` library must bind.
#[cfg(feature = "docling")]
fn docling_missing_prerequisite(bytes: &[u8]) -> Option<String> {
    use tpe::backend::BackendError;

    let models_dir = std::env::var("DOCLING_RS_MODELS_DIR").ok();
    for model in DOCLING_MODELS {
        let in_cwd = Path::new(".models").join(model).is_file();
        let in_env = models_dir
            .as_deref()
            .is_some_and(|dir| Path::new(dir).join(model).is_file());
        if !in_cwd && !in_env {
            return Some(format!(
                "docling model {model} not found under .models/ or DOCLING_RS_MODELS_DIR"
            ));
        }
    }
    let pdfium = tpe::backend::by_name("pdfium").expect("pdfium backend is registered");
    match pdfium.open(bytes, None) {
        Err(BackendError::Unsupported(message)) => Some(message),
        // Any other outcome means the library bound; the session (and its
        // process-wide gate) is dropped here, before docling runs.
        Ok(_) | Err(_) => None,
    }
}

#[cfg(feature = "docling")]
#[test]
fn docling_ocr_recovers_scanned_text() {
    let bytes = scanned_fixture();
    if let Some(reason) = docling_missing_prerequisite(&bytes) {
        eprintln!("skipped: {reason}");
        return;
    }
    let (_dir, path) = write_temp_pdf(&bytes);
    let result = run_job(&job_for(&path, "docling")).expect("docling converts the scanned page");
    assert_eq!(result.pages.len(), 1);

    for page in &result.pages {
        for span in &page.spans {
            assert!(
                !span.text.contains("data:image") && !span.text.contains("base64"),
                "pixel data leaked into a span: {:?}",
                span.text
            );
        }
        assert!(!page.text.contains("data:image") && !page.text.contains("base64"));
    }

    // The ordered page text; the spans in backend order if it is blank.
    let recovered: String = result
        .pages
        .iter()
        .map(|page| {
            if page.text.trim().is_empty() {
                page.spans
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<Vec<&str>>()
                    .join(" ")
            } else {
                page.text.clone()
            }
        })
        .collect::<Vec<String>>()
        .join("\n");
    eprintln!("docling OCR recovered: {recovered:?}");
    eprintln!("page warnings: {:?}", result.pages[0].warnings);

    let expected = normalise(expected_text());
    let found = normalise(&recovered);
    let common_chars = lcs_len(&expected, &found);
    let missing = missing_words(expected_text(), &recovered);
    let word_count = expected_text().split_whitespace().count();
    let exact_words = word_count - missing.len();
    eprintln!(
        "{common_chars} of {} characters in order; {exact_words} of {word_count} words exact, \
         missing {missing:?}",
        expected.len()
    );
    // At least 80 % of the expected characters, in order.
    assert!(
        common_chars * 5 >= expected.len() * 4,
        "recovered {common_chars} of {} characters in order from {recovered:?}",
        expected.len()
    );
    assert!(
        exact_words >= MIN_EXACT_WORDS,
        "recovered {exact_words} of {word_count} words exactly from {recovered:?}; \
         missing {missing:?}"
    );
}
