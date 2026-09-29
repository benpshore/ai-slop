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
        record.policy["formats_enabled"],
        cfg!(feature = "formats").to_string()
    );
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
fn prose_mentioning_pdf_magic_remains_exact_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prose.txt");
    let text = "The PDF format starts with %PDF-1.7.\nThis is prose.\n";
    fs::write(&path, text).unwrap();
    let record = run(&path, &Options::default());
    assert_eq!(record.format, Format::Text);
    assert_eq!(record.outcome, Outcome::Extracted);
    assert_eq!(record.content.unwrap()["text"], text);
}

#[test]
fn encrypted_pdf_credentials_reach_backend_without_entering_records_or_policy() {
    const PASSWORD: &str = "fixture-only-pdf-password";
    let mut document = lopdf::Document::load_mem(&common::synthetic_paper()).unwrap();
    document.trailer.set(
        "ID",
        vec![
            lopdf::Object::string_literal("ingest-test-id-01"),
            lopdf::Object::string_literal("ingest-test-id-01"),
        ],
    );
    let encryption = lopdf::EncryptionState::try_from(lopdf::EncryptionVersion::V2 {
        document: &document,
        owner_password: "fixture-only-owner-password",
        user_password: PASSWORD,
        key_length: 128,
        permissions: lopdf::Permissions::all(),
    })
    .unwrap();
    document.encrypt(&encryption).unwrap();
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    let (_dir, path) = common::write_temp_pdf(&bytes);
    let options = Options::default();
    let missing = run(&path, &options);
    let wrong = tpe::ingest::run_with_password(&path, &options, Some("wrong-fixture-password"));
    let correct = tpe::ingest::run_with_password(&path, &options, Some(PASSWORD));
    assert_eq!(missing.outcome, Outcome::Failed);
    assert_eq!(wrong.outcome, Outcome::Failed);
    assert_eq!(
        correct.outcome,
        Outcome::Extracted,
        "{:?}",
        correct.warnings
    );
    assert!(
        correct.content.as_ref().unwrap()["pages"][0]["text"]
            .as_str()
            .unwrap()
            .contains(common::TITLE)
    );
    assert_eq!(
        correct.sha256.as_deref(),
        Some(tpe::schema::sha256_hex(&bytes).as_str())
    );
    assert_eq!(correct.policy, wrong.policy);
    assert_eq!(correct.policy_digest, wrong.policy_digest);
    assert_eq!(correct.policy_digest, missing.policy_digest);
    for record in [&correct, &wrong, &missing] {
        let json = serde_json::to_string(record).unwrap();
        assert!(!json.contains(PASSWORD));
        assert!(!json.contains("wrong-fixture-password"));
    }
    for (password, succeeds) in [(PASSWORD, true), ("wrong-fixture-password", false)] {
        let output = Command::new(env!("CARGO_BIN_EXE_tpe"))
            .args(["ingest", "--password-env", "INGEST_TEST_PDF_PASSWORD"])
            .arg(&path)
            .env("INGEST_TEST_PDF_PASSWORD", password)
            .output()
            .unwrap();
        assert_eq!(output.status.success(), succeeds, "{:?}", output.stderr);
        assert!(!String::from_utf8_lossy(&output.stdout).contains(password));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(password));
        let record: tpe::ingest::Record = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(record.outcome == Outcome::Extracted, succeeds);
    }
    let output = Command::new(env!("CARGO_BIN_EXE_tpe"))
        .args(["ingest", "--password-env", "INGEST_TEST_PDF_PASSWORD"])
        .arg(&path)
        .env_remove("INGEST_TEST_PDF_PASSWORD")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing or not valid UTF-8"));
}

fn raster_with_text(image_height: i64, text: &[u8], font: bool) -> Vec<u8> {
    use lopdf::{Document, Stream, dictionary};
    let mut doc = Document::load_mem(&common::raster::scanned_fixture()).unwrap();
    let page_id = doc.get_pages()[&1];
    let font_id = doc.add_object(dictionary! {"Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica", "Encoding" => "WinAnsiEncoding"});
    let mut content =
        format!("q 612 0 0 {image_height} 0 40 cm /Im0 Do Q\nBT /F1 10 Tf 300 15 Td (")
            .into_bytes();
    content.extend_from_slice(text);
    content.extend_from_slice(b") Tj ET");
    let content_id = doc.add_object(Stream::new(dictionary! {}, content));
    let page = doc.get_object_mut(page_id).unwrap().as_dict_mut().unwrap();
    page.set("Contents", content_id);
    if font {
        page.get_mut(b"Resources")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Font", dictionary! {"F1" => font_id});
    }
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn digital_page_number_does_not_hide_a_dominant_scanned_image() {
    let (_dir, path) = common::write_temp_pdf(&raster_with_text(748, b"1", true));
    let record = run(&path, &Options::default());
    assert_eq!(record.outcome, Outcome::NeedsOcr);
    let content = record.content.unwrap();
    assert_eq!(content["pages"][0]["text"], "1");
    assert_eq!(content["ocr_candidates"], serde_json::json!([1]));
    assert_eq!(
        content["ocr_evidence"][0]["reason"],
        "sparse_text_dominant_raster"
    );
    assert!(
        content["ocr_evidence"][0]["dominant_raster_fraction"]
            .as_f64()
            .unwrap()
            > 0.9
    );
}

#[test]
fn small_captioned_image_is_not_a_dominant_scan() {
    let (_dir, path) = common::write_temp_pdf(&raster_with_text(100, b"A photograph", true));
    let record = run(&path, &Options::default());
    assert_eq!(record.outcome, Outcome::Extracted, "{:?}", record.warnings);
    assert_eq!(
        record.content.unwrap()["ocr_candidates"],
        serde_json::json!([])
    );
}

#[test]
fn font_fallback_is_review_required_even_without_replacement_characters() {
    let (_dir, path) = common::write_temp_pdf(&raster_with_text(100, b"Hi", false));
    let record = run(&path, &Options::default());
    assert_eq!(record.outcome, Outcome::ReviewRequired);
    assert_eq!(record.content.unwrap()["pages"][0]["text"], "Hi");
    assert!(
        record
            .warnings
            .iter()
            .any(|warning| warning.contains("Latin-1"))
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

    #[test]
    fn word_notes_even_headers_and_anchors_survive_as_hashed_xml_evidence() {
        let document = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body><w:p><w:r><w:t>Body text</w:t></w:r><w:r><w:footnoteReference w:id="1"/><w:endnoteReference w:id="2"/></w:r></w:p><w:sectPr><w:headerReference w:type="even" r:id="rHead"/><w:footerReference w:type="default" r:id="rFoot"/></w:sectPr></w:body></w:document>"#;
        let relationships = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rNote" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target="footnotes.xml"/><Relationship Id="rEnd" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/endnotes" Target="endnotes.xml"/><Relationship Id="rHead" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header2.xml"/><Relationship Id="rFoot" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer" Target="footer1.xml"/></Relationships>"#;
        let notes = r#"<w:footnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:footnote w:id="1"><w:p><w:r><w:t>D. Loutchko, Essential reference. 2026.</w:t></w:r></w:p></w:footnote></w:footnotes>"#;
        let endnotes = r#"<w:endnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:endnote w:id="2"><w:p><w:r><w:t>Essential endnote citation</w:t></w:r></w:p></w:endnote></w:endnotes>"#;
        let header = r#"<w:hdr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:p><w:r><w:t>Even-page header</w:t></w:r></w:p></w:hdr>"#;
        let footer = r#"<w:ftr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:p><w:r><w:t>Footer</w:t></w:r></w:p></w:ftr>"#;
        let comments = r#"<w:comments xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:comment w:id="3"><w:p><w:r><w:t>Reviewer evidence</w:t></w:r></w:p></w:comment></w:comments>"#;
        let settings = r#"<w:settings xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:evenAndOddHeaders/></w:settings>"#;
        let parts = [
            ("[Content_Types].xml", TYPES),
            ("_rels/.rels", RELS),
            ("word/document.xml", document),
            ("word/_rels/document.xml.rels", relationships),
            ("word/footnotes.xml", notes),
            ("word/endnotes.xml", endnotes),
            ("word/header2.xml", header),
            ("word/footer1.xml", footer),
            ("word/comments.xml", comments),
            ("word/settings.xml", settings),
        ];
        let record = process("notes.docx", &package(&parts), &Options::default());
        assert_eq!(
            record.outcome,
            Outcome::ReviewRequired,
            "{:?}",
            record.warnings
        );
        let content = record.content.unwrap();
        let evidence = content["tpe_supplemental_parts"].as_array().unwrap();
        for (name, source_xml) in parts.iter().filter(|(name, _)| name.starts_with("word/")) {
            let part = evidence
                .iter()
                .find(|part| part["path"] == *name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert_eq!(part["xml"], *source_xml);
            assert_eq!(
                part["sha256"],
                tpe::schema::sha256_hex(source_xml.as_bytes())
            );
        }
        assert!(
            record
                .warnings
                .iter()
                .any(|warning| warning.contains("reading order"))
        );
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
                "xl/styles.xml",
                r#"<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><cellXfs count="2"><xf numFmtId="0"/><xf numFmtId="14"/></cellXfs></styleSheet>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:XFD1048576"/><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>D. Loutchko</t></is></c><c r="B1"><v>12.5</v></c><c r="C1"><f>B1*2</f><v>25</v></c><c r="D1"><f t="shared" si="0" ref="D1:D2">B1+1</f><v>13.5</v></c></row><row r="2"><c r="D2"><f t="shared" si="0"/><v>8</v></c></row><row r="1048576"><c r="XFD1048576" t="inlineStr"><is><t>far corner</t></is></c></row></sheetData><mergeCells count="1"><mergeCell ref="A3:B3"/></mergeCells></worksheet>"#,
            ),
            (
                "xl/worksheets/sheet2.xml",
                r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="b"><v>1</v></c><c r="B1" t="e"><v>#DIV/0!</v></c><c r="C1"><f>1/0</f></c><c r="D1" s="1"><v>45000.5</v></c></row></sheetData></worksheet>"#,
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
        let date = &sheets[1]["cells"][3]["value"];
        assert_eq!(date["kind"], "excel_datetime");
        assert_eq!(date["serial"], 45000.5);
        assert_eq!(
            date["calendar"],
            serde_json::json!([2023, 3, 15, 12, 0, 0, 0])
        );
        assert_eq!(date.get("timezone"), Some(&Value::Null));
    }

    #[test]
    fn chart_sheets_keep_visibility_without_claiming_cell_extraction() {
        for (state, expected) in [
            ("visible", "visible"),
            ("hidden", "hidden"),
            ("veryHidden", "very_hidden"),
        ] {
            let workbook = format!(
                r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Chart evidence" sheetId="1" state="{state}" r:id="rChart"/></sheets></workbook>"#
            );
            let parts = [
                ("[Content_Types].xml", TYPES),
                (
                    "_rels/.rels",
                    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rBook" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
                ),
                ("xl/workbook.xml", workbook.as_str()),
                (
                    "xl/_rels/workbook.xml.rels",
                    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet" Target="chartsheets/sheet1.xml"/></Relationships>"#,
                ),
                (
                    "xl/chartsheets/sheet1.xml",
                    r#"<chartsheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"/>"#,
                ),
            ];
            let record = process("chart.xlsx", &package(&parts), &Options::default());
            assert_eq!(
                record.outcome,
                Outcome::ReviewRequired,
                "{:?}",
                record.warnings
            );
            let content = record.content.unwrap();
            assert_eq!(content["sheets"][0]["name"], "Chart evidence");
            assert_eq!(content["sheets"][0]["sheet_type"], "ChartSheet");
            assert_eq!(content["sheets"][0]["visibility"], expected);
            assert_eq!(content["sheets"][0]["cells"], Value::Null);
        }
    }

    #[test]
    fn shared_formula_derived_cell_can_precede_anchor_in_xml_order() {
        use std::io::Read;
        let bytes = workbook();
        let mut archive = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
        let mut parts = Vec::new();
        for index in 0..archive.len() {
            let mut member = archive.by_index(index).unwrap();
            let name = member.name().to_owned();
            let mut xml = String::new();
            member.read_to_string(&mut xml).unwrap();
            if name == "xl/worksheets/sheet1.xml" {
                let first = xml.find("<row ").unwrap();
                let second = first + xml[first..].find("</row>").unwrap() + 6;
                let end = second + xml[second..].find("</row>").unwrap() + 6;
                let reversed = format!("{}{}", &xml[second..end], &xml[first..second]);
                xml.replace_range(first..end, &reversed);
            }
            parts.push((name, xml));
        }
        let refs: Vec<(&str, &str)> = parts
            .iter()
            .map(|(name, xml)| (name.as_str(), xml.as_str()))
            .collect();
        let record = process("late-anchor.xlsx", &package(&refs), &Options::default());
        assert_eq!(record.outcome, Outcome::Extracted, "{:?}", record.warnings);
        let content = record.content.unwrap();
        let cells = content["sheets"][0]["cells"].as_array().unwrap();
        assert_eq!(cells[0]["row"], 2);
        assert_eq!(cells[0]["formula"]["kind"], "shared_derived");
        assert_eq!(cells[4]["row"], 1);
        assert_eq!(cells[4]["formula"]["kind"], "shared_anchor");
        assert_eq!(cells[4]["formula"]["text"], "B1+1");
        assert_eq!(
            cells[0]["formula"]["shared_index"],
            cells[4]["formula"]["shared_index"]
        );
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
        assert_eq!(
            content["rows"],
            serde_json::json!([
                ["name", "note"],
                ["Loutchko, D.", "first line\nsecond line"]
            ])
        );
        assert_eq!(content["dialect"]["delimiter"], ",");
        assert_eq!(content["dialect"]["has_headers"], false);
    }

    #[test]
    fn csv_quoted_header_cannot_change_dialect_and_ragged_rows_stay_ragged() {
        let source = "\"Name; aliases; initials\",Count\r\n\"Loutchko; D.; DL\",1\r\n\nonly one cell\nlast,,\n";
        let record = process("header.csv", source.as_bytes(), &Options::default());
        assert_eq!(record.outcome, Outcome::Extracted, "{:?}", record.warnings);
        let content = record.content.unwrap();
        assert_eq!(
            content["rows"],
            serde_json::json!([
                ["Name; aliases; initials", "Count"],
                ["Loutchko; D.; DL", "1"],
                ["only one cell"],
                ["last", "", ""]
            ])
        );
        assert_eq!(content["source_text"], source);
        let options = Options {
            max_cells: 7,
            ..Options::default()
        };
        let limited = process("header.csv", source.as_bytes(), &options);
        assert_eq!(limited.outcome, Outcome::Failed);
        assert!(limited.content.is_none());
        for bad in ["a,\"unclosed\n", "\"closed\"junk,b\n", "a\"b,c\n"] {
            assert_eq!(
                process("bad.csv", bad.as_bytes(), &Options::default()).outcome,
                Outcome::Failed
            );
        }
    }

    #[test]
    fn parser_limit_environment_changes_are_in_recorded_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.txt");
        fs::write(&path, "provenance").unwrap();
        let execute = |depth: &str| {
            let output = Command::new(env!("CARGO_BIN_EXE_tpe"))
                .args(["ingest"])
                .arg(&path)
                .env("DOCLING_RS_MAX_XML_DEPTH", depth)
                .env("DOCLING_RS_MAX_HTML_DEPTH", " 123 ")
                .env("DOCLING_RS_MAX_PART_BYTES", "4096")
                .env("UNRELATED_SECRET", "must never be recorded")
                .output()
                .unwrap();
            assert!(output.status.success());
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(!text.contains("must never be recorded"));
            serde_json::from_str::<tpe::ingest::Record>(&text).unwrap()
        };
        let first = execute("512");
        let second = execute("3");
        let fallback = execute("invalid");
        assert_eq!(first.policy["formats_enabled"], "true");
        assert_eq!(first.policy["DOCLING_RS_MAX_HTML_DEPTH"], "123");
        assert_eq!(first.policy["DOCLING_RS_MAX_PART_BYTES"], "4096");
        assert_eq!(second.policy["DOCLING_RS_MAX_XML_DEPTH"], "3");
        assert_ne!(first.policy_digest, second.policy_digest);
        assert_eq!(first.policy_digest, fallback.policy_digest);
        assert_eq!(
            first.policy_digest,
            tpe::schema::config_digest(&first.policy)
        );
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
        // ZipWriter forbids duplicate names, so deliberately corrupt this
        // fixture's local-header and central-directory filenames. Both names
        // have the same byte length; payload bytes and their CRCs stay intact.
        let mut bytes = package(&[("word/document.xml", "one"), ("word/document.xmL", "two")]);
        let from = b"word/document.xmL";
        let mut start = 0;
        let mut replacements = 0;
        while let Some(at) = bytes[start..].windows(from.len()).position(|w| w == from) {
            bytes[start + at + from.len() - 1] = b'l';
            start += at + from.len();
            replacements += 1;
        }
        assert_eq!(replacements, 2, "one local and one central filename");
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
