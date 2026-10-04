#![cfg(feature = "pdf-oxide")]

use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream, dictionary};
use tpe::backend::pdf_oxide_backend::PdfOxideBackend;
use tpe::backend::{BackendError, Extractor, by_name, probe_pdf};
use tpe::pipeline::run_job;
use tpe::schema::{Job, Status};

fn fixture(actual_text: Option<&str>, damaged: bool) -> Vec<u8> {
    let mut document = Document::with_version("1.5");
    let tree = document.new_object_id();
    let font = document.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let mut operations = vec![
        Operation::new("BT", vec![]),
        Operation::new("Tf", vec!["F1".into(), 12.into()]),
        Operation::new("Td", vec![72.into(), 700.into()]),
    ];
    if let Some(text) = actual_text {
        let mut unicode = vec![0xfe, 0xff];
        unicode.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
        operations.push(Operation::new(
            "BDC",
            vec![
                "Span".into(),
                dictionary! { "ActualText" => Object::string_literal(unicode) }.into(),
            ],
        ));
    }
    operations.push(Operation::new("Tj", vec![Object::string_literal("X")]));
    if actual_text.is_some() {
        operations.push(Operation::new("EMC", vec![]));
    }
    operations.push(Operation::new("ET", vec![]));
    let content = if damaged {
        Stream::new(
            dictionary! { "Filter" => "ASCIIHexDecode" },
            b"GG>".to_vec(),
        )
    } else {
        Stream::new(dictionary! {}, Content { operations }.encode().unwrap())
    };
    let content = document.add_object(content);
    let page = document.add_object(dictionary! {
        "Type" => "Page", "Parent" => tree, "Contents" => content,
        "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
        "Rotate" => 90,
    });
    document.objects.insert(
        tree,
        dictionary! {
            "Type" => "Pages", "Count" => 1, "Kids" => vec![page.into()],
            "MediaBox" => vec![10.into(), 20.into(), 610.into(), 800.into()],
        }
        .into(),
    );
    let root = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => tree });
    document.trailer.set("Root", root);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn immutable_bytes_real_geometry_fonts_and_ranges() {
    let mut bytes = fixture(None, false);
    let mut session = PdfOxideBackend.open(&bytes, None).unwrap();
    bytes.fill(0);
    assert_eq!(session.page_count(), 1);
    let page = session.page_text(1).unwrap();
    assert_eq!((page.width, page.height, page.rotation), (600.0, 780.0, 90));
    assert_eq!(
        page.spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>(),
        "X"
    );
    let span = &page.spans[0];
    assert_eq!(span.font.as_deref(), Some("Helvetica"));
    assert_eq!(span.size, Some(12.0));
    assert!((span.bbox.unwrap().x0 - 72.0).abs() < 0.1);
    assert!(span.bbox.unwrap().y0 > 680.0);
    assert_eq!(page.extraction_status(), Status::Partial);
    for page in [0, 2] {
        assert!(matches!(
            session.page_text(page),
            Err(BackendError::PageRange { .. })
        ));
    }
}

#[test]
fn actual_text_recovery_is_distinct_from_lopdf() {
    let bytes = fixture(Some("Recovered text"), false);
    let text = |backend: &dyn Extractor| {
        backend
            .open(&bytes, None)
            .unwrap()
            .page_text(1)
            .unwrap()
            .spans
            .into_iter()
            .map(|span| span.text)
            .collect::<String>()
    };
    assert_eq!(text(by_name("lopdf").unwrap().as_ref()), "X");
    assert_eq!(text(&PdfOxideBackend), "Recovered text");
}

#[test]
fn replacement_glyphs_are_retained_without_global_span_configuration() {
    let mut document = Document::load_mem(&fixture(None, false)).unwrap();
    let cmap = document.add_object(Stream::new(
        dictionary! {},
        b"/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
        /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
        /CMapName /Unmapped def\n/CMapType 2 def\n\
        1 begincodespacerange\n<00><FF>\nendcodespacerange\n\
        1 beginbfchar\n<58><FFFD>\nendbfchar\n\
        endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n"
            .to_vec(),
    ));
    for object in document.objects.values_mut() {
        if let Ok(font) = object.as_dict_mut()
            && font.has_type(b"Font")
        {
            font.set("ToUnicode", cmap);
        }
    }
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    let page = PdfOxideBackend
        .open(&bytes, None)
        .unwrap()
        .page_text(1)
        .unwrap();
    assert!(page.spans.iter().any(|span| span.text.contains('\u{fffd}')));
    assert!(
        page.warnings
            .iter()
            .any(|warning| warning.starts_with("unicode_mapping:"))
    );
}

#[test]
fn uri_annotation_keeps_actual_target_and_indirect_rectangle() {
    let mut document = Document::load_mem(&fixture(None, false)).unwrap();
    let target = "https://doi.org/10.1234/actual-target";
    let uri = document.add_object(Object::string_literal(target));
    let action = document.add_object(dictionary! { "S" => "URI", "URI" => uri });
    let rectangle = document.add_object(vec![72.into(), 695.into(), 110.into(), 713.into()]);
    let annotation = document.add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Link", "A" => action, "Rect" => rectangle,
        "Contents" => Object::string_literal("Different label, not a DOI"),
    });
    let annotations = document.add_object(vec![Object::Reference(annotation)]);
    let page = *document.get_pages().get(&1).unwrap();
    document
        .get_object_mut(page)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Annots", annotations);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    let result = PdfOxideBackend
        .open(&bytes, None)
        .unwrap()
        .page_text(1)
        .unwrap();
    assert_eq!(result.links.len(), 1);
    assert_eq!(result.links[0].uri, target);
    assert_eq!(
        result.links[0].bbox,
        Some(tpe::schema::BBox {
            x0: 72.0,
            y0: 695.0,
            x1: 110.0,
            y1: 713.0
        })
    );
}

#[test]
fn silent_content_failure_stays_partial_in_pipeline_and_ledger() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("damaged.pdf");
    std::fs::write(&path, fixture(None, true)).unwrap();
    let result = run_job(&Job {
        path: path.to_string_lossy().into_owned(),
        backend: "pdf-oxide".into(),
        pages: None,
        password: None,
        max_bytes: None,
        figures_dir: None,
    })
    .unwrap();
    assert_eq!(result.backend.name, "pdf-oxide");
    assert_eq!(result.backend.version, "0.3.78");
    assert_eq!(result.document.pages, 1);
    assert_eq!(result.status, Status::Partial);
    assert_eq!(result.pages[0].extraction_status(), Status::Partial);
    assert_eq!(result.chunks[0].status, Status::Partial);
    assert!(result.pages[0].text.is_empty());
    let roundtrip = serde_json::from_str::<tpe::schema::ExtractionResult>(
        &serde_json::to_string(&result).unwrap(),
    )
    .unwrap();
    assert_eq!(roundtrip, result);
    let mut ledger = tpe::ledger::Ledger::open_in_memory().unwrap();
    let run = ledger.write_result(&result).unwrap();
    assert_eq!(ledger.load_result(run).unwrap().status, Status::Partial);
}

#[test]
fn supervised_cli_runs_pdf_oxide_and_reports_partial() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("probe.pdf");
    std::fs::write(&path, probe_pdf().unwrap()).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tpe"))
        .args(["extract", "--backend", "pdf-oxide", "--json"])
        .arg("--db")
        .arg(directory.path().join("ledger.sqlite"))
        .arg(&path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "partial");
    assert_eq!(result["backend"]["name"], "pdf-oxide");
}
