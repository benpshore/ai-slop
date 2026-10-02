//! Real native text-page evidence and controller/worker limit outcomes.
#![cfg(all(feature = "pdfium", target_os = "linux"))]

use std::path::Path;
use std::process::Command;

use lopdf::{Document, Object, Stream, dictionary};
use tpe::pdfium_probe::{Outcome, Report};

fn fixture() -> Vec<u8> {
    let mut doc = Document::with_version("1.7");
    let tree = doc.new_object_id();
    let font =
        doc.add_object(dictionary! { "Type"=>"Font", "Subtype"=>"Type1", "BaseFont"=>"Helvetica" });
    let leaf = doc.add_object(Stream::new(dictionary! {
        "Type"=>"XObject", "Subtype"=>"Form", "BBox"=>vec![0.into(),0.into(),200.into(),300.into()],
        "Matrix"=>vec![1.into(),0.into(),0.into(),1.into(),5.into(),7.into()],
        "Resources"=>dictionary! { "Font"=>dictionary! { "F1"=>font } },
    }, b"BT /F1 12 Tf 1 0 0 1 10 20 Tm (Nested) Tj ET".to_vec()));
    let middle = doc.add_object(Stream::new(dictionary! {
        "Type"=>"XObject", "Subtype"=>"Form", "BBox"=>vec![0.into(),0.into(),200.into(),300.into()],
        "Resources"=>dictionary! { "XObject"=>dictionary! { "A"=>leaf } },
    }, b"/A Do q 1 0 0 1 40 0 cm /A Do Q".to_vec()));
    let content = doc.add_object(Stream::new(
        dictionary! {},
        b"/B Do q 1 0 0 1 0 80 cm /B Do Q".to_vec(),
    ));
    let mut kids = Vec::new();
    for rotation in [0, 90] {
        let page = doc.add_object(dictionary! {
            "Type"=>"Page", "Parent"=>tree, "Contents"=>content, "Rotate"=>rotation,
            "Resources"=>dictionary! { "XObject"=>dictionary! { "B"=>middle } },
        });
        kids.push(Object::Reference(page));
    }
    doc.objects.insert(
        tree,
        Object::Dictionary(dictionary! {
            "Type"=>"Pages", "Kids"=>kids, "Count"=>2,
            "MediaBox"=>vec![0.into(),0.into(),200.into(),300.into()],
        }),
    );
    let catalog = doc.add_object(dictionary! { "Type"=>"Catalog", "Pages"=>tree });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

fn probe(input: &Path, args: &[&str], library: Option<&str>) -> Report {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tpe-pdfium-probe"));
    command.arg("run").arg(input).args(args);
    if let Some(library) = library {
        command.env("PDFIUM_DYNAMIC_LIB_PATH", library);
    }
    let output = command.output().unwrap();
    let report: Report = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid report: {error}; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(output.status.success(), report.outcome == Outcome::Complete);
    if let Some(pid) = report.worker_pid {
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "worker still alive/unreaped"
        );
    }
    report
}

fn assert_library_identity(report: &Report) {
    let configured = std::path::PathBuf::from(std::env::var_os("PDFIUM_DYNAMIC_LIB_PATH").unwrap());
    let library = if configured.is_file() {
        configured
    } else {
        configured.join("libpdfium.so")
    };
    assert_eq!(
        report.backend.as_ref().unwrap().library_sha256,
        tpe::schema::sha256_hex(&std::fs::read(library).unwrap())
    );
}

#[test]
fn unavailable_library_is_explicit_and_controller_is_not_poisoned() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("input.pdf");
    std::fs::write(&path, fixture()).unwrap();
    let result = probe(&path, &[], Some("/nonexistent/pdfium-probe-library"));
    assert_eq!(result.outcome, Outcome::Unavailable);
    assert!(result.pages.is_empty());
    assert!(result.applied_limits.is_some());
}

#[test]
fn nested_forms_return_native_character_geometry_and_explicit_limits() {
    if std::env::var_os("PDFIUM_DYNAMIC_LIB_PATH").is_none() {
        eprintln!("skipped: set PDFIUM_DYNAMIC_LIB_PATH to run real PDFium evidence test");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("input.pdf");
    let bytes = fixture();
    std::fs::write(&path, &bytes).unwrap();
    let result = probe(&path, &[], None);
    assert_eq!(result.outcome, Outcome::Complete, "{:?}", result.detail);
    if let Some(dir) = std::env::var_os("TPE_PDFIUM_PROBE_EVIDENCE_DIR") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("nested-forms.pdf"), &bytes).unwrap();
        std::fs::write(
            dir.join("evidence.json"),
            serde_json::to_vec_pretty(&result).unwrap(),
        )
        .unwrap();
    }
    assert_eq!(
        result.input_sha256.as_deref(),
        Some(tpe::schema::sha256_hex(&bytes).as_str())
    );
    assert_eq!(result.total_pages, Some(2));
    assert_eq!(result.pages.len(), 2);
    assert_eq!(result.pages[1].rotation_degrees, 90);
    let applied = result.applied_limits.as_ref().unwrap();
    assert!(applied.address_space_bytes <= 512 * 1024 * 1024);
    assert!(applied.cpu_seconds <= 15);
    assert!(applied.file_bytes <= 32 * 1024 * 1024);
    assert_eq!(applied.core_bytes, 0);
    assert_library_identity(&result);
    for page in &result.pages {
        let letters: String = page
            .characters
            .iter()
            .filter_map(|c| c.unicode_scalar)
            .filter(char::is_ascii_alphabetic)
            .collect();
        assert_eq!(letters, "NestedNestedNestedNested");
        let mut origins: Vec<_> = page
            .characters
            .iter()
            .filter(|c| c.unicode_scalar == Some('N'))
            .map(|c| c.origin.unwrap())
            .collect();
        origins.sort_by(|a, b| a[1].total_cmp(&b[1]).then(a[0].total_cmp(&b[0])));
        for (actual, expected) in
            origins
                .iter()
                .zip([[15.0, 27.0], [55.0, 27.0], [15.0, 107.0], [55.0, 107.0]])
        {
            assert!(
                (actual[0] - expected[0]).abs() < 0.01 && (actual[1] - expected[1]).abs() < 0.01,
                "{actual:?}"
            );
        }
        assert!(
            page.characters
                .iter()
                .filter(|c| c.unicode_scalar == Some('N'))
                .all(|c| c.tight_bounds.is_some())
        );
    }
    for args in [
        vec!["--max-pages", "1"],
        vec!["--max-chars", "1"],
        vec!["--max-output-bytes", "4096"],
    ] {
        let limited = probe(&path, &args, None);
        assert_eq!(limited.outcome, Outcome::Limited, "{:?}", limited.detail);
        assert!(limited.pages.is_empty());
    }
    // A 16-bit wrapper count would wrap this to zero. The native count must
    // reject it before loading any page or claiming a complete empty result.
    let mut large = Document::with_version("1.7");
    let pages = large.new_object_id();
    let page = large.add_object(dictionary! { "Type"=>"Page", "Parent"=>pages,
    "MediaBox"=>vec![0.into(),0.into(),100.into(),100.into()] });
    large.objects.insert(
        pages,
        Object::Dictionary(dictionary! {
            "Type"=>"Pages", "Count"=>65_536, "Kids"=>vec![Object::Reference(page);65_536],
        }),
    );
    let catalog = large.add_object(dictionary! { "Type"=>"Catalog", "Pages"=>pages });
    large.trailer.set("Root", catalog);
    large.save(&path).unwrap();
    let limited = probe(&path, &[], None);
    assert_eq!(limited.outcome, Outcome::Limited);
    assert_eq!(limited.total_pages, Some(65_536));
    std::fs::write(&path, b"not a PDF").unwrap();
    assert_eq!(probe(&path, &[], None).outcome, Outcome::WorkerFailed);
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(probe(&path, &[], None).outcome, Outcome::Complete);

    // Hashing a bounded 32 MiB snapshot in the child outlasts a 1 ms deadline.
    let mut padded = bytes;
    padded.resize(32 * 1024 * 1024, b' ');
    std::fs::write(&path, padded).unwrap();
    let timed_out = probe(&path, &["--timeout-ms", "1"], None);
    assert_eq!(timed_out.outcome, Outcome::Timeout);
    assert!(timed_out.pages.is_empty());
}
