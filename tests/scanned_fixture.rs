//! A synthetic scanned page (text only as pixels, no text layer) through the
//! backends: `lopdf` and `pdfium` must report no text, `pdfium` must report
//! the raster as one figure, and docling's OCR must read the text back.

mod common;

use std::path::Path;

use tpe::pipeline::run_job;
use tpe::schema::Job;

use common::raster::{SCAN_SCALE, expected_text, render_lines, scanned_fixture};
use common::write_temp_pdf;

/// A job for `backend` over every page of `path`.
fn job_for(path: &Path, backend: &str) -> Job {
    Job {
        path: path.to_string_lossy().into_owned(),
        backend: backend.to_string(),
        pages: None,
        password: None,
        max_bytes: None,
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

/// True when `needle` occurs anywhere in `haystack`.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn lcs_counts_characters_in_order() {
    let expected = normalise(expected_text());
    assert_eq!(expected.iter().collect::<String>(), "HELLOWORLD2026");
    assert_eq!(lcs_len(&expected, &expected), 14);
    assert_eq!(lcs_len(&expected, &normalise("hello, w0rld 2O26!")), 12);
    assert_eq!(lcs_len(&expected, &[]), 0);
}

#[test]
fn raster_draws_one_line_of_large_glyphs() {
    let image = render_lines(&[expected_text()], SCAN_SCALE);
    assert_eq!(image.width, 2550);
    assert_eq!(image.height % 25, 0);
    let width = usize::try_from(image.width).unwrap();
    let height = usize::try_from(image.height).unwrap();
    assert_eq!(image.pixels.len(), width * height);
    let inked_rows: Vec<usize> = image
        .pixels
        .chunks(width)
        .enumerate()
        .filter(|(_, row)| row.contains(&0))
        .map(|(index, _)| index)
        .collect();
    // Seven font dots at scale 7: one band of 49 pixel rows, one inch down.
    assert_eq!(inked_rows.first(), Some(&300));
    assert_eq!(inked_rows.last(), Some(&348));
    assert_eq!(inked_rows.len(), 49);
    assert!(image.pixels.iter().all(|&value| value == 0 || value == 255));
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

/// The `lopdf` backend reads text operators only; it does not report image
/// `XObject`s as figures, so the only observable is that no text comes back.
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
    // At least 80 % of the expected characters, in order.
    assert!(
        common_chars * 5 >= expected.len() * 4,
        "recovered {common_chars} of {} characters in order from {recovered:?}",
        expected.len()
    );
}
