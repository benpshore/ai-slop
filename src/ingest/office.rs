//! Model-free Office/HTML adapters. XLSX uses a sparse cell stream instead of
//! the document converter's dense rectangles and lossy table-only view.

use std::collections::HashSet;
use std::io::{self, Cursor, Read};

use anyhow::{Context, Result, bail};
use calamine::{DataRef, Reader, SheetType, SheetVisible, Xlsx, XlsxFormulaMetadata};
use docling_formats::{ConversionStatus, DocumentConverter, InputFormat, SourceDocument};
use serde_json::{Value, json};
use zip::ZipArchive;

use super::{Format, Options, Outcome, Record, identity};

/// Check both declared and actual expansion, and CRCs, before conversion.
/// Nothing is extracted to disk. Ambiguous/duplicate containers fail closed.
pub(super) fn inspect_archive(bytes: &[u8], options: &Options) -> Result<Format> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    // zip's name map collapses duplicate central-directory entries. Count
    // the original records too so ambiguity cannot hide behind that map.
    let mut offset = usize::try_from(archive.central_directory_start())?;
    let mut count = 0;
    while bytes.get(offset..offset.saturating_add(4)) == Some(b"PK\x01\x02") {
        let header = bytes
            .get(offset..offset.saturating_add(46))
            .context("truncated ZIP directory")?;
        let length = |n| usize::from(u16::from_le_bytes([header[n], header[n + 1]]));
        offset = offset
            .checked_add(46 + length(28) + length(30) + length(32))
            .context("ZIP directory overflow")?;
        count += 1;
        if count > options.max_archive_entries {
            bail!("archive exceeds {} members", options.max_archive_entries);
        }
    }
    if count != archive.len() {
        bail!("duplicate or inconsistent ZIP directory entries");
    }
    if archive.len() > options.max_archive_entries {
        bail!("archive exceeds {} members", options.max_archive_entries);
    }
    let mut names = HashSet::new();
    let mut declared = 0_u64;
    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        if !names.insert(entry.name().to_owned()) {
            bail!("duplicate archive member: {}", entry.name());
        }
        declared = declared
            .checked_add(entry.size())
            .context("archive size overflow")?;
        if declared > options.max_expanded_bytes {
            bail!(
                "archive exceeds {} expanded bytes",
                options.max_expanded_bytes
            );
        }
    }
    let word = names.contains("word/document.xml");
    let excel = names.contains("xl/workbook.xml");
    if word && excel {
        bail!("ambiguous container contains both Word and Excel roots");
    }
    if !(word || excel) {
        return Ok(Format::Unknown);
    }
    if !names.contains("[Content_Types].xml") || !names.contains("_rels/.rels") {
        bail!("incomplete OOXML package: missing content types or package relationships");
    }
    let mut actual = 0_u64;
    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        let expected = entry.size();
        let remaining = options.max_expanded_bytes.saturating_sub(actual);
        let count = io::copy(
            &mut entry.take(remaining.saturating_add(1)),
            &mut io::sink(),
        )?;
        actual = actual.checked_add(count).context("archive size overflow")?;
        if actual > options.max_expanded_bytes || count != expected {
            bail!("archive member expansion exceeds its declared size or the configured limit");
        }
    }
    Ok(if word { Format::Docx } else { Format::Xlsx })
}

pub(super) fn extract(mut bytes: Vec<u8>, options: &Options, record: &mut Record) -> Result<()> {
    if record.format == Format::Xlsx {
        return extract_xlsx(&bytes, options, record);
    }
    let format = match record.format {
        Format::Docx => InputFormat::Docx,
        Format::Html => InputFormat::Html,
        Format::Markdown => InputFormat::Md,
        Format::Csv => InputFormat::Csv,
        _ => bail!("unsupported declarative format"),
    };
    // No silent codepage guess. An encoding adapter can be added separately
    // when it can record the selected encoding and validate it against a corpus.
    if record.format != Format::Docx {
        let text = std::str::from_utf8(&bytes).context("text input must be UTF-8")?;
        if text.contains('\0') {
            bail!("NUL in text input");
        }
    }
    if record.format == Format::Html && !bytes.starts_with(b"\xef\xbb\xbf") {
        // docling 1.69.2's HTML branch ignores SourceDocument.encoding and
        // gives a declared legacy charset priority over UTF-8. A BOM makes
        // our already-validated UTF-8 policy authoritative for that branch.
        bytes.splice(..0, [0xef, 0xbb, 0xbf]);
    }
    record.extractor = Some(identity("docling-declarative", "1.69.2"));
    // An in-memory source has no base directory. External images, scripts,
    // remote URLs and sidecar files are never fetched or executed here.
    let source =
        SourceDocument::from_bytes("document", format, bytes).with_encoding(Some("utf-8".into()));
    let converted = DocumentConverter::new()
        .strict(true)
        .fetch_images(false)
        .convert(source)?;
    record.outcome = match converted.status {
        ConversionStatus::Success => Outcome::Extracted,
        ConversionStatus::PartialSuccess => Outcome::ReviewRequired,
        ConversionStatus::Failure => bail!("document converter reported failure"),
    };
    let content = converted.document.export_to_json_value();
    if converted.document.nodes.is_empty() {
        record.outcome = Outcome::ReviewRequired;
        record.warnings.push(
            "converter returned no document nodes; image-only or unsupported content may remain"
                .into(),
        );
    }
    if content
        .get("pictures")
        .and_then(Value::as_array)
        .is_some_and(|pictures| !pictures.is_empty())
    {
        record.outcome = Outcome::ReviewRequired;
        record.warnings.push(
            "embedded pictures are represented structurally; their text has not been OCRed".into(),
        );
    }
    record.content = Some(content);
    Ok(())
}

fn extract_xlsx(bytes: &[u8], options: &Options, record: &mut Record) -> Result<()> {
    record.extractor = Some(identity("calamine-sparse", "0.36.1"));
    let mut workbook = Xlsx::new(Cursor::new(bytes))?;
    let metadata = workbook.sheets_metadata().to_vec();
    let mut sheets = Vec::new();
    let mut total_cells = 0_usize;
    for sheet in metadata {
        if sheet.typ != SheetType::WorkSheet {
            record.outcome = Outcome::ReviewRequired;
            record.warnings.push(format!(
                "sheet {:?} is {:?}; cell extraction does not cover this sheet type",
                sheet.name, sheet.typ
            ));
            sheets.push(json!({"name": sheet.name, "sheet_type": format!("{:?}", sheet.typ), "cells": null}));
            continue;
        }
        let merges = workbook.merge_cells_by_sheet_name(&sheet.name)?;
        let merged_ranges: Vec<Value> = merges
            .into_iter()
            .map(|range| {
                json!({
                    "start": [range.start.0 + 1, range.start.1 + 1],
                    "end": [range.end.0 + 1, range.end.1 + 1],
                })
            })
            .collect();
        let mut cells = Vec::new();
        let mut positions = HashSet::new();
        let mut reader = workbook.worksheet_cells_reader(&sheet.name)?;
        while let Some(cell) = reader.next_cell_with_formula_metadata()? {
            if cell.pos.0 >= 1_048_576 || cell.pos.1 >= 16_384 {
                bail!("cell outside Excel coordinate limits");
            }
            if !positions.insert(cell.pos) {
                bail!("duplicate cell {:?} in sheet {:?}", cell.pos, sheet.name);
            }
            // Include explicit empty cells: an empty cache on a formula cell
            // is information, and must not become an invented numeric zero.
            total_cells += 1;
            if total_cells > options.max_cells {
                bail!("workbook exceeds {} cells", options.max_cells);
            }
            cells.push(json!({
                "row": cell.pos.0 + 1,
                "column": cell.pos.1 + 1,
                "value": cell_value(&cell.value)?,
                "formula": cell.formula.as_ref().map(formula_value).transpose()?,
            }));
        }
        let visibility = match sheet.visible {
            SheetVisible::Visible => "visible",
            SheetVisible::Hidden => "hidden",
            SheetVisible::VeryHidden => "very_hidden",
        };
        sheets.push(json!({"name": sheet.name, "visibility": visibility, "cells": cells, "merged_ranges": merged_ranges}));
    }
    record.content = Some(json!({"coordinate_origin": 1, "sheets": sheets}));
    record.warnings.push("cell data only: formulas are not recalculated; cached values may be stale; charts, macros and display formatting are outside this adapter's scope".into());
    Ok(())
}

fn formula_value(formula: &XlsxFormulaMetadata) -> Result<Value> {
    Ok(match formula {
        XlsxFormulaMetadata::Normal { formula } => json!({"kind": "normal", "text": formula}),
        XlsxFormulaMetadata::Shared {
            shared_index,
            range,
            formula,
        } => json!({
            "kind": "shared_anchor", "shared_index": shared_index, "text": formula,
            "range": range.map(|r| json!({"start": [r.start.0 + 1, r.start.1 + 1], "end": [r.end.0 + 1, r.end.1 + 1]})),
        }),
        XlsxFormulaMetadata::SharedDerived { shared_index } => {
            json!({"kind": "shared_derived", "shared_index": shared_index})
        }
        _ => bail!("unsupported formula metadata; refusing to discard it"),
    })
}

fn cell_value(value: &DataRef<'_>) -> Result<Value> {
    Ok(match value {
        DataRef::Int(value) => json!({"kind": "integer", "value": value}),
        DataRef::Float(value) => {
            if !value.is_finite() {
                bail!("non-finite Excel number");
            }
            json!({"kind": "number", "value": value})
        }
        DataRef::String(value) => json!({"kind": "string", "value": value}),
        DataRef::SharedString(value) => json!({"kind": "string", "value": value}),
        DataRef::Bool(value) => json!({"kind": "boolean", "value": value}),
        DataRef::DateTime(value) => {
            json!({"kind": "excel_datetime", "serial": value.as_f64(), "calendar": value.to_ymd_hms_milli(), "is_duration": value.is_duration()})
        }
        DataRef::DateTimeIso(value) => json!({"kind": "iso_datetime", "value": value}),
        DataRef::DurationIso(value) => json!({"kind": "iso_duration", "value": value}),
        DataRef::Error(value) => json!({"kind": "error", "value": value.to_string()}),
        DataRef::Empty => json!({"kind": "empty"}),
    })
}
