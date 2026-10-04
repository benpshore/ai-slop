//! Optional spatial text projection using `LiteParse`'s pure stage API.
//!
//! PDF decoding, Unicode evidence, native provenance and figure handling stay
//! with the existing `PdfiumBackend`. This module never initializes `LiteParse`'s
//! separate `PDFium` wrapper, enables OCR, converts files, or starts processes.
//! It consumes only Rust-owned spans after the native session has closed.
//!
//! The output is a spatial character grid, useful for aligned forms and table
//! rows. It is not tagged-PDF structure or independent extraction evidence.
//! Original spans, fonts, boxes, figures and warnings remain available. Grid
//! rows have no invented source box or span attribution. Projection is accepted
//! only when it preserves the complete multiset of non-whitespace characters;
//! otherwise ordinary native layout is retained with an explicit Partial warning.

use std::collections::BTreeMap;

use liteparse::types::{Page, TextItem};

use super::pdfium_backend::PdfiumBackend;
use super::{BackendError, DocumentSession, Extractor};
use crate::schema::{BackendIdentity, Line, PageText, config_digest};

const LITEPARSE_VERSION: &str = "2.15.1";
const MAX_ITEMS: usize = 4096;
const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_GRID_COLUMNS: f32 = 4096.0;
const MAX_GRID_ROWS: f32 = 16384.0;

/// Opt-in spatial projection of the existing native `PDFium` evidence.
#[derive(Clone, Debug, Default)]
pub struct LiteParseLayoutBackend {
    pub pdfium: PdfiumBackend,
}

impl Extractor for LiteParseLayoutBackend {
    fn identity(&self) -> BackendIdentity {
        let native = self.pdfium.identity();
        let config = BTreeMap::from([
            ("source_backend".to_string(), native.name),
            ("source_version".to_string(), native.version.clone()),
            ("source_config".to_string(), native.config_digest),
            ("layout_policy".to_string(), "1".to_string()),
            ("max_items".to_string(), MAX_ITEMS.to_string()),
            ("max_text_bytes".to_string(), MAX_TEXT_BYTES.to_string()),
            ("max_grid_columns".to_string(), MAX_GRID_COLUMNS.to_string()),
            ("max_grid_rows".to_string(), MAX_GRID_ROWS.to_string()),
        ]);
        BackendIdentity {
            name: "liteparse-layout".to_string(),
            version: format!("{LITEPARSE_VERSION}+{}", native.version),
            config_digest: config_digest(&config),
        }
    }

    fn open(
        &self,
        bytes: &[u8],
        password: Option<&str>,
    ) -> Result<Box<dyn DocumentSession>, BackendError> {
        Ok(Box::new(LayoutSession {
            native: self.pdfium.open(bytes, password)?,
        }))
    }

    fn provides_reading_order(&self) -> bool {
        true
    }

    fn provides_line_layout(&self) -> bool {
        true
    }
}

struct LayoutSession {
    native: Box<dyn DocumentSession>,
}

impl DocumentSession for LayoutSession {
    fn page_count(&self) -> u32 {
        self.native.page_count()
    }

    fn page_text(&mut self, page: u32) -> Result<PageText, BackendError> {
        let mut text = self.native.page_text(page)?;
        project_page(&mut text);
        Ok(text)
    }

    fn info(&self) -> BTreeMap<String, String> {
        self.native.info()
    }

    fn take_figure_bytes(&mut self, page: u32, index: u32) -> Option<Vec<u8>> {
        self.native.take_figure_bytes(page, index)
    }
}

/// Apply only `stages::project`; no `LiteParse` native handle or parser is used.
fn project_page(page: &mut PageText) {
    match spatial_text(page) {
        Ok(text) => {
            page.lines = text
                .lines()
                .map(|text| Line {
                    text: text.to_string(),
                    ..Line::default()
                })
                .collect();
            page.text = text;
        }
        Err(reason) => {
            // Preserve source evidence even if the ordinary layout pass applies
            // a page rotation to its working copy's geometry.
            let mut fallback = page.clone();
            crate::reading_order::order_page(&mut fallback);
            page.lines = fallback.lines;
            page.text = fallback.text;
            page.warnings = fallback.warnings;
            page.warnings.push(format!(
                "extraction_incomplete: liteparse-layout unavailable ({reason}); native layout retained"
            ));
        }
    }
}

fn spatial_text(page: &PageText) -> Result<String, &'static str> {
    if page.rotation != 0 {
        return Err("rotated pages require native layout");
    }
    if !page.width.is_finite()
        || !page.height.is_finite()
        || page.width <= 0.0
        || page.height <= 0.0
    {
        return Err("invalid page geometry");
    }
    if page.spans.len() > MAX_ITEMS {
        return Err("4096-item projection limit exceeded");
    }
    let mut text_bytes = 0_usize;
    let mut text_items = Vec::with_capacity(page.spans.len());
    for span in &page.spans {
        text_bytes = text_bytes.saturating_add(span.text.len());
        if text_bytes > MAX_TEXT_BYTES {
            return Err("1 MiB source-text projection limit exceeded");
        }
        if span.text.trim().is_empty() {
            continue;
        }
        let bbox = span.bbox.ok_or("source text has no geometry")?;
        let width = bbox.x1 - bbox.x0;
        let height = bbox.y1 - bbox.y0;
        if ![bbox.x0, bbox.y0, bbox.x1, bbox.y1, width, height]
            .into_iter()
            .all(f32::is_finite)
            || bbox.x0 < 0.0
            || bbox.y0 < 0.0
            || bbox.x1 > page.width
            || bbox.y1 > page.height
            || width <= 0.0
            || height <= 0.0
        {
            return Err("source text has invalid or out-of-page geometry");
        }
        // Avoid a tiny inferred character/line size inducing an enormous grid.
        // Native worker address-space/time limits remain the hard outer bound.
        #[allow(clippy::cast_precision_loss)]
        let characters = span.text.chars().count() as f32;
        if page.width / (width / characters.max(1.0)) > MAX_GRID_COLUMNS
            || page.height / height > MAX_GRID_ROWS
        {
            return Err("spatial grid dimension limit exceeded");
        }
        let size = span.size.filter(|size| size.is_finite() && *size > 0.0);
        text_items.push(TextItem {
            text: span.text.clone(),
            x: bbox.x0,
            y: page.height - bbox.y1,
            width,
            height,
            font_name: span.font.clone(),
            font_size: size,
            font_height: size,
            ..TextItem::default()
        });
    }
    if text_items.is_empty() {
        return Ok(String::new());
    }
    let projected = liteparse::stages::project(vec![Page {
        page_number: usize::try_from(page.page).map_err(|_| "page number overflow")?,
        page_label: None,
        page_width: page.width,
        page_height: page.height,
        content_bounds: None,
        text_items,
        graphics: Vec::new(),
        vector_graphics: None,
        struct_nodes: Vec::new(),
        image_refs: Vec::new(),
        annotations: None,
        form_fields: None,
        structure_tree: None,
    }]);
    let text = projected
        .into_iter()
        .next()
        .ok_or("projection returned no page")?
        .text;
    if text.len() > MAX_TEXT_BYTES {
        return Err("1 MiB output-grid limit exceeded");
    }
    let source = character_counts(page.spans.iter().map(|span| span.text.as_str()));
    if source != character_counts(std::iter::once(text.as_str())) {
        return Err("projection changed source-character coverage");
    }
    Ok(text)
}

fn character_counts<'a>(texts: impl Iterator<Item = &'a str>) -> BTreeMap<char, usize> {
    let mut counts = BTreeMap::new();
    for character in texts
        .flat_map(str::chars)
        .filter(|character| !character.is_whitespace())
    {
        *counts.entry(character).or_default() += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{BBox, Figure, Job, Link, Span, Status};

    fn span(text: &str, x: f32, y: f32, seq: u32) -> Span {
        #[allow(clippy::cast_precision_loss)]
        let width = text.chars().count() as f32 * 6.0;
        Span {
            text: text.to_string(),
            bbox: Some(BBox {
                x0: x,
                y0: y,
                x1: x + width,
                y1: y + 12.0,
            }),
            font: Some("Courier".to_string()),
            size: Some(12.0),
            seq,
        }
    }

    fn aligned_page() -> PageText {
        let mut page = PageText::new(1, 612.0, 792.0, 0);
        page.spans = vec![
            span("Alpha", 40.0, 700.0, 0),
            span("123", 300.0, 700.0, 1),
            span("Beta", 40.0, 680.0, 2),
            span("456", 300.0, 680.0, 3),
        ];
        page
    }

    #[test]
    fn pure_projection_preserves_aligned_rows_and_native_evidence() {
        let mut page = aligned_page();
        page.warnings
            .push("unicode_mapping: native mapping unresolved".to_string());
        page.links.push(Link {
            uri: "https://doi.org/10.1000/reference".to_string(),
            bbox: page.spans[0].bbox,
        });
        page.figures.push(Figure {
            index: 0,
            bbox: page.spans[0].bbox,
            kind: "raster".to_string(),
            mime: Some("image/png".to_string()),
            width_px: Some(30),
            height_px: Some(20),
            sha256: Some(crate::schema::sha256_hex(b"retained source image")),
            file: None,
            caption: None,
        });
        let original = page.clone();
        project_page(&mut page);
        assert_eq!(page.spans, original.spans);
        assert_eq!(page.figures, original.figures);
        assert_eq!(page.links, original.links);
        assert_eq!(page.warnings, original.warnings);
        let first = page
            .lines
            .iter()
            .find(|line| line.text.contains("Alpha"))
            .unwrap();
        let second = page
            .lines
            .iter()
            .find(|line| line.text.contains("Beta"))
            .unwrap();
        assert!(first.text.contains("123"), "{}", page.text);
        assert!(second.text.contains("456"), "{}", page.text);
        assert_eq!(first.text.find("123"), second.text.find("456"));
        assert!(first.text.contains("  "));
        assert!(
            page.lines
                .iter()
                .all(|line| line.bbox.is_none() && line.spans.is_empty())
        );
        assert_eq!(page.extraction_status(), Status::Partial);
    }

    #[test]
    fn missing_geometry_is_preserved_and_never_claims_complete_projection() {
        let mut page = aligned_page();
        page.spans[0].bbox = None;
        let original = page.spans.clone();
        project_page(&mut page);
        assert_eq!(page.spans, original);
        assert!(page.text.contains("Alpha"));
        assert_eq!(page.extraction_status(), Status::Partial);
        assert!(
            page.warnings
                .iter()
                .any(|warning| warning.contains("no geometry"))
        );
    }

    #[test]
    fn duplicate_text_cannot_silently_disappear_during_projection() {
        let mut page = aligned_page();
        page.spans.push(page.spans[0].clone());
        let original = page.spans.clone();
        project_page(&mut page);
        assert_eq!(page.spans, original);
        let source = character_counts(original.iter().map(|span| span.text.as_str()));
        if source != character_counts(std::iter::once(page.text.as_str())) {
            assert_eq!(page.extraction_status(), Status::Partial);
        }
    }

    #[test]
    fn oversized_or_rotated_projection_keeps_source_and_reports_partial() {
        for rotation in [0, 90] {
            let mut page = aligned_page();
            page.rotation = rotation;
            if rotation == 0 {
                page.spans[0].bbox.as_mut().unwrap().x1 = 40.00001;
            }
            let spans = page.spans.clone();
            project_page(&mut page);
            assert_eq!(page.spans, spans);
            assert_eq!(page.extraction_status(), Status::Partial);
        }
    }

    #[test]
    fn dependency_version_matches_lockfile() {
        let lock = include_str!("../../Cargo.lock");
        let package = lock
            .split("[[package]]")
            .find(|entry| entry.contains("name = \"liteparse\"\n"))
            .unwrap();
        assert!(package.contains(&format!("version = \"{LITEPARSE_VERSION}\"\n")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "run alone to inspect only this projection's process mappings"]
    fn pure_stage_never_loads_a_pdfium_library() {
        let native_loaded = || {
            std::fs::read_to_string("/proc/self/maps")
                .unwrap()
                .contains("libpdfium")
        };
        assert!(!native_loaded());
        let mut page = aligned_page();
        project_page(&mut page);
        assert!(page.text.contains("Alpha"));
        assert!(!native_loaded());
    }

    fn aligned_pdf() -> Vec<u8> {
        use lopdf::content::{Content, Operation};
        use lopdf::{Document, Object, Stream, dictionary};
        let mut doc = Document::with_version("1.7");
        let tree = doc.new_object_id();
        let font = doc.add_object(
            dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Courier" },
        );
        let mut operations = Vec::new();
        for (text, x, y) in [
            ("Alpha", 40, 700),
            ("123", 300, 700),
            ("Beta", 40, 680),
            ("456", 300, 680),
        ] {
            operations.extend([
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![x.into(), y.into()]),
                Operation::new("Tj", vec![Object::string_literal(text)]),
                Operation::new("ET", vec![]),
            ]);
        }
        let stream = doc.add_object(Stream::new(
            dictionary! {},
            Content { operations }.encode().unwrap(),
        ));
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => tree, "Contents" => stream,
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
        });
        doc.objects.insert(
            tree,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            }),
        );
        let root = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => tree });
        doc.trailer.set("Root", root);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn native_wrapper_pipeline_retains_grid_and_reopens_pdfium_safely() {
        if std::env::var_os("PDFIUM_DYNAMIC_LIB_PATH").is_none() {
            eprintln!("skipped: set PDFIUM_DYNAMIC_LIB_PATH for native layout integration");
            return;
        }
        let bytes = aligned_pdf();
        let native = PdfiumBackend::default();
        let expected = native.open(&bytes, None).unwrap().page_text(1).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("aligned.pdf");
        std::fs::write(&path, &bytes).unwrap();
        let job = Job {
            path: path.to_string_lossy().into_owned(),
            backend: "liteparse-layout".to_string(),
            pages: None,
            password: None,
            max_bytes: None,
            figures_dir: None,
        };
        let result =
            crate::pipeline::run_job_with(&LiteParseLayoutBackend::default(), &job).unwrap();
        assert_eq!(result.pages.len(), 1);
        let page = &result.pages[0];
        assert_eq!(page.spans, expected.spans);
        assert_eq!(page.links, expected.links);
        assert_eq!(page.figures, expected.figures);
        let first = page
            .lines
            .iter()
            .find(|line| line.text.contains("Alpha"))
            .unwrap();
        let second = page
            .lines
            .iter()
            .find(|line| line.text.contains("Beta"))
            .unwrap();
        assert!(first.text.contains("123"), "{}", page.text);
        assert_eq!(first.text.find("123"), second.text.find("456"));
        assert_eq!(result.status, Status::Complete);
        assert_eq!(native.open(&bytes, None).unwrap().page_count(), 1);
    }
}
