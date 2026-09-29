//! Fidelity assertions use authored source values, not another extractor's
//! normalized output. These fixtures test contracts, not corpus accuracy.

mod common;

use std::fs;
use std::process::Command;

use tpe::ingest::{Format, Options, Outcome, run};

#[test]
fn pdf_magic_overrides_extension_and_keeps_source_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("paper.txt");
    let bytes = common::synthetic_paper();
    fs::write(&path, &bytes).unwrap();
    let record = run(&path, &Options::default());
    assert_eq!(record.format, Format::Pdf);
    assert_eq!(record.outcome, Outcome::Extracted, "{:?}", record.warnings);
    assert_eq!(
        record.sha256.as_deref(),
        Some(tpe::schema::sha256_hex(&bytes).as_str())
    );
    let content = record.content.unwrap();
    assert_eq!(content["pages"].as_array().unwrap().len(), 2);
    assert!(
        content["pages"][0]["text"]
            .as_str()
            .unwrap()
            .contains(common::TITLE)
    );
    assert!(!content["pages"][0]["spans"].as_array().unwrap().is_empty());
}

#[test]
fn scan_is_not_reported_as_successful_empty_text() {
    let (_dir, path) = common::write_temp_pdf(&common::raster::scanned_fixture());
    let record = run(&path, &Options::default());
    assert_eq!(record.outcome, Outcome::NeedsOcr);
    assert_eq!(
        record.content.unwrap()["ocr_candidates"],
        serde_json::json!([1])
    );
}

#[test]
fn plain_text_is_exact_and_failed_input_does_not_stop_batch() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.txt");
    let good = dir.path().join("good.txt");
    let text = "D. Loutchko\r\n  Café α₂ — 100%\n";
    fs::write(&bad, [0xff, 0xfe, 0x01]).unwrap();
    fs::write(&good, text).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tpe"))
        .arg("ingest")
        .arg(&bad)
        .arg(&good)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let records: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["outcome"], "failed");
    assert_eq!(records[1]["outcome"], "extracted");
    assert_eq!(records[1]["content"]["text"], text);
}

#[test]
fn media_requires_explicit_adapters_and_limits_fail_visibly() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("sample.wav");
    fs::write(&wav, b"RIFF1234WAVE").unwrap();
    assert_eq!(
        run(&wav, &Options::default()).outcome,
        Outcome::NeedsTranscription
    );
    let options = Options {
        max_bytes: 4,
        ..Options::default()
    };
    let result = run(&wav, &options);
    assert_eq!(result.outcome, Outcome::Failed);
    assert!(result.content.is_none());
    assert!(result.warnings[0].contains("limit"));
}

#[cfg(feature = "formats")]
mod office {
    use super::*;
    use serde_json::Value;
    use std::io::{Cursor, Write};
    use zip::{ZipWriter, write::SimpleFileOptions};

    fn package(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, text) in parts {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(text.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    const TYPES: &str = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/></Types>"#;
    const RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;

    fn process(name: &str, bytes: &[u8], options: &Options) -> tpe::ingest::Record {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        fs::write(&path, bytes).unwrap();
        run(&path, options)
    }

    fn strings<'a>(value: &'a Value, key: &str, out: &mut Vec<&'a str>) {
        match value {
            Value::Object(object) => {
                if let Some(text) = object.get(key).and_then(Value::as_str) {
                    out.push(text);
                }
                for child in object.values() {
                    strings(child, key, out);
                }
            }
            Value::Array(array) => {
                for child in array {
                    strings(child, key, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn docx_preserves_initials_unicode_paragraphs_and_table_cells() {
        let bytes = package(&[
            ("[Content_Types].xml", TYPES),
            ("_rels/.rels", RELS),
            (
                "word/document.xml",
                r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>D. Loutchko — Café α₂</w:t></w:r></w:p><w:p><w:r><w:t>References</w:t></w:r></w:p><w:p><w:r><w:t>[1] A. Author. Exact-title. 2026.</w:t></w:r></w:p><w:tbl><w:tblGrid><w:gridCol/><w:gridCol/></w:tblGrid><w:tr><w:tc><w:p><w:r><w:t>Sample</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>12.50 μg</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#,
            ),
        ]);
        // Content-based Office routing also works for extensionless downloads.
        let record = process("download", &bytes, &Options::default());
        assert_eq!(record.format, Format::Docx);
        assert_eq!(record.outcome, Outcome::Extracted, "{:?}", record.warnings);
        let content = record.content.unwrap();
        let mut text = Vec::new();
        strings(&content, "text", &mut text);
        for expected in [
            "D. Loutchko — Café α₂",
            "References",
            "[1] A. Author. Exact-title. 2026.",
            "Sample",
            "12.50 μg",
        ] {
            assert!(text.contains(&expected), "missing {expected:?}: {text:?}");
        }
    }

    fn workbook() -> Vec<u8> {
        package(&[
            ("[Content_Types].xml", TYPES),
            (
                "_rels/.rels",
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
            ),
            (
                "xl/workbook.xml",
                r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Measurements α" sheetId="1" r:id="rId1"/><sheet name="Hidden source" sheetId="2" state="hidden" r:id="rId2"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:XFD1048576"/><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>D. Loutchko</t></is></c><c r="B1"><v>12.5</v></c><c r="C1"><f>B1*2</f><v>25</v></c><c r="D1"><f t="shared" si="0" ref="D1:D2">B1+1</f><v>13.5</v></c></row><row r="2"><c r="D2"><f t="shared" si="0"/><v>8</v></c></row><row r="1048576"><c r="XFD1048576" t="inlineStr"><is><t>far corner</t></is></c></row></sheetData><mergeCells count="1"><mergeCell ref="A3:B3"/></mergeCells></worksheet>"#,
            ),
            (
                "xl/worksheets/sheet2.xml",
                r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="b"><v>1</v></c><c r="B1" t="e"><v>#DIV/0!</v></c><c r="C1"><f>1/0</f></c></row></sheetData></worksheet>"#,
            ),
        ])
    }

    #[test]
    fn sparse_excel_keeps_formula_metadata_cached_values_and_hidden_sheets() {
        let record = process("sparse.xlsx", &workbook(), &Options::default());
        assert_eq!(record.outcome, Outcome::Extracted, "{:?}", record.warnings);
        let content = record.content.unwrap();
        let sheets = content["sheets"].as_array().unwrap();
        assert_eq!(sheets.len(), 2);
        assert_eq!(sheets[0]["name"], "Measurements α");
        let cells = sheets[0]["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 6, "must not allocate 17 billion empty cells");
        assert_eq!(cells[0]["value"]["value"], "D. Loutchko");
        assert_eq!(cells[2]["formula"]["text"], "B1*2");
        assert_eq!(cells[2]["value"]["value"], 25.0);
        assert_eq!(cells[3]["formula"]["kind"], "shared_anchor");
        assert_eq!(cells[4]["formula"]["kind"], "shared_derived");
        assert_eq!(cells[4]["formula"]["shared_index"], 0);
        assert_eq!(cells[5]["row"], 1_048_576);
        assert_eq!(cells[5]["column"], 16_384);
        assert_eq!(
            sheets[0]["merged_ranges"][0]["end"],
            serde_json::json!([3, 2])
        );
        assert_eq!(sheets[1]["visibility"], "hidden");
        assert_eq!(sheets[1]["cells"][0]["value"]["value"], true);
        assert_eq!(sheets[1]["cells"][1]["value"]["kind"], "error");
        assert_eq!(sheets[1]["cells"][2]["value"]["kind"], "empty");
        assert_eq!(sheets[1]["cells"][2]["formula"]["text"], "1/0");
    }

    #[test]
    fn limits_reject_whole_workbooks_instead_of_silently_dropping_sheets() {
        for options in [
            Options {
                max_cells: 6,
                ..Options::default()
            },
            Options {
                max_archive_entries: 2,
                ..Options::default()
            },
            Options {
                max_expanded_bytes: 20,
                ..Options::default()
            },
        ] {
            let record = process("book.xlsx", &workbook(), &options);
            assert_eq!(record.outcome, Outcome::Failed);
            assert!(record.content.is_none());
        }
    }

    #[test]
    fn html_keeps_text_and_links_without_running_scripts() {
        let record = process("page.html", br#"<!doctype html><html><body><h1>Laboratory records</h1><p>D. Loutchko &amp; A. Author.</p><p><a href="https://example.org/source">Exact source</a></p><table><tr><th>Sample</th><th>Mass</th></tr><tr><td>A-1</td><td>12.50 mg</td></tr></table><script>SHOULD_NOT_EXECUTE</script></body></html>"#, &Options::default());
        assert_eq!(record.outcome, Outcome::Extracted, "{:?}", record.warnings);
        let content = record.content.unwrap();
        let mut text = Vec::new();
        strings(&content, "text", &mut text);
        assert!(text.contains(&"D. Loutchko & A. Author."), "{text:?}");
        assert!(text.contains(&"12.50 mg"), "{text:?}");
        assert!(!text.iter().any(|t| t.contains("SHOULD_NOT_EXECUTE")));
        let mut links = Vec::new();
        strings(&content, "hyperlink", &mut links);
        assert!(links.contains(&"https://example.org/source"), "{links:?}");
    }

    #[test]
    fn csv_preserves_quoted_commas_and_multiline_fields() {
        let record = process(
            "table.csv",
            b"name,note\n\"Loutchko, D.\",\"first line\nsecond line\"\n",
            &Options::default(),
        );
        assert_eq!(record.outcome, Outcome::Extracted, "{:?}", record.warnings);
        let content = record.content.unwrap();
        let mut text = Vec::new();
        strings(&content, "text", &mut text);
        assert!(text.contains(&"Loutchko, D."), "{text:?}");
        assert!(text.contains(&"first line\nsecond line"), "{text:?}");
    }

    #[test]
    fn html_utf8_policy_cannot_be_overridden_by_stale_meta_charset() {
        let record = process(
            "page.html",
            "<html><head><meta charset=\"windows-1252\"></head><body><p>Café α₂</p></body></html>"
                .as_bytes(),
            &Options::default(),
        );
        assert_eq!(record.outcome, Outcome::Extracted, "{:?}", record.warnings);
        let content = record.content.unwrap();
        let mut text = Vec::new();
        strings(&content, "text", &mut text);
        assert!(text.contains(&"Café α₂"), "{text:?}");
    }

    #[test]
    fn duplicate_zip_members_are_rejected_before_conversion() {
        let mut bytes = package(&[("word/document.xml", "one"), ("word/document.xmL", "two")]);
        let from = b"word/document.xmL";
        let mut start = 0;
        while let Some(at) = bytes[start..].windows(from.len()).position(|w| w == from) {
            bytes[start + at + from.len() - 1] = b'l';
            start += at + from.len();
        }
        let record = process("duplicate.docx", &bytes, &Options::default());
        assert_eq!(record.outcome, Outcome::Failed);
        assert!(
            record
                .warnings
                .iter()
                .any(|warning| warning.contains("duplicate")),
            "{:?}",
            record.warnings
        );
    }
}
