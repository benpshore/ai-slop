//! These opt-in native integration tests require explicitly configured runtimes.
//! Run with `--features mupdf --test native_provider -- --ignored`.
#![cfg(feature = "mupdf")]
use lopdf::{Document, Object, dictionary};
use tpe::backend::{
    BackendError, Extractor,
    native_provider::{Engine, NativeProviderBackend},
};

fn backend() -> NativeProviderBackend {
    NativeProviderBackend::new(Engine::MuPdf)
}

#[test]
#[ignore = "requires separately licensed MuPDF provider and runtime paths"]
fn native_text_geometry_and_uri_annotations_round_trip() {
    let mut pdf = Document::load_mem(&tpe::backend::probe_pdf().unwrap()).unwrap();
    let id = *pdf.get_pages().get(&1).unwrap();
    pdf.get_object_mut(id)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Rotate", 90);
    pdf.get_object_mut(id).unwrap().as_dict_mut().unwrap().set(
        "CropBox",
        vec![10.into(), 20.into(), 510.into(), 770.into()],
    );
    let uri = "https://doi.org/10.1234/visible-differs?quote=\"value\"&x=1";
    let link=pdf.add_object(dictionary!{"Type"=>"Annot","Subtype"=>"Link","Rect"=>vec![72.into(),710.into(),105.into(),732.into()],"A"=>dictionary!{"S"=>"URI","URI"=>Object::string_literal(uri)}});
    let relative = "relative/path?literal=\0control";
    let relative_link = pdf.add_object(dictionary! {"Subtype"=>"Link", "Rect"=>vec![72.into(),680.into(),105.into(),700.into()], "A"=>dictionary!{"S"=>"URI", "URI"=>Object::string_literal(relative)}});
    let no_rect = pdf.add_object(dictionary! {"Subtype"=>"Link", "A"=>dictionary!{"S"=>"URI", "URI"=>Object::string_literal("urn:missing-rectangle")}});
    let go_to = pdf.add_object(dictionary! {"Subtype"=>"Link", "Rect"=>vec![72.into(),650.into(),105.into(),670.into()], "A"=>dictionary!{"S"=>"GoTo", "D"=>vec![Object::Reference(id), Object::Name(b"Fit".to_vec())]}});
    let catalog = pdf.trailer.get(b"Root").unwrap().as_reference().unwrap();
    pdf.get_object_mut(catalog)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set(
            "URI",
            dictionary! {"Base"=>Object::string_literal("https://doi.org/")},
        );
    pdf.get_object_mut(id).unwrap().as_dict_mut().unwrap().set(
        "Annots",
        vec![
            Object::Reference(link),
            Object::Reference(relative_link),
            Object::Reference(go_to),
            Object::Reference(no_rect),
        ],
    );
    let mut bytes = Vec::new();
    pdf.save_to(&mut bytes).unwrap();
    let extractor = backend();
    let identity = extractor.identity();
    let mut session = extractor.open(&bytes, None).unwrap();
    drop(bytes); // Provider must retain its own immutable input after open returns.
    assert_eq!(session.page_count(), 1);
    let page = session.page_text(1).unwrap();
    assert_eq!((page.width, page.height, page.rotation), (500.0, 750.0, 90));
    assert!(
        page.links
            .iter()
            .any(|link| link.uri == "urn:missing-rectangle" && link.bbox.is_none())
    );
    assert!(
        page.spans
            .iter()
            .any(|s| s.text.contains("probe") && s.bbox.is_some() && s.font.is_some())
    );
    assert_eq!(
        page.links.len(),
        3,
        "GoTo must not masquerade as a URI annotation"
    );
    assert!(
        page.links.iter().any(|link| link.uri == relative),
        "relative URI must not be resolved against catalog Base or truncated at NUL"
    );
    let link = page
        .links
        .iter()
        .find(|l| l.uri == uri)
        .expect("exact annotation URI");
    let bbox = link.bbox.unwrap();
    assert!((bbox.x0 - 72.0).abs() < 0.01 && (bbox.y0 - 710.0).abs() < 0.01);
    assert!(matches!(
        session.page_text(0),
        Err(BackendError::PageRange { .. })
    ));
    assert!(matches!(
        session.page_text(2),
        Err(BackendError::PageRange { .. })
    ));
    assert_eq!(identity, extractor.identity());
}

#[test]
#[ignore = "requires separately licensed MuPDF provider and runtime paths"]
fn independent_native_sessions_and_malformed_input() {
    let extractor = backend();
    let bytes = tpe::backend::probe_pdf().unwrap();
    let mut first = extractor.open(&bytes, None).unwrap();
    let mut second = extractor.open(&bytes, None).unwrap();
    assert_eq!(
        first.page_text(1).unwrap().spans,
        second.page_text(1).unwrap().spans
    );
    drop(first);
    assert!(!second.page_text(1).unwrap().spans.is_empty());
    assert!(extractor.open(b"not a PDF", None).is_err());
}

#[cfg(feature = "poppler")]
#[test]
#[ignore = "requires separately licensed Poppler provider and runtime paths"]
fn poppler_uses_the_same_validated_rust_provider_boundary() {
    let extractor = NativeProviderBackend::new(Engine::Poppler);
    let mut pdf = Document::load_mem(&tpe::backend::probe_pdf().unwrap()).unwrap();
    let page_id = *pdf.get_pages().get(&1).unwrap();
    let uri = "https://doi.org/10.1234/poppler-target";
    let annotation = pdf.add_object(dictionary! {
        "Subtype" => "Link", "Rect" => vec![72.into(), 710.into(), 105.into(), 732.into()],
        "A" => dictionary! {"S" => "URI", "URI" => Object::string_literal(uri)},
    });
    pdf.get_object_mut(page_id)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Annots", vec![Object::Reference(annotation)]);
    let mut bytes = Vec::new();
    pdf.save_to(&mut bytes).unwrap();
    let mut session = extractor.open(&bytes, None).unwrap();
    drop(bytes);
    let page = session.page_text(1).unwrap();
    assert!(
        page.spans
            .iter()
            .any(|span| span.text.contains("probe") && span.font.is_some())
    );
    let link = page.links.iter().find(|link| link.uri == uri).unwrap();
    let rectangle = link.bbox.unwrap();
    assert!((rectangle.x0 - 72.0).abs() < 0.01);
    assert!((rectangle.y0 - 710.0).abs() < 0.01);
    assert!(extractor.open(b"not a PDF", None).is_err());
}
