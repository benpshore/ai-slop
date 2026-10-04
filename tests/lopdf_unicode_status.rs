//! Real font-decoding failures must remain partial through both public pipelines.
mod common;

use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, Stream, dictionary};
use std::process::Command;
use tpe::backend::Extractor;
use tpe::backend::lopdf_backend::LopdfBackend;
use tpe::bibliography::{Record, scan_backward};
use tpe::ledger::Ledger;
use tpe::pipeline::run_job;
use tpe::schema::{Job, Status};

fn pdf(font: Option<Dictionary>, shown: &[u8]) -> Vec<u8> {
    let mut document = Document::with_version("1.5");
    let tree = document.new_object_id();
    let mut resources = dictionary! {};
    if let Some(font) = font {
        let id = document.add_object(font);
        resources.set("Font", dictionary! { "F1" => id });
    }
    let contents = Content {
        operations: vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![72.into(), 500.into()]),
            Operation::new("Tj", vec![Object::string_literal(shown)]),
            Operation::new("ET", vec![]),
        ],
    }
    .encode()
    .unwrap();
    let stream = document.add_object(Stream::new(dictionary! {}, contents));
    let page = document.add_object(dictionary! {
        "Type" => "Page", "Parent" => tree, "Contents" => stream,
        "Resources" => resources,
    });
    document.objects.insert(
        tree,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        }),
    );
    let catalog = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => tree });
    document.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    bytes
}

fn mapped_font() -> Dictionary {
    dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" }
}

fn unknown_font() -> Dictionary {
    dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Times-Roman",
        "Encoding" => dictionary! {
            "Type" => "Encoding", "BaseEncoding" => "WinAnsiEncoding",
            "Differences" => vec![66.into(), "zzunknownglyph".into()],
        },
    }
}

#[test]
fn unknown_bytes_keep_their_original_position_between_known_characters() {
    let bytes = pdf(Some(unknown_font()), b"ABC");
    let mut session = LopdfBackend::default().open(&bytes, None).unwrap();
    let page = session.page_text(1).unwrap();
    assert_eq!(page.spans[0].text, "A\u{fffd}C");
    assert!(
        page.warnings
            .iter()
            .any(|w| w.starts_with("unicode_mapping:"))
    );
}

#[test]
fn mapping_uncertainty_survives_pipeline_json_ledger_and_bibliography() {
    for (font, shown, retained, expected) in [
        (
            Some(unknown_font()),
            b"ABC".as_slice(),
            "A\u{fffd}C",
            Status::Partial,
        ),
        (
            None,
            b"Retained fallback".as_slice(),
            "Retained fallback",
            Status::Partial,
        ),
        (
            Some(mapped_font()),
            b"ABC".as_slice(),
            "ABC",
            Status::Complete,
        ),
        (None, b"".as_slice(), "", Status::Complete),
    ] {
        let bytes = pdf(font, shown);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.pdf");
        std::fs::write(&path, &bytes).unwrap();
        let result = run_job(&Job {
            path: path.to_string_lossy().into_owned(),
            backend: "lopdf".into(),
            pages: None,
            password: None,
            max_bytes: None,
            figures_dir: None,
        })
        .unwrap();
        assert_eq!(result.status, expected, "{:?}", result.pages[0].warnings);
        assert_eq!(result.chunks[0].status, expected);
        assert_eq!(result.pages[0].text, retained);
        assert_eq!(
            serde_json::to_value(&result).unwrap()["status"],
            expected.as_str()
        );
        if expected == Status::Partial {
            assert!(
                result
                    .warnings
                    .iter()
                    .any(|w| w.starts_with("page 1: unicode_mapping:"))
            );
        }
        let db = dir.path().join("ledger.sqlite");
        Ledger::open(&db).unwrap().write_result(&result).unwrap();
        let connection = rusqlite::Connection::open(db).unwrap();
        for table in ["runs", "chunks"] {
            let status: String = connection
                .query_row(&format!("SELECT status FROM {table}"), [], |row| row.get(0))
                .unwrap();
            assert_eq!(status, expected.as_str());
        }
        let backend = LopdfBackend::default();
        let scan = scan_backward(&backend, &bytes, None).unwrap();
        assert!(!scan.found);
        let record = Record::from_scan("input.pdf", "hash".into(), backend.identity(), scan, 0.0);
        assert_eq!(record.extraction_status, expected);
        assert_eq!(
            serde_json::to_value(record).unwrap()["extraction_status"],
            expected.as_str()
        );
    }
}

#[test]
fn bibliography_cli_preserves_found_partial_records_but_returns_failure() {
    for unmapped in [false, true] {
        let mut document = Document::load_mem(&common::synthetic_paper()).unwrap();
        if unmapped {
            for object in document.objects.values_mut() {
                if let Ok(font) = object.as_dict_mut()
                    && font.has_type(b"Font")
                {
                    font.set("Encoding", unknown_font().get(b"Encoding").unwrap().clone());
                }
            }
        }
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        let (_dir, path) = common::write_temp_pdf(&bytes);
        let output = Command::new(env!("CARGO_BIN_EXE_tpe"))
            .args(["bibliography", "--backend", "lopdf"])
            .arg(path)
            .output()
            .unwrap();
        let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.success(), !unmapped, "{record}");
        assert_eq!(record["status"], "found");
        assert_eq!(record["references"].as_array().unwrap().len(), 3);
        assert_eq!(
            record["extraction_status"],
            if unmapped { "partial" } else { "complete" }
        );
    }
}

#[test]
fn bibliography_cli_reports_zero_page_pdf_as_failed() {
    let mut document = Document::with_version("1.5");
    let tree = document.add_object(dictionary! {
        "Type" => "Pages", "Kids" => Vec::<Object>::new(), "Count" => 0,
    });
    let catalog = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => tree });
    document.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    let (_dir, path) = common::write_temp_pdf(&bytes);
    let output = Command::new(env!("CARGO_BIN_EXE_tpe"))
        .args(["bibliography", "--backend", "lopdf"])
        .arg(path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["status"], "failed");
    assert_eq!(record["extraction_status"], "failed");
    assert!(record["error"].as_str().unwrap().contains("out of range"));
    assert!(record["references"].as_array().unwrap().is_empty());
}

#[test]
fn mixed_case_reference_headings_preserve_lists_and_reference_text() {
    let original = common::synthetic_paper();
    let backend = LopdfBackend::default();
    let baseline = scan_backward(&backend, &original, None).unwrap();
    let expected: Vec<_> = baseline.references.iter().map(|entry| &entry.raw).collect();
    assert_eq!(expected.len(), 3);
    for heading in ["references", "reFerences", "rEfErEnCEs", "bibliography"] {
        let mut document = Document::load_mem(&original).unwrap();
        let mut changed = 0;
        for object in document.objects.values_mut() {
            if let Ok(stream) = object.as_stream_mut() {
                let mut content = Content::decode(&stream.content).unwrap();
                for operation in &mut content.operations {
                    if operation.operator == "Tj"
                        && operation.operands[0]
                            .as_str()
                            .is_ok_and(|text| text == b"References")
                    {
                        operation.operands[0] = Object::string_literal(heading);
                        changed += 1;
                    }
                }
                stream.set_content(content.encode().unwrap());
            }
        }
        assert_eq!(changed, 1);
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        let scan = scan_backward(&backend, &bytes, None).unwrap();
        assert!(scan.found, "{heading}");
        assert_eq!(
            scan.references
                .iter()
                .map(|entry| &entry.raw)
                .collect::<Vec<_>>(),
            expected
        );
        let (_directory, path) = common::write_temp_pdf(&bytes);
        let result = run_job(&Job {
            path: path.to_string_lossy().into_owned(),
            backend: "lopdf".into(),
            pages: None,
            password: None,
            max_bytes: None,
            figures_dir: None,
        })
        .unwrap();
        assert_eq!(result.status, Status::Complete);
        assert_eq!(
            scan.heading.as_deref(),
            Some(heading),
            "{:?}",
            result.pages[1].text
        );
        assert_eq!(
            result
                .references
                .iter()
                .map(|entry| &entry.raw)
                .collect::<Vec<_>>(),
            expected
        );
        assert!(result.pages.iter().any(|page| page.text.contains(heading)));
    }
}
