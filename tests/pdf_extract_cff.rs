//! Owned, generated CFF/PDF fixtures: nonstandard embedded encodings, no OCR.
use lopdf::{Dictionary, Document, Object, Stream, dictionary};
use tpe::backend::Extractor;
use tpe::backend::lopdf_backend::LopdfBackend;
use tpe::schema::Status;

fn index(items: &[&[u8]]) -> Vec<u8> {
    let mut out = u16::try_from(items.len()).unwrap().to_be_bytes().to_vec();
    if items.is_empty() {
        return out;
    }
    out.push(2);
    let mut offset = 1_u16;
    out.extend(offset.to_be_bytes());
    for item in items {
        offset += u16::try_from(item.len()).unwrap();
        out.extend(offset.to_be_bytes());
    }
    for item in items {
        out.extend(*item);
    }
    out
}

// Compact Font Format 1: a custom encoding maps A/B/C to the named glyphs.
// Each glyph is a valid empty Type2 outline (endchar). All names and bytes are
// generated here, without third-party font assets or external executables.
fn cff(names: &[&str], supplement: bool) -> Vec<u8> {
    cff_fontset(names, supplement, 1)
}

fn cff_fontset(names: &[&str], supplement: bool, fonts: usize) -> Vec<u8> {
    let font_names = [b"CffFixture".as_slice(), b"OtherFixture".as_slice()];
    let name = index(&font_names[..fonts]);
    let strings = index(&names.iter().map(|s| s.as_bytes()).collect::<Vec<_>>());
    let top_size = index(&vec![[0; 18].as_slice(); fonts]).len();
    let encoding_offset = 4 + name.len() + top_size + strings.len() + 2;
    let mut encoding = vec![
        if supplement { 128 } else { 0 },
        u8::try_from(names.len()).unwrap(),
    ];
    encoding.extend((0..names.len()).map(|n| 65 + u8::try_from(n).unwrap()));
    if supplement {
        // Override A with the second glyph. Upstream bulk map ignores this:
        // recovery must refuse its incorrect A mapping instead of Complete.
        encoding.extend([1, 65, 1, 136]); // SID 392
    }
    let charset_offset = encoding_offset + encoding.len();
    let mut charset = vec![0];
    for n in 0..names.len() {
        charset.extend((391 + u16::try_from(n).unwrap()).to_be_bytes());
    }
    let charstrings_offset = charset_offset + charset.len();
    let mut top = Vec::new();
    for (offset, operator) in [
        (charset_offset, 15),
        (encoding_offset, 16),
        (charstrings_offset, 17),
    ] {
        top.push(29);
        top.extend(u32::try_from(offset).unwrap().to_be_bytes());
        top.push(operator);
    }
    let mut result = vec![1, 0, 4, 4];
    result.extend(name);
    result.extend(index(&vec![top.as_slice(); fonts]));
    result.extend(strings);
    result.extend([0, 0]); // Global Subrs INDEX.
    result.extend(encoding);
    result.extend(charset);
    result.extend(index(&vec![[14].as_slice(); names.len() + 1]));
    result
}

fn pdf(program: Vec<u8>, shown: &[u8], encoding: bool, unused: bool) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages = doc.new_object_id();
    let file = doc.add_object(Stream::new(dictionary! { "Subtype" => "Type1C" }, program));
    let descriptor =
        doc.add_object(dictionary! { "Type" => "FontDescriptor", "FontFile3" => file });
    let mut font = dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "OriginalCffFont",
        "FontDescriptor" => descriptor, "FirstChar" => 0, "LastChar" => 255,
        "Widths" => vec![Object::Integer(600); 256],
    };
    if encoding {
        font.set("Encoding", "WinAnsiEncoding");
    }
    let font = doc.add_object(font);
    let fallback = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica", "Encoding" => "WinAnsiEncoding", "FirstChar" => 0, "LastChar" => 255, "Widths" => vec![Object::Integer(600); 256] });
    let mut content = b"BT /F1 12 Tf 1 0 0 1 72 500 Tm <".to_vec();
    for byte in shown {
        content.extend(format!("{byte:02X}").as_bytes());
    }
    content.extend(b"> Tj ET");
    let stream = doc.add_object(Stream::new(Dictionary::new(), content));
    let page = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages, "Contents" => stream,
        "Resources" => dictionary! { "Font" => dictionary! { "F1" => if unused { fallback } else { font }, "Unused" => font } },
    });
    doc.objects.insert(
        pages,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn cff_recovery_preserves_original_geometry_font_and_input() {
    let bytes = pdf(
        cff(&["alpha", "beta", "gamma"], false),
        b"ABC",
        false,
        false,
    );
    let original = bytes.clone();
    let mut session = LopdfBackend::default().open(&bytes, None).unwrap();
    assert_eq!(session.page_count(), 1);
    let page = session.page_text(1).unwrap();
    let control = pdf(cff(&["alpha", "beta", "gamma"], false), b"ABC", true, false);
    let mut control_session = LopdfBackend::default().open(&control, None).unwrap();
    let control_page = control_session.page_text(1).unwrap();
    assert_eq!(control_page.spans[0].text, "ABC");
    assert_eq!(page.spans[0].bbox, control_page.spans[0].bbox);
    assert!(page.spans[0].bbox.is_some());
    assert_eq!(page.spans[0].font, control_page.spans[0].font);
    assert_eq!(page.spans[0].size, control_page.spans[0].size);
    assert_eq!(bytes, original);
    #[cfg(feature = "pdf-extract")]
    {
        assert_eq!(page.spans[0].text, "αβγ");
        assert_eq!(
            page.extraction_status(),
            Status::Complete,
            "{:?}",
            page.warnings
        );
    }
    #[cfg(not(feature = "pdf-extract"))]
    {
        assert_eq!(page.spans[0].text, "ABC");
        assert_eq!(page.extraction_status(), Status::Partial);
        assert!(
            page.warnings
                .iter()
                .any(|w| w.contains("pdf-extract feature"))
        );
    }
}

#[test]
fn malformed_or_unused_cff_never_silently_becomes_complete() {
    for program in [
        b"bad CFF".to_vec(),
        cff_fontset(&["alpha", "beta", "gamma"], false, 2),
    ] {
        for unused in [false, true] {
            let bytes = pdf(program.clone(), b"ABC", false, unused);
            let mut session = LopdfBackend::default().open(&bytes, None).unwrap();
            let page = session.page_text(1).unwrap();
            assert_eq!(page.spans[0].text, "ABC");
            assert_eq!(
                page.extraction_status(),
                if unused {
                    Status::Complete
                } else {
                    Status::Partial
                }
            );
        }
    }
}

#[cfg(feature = "pdf-extract")]
#[test]
fn cff_controls_cannot_mistake_missing_or_supplemented_glyphs_for_mappings() {
    for (names, supplemented, shown, expected, status) in [
        (
            vec!["A", "B", "gamma"],
            false,
            b"ABC".as_slice(),
            "ABγ",
            Status::Complete,
        ),
        (
            vec!["alpha", "unknownCffGlyph", "gamma"],
            false,
            b"ABC",
            "α�γ",
            Status::Partial,
        ),
        (
            vec!["alpha", "beta", "gamma"],
            false,
            b"ADC",
            "α�γ",
            Status::Partial,
        ),
        (
            vec!["alpha", "beta", "gamma"],
            true,
            b"ABC",
            "�βγ",
            Status::Partial,
        ),
    ] {
        let bytes = pdf(cff(&names, supplemented), shown, false, false);
        let mut session = LopdfBackend::default().open(&bytes, None).unwrap();
        let page = session.page_text(1).unwrap();
        assert_eq!(
            page.spans[0].text, expected,
            "names={names:?} shown={shown:?} {:?}",
            page.warnings
        );
        assert_eq!(page.extraction_status(), status);
    }
}

#[cfg(feature = "pdf-extract")]
#[test]
fn oversized_cff_is_bounded_only_when_the_font_is_used() {
    for compressed in [false, true] {
        for unused in [false, true] {
            let mut bytes = pdf(vec![0; 256 * 1024 + 1], b"ABC", false, unused);
            if compressed {
                let mut doc = Document::load_mem(&bytes).unwrap();
                for obj in doc.objects.values_mut() {
                    if let Ok(stream) = obj.as_stream_mut()
                        && stream
                            .dict
                            .get(b"Subtype")
                            .and_then(Object::as_name)
                            .is_ok_and(|name| name == b"Type1C")
                    {
                        stream.compress().unwrap();
                        assert!(stream.content.len() < 256 * 1024);
                    }
                }
                bytes.clear();
                doc.save_to(&mut bytes).unwrap();
            }
            let mut session = LopdfBackend::default().open(&bytes, None).unwrap();
            if unused {
                assert_eq!(
                    session.page_text(1).unwrap().extraction_status(),
                    Status::Complete
                );
            } else {
                let expected = if compressed {
                    "resource_limit: CFF decoded stream"
                } else {
                    "resource_limit: CFF encoded stream"
                };
                assert!(
                    session
                        .page_text(1)
                        .unwrap_err()
                        .to_string()
                        .contains(expected)
                );
            }
        }
    }
}

#[test]
fn explicit_to_unicode_takes_priority_without_parsing_the_cff() {
    let bytes = pdf(vec![0; 256 * 1024 + 1], b"ABC", false, false);
    let mut doc = Document::load_mem(&bytes).unwrap();
    let map = doc.add_object(Stream::new(dictionary! {}, b"/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n/CMapName /Test def\n/CMapType 2 def\n1 begincodespacerange\n<00> <FF>\nendcodespacerange\n3 beginbfchar\n<41> <0058>\n<42> <0059>\n<43> <005A>\nendbfchar\nendcmap\nCMapName currentdict /CMap defineresource pop\nend\nend".to_vec()));
    for obj in doc.objects.values_mut() {
        if let Ok(font) = obj.as_dict_mut()
            && font.has_type(b"Font")
        {
            font.set("ToUnicode", map);
        }
    }
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    let mut session = LopdfBackend::default().open(&bytes, None).unwrap();
    let page = session.page_text(1).unwrap();
    assert_eq!(page.spans[0].text, "XYZ");
    assert_eq!(
        page.extraction_status(),
        Status::Complete,
        "{:?}",
        page.warnings
    );
}
