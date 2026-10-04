//! Docling's pinned pure-Rust page parser, without `PDFium`, model discovery,
//! inference, or subprocesses. Each requested page is parsed on demand. Rust
//! owns ordering and cleanup; native evidence supplies annotations/images and
//! a coverage check because upstream does not report dropped glyphs.
use std::collections::BTreeMap;

use docling_pdf::textparse::PageTextParser;
use lopdf::{Document, ObjectId};
use unicode_normalization::UnicodeNormalization;

use super::{BackendError, DocumentSession, Extractor, lopdf_backend::LopdfBackend};
use crate::schema::{BBox, BackendIdentity, PageText, Span, config_digest};

const VERSION: &str = "1.69.2";
const MAX_CELLS: usize = 200_000;
const MAX_CHARACTERS: usize = 1_000_000;

/// No runtime assets or environment-selected OCR engines are consulted.
#[derive(Clone, Copy, Debug, Default)]
pub struct DoclingTextBackend;
impl Extractor for DoclingTextBackend {
    fn identity(&self) -> BackendIdentity {
        let native = LopdfBackend::default().identity();
        let config = BTreeMap::from([
            (
                "parser".to_string(),
                "retained-per-page-word-cells".to_string(),
            ),
            ("evidence_policy".to_string(), "1".to_string()),
            ("native_version".to_string(), native.version),
            ("native_config".to_string(), native.config_digest),
        ]);
        BackendIdentity {
            name: "docling-text".to_string(),
            version: VERSION.to_string(),
            config_digest: config_digest(&config),
        }
    }
    fn open(
        &self,
        bytes: &[u8],
        password: Option<&str>,
    ) -> Result<Box<dyn DocumentSession>, BackendError> {
        let native = LopdfBackend::default().open(bytes, password)?;
        if native.page_count() == 0 {
            return Err(BackendError::Malformed("document has no pages".to_string()));
        }
        let doc = Document::load_mem(bytes).map_err(|e| BackendError::Malformed(e.to_string()))?;
        if doc.is_encrypted() {
            return Err(BackendError::Unsupported(
                "docling-text has no password API; choose a native backend for encrypted files"
                    .to_string(),
            ));
        }
        let boxes = doc
            .get_pages()
            .values()
            .map(|id| page_box(&doc, *id))
            .collect();
        drop(doc);
        let parser = PageTextParser::open(bytes).ok_or_else(|| {
            BackendError::Malformed("docling-text parser could not open the document".to_string())
        })?;
        Ok(Box::new(Session {
            native,
            parser,
            boxes,
        }))
    }
}
struct Session {
    native: Box<dyn DocumentSession>,
    parser: PageTextParser,
    boxes: Vec<Option<BBox>>,
}
impl DocumentSession for Session {
    fn page_count(&self) -> u32 {
        self.native.page_count()
    }
    fn info(&self) -> BTreeMap<String, String> {
        self.native.info()
    }
    fn take_figure_bytes(&mut self, page: u32, index: u32) -> Option<Vec<u8>> {
        self.native.take_figure_bytes(page, index)
    }
    fn page_text(&mut self, page: u32) -> Result<PageText, BackendError> {
        let mut native = self.native.page_text(page)?;
        let index = (page - 1) as usize;
        let Some(bounds) = self.boxes.get(index).copied().flatten() else {
            native.warnings.push("extraction_incomplete: docling-text page geometry is unverified; native text retained".to_string());
            return Ok(native);
        };
        let cells = self.parser.cells(index).words;
        if cells.len() > MAX_CELLS {
            native.warnings.push(
                "resource_limit: docling-text exceeded page cell limit; native text retained"
                    .to_string(),
            );
            return Ok(native);
        }
        let mut candidate = Vec::with_capacity(cells.len());
        let mut characters = 0;
        for cell in cells {
            characters += cell.text.chars().count();
            if characters > MAX_CHARACTERS {
                native.warnings.push("resource_limit: docling-text exceeded page character limit; native text retained".to_string());
                return Ok(native);
            }
            if ![cell.l, cell.t, cell.r, cell.b]
                .iter()
                .all(|v| v.is_finite())
                || cell.l > cell.r
                || cell.t > cell.b
            {
                native.warnings.push("extraction_incomplete: docling-text returned invalid cell geometry; native text retained".to_string());
                return Ok(native);
            }
            candidate.push(Span {
                text: cell.text.nfc().collect(),
                bbox: Some(BBox {
                    x0: bounds.x0 + cell.l,
                    y0: bounds.y1 - cell.b,
                    x1: bounds.x0 + cell.r,
                    y1: bounds.y1 - cell.t,
                }),
                font: None,
                size: None,
                seq: u32::try_from(candidate.len()).map_err(|e| BackendError::Page {
                    page,
                    message: e.to_string(),
                })?,
            });
        }
        merge_candidate(&mut native, candidate);
        crate::router::mark_incomplete(&mut native);
        Ok(native)
    }
}

fn coverage(spans: &[Span]) -> BTreeMap<char, usize> {
    let mut result = BTreeMap::new();
    for ch in spans
        .iter()
        .flat_map(|s| s.text.nfc())
        .filter(|c| !c.is_whitespace())
    {
        *result.entry(ch).or_insert(0) += 1;
    }
    result
}
fn merge_candidate(native: &mut PageText, candidate: Vec<Span>) {
    let source = coverage(&native.spans);
    let proposed = coverage(&candidate);
    if source != proposed {
        native.warnings.push(
            "extraction_incomplete: docling-text scalar coverage differs from native evidence"
                .to_string(),
        );
        // A silent upstream drop must not erase visible native evidence.
        if proposed.values().sum::<usize>() < source.values().sum::<usize>() {
            native
                .warnings
                .push("docling-text: shorter candidate rejected; native text retained".to_string());
            return;
        }
    }
    native.spans = candidate;
}

// The pinned upstream parser translates cells by its clipped CropBox. Reproduce
// that frame explicitly, with bounded inheritance; absent geometry is never
// replaced with an invented Letter-size page.
pub(super) fn page_box(doc: &Document, page: ObjectId) -> Option<BBox> {
    let media = inherited_rect(doc, page, b"MediaBox")?;
    let crop = inherited_rect(doc, page, b"CropBox")
        .map(|crop| BBox {
            x0: crop.x0.max(media.x0),
            y0: crop.y0.max(media.y0),
            x1: crop.x1.min(media.x1),
            y1: crop.y1.min(media.y1),
        })
        .filter(|b| b.x1 > b.x0 && b.y1 > b.y0)
        .unwrap_or(media);
    Some(crop)
}
fn inherited_rect(doc: &Document, mut id: ObjectId, key: &[u8]) -> Option<BBox> {
    for _ in 0..32 {
        let dict = doc.get_dictionary(id).ok()?;
        if let Ok(value) = dict.get(key)
            && let Ok((_, value)) = doc.dereference(value)
            && let Ok(values) = value.as_array()
        {
            if values.len() != 4 {
                return None;
            }
            let mut coords = [0.0; 4];
            for (slot, value) in coords.iter_mut().zip(values) {
                *slot = value.as_float().ok()?;
            }
            if coords.iter().any(|n| !n.is_finite()) {
                return None;
            }
            let bounds = BBox {
                x0: coords[0].min(coords[2]),
                y0: coords[1].min(coords[3]),
                x1: coords[0].max(coords[2]),
                y1: coords[1].max(coords[3]),
            };
            return (bounds.x1 > bounds.x0 && bounds.y1 > bounds.y0).then_some(bounds);
        }
        id = dict.get(b"Parent").ok()?.as_reference().ok()?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Object, dictionary};
    fn span(text: &str) -> Span {
        Span {
            text: text.to_string(),
            bbox: None,
            font: None,
            size: None,
            seq: 0,
        }
    }
    #[test]
    fn silent_upstream_drop_keeps_native_text_and_marks_partial() {
        let mut page = PageText::new(1, 100.0, 100.0, 0);
        page.spans = vec![span("ABC")];
        merge_candidate(&mut page, vec![span("AC")]);
        assert_eq!(page.spans[0].text, "ABC");
        assert_eq!(page.extraction_status(), crate::schema::Status::Partial);
    }
    #[test]
    fn null_cropbox_keeps_upstream_inheritance_frame() {
        let mut doc = Document::load_mem(&super::super::probe_pdf().unwrap()).unwrap();
        let id = doc.get_pages()[&1];
        let parent = doc
            .get_dictionary(id)
            .unwrap()
            .get(b"Parent")
            .unwrap()
            .as_reference()
            .unwrap();
        doc.get_object_mut(parent)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set(
                "CropBox",
                vec![10.into(), 20.into(), 510.into(), 770.into()],
            );
        doc.get_object_mut(id)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("CropBox", Object::Null);
        let bounds = page_box(&doc, id).unwrap();
        assert_eq!(
            (bounds.x0, bounds.y0, bounds.x1, bounds.y1),
            (10.0, 20.0, 510.0, 770.0)
        );
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let mut session = DoclingTextBackend.open(&bytes, None).unwrap();
        let page = session.page_text(1).unwrap();
        assert!((page.spans[0].bbox.unwrap().x0 - 72.0).abs() < 1.0);
    }
    #[test]
    fn actual_parser_retains_raw_links_crop_offsets_and_page_ranges() {
        let mut doc = Document::load_mem(&super::super::probe_pdf().unwrap()).unwrap();
        let id = doc.get_pages()[&1];
        let annotation=doc.add_object(dictionary! {"Subtype"=>"Link","Rect"=>vec![72.into(),710.into(),105.into(),732.into()],"A"=>dictionary!{"S"=>"URI","URI"=>Object::string_literal("https://doi.org/10.1234/exact")}});
        let page = doc.get_object_mut(id).unwrap().as_dict_mut().unwrap();
        page.set(
            "CropBox",
            vec![10.into(), 20.into(), 510.into(), 770.into()],
        );
        page.set("Rotate", 90);
        page.set("Annots", vec![Object::Reference(annotation)]);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let mut session = DoclingTextBackend.open(&bytes, None).unwrap();
        drop(bytes);
        let page = session.page_text(1).unwrap();
        assert!(page.spans.iter().any(|s| s.text == "probe"));
        assert_eq!(page.rotation, 90);
        assert_eq!(page.links[0].uri, "https://doi.org/10.1234/exact");
        assert!((page.spans[0].bbox.unwrap().x0 - 72.0).abs() < 1.0);
        assert!(matches!(
            session.page_text(2),
            Err(BackendError::PageRange { .. })
        ));
    }
}
