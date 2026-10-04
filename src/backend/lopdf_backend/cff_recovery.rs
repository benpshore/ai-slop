//! Recover one embedded `Type1C` font's byte encoding through pdf-extract.
//!
//! The source PDF is never sent to pdf-extract: its `OutputDev` lacks font IDs
//! and does not report every unmapped glyph. Two controlled pages containing
//! only the selected, bounded CFF program establish its actual mappings.
//! Distinct A/B fallback encodings expose missing mappings: only identical
//! nonempty results from both probes are accepted. Geometry and resource
//! interpretation remain with the original lopdf session.

use pdf_extract::{Dictionary, Document, Object, OutputDev, OutputError, Stream, dictionary};

const CODES: usize = 256;

#[derive(Default)]
struct Probe {
    values: Vec<String>,
    began: bool,
    ended: bool,
}

impl OutputDev for Probe {
    fn begin_page(
        &mut self,
        page: u32,
        _: &pdf_extract::MediaBox,
        _: Option<(f64, f64, f64, f64)>,
    ) -> Result<(), OutputError> {
        if page != 1 || self.began {
            return Err(std::fmt::Error.into());
        }
        self.began = true;
        Ok(())
    }

    fn end_page(&mut self) -> Result<(), OutputError> {
        self.ended = true;
        Ok(())
    }

    fn output_character(
        &mut self,
        _: &pdf_extract::Transform,
        _: f64,
        _: f64,
        _: f64,
        text: &str,
    ) -> Result<(), OutputError> {
        if self.values.len() >= CODES || text.len() > 4 {
            return Err(std::fmt::Error.into());
        }
        self.values.push(text.to_owned());
        Ok(())
    }

    fn begin_word(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
    fn end_word(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
    fn end_line(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
}

fn probe(program: &[u8], fallback: &str) -> Result<Vec<String>, &'static str> {
    let mut doc = Document::with_version("1.5");
    let pages = doc.new_object_id();
    let file = doc.add_object(Stream::new(
        dictionary! { "Subtype" => "Type1C" },
        program.to_vec(),
    ));
    let descriptor = doc.add_object(dictionary! {
        "Type" => "FontDescriptor", "FontName" => "RecoveryProbe",
        "Flags" => 4, "FontBBox" => vec![0.into(), 0.into(), 1000.into(), 1000.into()],
        "ItalicAngle" => 0, "Ascent" => 1000, "Descent" => 0,
        "CapHeight" => 1000, "StemV" => 80, "FontFile3" => file,
    });
    let mut differences = vec![Object::Integer(0)];
    differences.extend((0..CODES).map(|_| Object::Name(fallback.as_bytes().to_vec())));
    let font = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "RecoveryProbe",
        "FontDescriptor" => descriptor, "FirstChar" => 0, "LastChar" => 255,
        "Widths" => vec![Object::Integer(1000); CODES],
        "Encoding" => dictionary! { "Type" => "Encoding", "Differences" => differences },
    });
    // One byte per Tj, including FE/FF separately: pdf-extract's legacy
    // string decoder interprets a leading FE FF pair as a UTF-16 BOM.
    let mut content = b"BT /F1 1 Tf 1 0 0 1 0 0 Tm\n".to_vec();
    for code in 0_u8..=255 {
        use std::io::Write;
        writeln!(content, "<{code:02X}> Tj").map_err(|_| "CFF probe content error")?;
    }
    content.extend_from_slice(b"ET");
    let stream = doc.add_object(Stream::new(Dictionary::new(), content));
    let page = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages, "Contents" => stream,
        "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
    });
    doc.objects.insert(
        pages,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1,
            "MediaBox" => vec![0.into(), 0.into(), 512.into(), 16.into()],
        }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
    doc.trailer.set("Root", catalog);
    let mut output = Probe::default();
    pdf_extract::output_doc_page(&doc, &mut output, 1).map_err(|_| "CFF probe failed")?;
    if !output.began || !output.ended || output.values.len() != CODES {
        return Err("incomplete CFF probe coverage");
    }
    Ok(output.values)
}

pub(super) fn recover(program: &[u8]) -> Result<Vec<Option<String>>, &'static str> {
    if program.len() > super::font_resources::MAX_CFF_STREAM {
        return Err("CFF program byte limit exceeded");
    }
    // pdf-extract selects the first CFF font regardless of the source font
    // name. A font set cannot establish the selected PDF font's mapping.
    let header = usize::from(*program.get(2).ok_or("malformed embedded CFF header")?);
    if header < 4 || program.get(header..header + 2) != Some(&[0, 1]) {
        return Err("unsupported embedded CFF font set");
    }
    // The upstream font parser contains assertions on malformed CFF data.
    // A caught panic is an explicit uncertain encoding, never a partial table.
    std::panic::catch_unwind(|| {
        let table = cff_parser::Table::parse(program).ok_or("malformed embedded CFF program")?;
        if table.glyph_name(cff_parser::GlyphId(0)) != Some(".notdef") {
            return Err("unsupported embedded CID CFF program");
        }
        let upstream_map = table.encoding.get_code_to_sid_table(&table.charset);
        // pdf-extract's bulk CFF map ignores encoding supplements and can map
        // absent glyphs in predefined encodings. Accept only entries that
        // agree with the font's actual code -> glyph -> SID route as well.
        let verified: Vec<bool> = (0_u8..=255)
            .map(|code| {
                let sid = table
                    .encoding
                    .code_to_gid(&table.charset, code)
                    .filter(|gid| gid.0 != 0 && gid.0 < table.number_of_glyphs())
                    .and_then(|gid| table.charset.gid_to_sid(gid));
                sid.is_some() && sid.as_ref() == upstream_map.get(&code)
            })
            .collect();
        let first = probe(program, "A")?;
        let second = probe(program, "B")?;

        Ok(first
            .into_iter()
            .zip(second)
            .zip(verified)
            .map(|((a, b), verified)| {
                let valid = verified
                    && a == b
                    && a.chars().count() == 1
                    && !a.chars().any(|ch| ch.is_control() || ch == '\u{fffd}');
                // ByteTable uses an empty successful entry for an unmapped
                // glyph; None means the entire source string failed.
                Some(if valid { a } else { String::new() })
            })
            .collect())
    })
    .unwrap_or(Err("malformed embedded CFF program"))
}
