//! A cutoff must survive all stages and publication to JSON/SQLite.
use std::collections::BTreeMap;

use tpe::backend::{BackendError, DocumentSession, Extractor};
use tpe::bibliography::{Record, scan_backward};
use tpe::ledger::Ledger;
use tpe::pipeline::run_job_with;
use tpe::schema::{BBox, BackendIdentity, Job, PageText, Span, Status};

const LIMIT: &str = "resource_limit: test decoder stopped early";

#[derive(Clone, Copy)]
enum Case {
    Ordinary,
    DecodeLimit,
    UnicodeMapping,
    CaptionLimit,
}

impl Extractor for Case {
    fn identity(&self) -> BackendIdentity {
        BackendIdentity {
            name: "cutoff-test".into(),
            version: "1".into(),
            config_digest: "test".into(),
        }
    }

    fn open(&self, _: &[u8], _: Option<&str>) -> Result<Box<dyn DocumentSession>, BackendError> {
        Ok(Box::new(*self))
    }
}

impl DocumentSession for Case {
    fn page_count(&self) -> u32 {
        1
    }

    fn page_text(&mut self, number: u32) -> Result<PageText, BackendError> {
        let mut page = PageText::new(number, 612.0, 792.0, 0);
        let count: u16 = if matches!(self, Self::CaptionLimit) {
            65
        } else {
            1
        };
        for seq in 0..count {
            let y = 760.0 - f32::from(seq) * 10.0;
            page.spans.push(Span {
                text: if count == 65 {
                    "Algorithm 1"
                } else {
                    "Retained text"
                }
                .into(),
                bbox: Some(BBox {
                    x0: 72.0,
                    y0: y,
                    x1: 140.0,
                    y1: y + 8.0,
                }),
                font: None,
                size: Some(8.0),
                seq: u32::from(seq),
            });
        }
        page.warnings.push(if matches!(self, Self::DecodeLimit) {
            LIMIT.into()
        } else if matches!(self, Self::UnicodeMapping) {
            "unicode_mapping: pdfium map_errors=1, zero_unicode=0".into()
        } else {
            "ordinary diagnostic".into()
        });
        Ok(page)
    }

    fn info(&self) -> BTreeMap<String, String> {
        BTreeMap::new()
    }
}

#[test]
fn decoder_and_late_processing_cutoffs_reach_json_chunks_and_ledger() {
    for case in [
        Case::Ordinary,
        Case::DecodeLimit,
        Case::UnicodeMapping,
        Case::CaptionLimit,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.pdf");
        std::fs::write(&input, b"%PDF fake input for test backend").unwrap();
        let job = Job {
            path: input.to_string_lossy().into_owned(),
            backend: "cutoff-test".into(),
            pages: None,
            password: None,
            max_bytes: None,
            figures_dir: None,
        };
        let result = run_job_with(&case, &job).unwrap();
        let expected = if matches!(case, Case::Ordinary) {
            Status::Complete
        } else {
            Status::Partial
        };
        assert_eq!(result.status, expected);
        assert_eq!(result.chunks[0].status, expected);
        assert!(
            !result.pages[0].text.is_empty(),
            "retained output is preserved"
        );
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["status"], expected.as_str());
        if expected == Status::Partial {
            assert!(result.warnings.iter().any(|w| w.starts_with(
                if matches!(case, Case::UnicodeMapping) {
                    "page 1: unicode_mapping:"
                } else {
                    "page 1: resource_limit:"
                }
            )));
        }
        let database = directory.path().join("ledger.sqlite");
        Ledger::open(&database)
            .unwrap()
            .write_result(&result)
            .unwrap();
        let connection = rusqlite::Connection::open(database).unwrap();
        for table in ["runs", "chunks"] {
            let stored: String = connection
                .query_row(&format!("SELECT status FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(stored, expected.as_str());
        }
    }
}

#[test]
fn no_list_does_not_erase_resource_cutoffs() {
    for case in [Case::DecodeLimit, Case::CaptionLimit] {
        let scan = scan_backward(&case, b"fake", None).unwrap();
        assert!(!scan.found);
        assert!(
            scan.warnings
                .iter()
                .any(|w| w.starts_with("resource_limit:"))
        );
        let record = Record::from_scan("input.pdf", "hash".into(), case.identity(), scan, 0.0);
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["status"], "not_found");
        assert_eq!(value["extraction_status"], "partial");
    }
}
